//! Runtime hardening: execution log and log lines, mutation timeout, write-time and startup schema validation.
//! (Mutation idempotency and logLines over the wire are tested end to end in `websocket.rs`.)

use std::time::{Duration, Instant};

use ferrix::db::{Database, OpenOptions};
use ferrix::testing::TestApp;
use ferrix::{App, AppOptions, FunctionRef, LogFilter, LogOutcome, MutationTimeout, TableDef};
use serde_json::json;

use crate::common::{items, todos};

/// Functions defined with the real macros.
pub mod fns {
    use ferrix::log;
    use ferrix::prelude::*;

    use crate::common::todos::{self, Todo};

    #[query]
    pub async fn logged_query(_ctx: &QueryCtx, n: u32) -> Result<u32> {
        log::info!(n, "querying");
        log::debug!("details");
        log::trace!("too fine to capture");
        Ok(n)
    }

    #[mutation]
    pub async fn logged_add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
        log::info!("adding {text}");
        ctx.db.insert(Todo { text, done: false, created_by: None }).await
    }

    #[mutation]
    pub async fn logged_fail(_ctx: &MutationCtx) -> Result<()> {
        log::warn!("about to fail");
        Err(UserError::new("nope", "failed on purpose").into())
    }

    /// Logs, then calls a mutation and a query that log too.
    #[action]
    pub async fn logged_action(ctx: &ActionCtx, text: String) -> Result<u32> {
        log::warn!("action start");
        ctx.run_mutation::<logged_add>((text,)).await?;
        let n = ctx.run_query::<logged_query>((7,)).await?;
        log::error!(code = 42, "action end");
        Ok(n)
    }

    /// Inserts, then sleeps (an await point) for `sleep_ms`.
    #[mutation]
    pub async fn slow_add(ctx: &MutationCtx, text: String, sleep_ms: u64) -> Result<()> {
        ctx.db.insert(Todo { text, done: false, created_by: None }).await?;
        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
        Ok(())
    }

    /// Patches `done` with a value of the wrong type.
    #[mutation]
    pub async fn bad_patch(ctx: &MutationCtx, id: Id<Todo>) -> Result<()> {
        ctx.db.patch(id, json!({ "done": "yes" })).await
    }

    /// A patch removing a required field (null for a non-Option field).
    #[mutation]
    pub async fn null_patch(ctx: &MutationCtx, id: Id<Todo>) -> Result<()> {
        ctx.db.patch(id, json!({ "text": null })).await
    }

    #[mutation]
    pub async fn good_patch(ctx: &MutationCtx, id: Id<Todo>) -> Result<()> {
        ctx.db.patch(id, json!({ "createdBy": "me" })).await
    }

    #[mutation]
    pub async fn insert_lossy(ctx: &MutationCtx, n: i64) -> Result<()> {
        ctx.db.insert(super::lossy::Lossy { n }).await.map(drop)
    }

    #[query]
    pub async fn count_todos(ctx: &QueryCtx) -> Result<usize> {
        Ok(ctx.db.query::<todos::Todo>().collect().await?.len())
    }
}

/// A table whose `Serialize` doesn't match its `Deserialize` (it writes `n` as a string): inserts must be rejected.
/// Hand-written, since `#[table]` derives `Serialize` itself.
pub mod lossy {
    #[derive(::ferrix::__private::serde::Deserialize, ::ferrix::__private::ts_rs::TS, Clone, Debug)]
    #[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
    #[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
    pub struct Lossy {
        pub n: i64,
    }

    impl ::ferrix::__private::serde::Serialize for Lossy {
        fn serialize<S: ::ferrix::__private::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use ::ferrix::__private::serde::ser::SerializeStruct;
            let mut st = s.serialize_struct("Lossy", 1)?;
            st.serialize_field("n", &self.n.to_string())?;
            st.end()
        }
    }

    impl ::ferrix::Table for Lossy {
        const NAME: &'static str = "lossy";
        fn table_def() -> ::ferrix::TableDef {
            ::ferrix::TableDef {
                name: "lossy",
                indexes: vec![],
                search_indexes: vec![],
                vector_indexes: vec![],
                ts: || ::ferrix::__private::ts_inline::<Lossy>(),
                validate: ::ferrix::__private::validate_doc::<Lossy>,
            }
        }
    }

    #[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::TABLES)]
    #[linkme(crate = ::ferrix::__private::linkme)]
    #[allow(non_upper_case_globals)]
    static __FERRIX_TABLE_Lossy: fn() -> ::ferrix::TableDef = <Lossy as ::ferrix::Table>::table_def;
}

fn prefix() -> String {
    // "hardening/fns:..."
    let p = fns::logged_query::path();
    p[..p.find(':').unwrap() + 1].to_string()
}

#[tokio::test]
async fn execution_log_records_every_execution_with_log_lines() {
    let t = TestApp::new().await.unwrap();
    let logs = t.app().logs().clone();
    let mut tail = logs.subscribe();

    assert_eq!(t.action::<fns::logged_action>(("hi".to_string(),)).await.unwrap(), 7);
    t.mutation::<fns::logged_fail>(()).await.unwrap_err();
    t.mutation::<todos::add_then_panic>(("p".to_string(),)).await.unwrap_err();
    logs.flush().await.unwrap();

    let all = logs.query(LogFilter { path_prefix: Some(prefix()), ..Default::default() }).await.unwrap();
    let summary: Vec<(String, &str, LogOutcome)> =
        all.iter().map(|e| (e.path.clone(), e.kind.as_str(), e.outcome)).collect();
    assert_eq!(
        summary,
        vec![
            (fns::logged_fail::path(), "mutation", LogOutcome::UserError),
            (fns::logged_action::path(), "action", LogOutcome::Success),
            (fns::logged_query::path(), "query", LogOutcome::Success),
            (fns::logged_add::path(), "mutation", LogOutcome::Success),
        ],
        "newest first; nested calls are logged as executions of their own"
    );
    let (fail, action, query, add) = (&all[0], &all[1], &all[2], &all[3]);
    assert_eq!(add.log_lines, vec!["[INFO] adding hi"]);
    assert_eq!(add.args, r#"{"text":"hi"}"#);
    assert!(!add.args_truncated);
    assert_eq!(query.log_lines, vec!["[INFO] querying n=7", "[DEBUG] details"]);
    assert_eq!(
        action.log_lines,
        vec!["[WARN] action start", "[INFO] adding hi", "[INFO] querying n=7", "[DEBUG] details", "[ERROR] action end code=42"],
        "an action's lines include those of the functions it called, in order"
    );
    assert!(action.duration_us >= query.duration_us && action.duration_us >= add.duration_us);
    assert_eq!(action.identity, None, "anonymous");
    assert_eq!((fail.error.as_deref(), fail.log_lines.clone()), (Some("failed on purpose"), vec!["[WARN] about to fail".to_string()]));
    assert!(all.iter().all(|e| e.timestamp > 0 && e.id > 0));

    let panicked = logs.query(LogFilter { outcome: Some(LogOutcome::ServerError), ..Default::default() }).await.unwrap();
    assert_eq!(panicked.len(), 1);
    assert_eq!(panicked[0].path, todos::add_then_panic::path());
    assert_eq!(panicked[0].error.as_deref(), Some("function panicked: boom"));

    // The live tail saw the same entries, oldest first, with ids.
    let first = tail.recv().await.unwrap();
    assert_eq!((first.id, first.path.clone()), (add.id, add.path.clone()));

    // Per-function stats.
    let stats = logs.stats(0).await.unwrap();
    let s = stats.iter().find(|s| s.path == fns::logged_fail::path()).unwrap();
    assert_eq!((s.calls, s.errors, s.user_errors, s.server_errors), (1, 1, 1, 0));
    let s = stats.iter().find(|s| s.path == fns::logged_add::path()).unwrap();
    assert_eq!((s.calls, s.errors), (1, 0));
    assert!(s.p50_us > 0 && s.p95_us >= s.p50_us);
    assert_eq!(logs.dropped(), 0);
}

#[tokio::test]
async fn execution_log_survives_restart_and_truncates_args() {
    let dir = tempfile::tempdir().unwrap();
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        app.run_mutation(&todos::add::path(), json!({ "text": "x".repeat(5000) }), &Default::default()).await.unwrap();
        app.logs().flush().await.unwrap();
    }
    assert!(dir.path().join("aux.db").exists(), "the execution log lives in its own file");
    let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
    let entries = app.logs().query(LogFilter::default()).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].args_truncated);
    assert_eq!(entries[0].args.len(), ferrix::exec_log::MAX_ARGS_BYTES);
}

#[tokio::test]
async fn mutation_timeout_rolls_back_and_frees_the_writer() {
    let options = AppOptions { dev: true, mutation_timeout: Duration::from_millis(200), ..Default::default() };
    let t = TestApp::with_options(options).await.unwrap();

    let start = Instant::now();
    let err = t.mutation::<fns::slow_add>(("slow".to_string(), 30_000)).await.unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(5), "timed out promptly");
    let timeout = err.downcast_ref::<MutationTimeout>().expect("a MutationTimeout");
    assert_eq!((timeout.path.as_str(), timeout.timeout), (fns::slow_add::path().as_str(), Duration::from_millis(200)));
    assert!(err.to_string().contains("timed out after 200ms and was rolled back"), "{err}");
    assert_eq!(t.query::<fns::count_todos>(()).await.unwrap(), 0, "the insert was rolled back");

    // The writer lock is free again, and a mutation that awaits less than the timeout commits.
    t.mutation::<todos::add>(("after".to_string(),)).await.unwrap();
    t.mutation::<fns::slow_add>(("quick".to_string(), 10)).await.unwrap();
    assert_eq!(t.query::<fns::count_todos>(()).await.unwrap(), 2);

    // The execution log has the timeout as a server error with its message.
    t.app().logs().flush().await.unwrap();
    let failed = t
        .app()
        .logs()
        .query(LogFilter { outcome: Some(LogOutcome::ServerError), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].error.as_deref().unwrap().contains("timed out"));
    assert!(failed[0].duration_us >= 200_000);
}

#[tokio::test]
async fn writes_are_validated_against_the_table_type() {
    let t = TestApp::new().await.unwrap();
    let id = t.mutation::<todos::add>(("a".to_string(),)).await.unwrap();

    let err = t.mutation::<fns::bad_patch>((id,)).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains(&format!("patched document {id} does not match table `todos`")), "{msg}");
    assert!(msg.contains("invalid type: string \"yes\", expected a boolean"), "{msg}");
    let err = t.mutation::<fns::null_patch>((id,)).await.unwrap_err();
    assert!(format!("{err:#}").contains("invalid type: null, expected a string"), "{err:#}");
    let doc = t.query::<todos::get>((id,)).await.unwrap().unwrap();
    assert_eq!((doc.text.as_str(), doc.done, doc.created_by.clone()), ("a", false, None), "nothing was written");

    t.mutation::<fns::good_patch>((id,)).await.unwrap();
    assert_eq!(t.query::<todos::get>((id,)).await.unwrap().unwrap().created_by.as_deref(), Some("me"));

    // insert/replace: the stored JSON must deserialize back.
    let err = t.mutation::<fns::insert_lossy>((5,)).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("inserted document") && msg.contains("does not match table `lossy`"), "{msg}");
    assert!(msg.contains("expected i64"), "{msg}");
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

/// `items` with a different TS type (so a new fingerprint) and a validator that rejects scores >= 5.
fn strict_items(ts: fn() -> String) -> TableDef {
    let mut def = <items::Item as ferrix::Table>::table_def();
    def.ts = ts;
    def.validate = |doc| match doc["score"].as_i64() {
        Some(s) if s >= 5 => Err(format!("score {s} is too high")),
        _ => Ok(()),
    };
    def
}

#[tokio::test]
async fn startup_refuses_documents_that_no_longer_match() {
    let dir = tempfile::tempdir().unwrap();
    let bad_ids = {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        for v in 0..30 {
            app.mutation::<items::insert>((1, v, format!("n{v}"))).await.unwrap();
        }
        let docs = app.query::<items::newest>((100u32,)).await.unwrap();
        let mut bad: Vec<String> = docs.iter().filter(|d| d.score >= 5).map(|d| d.id.to_string()).collect();
        bad.sort();
        bad
    };
    assert_eq!(bad_ids.len(), 25);
    let open = |def: TableDef, skip: bool| {
        Database::open_with(dir.path(), &[def], &OpenOptions { readers: 1, skip_schema_check: skip }).map(drop)
    };

    let err = open(strict_items(|| "{ changed: true }".into()), false).unwrap_err().to_string();
    assert!(err.contains("schema check failed"), "{err}");
    assert!(err.contains("table `items`: 25 of 30 document(s) do not match (showing the first 20)"), "{err}");
    // Ids are listed in id order, each with its error.
    for id in &bad_ids[..20] {
        assert!(err.contains(&format!("  {id}: score ")), "{id} missing in {err}");
    }
    assert!(!err.contains(&bad_ids[20]), "at most 20 per table");
    assert!(err.contains("--skip-schema-check") && err.contains("Option<T>"), "{err}");

    // The escape hatch opens anyway but records nothing: the next normal start checks again.
    open(strict_items(|| "{ changed: true }".into()), true).unwrap();
    assert!(open(strict_items(|| "{ changed: true }".into()), false).is_err());

    // A passing check stores the fingerprint...
    let mut lenient = strict_items(|| "{ changed: true }".into());
    lenient.validate = |_| Ok(());
    open(lenient, false).unwrap();
    // ...so an unchanged fingerprint is not re-validated (startup stays fast), even with a stricter validator.
    open(strict_items(|| "{ changed: true }".into()), false).unwrap();
    // An index change is a schema change too.
    let mut reindexed = strict_items(|| "{ changed: true }".into());
    reindexed.indexes.pop();
    assert!(open(reindexed, false).is_err());

    // Through App::open with the real types everything matches (and the option is plumbed through).
    App::open(dir.path(), AppOptions::default()).await.unwrap();
    App::open(dir.path(), AppOptions { skip_schema_check: true, ..Default::default() }).await.unwrap();
}

//! `#[migration]` against the real runtime: dry runs, resuming after a crash between batches (each document exactly
//! once), failures and reruns, cancel, the admin system functions, and an app whose table has old-schema documents
//! starting with the migration pending (lenient writes), running it, and passing the schema check afterwards.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ferrix::migrations::{MigrationRef, MigrationState, RunOptions, RunOutcome};
use ferrix::testing::TestApp;
use ferrix::{App, FunctionRef, Identity};
use serde_json::{Value, json};

use crate::shared::Client;

pub mod m {
    use super::*;
    use ferrix::prelude::*;

    // ---- tasks: the old-schema app (documents written before `priority` existed).

    #[table(name = "mig_tasks")]
    pub struct Task {
        pub title: String,
        pub priority: i64,
    }

    #[mutation]
    pub async fn add_task(ctx: &MutationCtx, title: String, priority: i64) -> Result<Id<Task>> {
        ctx.db.insert(Task { title, priority }).await
    }

    #[mutation]
    pub async fn rename_task(ctx: &MutationCtx, id: Id<Task>, title: String) -> Result<()> {
        ctx.db.patch(id, json!({ "title": title })).await
    }

    #[mutation]
    pub async fn set_priority_raw(ctx: &MutationCtx, id: Id<Task>, priority: String) -> Result<()> {
        ctx.db.patch(id, json!({ "priority": priority })).await
    }

    /// Typed: fails while documents don't match.
    #[query]
    pub async fn tasks(ctx: &QueryCtx) -> Result<Vec<Doc<Task>>> {
        ctx.db.query::<Task>().collect().await
    }

    /// Untyped: each task's priority (null when missing), in creation order.
    #[query]
    pub async fn priorities(ctx: &QueryCtx) -> Result<Vec<Option<i64>>> {
        let docs = ctx.db.query_table("mig_tasks").collect().await?;
        Ok(docs.into_iter().map(|d| d.get("priority").and_then(Value::as_i64)).collect())
    }

    /// The old documents have no priority: derive it from the title.
    #[migration(table = Task, batch = 2, name = "0001_task_priority")]
    pub async fn task_priority(_ctx: &MutationCtx, doc: Value) -> Result<Option<Value>> {
        if doc.get("priority").is_some() {
            return Ok(None);
        }
        let urgent = doc["title"].as_str().unwrap_or_default().starts_with('!');
        Ok(Some(json!({ "priority": if urgent { 10 } else { 1 } })))
    }

    // ---- counted: resume after a crash; every document exactly once per run.

    #[table(name = "mig_counted")]
    pub struct Counted {
        pub n: i64,
        pub visits: i64,
    }

    #[mutation]
    pub async fn add_counted(ctx: &MutationCtx, n: i64) -> Result<Id<Counted>> {
        ctx.db.insert(Counted { n, visits: 0 }).await
    }

    #[query]
    pub async fn counted(ctx: &QueryCtx) -> Result<Vec<Doc<Counted>>> {
        ctx.db.query::<Counted>().collect().await
    }

    /// Typed variant: the old shape still deserializes.
    #[migration(table = Counted, batch = 3)]
    pub async fn count_visits(_ctx: &MutationCtx, doc: Doc<Counted>) -> Result<DocChange> {
        Ok(DocChange::Patch(json!({ "visits": doc.visits + 1 })))
    }

    // ---- flaky: a migration that fails on one document until "fixed".

    pub static FIXED: AtomicBool = AtomicBool::new(false);

    #[table(name = "mig_flaky")]
    pub struct Flaky {
        pub label: String,
        pub migrated: bool,
    }

    #[mutation]
    pub async fn add_flaky(ctx: &MutationCtx, label: String) -> Result<Id<Flaky>> {
        ctx.db.insert(Flaky { label, migrated: false }).await
    }

    #[query]
    pub async fn flaky(ctx: &QueryCtx) -> Result<Vec<Doc<Flaky>>> {
        ctx.db.query::<Flaky>().collect().await
    }

    #[migration(table = Flaky, batch = 2, name = "flaky")]
    pub async fn migrate_flaky(_ctx: &MutationCtx, doc: Doc<Flaky>) -> Result<Option<Value>> {
        if doc.label == "bad" && !FIXED.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("cannot migrate `{}`", doc.label));
        }
        Ok(Some(json!({ "migrated": true })))
    }

    // ---- dry: every kind of change, plus a side effect through ctx.db (rolled back in dry runs).

    #[table(name = "mig_dry")]
    pub struct Dry {
        pub v: i64,
        pub doubled: Option<i64>,
    }

    #[table(name = "mig_dry_log")]
    pub struct DryLog {
        pub seen: i64,
    }

    #[mutation]
    pub async fn add_dry(ctx: &MutationCtx, v: i64) -> Result<Id<Dry>> {
        ctx.db.insert(Dry { v, doubled: None }).await
    }

    #[query]
    pub async fn dry_docs(ctx: &QueryCtx) -> Result<Vec<Doc<Dry>>> {
        ctx.db.query::<Dry>().collect().await
    }

    #[query]
    pub async fn dry_logs(ctx: &QueryCtx) -> Result<u32> {
        Ok(ctx.db.query::<DryLog>().collect().await?.len() as u32)
    }

    #[migration(table = Dry, name = "dry")]
    pub async fn migrate_dry(ctx: &MutationCtx, doc: Value) -> Result<DocChange> {
        let v = doc["v"].as_i64().unwrap_or_default();
        ctx.db.insert(DryLog { seen: v }).await?;
        Ok(match v {
            0 => DocChange::Delete,
            v if v < 0 => DocChange::Replace(json!({ "_id": doc["_id"], "v": -v, "doubled": null })),
            v => DocChange::Patch(json!({ "doubled": v * 2 })),
        })
    }

    // ---- slow: canceled during a run.

    #[table(name = "mig_slow")]
    pub struct Slow {
        pub done: bool,
    }

    #[mutation]
    pub async fn add_slow(ctx: &MutationCtx) -> Result<Id<Slow>> {
        ctx.db.insert(Slow { done: false }).await
    }

    #[migration(table = Slow, batch = 1, name = "slow")]
    pub async fn migrate_slow(_ctx: &MutationCtx, _doc: Value) -> Result<Option<Value>> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok(Some(json!({ "done": true })))
    }
}

use m::*;

fn admin() -> Identity {
    Identity { admin_id: Some(ferrix::id::DocId::new_random(1).encode()), ..Default::default() }
}

async fn summary(app: &App, name: &str) -> ferrix::migrations::MigrationSummary {
    app.migration_summaries().await.unwrap().into_iter().find(|m| m.name == name).unwrap()
}

/// Make the stored documents of `table` look written by an older version: apply `f` to each document's JSON and
/// forget the table's schema fingerprint (so the next start validates it).
fn rewrite_documents(dir: &std::path::Path, table: &str, f: impl Fn(&mut serde_json::Map<String, Value>)) {
    let conn = rusqlite::Connection::open(dir.join("data.db")).unwrap();
    let number: u32 = conn.query_row("SELECT table_number FROM _tables WHERE name = ?1", [table], |r| r.get(0)).unwrap();
    let rows: Vec<(Vec<u8>, String)> = conn
        .prepare("SELECT id, json FROM documents WHERE table_number = ?1")
        .unwrap()
        .query_map([number], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (id, json) in rows {
        let mut doc: serde_json::Map<String, Value> = serde_json::from_str(&json).unwrap();
        f(&mut doc);
        conn.execute("UPDATE documents SET json = ?1 WHERE id = ?2", rusqlite::params![serde_json::to_string(&doc).unwrap(), id])
            .unwrap();
    }
    conn.execute("DELETE FROM _schema_fingerprints WHERE table_name = ?1", [table]).unwrap();
}

#[tokio::test]
async fn names_tables_and_registry() {
    assert_eq!(task_priority::name(), "0001_task_priority");
    assert_eq!(task_priority::table(), "mig_tasks");
    assert_eq!(count_visits::name(), "migrations/m:count_visits");
    let registry = ferrix::migrations::registry().unwrap();
    assert_eq!(registry.get("flaky").unwrap().batch_size, 2);
    assert_eq!(registry.get("migrations/m:count_visits").unwrap().batch_size, 3);
    assert_eq!(registry.get("dry").unwrap().batch_size, 100);
    // Declared migrations are listed as pending before they ever ran.
    let t = TestApp::new().await.unwrap();
    let s = summary(t.app(), "flaky").await;
    assert_eq!((s.state, s.declared, s.processed, s.batch_size), (MigrationState::Pending, true, 0, Some(2)));
}

#[tokio::test]
async fn dry_run_reports_and_writes_nothing() {
    let t = TestApp::new().await.unwrap();
    for v in [1, 2, 0, -3] {
        t.mutation::<add_dry>((v,)).await.unwrap();
    }
    let before = t.query::<dry_docs>(()).await.unwrap();
    let r = t.app().run_migration::<migrate_dry>(RunOptions { dry_run: true, ..Default::default() }).await.unwrap();
    assert_eq!((r.outcome, r.processed, r.changed, r.batches), (RunOutcome::DryRun, 4, 4, 1));
    assert_eq!(r.examples.len(), 4);
    let after: Vec<Option<Value>> = r.examples.iter().map(|e| e.after.clone()).collect();
    assert_eq!(after[0].as_ref().unwrap()["doubled"], json!(2));
    assert_eq!(after[2], None, "v = 0 would be deleted");
    assert_eq!(after[3].as_ref().unwrap()["v"], json!(3), "replaced");
    assert_eq!(r.examples[3].before["v"], json!(-3));

    // Nothing was written: no document changed, no side effect through ctx.db, no _migrations row.
    let unchanged = t.query::<dry_docs>(()).await.unwrap();
    assert_eq!(serde_json::to_value(&unchanged).unwrap(), serde_json::to_value(&before).unwrap());
    assert_eq!(t.query::<dry_logs>(()).await.unwrap(), 0);
    assert_eq!(summary(t.app(), "dry").await.state, MigrationState::Pending);

    // The real run does what the dry run said.
    let r = t.app().run_migration::<migrate_dry>(RunOptions::default()).await.unwrap();
    assert_eq!((r.outcome, r.processed, r.changed), (RunOutcome::Completed, 4, 4));
    let docs = t.query::<dry_docs>(()).await.unwrap();
    let vs: Vec<(i64, Option<i64>)> = docs.iter().map(|d| (d.v, d.doubled)).collect();
    assert_eq!(vs, vec![(1, Some(2)), (2, Some(4)), (3, None)]);
    assert_eq!(t.query::<dry_logs>(()).await.unwrap(), 4);
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

#[tokio::test]
async fn resumes_after_a_crash_between_batches_processing_each_document_once() {
    let t = TestApp::new().await.unwrap();
    for n in 0..10 {
        t.mutation::<add_counted>((n,)).await.unwrap();
    }
    // Two batches of 3, then the process "dies" (the run stops with the migration still running).
    let r = t.app().run_migration::<count_visits>(RunOptions { max_batches: Some(2), ..Default::default() }).await.unwrap();
    assert_eq!((r.outcome, r.batches, r.processed), (RunOutcome::Stopped, 2, 6));
    let s = summary(t.app(), &count_visits::name()).await;
    assert_eq!((s.state, s.processed, s.documents), (MigrationState::Running, 6, 10));

    // After the restart the row is pending again (not stuck as running) and the next run resumes at the cursor.
    let t = t.restart().await.unwrap();
    assert_eq!(summary(t.app(), &count_visits::name()).await.state, MigrationState::Pending);
    let r = t.app().run_migration::<count_visits>(RunOptions::default()).await.unwrap();
    assert_eq!((r.outcome, r.processed, r.batches), (RunOutcome::Completed, 4, 2));
    let visits: Vec<i64> = t.query::<counted>(()).await.unwrap().iter().map(|d| d.visits).collect();
    assert_eq!(visits, vec![1; 10], "each document exactly once");
    let s = summary(t.app(), &count_visits::name()).await;
    assert_eq!((s.state, s.processed, s.changed), (MigrationState::Done, 10, 10));
    assert!(s.finished_at.is_some());

    // Done means done; a restart from scratch runs it again.
    let r = t.app().run_migration::<count_visits>(RunOptions::default()).await.unwrap();
    assert_eq!(r.outcome, RunOutcome::AlreadyDone);
    let r = t.app().run_migration::<count_visits>(RunOptions { restart: true, ..Default::default() }).await.unwrap();
    assert_eq!((r.outcome, r.processed), (RunOutcome::Completed, 10));
    let visits: Vec<i64> = t.query::<counted>(()).await.unwrap().iter().map(|d| d.visits).collect();
    assert_eq!(visits, vec![2; 10]);
}

#[tokio::test]
async fn a_failing_document_fails_the_migration_and_a_rerun_resumes_after_the_fix() {
    let t = TestApp::new().await.unwrap();
    let mut ids = Vec::new();
    for label in ["a", "b", "bad", "c", "d"] {
        ids.push(t.mutation::<add_flaky>((label.to_string(),)).await.unwrap());
    }
    let r = t.app().run_migration::<migrate_flaky>(RunOptions::default()).await.unwrap();
    assert_eq!(r.outcome, RunOutcome::Failed);
    assert_eq!(r.failed_document.as_deref(), Some(ids[2].to_string().as_str()));
    assert!(r.error.as_deref().unwrap().contains("cannot migrate `bad`"), "{:?}", r.error);
    let s = summary(t.app(), "flaky").await;
    assert_eq!((s.state, s.processed), (MigrationState::Failed, 2), "the failing batch was rolled back");
    assert_eq!(s.failed_document.as_deref(), Some(ids[2].to_string().as_str()));
    assert!(s.error.as_deref().unwrap().contains("cannot migrate `bad`"));
    let migrated: Vec<bool> = t.query::<flaky>(()).await.unwrap().iter().map(|d| d.migrated).collect();
    assert_eq!(migrated, vec![true, true, false, false, false]);
    // The batch's execution is in the log as a failed system mutation.
    t.app().logs().flush().await.unwrap();
    let failed = t
        .app()
        .logs()
        .query(ferrix::LogFilter { path_prefix: Some("_system/migrations:run_batch".into()), ..Default::default() })
        .await
        .unwrap();
    assert!(failed.iter().any(|e| e.error.as_deref().is_some_and(|e| e.contains("cannot migrate"))));

    // Rerun after fixing the code: resumes at the failed batch.
    FIXED.store(true, Ordering::SeqCst);
    let r = t.app().run_migration::<migrate_flaky>(RunOptions::default()).await.unwrap();
    assert_eq!((r.outcome, r.processed), (RunOutcome::Completed, 3));
    let s = summary(t.app(), "flaky").await;
    assert_eq!((s.state, s.processed, s.error, s.failed_document), (MigrationState::Done, 5, None, None));
    assert!(t.query::<flaky>(()).await.unwrap().iter().all(|d| d.migrated));
}

#[tokio::test]
async fn admin_functions_list_run_status_and_cancel() {
    let t = TestApp::new().await.unwrap();
    for _ in 0..6 {
        t.mutation::<add_slow>(()).await.unwrap();
    }
    let app = t.app();
    let path = |f: &str| format!("_system/migrations:{f}");

    // Admin only.
    let err = app.run_query(&path("list"), json!({}), &Identity::default()).await.result.unwrap_err();
    assert!(format!("{err:#}").contains("signed in as an administrator"), "{err:#}");

    let list = app.run_query(&path("list"), json!({}), &admin()).await.result.unwrap();
    let slow = list.as_array().unwrap().iter().find(|m| m["name"] == "slow").unwrap().clone();
    assert_eq!(slow["state"], "pending");
    assert_eq!(slow["table"], "mig_slow");
    assert_eq!(slow["documents"], 6);
    assert_eq!(slow["batchSize"], 1);

    // A dry run through `run` returns the report.
    let res = app.run_action(&path("run"), json!({ "name": "slow", "dryRun": true }), &admin()).await.unwrap();
    assert_eq!(res["started"], json!([]));
    assert_eq!(res["reports"][0]["outcome"], "dryRun");
    assert_eq!(res["reports"][0]["changed"], 6);

    // A real run continues in the background; cancel it once it has made progress.
    let res = app.run_action(&path("run"), json!({ "name": "slow" }), &admin()).await.unwrap();
    assert_eq!(res["started"], json!(["slow"]));
    let mut waited = 0;
    while summary(app, "slow").await.processed < 2 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += 1;
        assert!(waited < 500, "the background run made no progress");
    }
    let canceled = app.run_mutation(&path("cancel"), json!({ "name": "slow" }), &admin()).await.unwrap().0;
    assert_eq!(canceled, "canceled");
    let mut waited = 0;
    let stopped = loop {
        let s = summary(app, "slow").await;
        let processed = s.processed;
        tokio::time::sleep(Duration::from_millis(80)).await;
        if summary(app, "slow").await.processed == processed {
            break s;
        }
        waited += 1;
        assert!(waited < 100, "the run didn't stop");
    };
    assert_eq!(stopped.state, MigrationState::Canceled);
    assert!(stopped.processed < 6, "stopped early: {}", stopped.processed);
    let status = app.run_query(&path("status"), json!({ "name": "slow" }), &admin()).await.result.unwrap();
    assert_eq!(status["state"], "canceled");
    assert_eq!(app.run_mutation(&path("cancel"), json!({ "name": "slow" }), &admin()).await.unwrap().0, "notRunning");

    // Running it again resumes.
    let r = app.run_migration_named("slow", RunOptions::default()).await.unwrap();
    assert_eq!(r.outcome, RunOutcome::Completed);
    assert_eq!(r.processed, 6 - stopped.processed);
    let s = summary(app, "slow").await;
    assert_eq!((s.state, s.processed, s.changed), (MigrationState::Done, 6, 6));

    let err = app.run_action(&path("run"), json!({ "name": "nope" }), &admin()).await.unwrap_err();
    assert!(format!("{err:#}").contains("No migration is named `nope`"));
    assert_eq!(app.run_query(&path("status"), json!({ "name": "nope" }), &admin()).await.result.unwrap(), Value::Null);
}

#[tokio::test]
async fn old_schema_documents_start_with_the_migration_pending_and_pass_the_check_after_it() {
    let t = TestApp::new().await.unwrap();
    let mut ids = Vec::new();
    for title in ["!fix prod", "write docs", "lunch", "!pager"] {
        ids.push(t.mutation::<add_task>((title.to_string(), 5)).await.unwrap());
    }
    // Simulate documents written before `priority` existed (all but the last one).
    let last = ids[3].to_string();
    rewrite_documents(t.dir(), "mig_tasks", |doc| {
        if doc["_id"] != last {
            doc.remove("priority");
        }
    });

    // The schema check fails for `mig_tasks`, but its migration is pending: the app starts, with the table migrating.
    let t = t.restart().await.unwrap();
    let app = t.app().clone();
    assert!(app.db().catalog().is_migrating("mig_tasks"));
    assert_eq!(app.db().catalog().migrating_tables(), vec!["mig_tasks".to_string()]);
    let err = t.query::<tasks>(()).await.unwrap_err();
    assert!(format!("{err:#}").contains("does not match table `mig_tasks`"), "{err:#}");

    // Lenient writes: an old document can still be patched (it stays invalid until migrated)...
    t.mutation::<rename_task>((ids[1], "write more docs".to_string())).await.unwrap();
    // ...but a valid document can't be made invalid, and inserts are strict.
    let err = t.mutation::<set_priority_raw>((ids[3], "high".to_string())).await.unwrap_err();
    assert!(format!("{err:#}").contains("does not match table `mig_tasks`"), "{err:#}");
    let err = app.run_mutation("_system/dashboard:insert_document", json!({ "table": "mig_tasks", "document": { "title": "x" } }), &admin()).await.unwrap_err();
    assert!(format!("{err:#}").contains("does not match table `mig_tasks`"), "{err:#}");
    t.mutation::<add_task>(("new".to_string(), 2)).await.unwrap();

    // Migration writes go through the normal write path: subscriptions update.
    let mut client = Client::new(&app).await;
    let first = client.subscribe(1, &priorities::path(), json!({})).await.unwrap();
    assert_eq!(first, json!([null, null, null, 5, 2]));

    let s = summary(&app, "0001_task_priority").await;
    assert!(s.table_migrating);
    let r = app.run_migration::<task_priority>(RunOptions::default()).await.unwrap();
    assert_eq!((r.outcome, r.processed, r.changed, r.batches), (RunOutcome::Completed, 5, 3, 3));
    assert_eq!(r.schema_check.as_deref(), Some("passed"));
    client.until(|c| c.value(1) == json!([10, 1, 1, 5, 2])).await;

    // The table matches again: strict writes, typed reads work, the fingerprint is recorded.
    assert!(!app.db().catalog().is_migrating("mig_tasks"));
    let titles: Vec<String> = t.query::<tasks>(()).await.unwrap().iter().map(|d| d.title.clone()).collect();
    assert_eq!(titles, vec!["!fix prod", "write more docs", "lunch", "!pager", "new"]);
    let s = summary(&app, "0001_task_priority").await;
    assert_eq!((s.state, s.schema_check.as_deref(), s.table_migrating), (MigrationState::Done, Some("passed"), false));
    drop(client);
    drop(app);
    let t = t.restart().await.unwrap();
    assert!(!t.app().db().catalog().is_migrating("mig_tasks"));
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());

    // Invalid documents again, but the table's only migration is done: the start is refused, suggesting a migration.
    rewrite_documents(t.dir(), "mig_tasks", |doc| {
        doc.remove("title");
    });
    let dir = t.dir().to_path_buf();
    let err = match App::open(&dir, ferrix::AppOptions::default()).await {
        Ok(_) => panic!("opened with invalid documents"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("table `mig_tasks`: 5 of 5 document(s) do not match"), "{err}");
    assert!(err.contains("`0001_task_priority` have all run"), "{err}");
    assert!(err.contains("#[migration(table = \"mig_tasks\")]"), "{err}");
}

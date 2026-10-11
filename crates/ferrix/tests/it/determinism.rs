//! The dev-mode determinism check (`AppOptions::determinism_check`): a query whose rerun at the same ts gives another
//! result is reported, in the server log and as a log line of its run.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ferrix::exec_log::LogFilter;
use ferrix::testing::TestApp;
use ferrix::{AppOptions, FunctionRef, Identity};
use serde_json::json;

static CALLS: AtomicU64 = AtomicU64::new(0);

pub mod fns {
    use super::*;
    use ferrix::log;
    use ferrix::prelude::*;

    /// A different number every run: what `SystemTime::now()` or a random number does to a query.
    #[query]
    pub async fn counter(_ctx: &QueryCtx) -> Result<u64> {
        log::info!("counting");
        Ok(CALLS.fetch_add(1, Ordering::Relaxed))
    }

    #[query]
    pub async fn constant(_ctx: &QueryCtx) -> Result<u64> {
        Ok(42)
    }

    /// Reads the time, so is never checked (nor cached), whatever else it returns.
    #[query]
    pub async fn timed(ctx: &QueryCtx) -> Result<(f64, u64)> {
        Ok((ctx.now(), CALLS.fetch_add(1, Ordering::Relaxed)))
    }

    /// Calls `counter` the way an action does: the warning reaches the action's lines, the rerun's lines don't.
    #[action]
    pub async fn via_action(ctx: &ActionCtx) -> Result<u64> {
        ctx.run_query::<counter>(()).await
    }
}

pub(crate) fn warnings(lines: &[String]) -> Vec<&String> {
    lines.iter().filter(|l| l.contains("gave a different result when run again at the same ts")).collect()
}

#[tokio::test]
async fn reruns_queries_and_warns_when_results_differ() {
    let t = TestApp::new().await.unwrap();
    let app = t.app();
    let anon = Identity::anonymous();

    let run = app.run_query(&fns::counter::path(), json!({}), &anon).await;
    let first = run.result.unwrap();
    assert_eq!(run.log_lines[0], "[INFO] counting", "the rerun's own lines are dropped");
    let warning = warnings(&run.log_lines);
    assert_eq!(warning.len(), 1, "{:?}", run.log_lines);
    assert!(warning[0].starts_with(&format!("[WARN] query `{}` gave", fns::counter::path())), "{}", warning[0]);
    // Tests running in parallel share CALLS: the rerun's value is larger, not necessarily by one.
    let rerun = warning[0].split(&format!("`{first}` then `")).nth(1).and_then(|r| r.split('`').next());
    assert!(rerun.and_then(|r| r.parse::<u64>().ok()).is_some_and(|r| r > first.as_u64().unwrap()), "{}", warning[0]);

    // At most once per interval (10 s in TestApp): the next run isn't checked.
    let run = app.run_query(&fns::counter::path(), json!({}), &anon).await;
    assert_eq!(run.log_lines, vec!["[INFO] counting"]);

    let run = app.run_query(&fns::constant::path(), json!({}), &anon).await;
    assert!(run.log_lines.is_empty(), "{:?}", run.log_lines);
    let run = app.run_query(&fns::timed::path(), json!({}), &anon).await;
    assert!(warnings(&run.log_lines).is_empty(), "runs that read the time are not checked");

    // The warning is in the execution log, which has one entry per run (reruns are not executions).
    app.logs().flush().await.unwrap();
    let logged =
        app.logs().query(LogFilter { path_prefix: Some(fns::counter::path()), ..Default::default() }).await.unwrap();
    assert_eq!(logged.len(), 2);
    assert_eq!(warnings(&logged[1].log_lines).len(), 1, "the oldest entry, the checked run");
}

#[tokio::test]
async fn nested_queries_report_to_the_calling_action() {
    let t = TestApp::new().await.unwrap();
    let run = t.app().execute_action(&fns::via_action::path(), json!({}), &Identity::anonymous()).await;
    run.result.unwrap();
    assert_eq!(run.log_lines.iter().filter(|l| *l == "[INFO] counting").count(), 1, "{:?}", run.log_lines);
    assert_eq!(warnings(&run.log_lines).len(), 1, "{:?}", run.log_lines);
}

#[tokio::test]
async fn off_unless_asked_for() {
    let t = TestApp::with_options(AppOptions { dev: true, ..Default::default() }).await.unwrap();
    let run = t.app().run_query(&fns::counter::path(), json!({}), &Identity::anonymous()).await;
    assert_eq!(run.log_lines, vec!["[INFO] counting"]);
    let t = TestApp::with_options(AppOptions { determinism_check: Some(Duration::ZERO), ..Default::default() })
        .await
        .unwrap();
    for _ in 0..2 {
        let run = t.app().run_query(&fns::counter::path(), json!({}), &Identity::anonymous()).await;
        assert_eq!(warnings(&run.log_lines).len(), 1, "every run with a zero interval, dev mode or not");
    }
}

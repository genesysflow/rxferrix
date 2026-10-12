//! Regression test (own test binary, so its own process and global tracing state): a scoped tracing dispatcher
//! that existed *before* the app opened must not stop rxferrix from installing its function-log capture layer.
//! `tracing::dispatcher::has_been_set()` turns true for scoped dispatchers too, which used to skip the install.

use rxferrix::log;
use rxferrix::prelude::*;
use rxferrix::testing::TestApp;

#[query]
pub async fn noisy(_ctx: &QueryCtx) -> Result<u32> {
    log::info!(answer = 42, "computing");
    Ok(42)
}

#[tokio::test]
async fn capture_works_after_a_scoped_dispatcher() -> Result<()> {
    // Any scoped subscriber, created and dropped before the app opens.
    tracing::subscriber::with_default(tracing_subscriber::registry(), || tracing::info!("scoped"));
    assert!(tracing::dispatcher::has_been_set(), "precondition: the scoped dispatcher marks tracing as set");

    let t = TestApp::new().await?;
    let run = t.app().run_query("noisy", json!({}), &Default::default()).await;
    assert_eq!(run.result?, json!(42));
    assert_eq!(run.log_lines, vec!["[INFO] computing answer=42".to_string()]);
    Ok(())
}

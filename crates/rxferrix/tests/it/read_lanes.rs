//! The slow lane of reads (`read_lanes`): queries known to be slow, one-off or live, share half the readers, so the
//! others always find one.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use rxferrix::testing::TestApp;
use rxferrix::{AppOptions, FunctionRef, Identity};
use serde_json::json;

use crate::common::todos;
use crate::shared::{self, Client};

static OPEN: AtomicBool = AtomicBool::new(true);
static RUNNING: AtomicUsize = AtomicUsize::new(0);

pub mod fns {
    use std::sync::atomic::Ordering;

    use rxferrix::prelude::*;

    use crate::common::todos::Todo;

    /// Takes at least `ms`, then waits until the test opens the gate; counted in `RUNNING` meanwhile.
    #[query]
    pub async fn gated(ctx: &QueryCtx, ms: u32) -> Result<usize> {
        super::RUNNING.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(ms.into())).await;
        while !super::OPEN.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        super::RUNNING.fetch_sub(1, Ordering::SeqCst);
        Ok(ctx.db.query::<Todo>().collect().await?.len())
    }
}

async fn running(n: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while RUNNING.load(Ordering::SeqCst) != n {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {n} gated queries running, found {}", RUNNING.load(Ordering::SeqCst)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_queries_share_half_the_readers() {
    // 4 readers: 2 slow permits.
    let t = TestApp::with_options(AppOptions { readers: 4, ..Default::default() }).await.unwrap();
    let app = t.app().clone();
    let gated = fns::gated::path();

    // Both lanes learn that `gated` is slow from a run: a one-off one, and a subscription's first.
    OPEN.store(true, Ordering::SeqCst);
    let run = app.run_query(&gated, json!({ "ms": 10 }), &Identity::default()).await;
    assert_eq!(run.result.unwrap(), json!(0));
    let mut live = Client::new(&app).await;
    assert_eq!(live.subscribe(1, &gated, json!({ "ms": 10 })).await, Ok(json!(0)));

    // Three one-off runs held at the gate: two run, the third waits for a slow permit (not holding a reader).
    OPEN.store(false, Ordering::SeqCst);
    let one_offs: Vec<_> = (0..3)
        .map(|_| {
            let (app, gated) = (app.clone(), gated.clone());
            tokio::spawn(async move { app.run_query(&gated, json!({ "ms": 0 }), &Identity::default()).await })
        })
        .collect();
    running(2).await;
    // A commit makes the subscription rerun: in the manager's slow lane, which waits for a permit too.
    let (_, ts) = t.mutation_ts::<todos::add>(("one".to_string(),)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(RUNNING.load(Ordering::SeqCst), 2, "slow queries hold at most the 2 slow permits");

    // The other two readers still serve everything else.
    let (everyone, anyone) = (shared::fns::everyone::path(), Identity::default());
    let fast = app.run_query(&everyone, json!({}), &anyone);
    let fast = tokio::time::timeout(Duration::from_secs(5), fast).await.expect("a fast query found no reader");
    assert_eq!(fast.result.unwrap(), json!(1));

    // Opening the gate lets every one of them finish, the subscription's rerun included.
    OPEN.store(true, Ordering::SeqCst);
    for one_off in one_offs {
        one_off.await.unwrap().result.unwrap();
    }
    live.until(|c| c.version.ts >= ts).await;
    assert_eq!(live.value(1), json!(1));
    running(0).await;
}

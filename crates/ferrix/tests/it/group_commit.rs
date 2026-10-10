//! Group commit (`ferrix::db`, module docs): concurrent mutations share SQLite transactions, each in its own savepoint.
//! What must still hold per mutation: its own ts (consecutive across successes), all-or-nothing writes when it fails
//! or its caller goes away, its result only once committed, idempotency under concurrent duplicates, and a mutation
//! known to be slow not holding back the ones batched before it.

use std::time::{Duration, Instant};

use ferrix::testing::TestApp;
use ferrix::{FunctionRef, IdempotencyKey, Identity};
use serde_json::json;

pub mod fns {
    use ferrix::prelude::*;

    use crate::common::items::Item;

    /// Two documents named `marker`, then fails if `fail`: either both stay or neither does.
    #[mutation]
    pub async fn gc_pair(ctx: &MutationCtx, marker: String, fail: bool) -> Result<()> {
        ctx.db.insert(Item { group: 7, score: 1, name: marker.clone() }).await?;
        tokio::task::yield_now().await;
        ctx.db.insert(Item { group: 7, score: 2, name: marker }).await?;
        if fail {
            return Err(UserError::new("planned", "failed on purpose").into());
        }
        Ok(())
    }

    #[query]
    pub async fn gc_count(ctx: &QueryCtx, marker: String) -> Result<usize> {
        Ok(ctx.db.query::<Item>().with_index(Item::by_name, |q| q.eq(Item::name, marker)).collect().await?.len())
    }

    /// Blocks for `us` microseconds inside the transaction (precise, unlike a timer), then inserts.
    #[mutation]
    pub async fn gc_quick(ctx: &MutationCtx, marker: String, us: u64) -> Result<()> {
        std::thread::sleep(std::time::Duration::from_micros(us));
        ctx.db.insert(Item { group: 8, score: 0, name: marker }).await.map(drop)
    }

    /// Same as `gc_quick`, under another path (so its duration is tracked separately).
    #[mutation]
    pub async fn gc_slow(ctx: &MutationCtx, marker: String, ms: u64) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        ctx.db.insert(Item { group: 8, score: 0, name: marker }).await.map(drop)
    }
}

async fn count(t: &TestApp, marker: &str) -> u64 {
    let run = t.app().run_query(&fns::gc_count::path(), json!({ "marker": marker }), &Identity::default()).await;
    run.result.unwrap().as_u64().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_mutations_get_consecutive_ts_and_failures_leave_nothing() {
    let t = TestApp::new().await.unwrap();
    let base = t.app().db().snapshot().await.unwrap().ts;
    let tasks: Vec<_> = (0..300)
        .map(|i| {
            let app = t.app().clone();
            tokio::spawn(async move {
                let args = json!({ "marker": format!("m{i}"), "fail": i % 3 == 0 });
                let run = app.execute_mutation(&fns::gc_pair::path(), args, &Identity::default(), None).await;
                (i, run.result.map(|(_, ts)| ts))
            })
        })
        .collect();
    let mut ts = Vec::new();
    for task in tasks {
        let (i, result) = task.await.unwrap();
        match result {
            Ok(commit_ts) => {
                assert_ne!(i % 3, 0, "m{i} should have failed");
                ts.push(commit_ts);
            }
            Err(e) => {
                assert_eq!(i % 3, 0, "m{i} failed: {e:#}");
                assert!(format!("{e:#}").contains("failed on purpose"), "{e:#}");
            }
        }
    }
    ts.sort();
    let expected: Vec<u64> = (base + 1..=base + 200).collect();
    assert_eq!(ts, expected, "one ts per successful mutation, none for failed ones");
    assert_eq!(t.app().db().snapshot().await.unwrap().ts, base + 200);
    for i in 0..300 {
        assert_eq!(count(&t, &format!("m{i}")).await, if i % 3 == 0 { 0 } else { 2 }, "m{i}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_mutations_are_all_or_nothing_and_never_wedge_the_writer() {
    let t = TestApp::new().await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..400u64 {
        let app = t.app().clone();
        let task = tokio::spawn(async move {
            let args = json!({ "marker": format!("c{i}"), "fail": false });
            app.execute_mutation(&fns::gc_pair::path(), args, &Identity::default(), None).await.result.map(drop)
        });
        // Every other one is cancelled after a few microseconds: waiting for the lock, mid-transaction, or waiting for
        // its batch to commit.
        if i % 2 == 0 {
            let abort = task.abort_handle();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_micros(i * 7 % 300)).await;
                abort.abort();
            });
        }
        tasks.push((i, task));
    }
    for (i, task) in tasks {
        match task.await {
            Ok(result) => {
                result.unwrap();
                assert_eq!(count(&t, &format!("c{i}")).await, 2, "c{i} completed");
            }
            Err(e) => {
                assert!(e.is_cancelled());
                let n = count(&t, &format!("c{i}")).await;
                assert!(n == 0 || n == 2, "c{i} was cancelled and left {n} documents");
            }
        }
    }
    // The writer is free: a new mutation commits promptly.
    let args = json!({ "marker": "after", "fail": false });
    let run = tokio::time::timeout(
        Duration::from_secs(10),
        t.app().execute_mutation(&fns::gc_pair::path(), args, &Identity::default(), None),
    )
    .await
    .expect("the writer is not wedged");
    run.result.unwrap();
    assert_eq!(count(&t, "after").await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_duplicates_run_once() {
    let t = TestApp::new().await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..50u64 {
        for _ in 0..2 {
            let app = t.app().clone();
            tasks.push(tokio::spawn(async move {
                let key = IdempotencyKey { session_id: "dup".into(), request_id: i };
                let args = json!({ "marker": format!("d{i}"), "fail": false });
                app.execute_mutation(&fns::gc_pair::path(), args, &Identity::default(), Some(&key)).await.result.map(|(v, _)| v)
            }));
        }
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    for i in 0..50 {
        assert_eq!(count(&t, &format!("d{i}")).await, 2, "d{i} ran exactly once");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_mutation_commits_the_batch_before_it_runs() {
    let t = TestApp::new().await.unwrap();
    let call = |path: String, args: serde_json::Value| {
        let app = t.app().clone();
        async move { app.execute_mutation(&path, args, &Identity::default(), None).await.result.map(drop) }
    };
    // Teach the writer that gc_slow is slow.
    call(fns::gc_slow::path(), json!({ "marker": "train", "ms": 60 })).await.unwrap();

    // A quick mutation (0.8 ms, well inside a young batch) takes the lock; the slow one queues behind it. Without the
    // slow-write rule, the slow one would continue the quick one's batch, and the quick one's caller would wait for it.
    let quick = tokio::spawn({
        let run = call(fns::gc_quick::path(), json!({ "marker": "quick", "us": 800 }));
        async move {
            let started = Instant::now();
            run.await.unwrap();
            started.elapsed()
        }
    });
    tokio::time::sleep(Duration::from_micros(200)).await;
    let slow = tokio::spawn(call(fns::gc_slow::path(), json!({ "marker": "slow", "ms": 150 })));
    let quick_took = quick.await.unwrap();
    slow.await.unwrap().unwrap();
    assert!(quick_took < Duration::from_millis(100), "the quick mutation waited {quick_took:?} for the slow one");
    assert_eq!(count(&t, "quick").await, 1);
    assert_eq!(count(&t, "slow").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_waiter_that_gives_up_is_not_handed_the_batch() {
    let t = TestApp::new().await.unwrap();
    let call = |marker: &str, us: u64| {
        let app = t.app().clone();
        let args = json!({ "marker": marker, "us": us });
        async move { app.execute_mutation(&fns::gc_quick::path(), args, &Identity::default(), None).await.result.map(drop) }
    };
    // `holder` holds the writer for 1.4 ms (its batch is still young when it finishes); `quitter` queues for it and is
    // cancelled meanwhile. When `holder` finishes, nobody is waiting: it must commit its batch itself rather than
    // leave it for a waiter that is gone. (The test task blocks its thread to time this finer than timers can.)
    let holder = tokio::spawn(call("holder", 1_400));
    std::thread::sleep(Duration::from_micros(200));
    let quitter = tokio::spawn(call("quitter", 0));
    std::thread::sleep(Duration::from_micros(300));
    quitter.abort();
    assert!(quitter.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(10), holder)
        .await
        .expect("the holder's batch was left for a cancelled waiter")
        .unwrap()
        .unwrap();
    assert_eq!(count(&t, "holder").await, 1);
    assert_eq!(count(&t, "quitter").await, 0);
}

/// Checkpoints run off the commit path (`ferrix::db::checkpoint`), but the WAL must still start over regularly: under
/// constant writes from many tasks it stays bounded (unbounded, it grew by hundreds of MB per second).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wal_stays_bounded_under_constant_writes() {
    let t = TestApp::new().await.unwrap();
    let stop = Instant::now() + Duration::from_secs(3);
    let tasks: Vec<_> = (0..16)
        .map(|w| {
            let app = t.app().clone();
            tokio::spawn(async move {
                let mut n = 0u64;
                while Instant::now() < stop {
                    let args = json!({ "marker": format!("w{w}"), "fail": false });
                    app.execute_mutation(&fns::gc_pair::path(), args, &Identity::default(), None).await.result.unwrap();
                    n += 1;
                }
                n
            })
        })
        .collect();
    let mut largest = 0;
    while Instant::now() < stop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        largest = largest.max(std::fs::metadata(t.dir().join("data.db-wal")).map_or(0, |m| m.len()));
    }
    let mut commits = 0;
    for task in tasks {
        commits += task.await.unwrap();
    }
    // The writer finishes a checkpoint past 16000 pages (64 MiB), and the WAL restarts; writes landing meanwhile add a
    // little. Unbounded, it passed 250 MiB within these 3 seconds.
    assert!(largest <= 160 << 20, "the WAL reached {} MiB over {commits} commits", largest >> 20);
    assert!(commits > 1000, "only {commits} commits: the test didn't load the writer");
}

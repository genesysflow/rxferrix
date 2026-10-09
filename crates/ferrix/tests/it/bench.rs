//! Subscription fan-out benchmark (ignored; run it in release mode):
//!
//! ```sh
//! cargo test --release -p ferrix --test it bench_subscriptions -- --ignored --nocapture
//! ```
//!
//! N clients (in-process sync workers) each subscribe to M queries; a writer commits K mutations per second for S
//! seconds, each changing one of the M queries. Reported: query executions per commit and commit-to-client latency
//! (from the mutation's return to the client receiving a Transition at or after its ts). Two scenarios: every client
//! watches the same M queries ("shared"), and every client watches its own M queries ("distinct": no sharing possible).
//! Tunables: `BENCH_CLIENTS` (100), `BENCH_QUERIES` (10), `BENCH_RATE` (200), `BENCH_SECS` (5).
//!
//! Uses only APIs that predate the shared subscription manager, so the same file measures the per-connection workers.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrix::sync::protocol::{ClientMessage, QuerySetModification, ServerMessage};
use ferrix::sync::worker::{self, Inbound};
use ferrix::testing::TestApp;
use ferrix::{FunctionRef, Identity};
use serde_json::json;

static EXECUTIONS: AtomicU64 = AtomicU64::new(0);

/// Per client: (endVersion.ts, when received) of every Transition.
type Receipts = Vec<Vec<(u64, Instant)>>;

pub mod fns {
    use ferrix::prelude::*;

    use crate::common::items::Item;

    /// Items in `group`. `salt` only makes otherwise identical queries distinct.
    #[query]
    pub async fn bench_group(ctx: &QueryCtx, group: i64, salt: u32) -> Result<usize> {
        let _ = salt;
        super::EXECUTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(ctx.db.query::<Item>().with_index(Item::by_group_score, |q| q.eq(Item::group, group)).collect().await?.len())
    }

    #[mutation]
    pub async fn bench_insert(ctx: &MutationCtx, group: i64) -> Result<()> {
        ctx.db.insert(Item { group, score: 0, name: String::new() }).await.map(drop)
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

async fn scenario(name: &str, shared: bool) {
    let (clients, queries, rate, secs) = (env("BENCH_CLIENTS", 100), env("BENCH_QUERIES", 10), env("BENCH_RATE", 200), env("BENCH_SECS", 5));
    let t = TestApp::with_options(ferrix::AppOptions { dev: false, ..Default::default() }).await.unwrap();
    let app = t.app().clone();

    // Subscribe everyone and wait for the first results.
    let receipts: Arc<Mutex<Receipts>> = Arc::new(Mutex::new(vec![Vec::new(); clients as usize]));
    let mut inputs = Vec::new();
    let mut tasks = Vec::new();
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(clients as usize);
    for c in 0..clients {
        let (tx, mut rx) = worker::spawn(app.clone());
        let mods = (0..queries)
            .map(|g| QuerySetModification::Add {
                query_id: g,
                udf_path: fns::bench_group::path(),
                args: json!({ "group": g, "salt": if shared { 0 } else { c } }),
            })
            .collect();
        tx.send(Inbound::Message(ClientMessage::ModifyQuerySet { base_version: 0, new_version: 1, modifications: mods }))
            .await
            .unwrap();
        inputs.push(tx);
        let (receipts, ready_tx) = (receipts.clone(), ready_tx.clone());
        tasks.push(tokio::spawn(async move {
            let mut ready = false;
            while let Some(msg) = rx.recv().await {
                if let ServerMessage::Transition { end_version, .. } = msg {
                    if !ready {
                        ready = true;
                        ready_tx.send(()).await.unwrap();
                        continue;
                    }
                    receipts.lock().unwrap()[c as usize].push((end_version.ts, Instant::now()));
                }
            }
        }));
    }
    for _ in 0..clients {
        ready_rx.recv().await.unwrap();
    }
    let setup_executions = EXECUTIONS.swap(0, Ordering::Relaxed);

    // Commit at a fixed rate.
    let mut commits: Vec<(u64, Instant)> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / rate as f64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let started = Instant::now();
    let total = rate * secs;
    for i in 0..total {
        tick.tick().await;
        let (_, ts) = app.run_mutation(&fns::bench_insert::path(), json!({ "group": i % queries }), &Identity::default()).await.unwrap();
        commits.push((ts, Instant::now()));
    }
    let elapsed = started.elapsed();
    let final_ts = commits.last().unwrap().0;
    // Wait until every client has seen the last commit.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let done = receipts.lock().unwrap().iter().all(|r| r.last().is_some_and(|(ts, _)| *ts >= final_ts));
        if done || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let executions = EXECUTIONS.swap(0, Ordering::Relaxed);
    drop(inputs);
    for task in tasks {
        let _ = task.await;
    }

    let receipts = receipts.lock().unwrap();
    let mut latencies = Vec::new();
    let mut missing = 0;
    for r in receipts.iter() {
        for (ts, at) in &commits {
            let i = r.partition_point(|(t, _)| t < ts);
            match r.get(i) {
                Some((_, got)) => latencies.push(got.saturating_duration_since(*at)),
                None => missing += 1,
            }
        }
    }
    latencies.sort();
    let mean = latencies.iter().sum::<Duration>() / latencies.len().max(1) as u32;
    println!(
        "[{name}] {clients} clients x {queries} queries, {total} commits in {:.2}s ({:.0}/s): setup {setup_executions} \
         executions; {executions} executions = {:.2} per commit; latency p50 {:?} p99 {:?} max {:?} mean {mean:?}{}",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64(),
        executions as f64 / total as f64,
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.99),
        latencies.last().copied().unwrap_or_default(),
        if missing > 0 { format!(" ({missing} commit/client pairs never covered)") } else { String::new() },
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_subscriptions() {
    scenario("shared", true).await;
    scenario("distinct", false).await;
}

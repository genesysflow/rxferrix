//! Subscription fan-out benchmark (ignored; run it in release mode):
//!
//! ```sh
//! cargo test --release -p ferrix --test it bench_subscriptions -- --ignored --nocapture
//! ```
//!
//! N clients (in-process sync workers) each subscribe to M queries; a writer commits K mutations per second for S
//! seconds, each changing one of the M queries. Reported: query executions per commit and commit-to-client latency
//! (from the mutation's return to the client receiving a Transition at or after its ts). Scenarios: every client
//! watches the same M queries ("shared"); every client watches its own M queries ("distinct": no sharing possible);
//! and "mixed": like "shared", plus `BENCH_SLOW_CLIENTS` (20) more clients that each watch one shared slow query
//! (`BENCH_SLOW_MS`, 20 ms of blocking work, invalidated by every commit), reported separately.
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

    /// Takes `ms` milliseconds of blocking work (like a heavy scan), then counts every item.
    #[query]
    pub async fn bench_slow(ctx: &QueryCtx, ms: u32) -> Result<usize> {
        super::EXECUTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(ms.into()));
        Ok(ctx.db.query::<Item>().collect().await?.len())
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

fn report(name: &str, receipts: &[Vec<(u64, Instant)>], commits: &[(u64, Instant)]) -> String {
    let mut latencies = Vec::new();
    let mut missing = 0;
    for r in receipts {
        for (ts, at) in commits {
            let i = r.partition_point(|(t, _)| t < ts);
            match r.get(i) {
                Some((_, got)) => latencies.push(got.saturating_duration_since(*at)),
                None => missing += 1,
            }
        }
    }
    latencies.sort();
    let mean = latencies.iter().sum::<Duration>() / latencies.len().max(1) as u32;
    format!(
        "{name}latency p50 {:?} p99 {:?} max {:?} mean {mean:?}{}",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.99),
        latencies.last().copied().unwrap_or_default(),
        if missing > 0 { format!(" ({missing} commit/client pairs never covered)") } else { String::new() },
    )
}

async fn scenario(name: &str, shared: bool, slow_clients: u64) {
    let (fast_clients, queries, rate, secs) = (env("BENCH_CLIENTS", 100), env("BENCH_QUERIES", 10), env("BENCH_RATE", 200), env("BENCH_SECS", 5));
    let clients = fast_clients + slow_clients;
    let slow_ms = env("BENCH_SLOW_MS", 20);
    let t = TestApp::with_options(ferrix::AppOptions { dev: false, ..Default::default() }).await.unwrap();
    let app = t.app().clone();

    // Subscribe everyone and wait for the first results.
    let receipts: Arc<Mutex<Receipts>> = Arc::new(Mutex::new(vec![Vec::new(); clients as usize]));
    let mut inputs = Vec::new();
    let mut tasks = Vec::new();
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(clients as usize);
    for c in 0..clients {
        let (tx, mut rx) = worker::spawn(app.clone());
        let mods = if c >= fast_clients {
            vec![QuerySetModification::Add { query_id: 0, udf_path: fns::bench_slow::path(), args: json!({ "ms": slow_ms }) }]
        } else {
            (0..queries)
                .map(|g| QuerySetModification::Add {
                    query_id: g,
                    udf_path: fns::bench_group::path(),
                    args: json!({ "group": g, "salt": if shared { 0 } else { c } }),
                })
                .collect()
        };
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
    if std::env::var_os("BENCH_STATS").is_some() {
        println!("[{name}] {:?}", app.subscription_stats());
    }
    drop(inputs);
    for task in tasks {
        let _ = task.await;
    }

    let receipts = receipts.lock().unwrap();
    let (fast, slow) = receipts.split_at(fast_clients as usize);
    let slow = if slow.is_empty() { String::new() } else { format!("; slow clients ({slow_clients}): {}", report("", slow, &commits)) };
    println!(
        "[{name}] {fast_clients} clients x {queries} queries, {total} commits in {:.2}s ({:.0}/s): setup {setup_executions} \
         executions; {executions} executions = {:.2} per commit; {}{slow}",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64(),
        executions as f64 / total as f64,
        report("", fast, &commits),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_subscriptions() {
    scenario("shared", true, 0).await;
    scenario("distinct", false, 0).await;
    scenario("mixed", true, env("BENCH_SLOW_CLIENTS", 20)).await;
}

/// Read scaling: P tasks each run the same kind of query (100 documents per run) in a loop, each through its own
/// snapshot (reader connection). Reports total queries per second and the mean time per query; with no contention
/// the time per query stays flat as P grows.
///
/// ```sh
/// cargo test --release -p ferrix --test it bench_parallel_reads -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_parallel_reads() {
    let secs = env("BENCH_SECS", 2) as f64 / 2.0;
    let t = TestApp::with_options(ferrix::AppOptions { dev: false, readers: 8, ..Default::default() }).await.unwrap();
    let app = t.app().clone();
    for i in 0..1000 {
        app.run_mutation(&fns::bench_insert::path(), json!({ "group": i % 10 }), &Identity::default()).await.unwrap();
    }
    for parallel in [1usize, 2, 4, 6, 8] {
        let stop = Instant::now() + Duration::from_secs_f64(secs);
        let tasks: Vec<_> = (0..parallel)
            .map(|p| {
                let app = app.clone();
                tokio::spawn(async move {
                    let (mut n, mut busy) = (0u64, Duration::ZERO);
                    while Instant::now() < stop {
                        let args = json!({ "group": (n as usize + p) % 10, "salt": 0 });
                        let t0 = Instant::now();
                        let run = app.run_query(&fns::bench_group::path(), args, &Identity::default()).await;
                        busy += t0.elapsed();
                        assert_eq!(run.result.unwrap(), json!(100));
                        n += 1;
                    }
                    (n, busy)
                })
            })
            .collect();
        let (mut total, mut busy) = (0u64, Duration::ZERO);
        for task in tasks {
            let (n, b) = task.await.unwrap();
            total += n;
            busy += b;
        }
        println!(
            "[parallel reads] {parallel} at once: {:.0} queries/s, {:.1} µs per query",
            total as f64 / secs,
            busy.as_secs_f64() * 1e6 / total.max(1) as f64
        );
    }
}

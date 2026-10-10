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

    /// `n` inserts in one transaction.
    #[mutation]
    pub async fn bench_insert_many(ctx: &MutationCtx, group: i64, n: u32) -> Result<()> {
        for _ in 0..n {
            ctx.db.insert(Item { group, score: 0, name: String::new() }).await?;
        }
        Ok(())
    }

    /// Reads every item in `group` (an index range), then inserts one elsewhere (so the group keeps its size): a
    /// typical "check, then write" mutation.
    #[mutation]
    pub async fn bench_scan_insert(ctx: &MutationCtx, group: i64) -> Result<()> {
        let seen = ctx.db.query::<Item>().with_index(Item::by_group_score, |q| q.eq(Item::group, group)).collect().await?;
        ctx.db.insert(Item { group: -3, score: seen.len() as i64, name: String::new() }).await.map(drop)
    }

    #[mutation]
    pub async fn bench_new_counter(ctx: &MutationCtx) -> Result<Id<Item>> {
        ctx.db.insert(Item { group: -2, score: 0, name: String::new() }).await
    }

    #[query]
    pub async fn bench_counter(ctx: &QueryCtx, id: Id<Item>) -> Result<i64> {
        Ok(ctx.db.get(id).await?.map_or(0, |item| item.value.score))
    }

    /// Read-modify-write of one document: every writer increments the same counter.
    #[mutation]
    pub async fn bench_increment(ctx: &MutationCtx, id: Id<Item>) -> Result<()> {
        let item = ctx.db.get(id).await?.ok_or_else(|| UserError::new("missing", "no counter"))?;
        ctx.db.patch(id, json!({ "score": item.value.score + 1 })).await
    }

    /// Awaits for `ms` milliseconds inside the transaction (like slow work between reads and writes), then inserts.
    #[mutation]
    pub async fn bench_slow_insert(ctx: &MutationCtx, ms: u32) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(ms.into())).await;
        ctx.db.insert(Item { group: -1, score: 0, name: String::new() }).await.map(drop)
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

/// What a burst of writers did: mutations completed, wall time, and each mutation's latency (sorted).
struct WriteRun {
    count: u64,
    elapsed: Duration,
    latencies: Vec<Duration>,
}

impl WriteRun {
    fn per_sec(&self) -> f64 {
        self.count as f64 / self.elapsed.as_secs_f64()
    }

    fn line(&self) -> String {
        format!(
            "{:.0} mutations/s ({:.1} µs each), latency p50 {:?} p99 {:?} max {:?}",
            self.per_sec(),
            1e6 / self.per_sec(),
            percentile(&self.latencies, 0.5),
            percentile(&self.latencies, 0.99),
            self.latencies.last().copied().unwrap_or_default(),
        )
    }
}

type MakeCall = Arc<dyn Fn(usize, u64) -> (String, serde_json::Value) + Send + Sync>;

/// `writers` tasks each run mutations back to back for `secs` seconds (`make(writer, n)` gives the n-th call of a
/// writer). With `idempotent`, each call carries an idempotency key, as every mutation over the WebSocket does.
async fn hammer(app: &ferrix::App, writers: usize, secs: f64, idempotent: bool, make: MakeCall) -> WriteRun {
    let started = Instant::now();
    let stop = started + Duration::from_secs_f64(secs);
    let tasks: Vec<_> = (0..writers)
        .map(|w| {
            let (app, make) = (app.clone(), make.clone());
            tokio::spawn(async move {
                let mut latencies = Vec::new();
                let mut n = 0u64;
                while Instant::now() < stop {
                    let (path, args) = make(w, n);
                    let key = idempotent.then(|| ferrix::IdempotencyKey { session_id: format!("bench-{w}"), request_id: n });
                    let t0 = Instant::now();
                    let run = app.execute_mutation(&path, args, &Identity::default(), key.as_ref()).await;
                    latencies.push(t0.elapsed());
                    run.result.unwrap();
                    n += 1;
                }
                latencies
            })
        })
        .collect();
    let mut latencies = Vec::new();
    for task in tasks {
        latencies.extend(task.await.unwrap());
    }
    let elapsed = started.elapsed();
    latencies.sort();
    WriteRun { count: latencies.len() as u64, elapsed, latencies }
}

fn call(path: String, args: serde_json::Value) -> (String, serde_json::Value) {
    (path, args)
}

async fn write_app(options: ferrix::AppOptions) -> TestApp {
    // BENCH_FSYNC=1: commits wait for the disk (`AppOptions::fsync_commits`).
    let fsync_commits = std::env::var("BENCH_FSYNC").is_ok_and(|v| !v.is_empty() && v != "0");
    TestApp::with_options(ferrix::AppOptions { dev: false, fsync_commits, ..options }).await.unwrap()
}

/// `clients` sync workers, each subscribed to the same `queries` queries over `items` (so every insert invalidates
/// one of them), drained in the background. Returns once each has its first results; dropping the senders stops them.
async fn subscribers(app: &ferrix::App, clients: u64, queries: u64) -> Vec<tokio::sync::mpsc::Sender<Inbound>> {
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(clients as usize);
    let mut inputs = Vec::new();
    for _ in 0..clients {
        let (tx, mut rx) = worker::spawn(app.clone());
        let modifications = (0..queries)
            .map(|g| QuerySetModification::Add {
                query_id: g,
                udf_path: fns::bench_group::path(),
                args: json!({ "group": g, "salt": 0 }),
            })
            .collect();
        tx.send(Inbound::Message(ClientMessage::ModifyQuerySet { base_version: 0, new_version: 1, modifications }))
            .await
            .unwrap();
        inputs.push(tx);
        let ready_tx = ready_tx.clone();
        tokio::spawn(async move {
            let mut ready = false;
            while let Some(msg) = rx.recv().await {
                if !ready && matches!(msg, ServerMessage::Transition { .. }) {
                    ready = true;
                    let _ = ready_tx.send(()).await;
                }
            }
        });
    }
    for _ in 0..clients {
        ready_rx.recv().await.unwrap();
    }
    inputs
}

/// Write throughput of the single writer (ignored; run it in release mode):
///
/// ```sh
/// cargo test --release -p ferrix --test it bench_writes -- --ignored --nocapture
/// ```
///
/// Mutations are serialized: each holds the writer lock for its whole run, so throughput is about 1 / (time per
/// mutation) however many writers there are, and latency grows with the queue. Scenarios: a one-insert mutation at
/// 1 to 64 concurrent writers; with idempotency keys (the WebSocket path); 10 inserts per mutation; read 100 documents
/// (an index range) then insert; every writer incrementing one counter (checked: no lost updates); with continuous
/// replication's journal; with 100 subscribed clients; and fast writers queued behind a mutation that awaits 20 ms. In-process
/// (`App::execute_mutation`): no JSON over a socket. SQLite runs in WAL mode with `synchronous = NORMAL`, so a commit
/// doesn't wait for fsync (`BENCH_FSYNC=1`: it does; point `TMPDIR` at a real disk to measure that). Tunable:
/// `BENCH_SECS` (2 per scenario), `BENCH_ONLY` (scenarios: insert, idempotent, many, scan, counter, replication,
/// subscribers, slow), `BENCH_WRITERS` (for insert: 1,4,16,64).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_writes() {
    let secs = env("BENCH_SECS", 2) as f64;
    // `BENCH_ONLY=insert,counter` runs only those scenarios (e.g. to profile one).
    let only = std::env::var("BENCH_ONLY").ok();
    let wanted = |name: &str| only.as_deref().is_none_or(|only| only.split(',').any(|o| o == name));
    let insert: MakeCall = Arc::new(|w, n| call(fns::bench_insert::path(), json!({ "group": (w as u64 + n) % 10 })));

    if wanted("insert") {
        let t = write_app(Default::default()).await;
        let writers: Vec<usize> = match std::env::var("BENCH_WRITERS") {
            Ok(list) => list.split(',').map(|w| w.parse().unwrap()).collect(),
            Err(_) => vec![1, 4, 16, 64],
        };
        for writers in writers {
            let run = hammer(t.app(), writers, secs, false, insert.clone()).await;
            println!("[writes] insert, {writers} writers: {}", run.line());
        }
    }
    if wanted("idempotent") {
        let t = write_app(Default::default()).await;
        let run = hammer(t.app(), 16, secs, true, insert.clone()).await;
        println!("[writes] insert with idempotency keys, 16 writers: {}", run.line());
    }
    if wanted("many") {
        let t = write_app(Default::default()).await;
        let many: MakeCall = Arc::new(|w, _| call(fns::bench_insert_many::path(), json!({ "group": w % 10, "n": 10 })));
        let run = hammer(t.app(), 16, secs, false, many).await;
        println!("[writes] 10 inserts per mutation, 16 writers: {} = {:.0} documents/s", run.line(), run.per_sec() * 10.0);
    }
    if wanted("scan") {
        // Groups of 100 items to scan.
        let t = write_app(Default::default()).await;
        for i in 0..1000 {
            t.app().run_mutation(&fns::bench_insert::path(), json!({ "group": 100 + i % 10 }), &Identity::default()).await.unwrap();
        }
        let scan: MakeCall = Arc::new(|w, _| call(fns::bench_scan_insert::path(), json!({ "group": 100 + w as u64 % 10 })));
        let run = hammer(t.app(), 16, secs, false, scan).await;
        println!("[writes] read 100 documents then insert, 16 writers: {}", run.line());
    }
    if wanted("counter") {
        let t = write_app(Default::default()).await;
        let (id, _) = t.app().run_mutation(&fns::bench_new_counter::path(), json!({}), &Identity::default()).await.unwrap();
        let increment: MakeCall = Arc::new({
            let id = id.clone();
            move |_, _| call(fns::bench_increment::path(), json!({ "id": id }))
        });
        let run = hammer(t.app(), 16, secs, false, increment).await;
        let after = t.app().run_query(&fns::bench_counter::path(), json!({ "id": id }), &Identity::default()).await;
        let score = after.result.unwrap().as_u64().unwrap();
        assert_eq!(score, run.count, "every increment is kept: no lost updates");
        println!("[writes] 16 writers incrementing one counter (final count = mutations): {}", run.line());
    }
    if wanted("replication") {
        let target = tempfile::tempdir().unwrap();
        let replication = ferrix::ops::replicate::ReplicationOptions::new(target.path().to_str().unwrap());
        let t = write_app(ferrix::AppOptions {
            ops: ferrix::ops::OpsOptions { replication: Some(replication), ..Default::default() },
            ..Default::default()
        })
        .await;
        let run = hammer(t.app(), 16, secs, false, insert.clone()).await;
        println!("[writes] insert with continuous replication, 16 writers: {}", run.line());
    }
    if wanted("subscribers") {
        let t = write_app(Default::default()).await;
        let inputs = subscribers(t.app(), 100, 10).await;
        let run = hammer(t.app(), 16, secs, false, insert.clone()).await;
        println!("[writes] insert with 100 clients x 10 subscribed queries, 16 writers: {}", run.line());
        drop(inputs);
    }
    if wanted("slow") {
        let t = write_app(Default::default()).await;
        let alone = hammer(t.app(), 16, secs / 2.0, false, insert.clone()).await;
        let app = t.app().clone();
        let slow = tokio::spawn(async move {
            let slow: MakeCall = Arc::new(|_, _| call(fns::bench_slow_insert::path(), json!({ "ms": 20 })));
            hammer(&app, 1, secs, false, slow).await
        });
        let fast = hammer(t.app(), 16, secs, false, insert.clone()).await;
        let slow = slow.await.unwrap();
        println!(
            "[writes] 16 fast writers alone: p50 {:?} p99 {:?}; next to one writer of 20 ms mutations ({} ran): {}",
            percentile(&alone.latencies, 0.5),
            percentile(&alone.latencies, 0.99),
            slow.count,
            fast.line()
        );
    }
}

//! How many reader connections the server needs (ignored; run in release mode):
//!
//! ```sh
//! cargo test --release -p rxferrix --test it bench_readers -- --ignored --nocapture
//! ```
//!
//! Every query holds a reader for its whole run, and runs its SQLite calls on a runtime worker thread. For each
//! number of readers (`BENCH_READERS`, default 2,4,6,8,12,16,32), `BENCH_CLIENTS` (64) tasks run a fast query (an
//! index range of 10 documents) back to back for `BENCH_SECS` (2), first alone, then while `BENCH_SLOW` (8) more
//! tasks run a slow one (all of a 20k-document table) back to back. Reported: fast queries per second and their
//! latency; slow queries per second. The runtime has `BENCH_THREADS` worker threads (default: one per core, as
//! `rxferrix serve` has).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rxferrix::testing::TestApp;
use rxferrix::{AppOptions, FunctionRef, Identity};
use serde_json::json;

pub mod fns {
    use rxferrix::prelude::*;

    #[table(name = "bench_reads")]
    #[index(by_group = [group])]
    pub struct Reading {
        pub group: i64,
        pub n: i64,
    }

    #[internal_mutation]
    pub async fn fill(ctx: &MutationCtx, from: i64, count: i64) -> Result<()> {
        for n in from..from + count {
            ctx.db.insert(Reading { group: n % 2000, n }).await?;
        }
        Ok(())
    }

    /// The 10 documents of a group.
    #[query]
    pub async fn group(ctx: &QueryCtx, group: i64) -> Result<usize> {
        let q = ctx.db.query::<Reading>().with_index(Reading::by_group, |r| r.eq(Reading::group, group));
        Ok(q.collect().await?.len())
    }

    /// Every document.
    #[query]
    pub async fn everything(ctx: &QueryCtx) -> Result<usize> {
        Ok(ctx.db.query::<Reading>().collect().await?.len())
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted.get(((sorted.len().max(1) - 1) as f64 * p) as usize).copied().unwrap_or_default()
}

#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn bench_readers() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let threads = env("BENCH_THREADS", cores as u64) as usize;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(threads).enable_all().build().unwrap();
    rt.block_on(run(threads));
}

async fn run(threads: usize) {
    let secs = Duration::from_secs(env("BENCH_SECS", 2));
    let slow_tasks = env("BENCH_SLOW", 8) as usize;
    let clients = env("BENCH_CLIENTS", 64) as i64;
    let readers: Vec<usize> = match std::env::var("BENCH_READERS") {
        Ok(list) => list.split(',').map(|r| r.parse().unwrap()).collect(),
        Err(_) => vec![2, 4, 6, 8, 12, 16, 32],
    };
    println!("{threads} runtime worker threads, {clients} fast clients; {slow_tasks} slow tasks in the second run");
    for readers in readers {
        let t = TestApp::with_options(AppOptions { readers, ..Default::default() }).await.unwrap();
        for from in (0..20_000).step_by(1000) {
            t.mutation::<fns::fill>((from, 1000)).await.unwrap();
        }
        let app = t.app().clone();
        for slow in [0, slow_tasks] {
            let stop = Arc::new(AtomicBool::new(false));
            let slow_runs: Vec<_> = (0..slow)
                .map(|_| {
                    let (app, stop) = (app.clone(), stop.clone());
                    tokio::spawn(async move {
                        let mut n = 0u64;
                        while !stop.load(Ordering::Relaxed) {
                            let run = app.run_query(&fns::everything::path(), json!({}), &Identity::default()).await;
                            assert_eq!(run.result.unwrap(), json!(20_000));
                            n += 1;
                        }
                        n
                    })
                })
                .collect();
            let started = Instant::now();
            let fast_runs: Vec<_> = (0..clients)
                .map(|c| {
                    let app = app.clone();
                    tokio::spawn(async move {
                        let mut lat = Vec::new();
                        while started.elapsed() < secs {
                            let args = json!({ "group": (c * 31 + lat.len() as i64) % 2000 });
                            let t0 = Instant::now();
                            let run = app.run_query(&fns::group::path(), args, &Identity::default()).await;
                            lat.push(t0.elapsed());
                            assert_eq!(run.result.unwrap(), json!(10));
                        }
                        lat
                    })
                })
                .collect();
            let mut lat = Vec::new();
            for f in fast_runs {
                lat.extend(f.await.unwrap());
            }
            let elapsed = started.elapsed().as_secs_f64();
            stop.store(true, Ordering::Relaxed);
            let mut slow_count = 0;
            for s in slow_runs {
                slow_count += s.await.unwrap();
            }
            lat.sort();
            let slow_part =
                if slow > 0 { format!(", slow queries {:>5.0}/s", slow_count as f64 / elapsed) } else { String::new() };
            println!(
                "{readers:>2} readers, {slow} slow: fast queries {:>7.0}/s, p50 {:>8.1?}, p99 {:>8.1?}, max {:>8.1?}{slow_part}",
                lat.len() as f64 / elapsed,
                percentile(&lat, 0.5),
                percentile(&lat, 0.99),
                lat.last().copied().unwrap_or_default(),
            );
        }
    }
}

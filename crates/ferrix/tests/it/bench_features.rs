//! What live aggregates and the analytics copy cost (ignored; run them in release mode):
//!
//! ```sh
//! cargo test --release -p ferrix --test it bench_aggregates -- --ignored --nocapture
//! cargo test --release -p ferrix --features olap --test it bench_olap_writes -- --ignored --nocapture
//! ```
//!
//! `bench_aggregates`: the same one-document inserts into a table without aggregates and into one with three (16
//! writers, `BENCH_SECS` each), then reading one group's count and sum: an aggregate `get` against counting the
//! group's documents through an index (`Query::count`) and summing them (`collect`), at 10k documents per group.
//! `bench_olap_writes`: inserts with and without `--olap` (the copy following in the background).

use std::time::{Duration, Instant};

use ferrix::testing::TestApp;
use ferrix::{App, AppOptions};

pub mod fns {
    use ferrix::prelude::*;

    #[table(name = "bench_plain")]
    #[index(by_group = [group])]
    pub struct Plain {
        pub group: String,
        pub amount: f64,
        pub quantity: i64,
    }

    #[table(name = "bench_aggregated")]
    #[index(by_group = [group])]
    #[aggregate(per_group, group = [group], sum = [amount, quantity])]
    #[aggregate(per_quantity, group = [quantity])]
    #[aggregate(total, sum = [amount])]
    pub struct Aggregated {
        pub group: String,
        pub amount: f64,
        pub quantity: i64,
    }

    #[internal_mutation]
    pub async fn insert_plain(ctx: &MutationCtx, group: String, amount: f64, quantity: i64) -> Result<Id<Plain>> {
        ctx.db.insert(Plain { group, amount, quantity }).await
    }

    #[internal_mutation]
    pub async fn insert_aggregated(ctx: &MutationCtx, group: String, amount: f64, quantity: i64) -> Result<Id<Aggregated>> {
        ctx.db.insert(Aggregated { group, amount, quantity }).await
    }

    /// Count and sum a group by reading its documents (what an app does without aggregates).
    #[internal_query]
    pub async fn scan_group(ctx: &QueryCtx, group: String) -> Result<(u64, f64)> {
        let n = ctx.db.query::<Plain>().with_index(Plain::by_group, |r| r.eq(Plain::group, group.clone())).count().await?;
        let docs = ctx.db.query::<Plain>().with_index(Plain::by_group, |r| r.eq(Plain::group, group)).collect().await?;
        Ok((n, docs.iter().map(|d| d.value.amount).sum()))
    }

    #[internal_query]
    pub async fn aggregate_group(ctx: &QueryCtx, group: String) -> Result<(u64, f64)> {
        let a = ctx.db.aggregate(Aggregated::per_group).get(|g| g.eq(Aggregated::group, group)).await?;
        Ok((a.count, a.sum(Aggregated::amount)))
    }
}

fn secs() -> f64 {
    std::env::var("BENCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(2.0)
}

/// `writers` tasks inserting back to back for `secs`: (inserts per second, p50, p99).
async fn hammer(app: &App, writers: usize, aggregated: bool) -> (f64, Duration, Duration) {
    let stop = Instant::now() + Duration::from_secs_f64(secs());
    let started = Instant::now();
    let tasks: Vec<_> = (0..writers)
        .map(|w| {
            let app = app.clone();
            tokio::spawn(async move {
                let mut lat = Vec::new();
                let mut n = 0i64;
                while Instant::now() < stop {
                    let args = (format!("g{}", (w as i64 + n) % 50), n as f64 * 0.25, n % 7);
                    let t0 = Instant::now();
                    if aggregated {
                        app.mutation::<fns::insert_aggregated>(args).await.unwrap();
                    } else {
                        app.mutation::<fns::insert_plain>(args).await.unwrap();
                    }
                    lat.push(t0.elapsed());
                    n += 1;
                }
                lat
            })
        })
        .collect();
    let mut lat = Vec::new();
    for t in tasks {
        lat.extend(t.await.unwrap());
    }
    lat.sort();
    let p = |q: f64| lat[((lat.len() as f64 - 1.0) * q) as usize];
    (lat.len() as f64 / started.elapsed().as_secs_f64(), p(0.5), p(0.99))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_aggregates() {
    for (label, aggregated) in [("no aggregates", false), ("three aggregates", true)] {
        let t = TestApp::with_options(AppOptions::default()).await.unwrap();
        let (rate, p50, p99) = hammer(t.app(), 16, aggregated).await;
        println!("inserts, {label:<17}: {rate:>8.0}/s, p50 {p50:>9.1?}, p99 {p99:>9.1?}");
    }
    // Reading one group of 10k documents: scanning it against the aggregate.
    let t = TestApp::with_options(AppOptions::default()).await.unwrap();
    for n in 0..10_000i64 {
        t.mutation::<fns::insert_plain>(("big".to_string(), n as f64, 1)).await.unwrap();
        t.mutation::<fns::insert_aggregated>(("big".to_string(), n as f64, 1)).await.unwrap();
    }
    for (label, aggregate) in [("count + sum by scanning", false), ("aggregate get", true)] {
        let started = Instant::now();
        let runs = 200;
        for _ in 0..runs {
            // `query_run` runs the query every time (`query` would answer from the query cache).
            let run = if aggregate {
                t.query_run::<fns::aggregate_group>(("big".to_string(),)).await.unwrap()
            } else {
                t.query_run::<fns::scan_group>(("big".to_string(),)).await.unwrap()
            };
            let (count, sum): (u64, f64) = serde_json::from_value(run.result.unwrap()).unwrap();
            assert_eq!((count, sum), (10_000, (0..10_000).sum::<i64>() as f64));
        }
        println!("one group of 10k documents, {label:<23}: {:>9.1?} per query", started.elapsed() / runs);
    }
}

#[cfg(feature = "olap")]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_olap_writes() {
    let scenarios = [
        ("without --olap", false, true, None),
        ("--olap, copy paused (journal only)", true, false, None),
        ("--olap", true, true, None),
        ("--olap, 2 DuckDB threads", true, true, Some(2)),
    ];
    for (label, olap, background, threads) in scenarios {
        let options = AppOptions {
            olap: ferrix::olap::OlapOptions { enabled: olap, background, threads, ..Default::default() },
            ..Default::default()
        };
        let t = TestApp::with_options(options).await.unwrap();
        if olap {
            t.olap_sync().await.unwrap();
        }
        let (rate, p50, p99) = hammer(t.app(), 16, false).await;
        let lag = if olap {
            let started = Instant::now();
            t.olap_sync().await.unwrap();
            format!(", copy caught up {:.1?} after the writes stopped", started.elapsed())
        } else {
            String::new()
        };
        println!("inserts, {label:<34}: {rate:>8.0}/s, p50 {p50:>9.1?}, p99 {p99:>9.1?}{lag}");
    }
}

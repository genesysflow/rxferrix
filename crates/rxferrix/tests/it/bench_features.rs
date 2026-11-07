//! What live aggregates and the analytics copy cost (ignored; run them in release mode):
//!
//! ```sh
//! cargo test --release -p rxferrix --test it bench_aggregates -- --ignored --nocapture
//! cargo test --release -p rxferrix --test it bench_aggregate_ranges -- --ignored --nocapture
//! cargo test --release -p rxferrix --test it bench_search_patches -- --ignored --nocapture
//! cargo test --release -p rxferrix --test it bench_filtered_first -- --ignored --nocapture
//! cargo test --release -p rxferrix --features olap --test it bench_olap_writes -- --ignored --nocapture
//! ```
//!
//! `bench_aggregates`: the same one-document inserts into a table without aggregates and into one with three (16
//! writers, `BENCH_SECS` each), then reading one group's count and sum: an aggregate `get` against counting the
//! group's documents through an index (`Query::count`) and summing them (`collect`), at 10k documents per group.
//! `bench_aggregate_ranges`: reads of an aggregate with `BENCH_GROUPS` (20k) groups that each touch a few of them: a
//! group's `get` (one that exists, and one that doesn't), a `sum_over` 10 groups and the last 10 `groups` of a range.
//! `bench_search_patches`: 16 writers patch a counter of documents (`BENCH_DOCS`, 2000) that also carry a 2 KB text
//! and a 1536-dimension embedding, in a table without search indexes and in one with a search index on the text and a
//! vector index on the embedding, for `BENCH_SECS` each. Neither index's fields change.
//! `bench_filtered_first`: `first()`, `take(10)` and a page of 10 of a filtered scan over 1 KiB documents, whose
//! first match is the 1st (or 8th) document of the range.
//! `bench_olap_writes`: inserts with and without `--olap` (the copy following in the background).

use std::time::{Duration, Instant};

use rxferrix::testing::TestApp;
use rxferrix::{App, AppOptions};

pub mod fns {
    use rxferrix::prelude::*;

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
    pub async fn insert_aggregated(
        ctx: &MutationCtx,
        group: String,
        amount: f64,
        quantity: i64,
    ) -> Result<Id<Aggregated>> {
        ctx.db.insert(Aggregated { group, amount, quantity }).await
    }

    #[table(name = "bench_unindexed_posts")]
    pub struct UnindexedPost {
        pub channel: String,
        pub body: String,
        pub embedding: Vec<f32>,
        pub views: i64,
    }

    #[table(name = "bench_indexed_posts")]
    #[search_index(search_body, search = body, filter = [channel])]
    #[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel])]
    pub struct IndexedPost {
        pub channel: String,
        pub body: String,
        pub embedding: Vec<f32>,
        pub views: i64,
    }

    #[table(name = "bench_notes")]
    #[index(by_n = [n])]
    pub struct Note {
        pub n: i64,
        pub body: String,
    }

    #[internal_mutation]
    pub async fn insert_notes(ctx: &MutationCtx, from: i64, count: i64) -> Result<()> {
        for n in from..from + count {
            ctx.db.insert(Note { n, body: "x".repeat(1024) }).await?;
        }
        Ok(())
    }

    /// The first `take` notes from `from` whose `n` is a multiple of `every`.
    #[internal_query]
    pub async fn filtered_notes(ctx: &QueryCtx, from: i64, every: i64, take: u32) -> Result<usize> {
        let q = ctx.db.query::<Note>().with_index(Note::by_n, |r| r.gte(Note::n, from));
        Ok(q.filter(move |d| d.value.n % every == 0).take(take as usize).await?.len())
    }

    /// A page of `take` notes from `from` whose `n` is a multiple of `every`.
    #[internal_query]
    pub async fn filtered_notes_page(ctx: &QueryCtx, from: i64, every: i64, take: u32) -> Result<usize> {
        let q = ctx.db.query::<Note>().with_index(Note::by_n, |r| r.gte(Note::n, from));
        let page = q.filter(move |d| d.value.n % every == 0).paginate(PaginationOpts::first(take as usize)).await?;
        Ok(page.page.len())
    }

    #[internal_mutation]
    pub async fn insert_post(ctx: &MutationCtx, indexed: bool, n: i64) -> Result<String> {
        let body = (0..300).map(|w| format!("word{} ", (n * 7 + w) % 997)).collect::<String>();
        let embedding = (0..1536).map(|i| ((n * 31 + i) % 101) as f32 / 101.0).collect();
        let channel = format!("c{}", n % 10);
        Ok(if indexed {
            ctx.db.insert(IndexedPost { channel, body, embedding, views: 0 }).await?.to_string()
        } else {
            ctx.db.insert(UnindexedPost { channel, body, embedding, views: 0 }).await?.to_string()
        })
    }

    #[internal_mutation]
    pub async fn view_post(ctx: &MutationCtx, indexed: bool, id: String, views: i64) -> Result<()> {
        let patch = json!({ "views": views });
        if indexed {
            ctx.db.patch(id.parse::<Id<IndexedPost>>().map_err(|e| anyhow::anyhow!("{e}"))?, patch).await
        } else {
            ctx.db.patch(id.parse::<Id<UnindexedPost>>().map_err(|e| anyhow::anyhow!("{e}"))?, patch).await
        }
    }

    /// Count and sum a group by reading its documents (what an app does without aggregates).
    #[internal_query]
    pub async fn scan_group(ctx: &QueryCtx, group: String) -> Result<(u64, f64)> {
        let n =
            ctx.db.query::<Plain>().with_index(Plain::by_group, |r| r.eq(Plain::group, group.clone())).count().await?;
        let docs = ctx.db.query::<Plain>().with_index(Plain::by_group, |r| r.eq(Plain::group, group)).collect().await?;
        Ok((n, docs.iter().map(|d| d.value.amount).sum()))
    }

    #[internal_query]
    pub async fn aggregate_group(ctx: &QueryCtx, group: String) -> Result<(u64, f64)> {
        let a = ctx.db.aggregate(Aggregated::per_group).get(|g| g.eq(Aggregated::group, group)).await?;
        Ok((a.count, a.sum(Aggregated::amount)))
    }

    /// The groups from `from` (inclusive) to `to` (exclusive), combined.
    #[internal_query]
    pub async fn aggregate_sum_over(ctx: &QueryCtx, from: String, to: String) -> Result<u64> {
        let a = ctx
            .db
            .aggregate(Aggregated::per_group)
            .sum_over(|g| g.gte(Aggregated::group, from).lt(Aggregated::group, to))
            .await?;
        Ok(a.count)
    }

    /// The last `limit` groups before `to`, newest first.
    #[internal_query]
    pub async fn aggregate_last_groups(ctx: &QueryCtx, to: String, limit: u32) -> Result<usize> {
        let groups = ctx
            .db
            .aggregate(Aggregated::per_group)
            .groups(|g| g.lt(Aggregated::group, to), Order::Desc, limit as usize)
            .await?;
        Ok(groups.len())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_aggregate_ranges() {
    let groups: usize = std::env::var("BENCH_GROUPS").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let t = TestApp::with_options(AppOptions::default()).await.unwrap();
    let group = |n: usize| format!("g{n:06}");
    for n in 0..groups {
        t.mutation::<fns::insert_aggregated>((group(n), n as f64, 1)).await.unwrap();
    }
    let runs = 500u32;
    let time =
        |label: &str, elapsed: Duration| println!("{groups} groups, {label:<36}: {:>9.1?} per query", elapsed / runs);
    let started = Instant::now();
    for _ in 0..runs {
        let run = t.query_run::<fns::aggregate_group>((group(10),)).await.unwrap();
        assert_eq!(serde_json::from_value::<(u64, f64)>(run.result.unwrap()).unwrap().0, 1);
    }
    time("get, an existing group", started.elapsed());
    let started = Instant::now();
    for _ in 0..runs {
        // Sorts before every group: nothing matches.
        let run = t.query_run::<fns::aggregate_group>(("a".to_string(),)).await.unwrap();
        assert_eq!(serde_json::from_value::<(u64, f64)>(run.result.unwrap()).unwrap().0, 0);
    }
    time("get, a missing group", started.elapsed());
    let started = Instant::now();
    for _ in 0..runs {
        let run = t.query_run::<fns::aggregate_sum_over>((group(10), group(20))).await.unwrap();
        assert_eq!(serde_json::from_value::<u64>(run.result.unwrap()).unwrap(), 10);
    }
    time("sum_over 10 groups", started.elapsed());
    let started = Instant::now();
    for _ in 0..runs {
        let run = t.query_run::<fns::aggregate_last_groups>((group(20), 10u32)).await.unwrap();
        assert_eq!(serde_json::from_value::<usize>(run.result.unwrap()).unwrap(), 10);
    }
    time("groups, the last 10 of a range (desc)", started.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_search_patches() {
    let docs: i64 = std::env::var("BENCH_DOCS").ok().and_then(|s| s.parse().ok()).unwrap_or(2000);
    for (label, indexed) in [("no search indexes", false), ("search + vector index", true)] {
        let t = TestApp::with_options(AppOptions::default()).await.unwrap();
        let mut ids = Vec::new();
        for n in 0..docs {
            ids.push(t.mutation::<fns::insert_post>((indexed, n)).await.unwrap());
        }
        let ids = std::sync::Arc::new(ids);
        let stop = Instant::now() + Duration::from_secs_f64(secs());
        let started = Instant::now();
        let tasks: Vec<_> = (0..16)
            .map(|w| {
                let (app, ids) = (t.app().clone(), ids.clone());
                tokio::spawn(async move {
                    let mut lat = Vec::new();
                    while Instant::now() < stop {
                        let id = ids[(w * 997 + lat.len() * 31) % ids.len()].clone();
                        let t0 = Instant::now();
                        app.mutation::<fns::view_post>((indexed, id, lat.len() as i64)).await.unwrap();
                        lat.push(t0.elapsed());
                    }
                    lat
                })
            })
            .collect();
        let mut lat = Vec::new();
        for task in tasks {
            lat.extend(task.await.unwrap());
        }
        lat.sort();
        let p = |q: f64| lat[((lat.len() as f64 - 1.0) * q) as usize];
        let rate = lat.len() as f64 / started.elapsed().as_secs_f64();
        println!("patches, {label:<21}: {rate:>8.0}/s, p50 {:>9.1?}, p99 {:>9.1?}", p(0.5), p(0.99));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_filtered_first() {
    let t = TestApp::with_options(AppOptions::default()).await.unwrap();
    for from in (0..10_000).step_by(1000) {
        t.mutation::<fns::insert_notes>((from, 1000)).await.unwrap();
    }
    for (label, every, take) in
        [("first(), 1st matches", 1, 1), ("first(), 8th matches", 8, 1), ("take(10), every 8th", 8, 10)]
    {
        let runs = 2000u32;
        let started = Instant::now();
        for i in 0..runs {
            let run = t.query_run::<fns::filtered_notes>((i64::from(i) * 4 % 9000, every, take)).await.unwrap();
            assert_eq!(run.result.unwrap(), serde_json::json!(take));
        }
        println!("filtered scan, {label:<22}: {:>9.1?} per query", started.elapsed() / runs);
    }
    let runs = 2000u32;
    let started = Instant::now();
    for i in 0..runs {
        let run = t.query_run::<fns::filtered_notes_page>((i64::from(i) * 4 % 9000, 8, 10)).await.unwrap();
        assert_eq!(run.result.unwrap(), serde_json::json!(10));
    }
    println!("filtered scan, {:<22}: {:>9.1?} per query", "page of 10, every 8th", started.elapsed() / runs);
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
            olap: rxferrix::olap::OlapOptions { enabled: olap, background, threads, ..Default::default() },
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

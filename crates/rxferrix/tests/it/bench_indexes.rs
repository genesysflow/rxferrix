//! What indexes cost, all of them being rows of one SQLite table (`index_entries`) (ignored; run in release mode):
//!
//! ```sh
//! cargo test --release -p rxferrix --test it bench_indexes -- --ignored --nocapture --test-threads 1
//! ```
//!
//! `bench_indexes_per_table`: one-document inserts and patches (16 writers, `BENCH_SECS` each) into tables with 1, 8
//! and 32 indexes; the patches change one indexed field, or all 32.
//! `bench_indexes_large_table`: the same writes into a table with 8 indexes already holding `BENCH_FILL` documents
//! (default 200k; far more than the writer's page cache), the patches at random places, with the process's memory.
//! `bench_indexes_shared_table`: lookups on a small table's index, before and after another table's 32 indexes fill
//! `index_entries` with millions of entries (`BENCH_FILL` documents, default 100k): through a query function, and as
//! the bare SQL range read (no function overhead), with the depth of the B-tree.
//! `bench_index_key_length`: `BENCH_KEY_DOCS` (20k) documents indexed by a string of 16 to 4096 bytes (a shared
//! prefix, the unique part last, like URLs or paths): load rate, lookups, scans, and what the entries take on disk.

use std::future::Future;
use std::time::{Duration, Instant};

use rxferrix::testing::TestApp;
use rxferrix::{App, AppOptions};
use serde_json::{Map, Value, json};

/// Declares a table with 32 integer fields `f00`..`f31` and the given attributes (`#[table]` first).
macro_rules! wide_table {
    ($(#[$($attr:tt)*])* $name:ident) => {
        $(#[$($attr)*])*
        pub struct $name {
            pub f00: i64, pub f01: i64, pub f02: i64, pub f03: i64, pub f04: i64, pub f05: i64, pub f06: i64,
            pub f07: i64, pub f08: i64, pub f09: i64, pub f10: i64, pub f11: i64, pub f12: i64, pub f13: i64,
            pub f14: i64, pub f15: i64, pub f16: i64, pub f17: i64, pub f18: i64, pub f19: i64, pub f20: i64,
            pub f21: i64, pub f22: i64, pub f23: i64, pub f24: i64, pub f25: i64, pub f26: i64, pub f27: i64,
            pub f28: i64, pub f29: i64, pub f30: i64, pub f31: i64,
        }
    };
}

pub mod fns {
    use rxferrix::prelude::*;
    use serde_json::Value;

    wide_table!(
        #[table(name = "bench_idx_1")]
        #[index(i00 = [f00])]
        Wide1
    );

    wide_table!(
        #[table(name = "bench_idx_8")]
        #[index(i00 = [f00])]
        #[index(i01 = [f01])]
        #[index(i02 = [f02])]
        #[index(i03 = [f03])]
        #[index(i04 = [f04])]
        #[index(i05 = [f05])]
        #[index(i06 = [f06])]
        #[index(i07 = [f07])]
        Wide8
    );

    wide_table!(
        #[table(name = "bench_idx_32")]
        #[index(i00 = [f00])]
        #[index(i01 = [f01])]
        #[index(i02 = [f02])]
        #[index(i03 = [f03])]
        #[index(i04 = [f04])]
        #[index(i05 = [f05])]
        #[index(i06 = [f06])]
        #[index(i07 = [f07])]
        #[index(i08 = [f08])]
        #[index(i09 = [f09])]
        #[index(i10 = [f10])]
        #[index(i11 = [f11])]
        #[index(i12 = [f12])]
        #[index(i13 = [f13])]
        #[index(i14 = [f14])]
        #[index(i15 = [f15])]
        #[index(i16 = [f16])]
        #[index(i17 = [f17])]
        #[index(i18 = [f18])]
        #[index(i19 = [f19])]
        #[index(i20 = [f20])]
        #[index(i21 = [f21])]
        #[index(i22 = [f22])]
        #[index(i23 = [f23])]
        #[index(i24 = [f24])]
        #[index(i25 = [f25])]
        #[index(i26 = [f26])]
        #[index(i27 = [f27])]
        #[index(i28 = [f28])]
        #[index(i29 = [f29])]
        #[index(i30 = [f30])]
        #[index(i31 = [f31])]
        Wide32
    );

    #[table(name = "bench_probe")]
    #[index(by_key = [key])]
    pub struct Probe {
        pub key: i64,
    }

    #[table(name = "bench_keys")]
    #[index(by_text = [text])]
    pub struct Keyed {
        pub text: String,
    }

    #[internal_mutation]
    pub async fn insert_docs(ctx: &MutationCtx, table: String, docs: Vec<Value>) -> Result<Vec<String>> {
        let mut ids = Vec::with_capacity(docs.len());
        for doc in docs {
            ids.push(ctx.db.insert_raw(&table, doc).await?);
        }
        Ok(ids)
    }

    #[internal_mutation]
    pub async fn patch_doc(ctx: &MutationCtx, id: String, patch: Value) -> Result<()> {
        ctx.db.patch_raw(&id, patch).await
    }

    /// The number of documents whose `field` equals `value`, through the index `index`.
    #[internal_query]
    pub async fn lookup(ctx: &QueryCtx, table: String, index: String, field: String, value: Value) -> Result<usize> {
        Ok(ctx.db.query_table(&table).with_index(&index, |r| r.eq(&field, value)).collect().await?.len())
    }

    /// The first `n` documents in the order of the index `index`.
    #[internal_query]
    pub async fn scan(ctx: &QueryCtx, table: String, index: String, n: u32) -> Result<usize> {
        Ok(ctx.db.query_table(&table).with_index(&index, |r| r).take(n as usize).await?.len())
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn secs() -> f64 {
    std::env::var("BENCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(2.0)
}

/// A document of `bench_idx_*` with `f00`..`f31` derived from `n`.
fn wide_doc(n: u64) -> Value {
    let fields: Map<String, Value> =
        (0..32).map(|f| (format!("f{f:02}"), json!(n.wrapping_mul(2654435761).rotate_left(f) as i64))).collect();
    Value::Object(fields)
}

/// `writers` tasks calling `op(writer, n)` back to back for `secs()`: (calls per second, p50, p99).
async fn hammer<F, Fut>(app: &App, writers: usize, op: F) -> (f64, Duration, Duration)
where
    F: Fn(App, usize, u64) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let stop = Instant::now() + Duration::from_secs_f64(secs());
    let started = Instant::now();
    let tasks: Vec<_> = (0..writers)
        .map(|w| {
            let (app, op) = (app.clone(), op.clone());
            tokio::spawn(async move {
                let mut lat = Vec::new();
                let mut n = 0;
                while Instant::now() < stop {
                    let t0 = Instant::now();
                    op(app.clone(), w, n).await;
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

/// Insert `docs` into `table`, `batch` per mutation: the new ids and the documents per second.
async fn load(t: &TestApp, table: &str, docs: impl Iterator<Item = Value>, batch: usize) -> (Vec<String>, f64) {
    let started = Instant::now();
    let mut ids = Vec::new();
    let docs: Vec<Value> = docs.collect();
    for chunk in docs.chunks(batch) {
        ids.extend(t.mutation::<fns::insert_docs>((table.to_string(), chunk.to_vec())).await.unwrap());
    }
    let rate = ids.len() as f64 / started.elapsed().as_secs_f64();
    (ids, rate)
}

/// Mean time of `runs` calls of `f`.
async fn mean<Fut: Future<Output = ()>>(runs: u32, mut f: impl FnMut(u32) -> Fut) -> Duration {
    let started = Instant::now();
    for i in 0..runs {
        f(i).await;
    }
    started.elapsed() / runs
}

/// What `index_entries` looks like on disk: (entries, pages, overflow pages, B-tree levels, page size).
fn index_tree(t: &TestApp) -> (i64, i64, i64, i64, i64) {
    let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
    let entries: i64 = conn.query_row("SELECT COUNT(*) FROM index_entries", [], |r| r.get(0)).unwrap();
    let (pages, overflow, levels): (i64, i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(pagetype = 'overflow'), 0),
                    COALESCE(MAX(CASE WHEN pagetype != 'overflow'
                                      THEN length(path) - length(replace(path, '/', '')) END), 0)
             FROM dbstat WHERE name = 'index_entries'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    (entries, pages, overflow, levels, page_size)
}

/// The id of the index `index` of `table`.
fn index_id(conn: &rusqlite::Connection, table: &str, index: &str) -> u32 {
    conn.query_row(
        "SELECT i.index_id FROM _indexes i JOIN _tables t ON t.table_number = i.table_number
         WHERE t.name = ?1 AND i.name = ?2",
        [table, index],
        |r| r.get(0),
    )
    .unwrap()
}

/// Mean time of the bare SQL read behind an `eq` lookup on `index` (the range of keys starting with the value), for
/// each of `values` in turn, on a connection of its own (warmed by one pass first).
fn sql_lookup(t: &TestApp, table: &str, index: &str, values: &[Value]) -> Duration {
    let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
    let id = index_id(&conn, table, index);
    let mut stmt =
        conn.prepare("SELECT COUNT(*) FROM index_entries WHERE index_id = ?1 AND key >= ?2 AND key < ?3").unwrap();
    let ranges: Vec<(Vec<u8>, Vec<u8>)> = values
        .iter()
        .map(|v| {
            let lo = rxferrix::keys::encode(v);
            let hi = rxferrix::keys::prefix_successor(&lo).unwrap();
            (lo, hi)
        })
        .collect();
    let mut run = || {
        for (lo, hi) in &ranges {
            let n: i64 = stmt.query_row(rusqlite::params![id, lo, hi], |r| r.get(0)).unwrap();
            assert_eq!(n, 1);
        }
    };
    run();
    let started = Instant::now();
    run();
    started.elapsed() / ranges.len() as u32
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_indexes_per_table() {
    for (indexes, table) in [(1, "bench_idx_1"), (8, "bench_idx_8"), (32, "bench_idx_32")] {
        let t = TestApp::with_options(AppOptions::default()).await.unwrap();
        let (rate, p50, p99) = hammer(t.app(), 16, move |app, w, n| async move {
            let doc = wide_doc(((w as u64) << 32) | n);
            app.mutation::<fns::insert_docs>((table.to_string(), vec![doc])).await.unwrap();
        })
        .await;
        println!("{indexes:>2} indexes, insert          : {rate:>8.0}/s, p50 {p50:>9.1?}, p99 {p99:>9.1?}");

        let (ids, _) = load(&t, table, (0..1600).map(wide_doc), 100).await;
        let ids = std::sync::Arc::new(ids);
        for (label, all) in [("patch one field ", false), ("patch 32 fields ", true)] {
            let ids = ids.clone();
            let (rate, p50, p99) = hammer(t.app(), 16, move |app, w, n| {
                let ids = ids.clone();
                async move {
                    // Each writer patches its own 100 documents, so writers don't touch the same ones.
                    let id = ids[w * 100 + (n % 100) as usize].clone();
                    let patch =
                        if all { wide_doc((1 << 40) | ((w as u64) << 32) | n) } else { json!({ "f00": n as i64 }) };
                    app.mutation::<fns::patch_doc>((id, patch)).await.unwrap();
                }
            })
            .await;
            println!("{indexes:>2} indexes, {label}: {rate:>8.0}/s, p50 {p50:>9.1?}, p99 {p99:>9.1?}");
        }
    }
}

/// The process's resident memory, from `/proc/self/status` (Linux; "?" elsewhere).
fn rss() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kb =
        status.lines().find_map(|l| l.strip_prefix("VmRSS:")).map(|v| v.trim().trim_end_matches(" kB").to_string());
    kb.and_then(|kb| kb.parse::<u64>().ok()).map_or("?".into(), |kb| format!("{} MiB", kb / 1024))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_indexes_large_table() {
    let t = TestApp::with_options(AppOptions::default()).await.unwrap();
    let fill = env("BENCH_FILL", 200_000);
    let (ids, rate) = load(&t, "bench_idx_8", (0..fill).map(wide_doc), 500).await;
    let (entries, pages, _, levels, page_size) = index_tree(&t);
    println!(
        "loaded {fill} documents with 8 indexes: {rate:.0} documents/s; index_entries {entries} entries, {} MiB, \
         {levels} levels; RSS {}",
        (pages * page_size) >> 20,
        rss()
    );
    let (rate, p50, p99) = hammer(t.app(), 16, move |app, w, n| async move {
        let doc = wide_doc((1 << 40) | ((w as u64) << 32) | n);
        app.mutation::<fns::insert_docs>(("bench_idx_8".to_string(), vec![doc])).await.unwrap();
    })
    .await;
    println!("insert into it          : {rate:>8.0}/s, p50 {p50:>9.1?}, p99 {p99:>9.1?}");
    let ids = std::sync::Arc::new(ids);
    for (label, all) in [("patch one field ", false), ("patch 32 fields ", true)] {
        let ids = ids.clone();
        let (rate, p50, p99) = hammer(t.app(), 16, move |app, w, n| {
            let ids = ids.clone();
            async move {
                // Documents all over the table; each writer its own (`w` modulo 16).
                let i = (((n * 16 + w as u64).wrapping_mul(2654435761) % (ids.len() as u64 / 16)) * 16) as usize + w;
                let patch = if all { wide_doc((2 << 40) | ((w as u64) << 32) | n) } else { json!({ "f00": n as i64 }) };
                app.mutation::<fns::patch_doc>((ids[i].clone(), patch)).await.unwrap();
            }
        })
        .await;
        println!("{label}at random: {rate:>8.0}/s, p50 {p50:>9.1?}, p99 {p99:>9.1?}");
    }
    println!("RSS {}", rss());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_indexes_shared_table() {
    let t = TestApp::with_options(AppOptions::default()).await.unwrap();
    let probes = 10_000u64;
    load(&t, "bench_probe", (0..probes).map(|k| json!({ "key": k as i64 })), 500).await;
    // Keys spread over the whole table, the same ones each round.
    let values: Vec<Value> = (0..1000).map(|i| json!((i * 7919 % probes) as i64)).collect();

    let measure = |label: String| {
        let (t, values) = (&t, &values);
        async move {
            let lookup = mean(values.len() as u32, |i| async move {
                let args = ("bench_probe".into(), "by_key".into(), "key".into(), values[i as usize].clone());
                let run = t.query_run::<fns::lookup>(args).await.unwrap();
                assert_eq!(run.result.unwrap(), json!(1));
            })
            .await;
            let scan = mean(200, |_| async move {
                let run = t.query_run::<fns::scan>(("bench_probe".into(), "by_key".into(), 100)).await.unwrap();
                assert_eq!(run.result.unwrap(), json!(100));
            })
            .await;
            let sql = sql_lookup(t, "bench_probe", "by_key", values);
            let (entries, pages, _, levels, _) = index_tree(t);
            println!(
                "{label:<34}: index_entries {entries:>9} entries, {pages:>7} pages, {levels} levels | eq lookup \
                 {lookup:>8.1?} (SQL alone {sql:>7.1?}), first 100 {scan:>8.1?}"
            );
        }
    };

    measure("probe table alone".into()).await;
    let fill = env("BENCH_FILL", 100_000);
    let (_, rate) = load(&t, "bench_idx_32", (0..fill).map(wide_doc), 500).await;
    println!("(loaded {fill} documents with 32 indexes each: {rate:.0} documents/s)");
    measure(format!("+ {fill} docs × 32 indexes elsewhere")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_index_key_length() {
    let docs = env("BENCH_KEY_DOCS", 20_000);
    for len in [16usize, 64, 128, 256, 512, 1024, 2048, 4096] {
        let t = TestApp::with_options(AppOptions::default()).await.unwrap();
        let t = &t;
        // A shared prefix, then the document's number: comparisons have to read most of the key.
        let text = |i: u64| Value::String(format!("{}{i:010}", "x".repeat(len - 10)));
        let (_, rate) = load(t, "bench_keys", (0..docs).map(|i| json!({ "text": text(i) })), 100).await;
        let values: Vec<Value> = (0..1000).map(|i| text(i * 7919 % docs)).collect();
        let lookup = mean(values.len() as u32, |i| {
            let args = ("bench_keys".into(), "by_text".into(), "text".into(), values[i as usize].clone());
            async move {
                let run = t.query_run::<fns::lookup>(args).await.unwrap();
                assert_eq!(run.result.unwrap(), json!(1));
            }
        })
        .await;
        let scan = mean(200, |_| async move {
            let run = t.query_run::<fns::scan>(("bench_keys".into(), "by_text".into(), 100)).await.unwrap();
            assert_eq!(run.result.unwrap(), json!(100));
        })
        .await;
        let sql = sql_lookup(t, "bench_keys", "by_text", &values);
        let (_, pages, overflow, levels, page_size) = index_tree(t);
        let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
        let id = index_id(&conn, "bench_keys", "by_text");
        let key_bytes: f64 = conn
            .query_row("SELECT AVG(length(key)) FROM index_entries WHERE index_id = ?1", [id], |r| r.get(0))
            .unwrap();
        println!(
            "{len:>4} B strings ({key_bytes:>4.0} B keys): load {rate:>7.0} docs/s | eq lookup {lookup:>8.1?} (SQL \
             alone {sql:>7.1?}), first 100 {scan:>8.1?} | index_entries {pages:>6} pages ({overflow:>5} overflow), \
             {levels} levels, {:>5.0} B/entry",
            pages as f64 * page_size as f64 / (docs * 2) as f64,
        );
    }
}

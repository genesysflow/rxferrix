//! Benchmarks over real sockets (ignored; run them in release mode). The other `bench_*` files measure the engine
//! in-process; these go through the HTTP API and the WebSocket endpoint, with their clients in the same process:
//!
//! ```sh
//! cargo test --release -p rxferrix --test it bench_network -- --ignored --nocapture --test-threads 1
//! scripts/bench.sh bench_network                                   # the same
//! TARGET=x86_64-unknown-linux-musl scripts/bench.sh bench_network  # built like the release binaries
//! ```
//!
//! * `bench_http_calls`: `BENCH_CONCURRENCY` (64) callers over keep-alive connections for `BENCH_SECS` (5) per
//!   scenario: a query and a mutation, each anonymous and signed in (`BENCH_USERS` (8) users, the callers spread over
//!   their access tokens), and an action returning a large result (1000 strings, 17 KB). Reported: calls per second
//!   and latency percentiles.
//! * `bench_uploads`: `BENCH_UPLOADERS` (16) tasks each get an upload URL (a mutation) and post a 1 KiB file to it, back
//!   to back for `BENCH_SECS` (10). Reported: uploads per second in each 2 s window, as the table of used upload tokens
//!   grows.
//! * `bench_ws_fanout`: `BENCH_CONNS` (1000) sockets each subscribe to the same `BENCH_QUERIES` (50) queries, all
//!   reading one index range, so every commit (`BENCH_RATE` (50) per second for `BENCH_SECS`) invalidates every query
//!   of every connection. Reported: commit-to-client latency and Transitions per connection.
//! * `bench_ws_fanout_lists`: the same with `BENCH_QUERIES` (10) queries that each return the items of one group,
//!   `BENCH_ITEMS` (500) to start with, each commit adding one; the clients accept result diffs, as the JS client
//!   does.
//! * `bench_ws_fanout_signed_in`: `bench_ws_fanout` with every socket signed in (as one of `BENCH_USERS` (8) users);
//!   the queries don't read the identity, so the sockets still share them.
//! * `bench_ws_slow_reader`: a socket watching a large result (`BENCH_ITEMS` (2000) strings, all changed by every
//!   commit) stops reading for `BENCH_PAUSE_SECS` (3) while `BENCH_RATE` (50) commits per second change it, next to a
//!   socket that keeps reading. Reported: the process's resident memory gained during the pause, and the frames and
//!   bytes the slow socket then reads before it has caught up.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use rxferrix::FunctionRef;
use rxferrix::auth::AuthConfig;
use rxferrix::testing::TestApp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::bench::{env, fns as bench_fns, percentile, report};

mod fns {
    use rxferrix::prelude::*;

    use crate::common::items::Item;

    /// One document's score: a point read, so a call costs little more than the framework's own work.
    #[query]
    pub async fn net_get(ctx: &QueryCtx, id: Id<Item>) -> Result<i64> {
        Ok(ctx.db.get(id).await?.map_or(0, |item| item.value.score))
    }

    #[mutation]
    pub async fn net_set(ctx: &MutationCtx, id: Id<Item>, score: i64) -> Result<()> {
        ctx.db.patch(id, json!({ "score": score })).await
    }

    /// How many items are in `group`. `salt` only makes otherwise identical queries distinct.
    #[query]
    pub async fn net_count(ctx: &QueryCtx, group: i64, salt: u32) -> Result<usize> {
        let _ = salt;
        Ok(ctx.db.query::<Item>().with_index(Item::by_group_score, |q| q.eq(Item::group, group)).collect().await?.len())
    }

    /// The items of `group`. `salt` only makes otherwise identical queries distinct.
    #[query]
    pub async fn net_items(ctx: &QueryCtx, group: i64, salt: u32) -> Result<Vec<Doc<Item>>> {
        let _ = salt;
        ctx.db.query::<Item>().with_index(Item::by_group_score, |q| q.eq(Item::group, group)).collect().await
    }

    /// `n` short strings: a large result that comes from no table.
    #[action]
    pub async fn net_list(ctx: &ActionCtx, n: u32) -> Result<Vec<String>> {
        let _ = ctx;
        Ok((0..n).map(|i| format!("item {i:08}")).collect())
    }

    #[mutation]
    pub async fn net_upload_url(ctx: &MutationCtx) -> Result<String> {
        ctx.storage.generate_upload_url().await
    }

    /// `n` strings that all change when the counter `id` does: a large result no diff can shrink.
    #[query]
    pub async fn net_blob(ctx: &QueryCtx, id: Id<Item>, n: u32) -> Result<Vec<String>> {
        let score = ctx.db.get(id).await?.map_or(0, |item| item.value.score);
        Ok((0..n).map(|i| format!("{score:08}:{i:08}")).collect())
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The part of a server message these benchmarks read.
#[derive(Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "endVersion")]
    end_version: Option<Version>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Version {
    query_set: u64,
    ts: u64,
}

/// Connect, subscribe to `queries` (path, args), and wait for their first results.
async fn subscribe(addr: SocketAddr, session: u64, queries: &[(String, Value)]) -> Socket {
    subscribe_with(addr, session, queries, false, None).await
}

/// [`subscribe`], accepting result diffs if `diffs`, signed in with `token` if given.
async fn subscribe_with(
    addr: SocketAddr,
    session: u64,
    queries: &[(String, Value)],
    diffs: bool,
    token: Option<&str>,
) -> Socket {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
    let features: &[&str] = if diffs { &["diffs"] } else { &[] };
    let connect = json!({ "type": "Connect", "sessionId": format!("bench{session}"), "connectionCount": 0,
        "lastCloseReason": null, "clientVersion": "0.1.0", "features": features });
    let modifications: Vec<Value> = queries
        .iter()
        .enumerate()
        .map(|(i, (path, args))| json!({ "type": "Add", "queryId": i, "udfPath": path, "args": args }))
        .collect();
    let modify = json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": modifications });
    ws.send(Message::text(connect.to_string())).await.unwrap();
    if let Some(token) = token {
        let authenticate = json!({ "type": "Authenticate", "baseVersion": 0, "token": token });
        ws.send(Message::text(authenticate.to_string())).await.unwrap();
    }
    ws.send(Message::text(modify.to_string())).await.unwrap();
    while let Some(msg) = ws.next().await {
        if let Some((1, _)) = transition(&msg.unwrap()) {
            return ws;
        }
    }
    panic!("socket closed before its first results");
}

/// A Transition's (endVersion.querySet, endVersion.ts).
fn transition(msg: &Message) -> Option<(u64, u64)> {
    let Message::Text(text) = msg else { return None };
    let frame: Frame = serde_json::from_str(text.as_str()).unwrap();
    assert_ne!(frame.kind, "FatalError", "{text}");
    let v = frame.end_version.filter(|_| frame.kind == "Transition")?;
    Some((v.query_set, v.ts))
}

/// The process's resident memory, in bytes (Linux only).
fn rss() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    Some(statm.split_whitespace().nth(1)?.parse::<u64>().ok()? * 4096)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

// ---------------------------------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------------------------------

/// `concurrency` callers post `body` to `url` (each with the token at its index, cycling) for `secs`.
async fn load(
    client: &reqwest::Client,
    url: &str,
    body: &str,
    tokens: &[Option<String>],
    concurrency: u64,
    secs: u64,
) -> (u64, Vec<Duration>) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let callers: Vec<_> = (0..concurrency as usize)
        .map(|i| {
            let (client, url, body, token) =
                (client.clone(), url.to_string(), body.to_string(), tokens[i % tokens.len()].clone());
            tokio::spawn(async move {
                let mut latencies = Vec::new();
                while Instant::now() < deadline {
                    let mut req = client.post(&url).header("content-type", "application/json").body(body.clone());
                    if let Some(token) = &token {
                        req = req.bearer_auth(token);
                    }
                    let started = Instant::now();
                    let res = req.send().await.unwrap();
                    let status = res.status();
                    let text = res.text().await.unwrap();
                    assert!(status.is_success(), "{status}: {text}");
                    latencies.push(started.elapsed());
                }
                latencies
            })
        })
        .collect();
    let mut latencies = Vec::new();
    for caller in callers {
        latencies.extend(caller.await.unwrap());
    }
    latencies.sort();
    (latencies.len() as u64, latencies)
}

/// Create `users` users and sign each in over HTTP: their access tokens.
async fn sign_in(t: &TestApp, client: &reqwest::Client, base: &str, users: u64) -> Vec<String> {
    let mut tokens = Vec::new();
    for u in 0..users {
        let email = format!("bench{u}@example.com");
        t.create_user(&email, Some("correct horse battery")).await.unwrap();
        let res = client
            .post(format!("{base}/api/auth/signin"))
            .json(&json!({ "email": email, "password": "correct horse battery" }))
            .send()
            .await
            .unwrap();
        let body: Value = res.json().await.unwrap();
        tokens.push(body["accessToken"].as_str().unwrap_or_else(|| panic!("{body}")).to_string());
    }
    tokens
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_http_calls() {
    let (concurrency, secs, users) = (env("BENCH_CONCURRENCY", 64), env("BENCH_SECS", 5), env("BENCH_USERS", 8));
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let addr = t.serve().await.unwrap();
    let client = reqwest::Client::builder().pool_max_idle_per_host(concurrency as usize).build().unwrap();
    let base = format!("http://{addr}");

    let id = t.mutation::<bench_fns::bench_new_counter>(()).await.unwrap();
    let signed_in: Vec<_> = sign_in(&t, &client, &base, users).await.into_iter().map(Some).collect();
    let anonymous = vec![None];

    let query = json!({ "path": fns::net_get::path(), "args": { "id": id } }).to_string();
    let mutation = json!({ "path": fns::net_set::path(), "args": { "id": id, "score": 1 } }).to_string();
    let action = json!({ "path": fns::net_list::path(), "args": { "n": 1000 } }).to_string();
    for (name, kind, body, tokens) in [
        ("query, anonymous", "query", &query, &anonymous),
        ("query, signed in", "query", &query, &signed_in),
        ("mutation, anonymous", "mutation", &mutation, &anonymous),
        ("mutation, signed in", "mutation", &mutation, &signed_in),
        ("action returning 1000 strings, anonymous", "action", &action, &anonymous),
    ] {
        let (calls, latencies) = load(&client, &format!("{base}/api/{kind}"), body, tokens, concurrency, secs).await;
        println!(
            "[http {name}] {concurrency} callers: {:.0} calls/s, p50 {:?} p99 {:?} max {:?}",
            calls as f64 / secs as f64,
            percentile(&latencies, 0.5),
            percentile(&latencies, 0.99),
            latencies.last().copied().unwrap_or_default(),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_uploads() {
    let (uploaders, secs) = (env("BENCH_UPLOADERS", 16), env("BENCH_SECS", 10));
    let storage = rxferrix::storage::StorageConfig::default().upload_rate_limit(None);
    let t = TestApp::with_options(rxferrix::AppOptions { dev: false, storage, ..Default::default() }).await.unwrap();
    let addr = t.serve().await.unwrap();
    let client = reqwest::Client::new();
    let started = Instant::now();
    let windows: Arc<Vec<AtomicU64>> = Arc::new((0..secs.div_ceil(2)).map(|_| AtomicU64::new(0)).collect());
    let tasks: Vec<_> = (0..uploaders)
        .map(|_| {
            let (app, client, windows) = (t.app().clone(), client.clone(), windows.clone());
            tokio::spawn(async move {
                while started.elapsed() < Duration::from_secs(secs) {
                    let url = app.mutation::<fns::net_upload_url>(()).await.unwrap();
                    let url = if url.starts_with('/') { format!("http://{addr}{url}") } else { url };
                    let res = client.post(&url).header("content-type", "text/plain").body(vec![b'x'; 1024]);
                    let res = res.send().await.unwrap();
                    let status = res.status();
                    // Read to the end, so the connection is kept alive for the next upload.
                    let body = res.text().await.unwrap();
                    assert!(status.is_success(), "{status}: {body}");
                    let window = (started.elapsed().as_secs() / 2) as usize;
                    if let Some(w) = windows.get(window) {
                        w.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let rates: Vec<String> = windows.iter().map(|w| format!("{:.0}", w.load(Ordering::Relaxed) as f64 / 2.0)).collect();
    let total: u64 = windows.iter().map(|w| w.load(Ordering::Relaxed)).sum();
    println!("[uploads] {uploaders} uploaders: {total} uploads; per 2 s window: {} uploads/s", rates.join(", "));
}

// ---------------------------------------------------------------------------------------------------------------------
// WebSockets
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_ws_fanout() {
    fanout("ws fanout", fns::net_count::path(), env("BENCH_QUERIES", 50), 0, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_ws_fanout_signed_in() {
    fanout("ws fanout, signed in", fns::net_count::path(), env("BENCH_QUERIES", 50), 0, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_ws_fanout_lists() {
    let queries = env("BENCH_QUERIES", 10);
    fanout("ws fanout, lists", fns::net_items::path(), queries, env("BENCH_ITEMS", 500), true, false).await;
}

/// Every socket subscribes to `queries` queries `path` (args `group` 0 and a salt); `items` items are inserted first.
/// With `signed_in`, the sockets sign in first, spread over `BENCH_USERS` (8) users.
async fn fanout(name: &str, path: String, queries: u64, items: u64, diffs: bool, signed_in: bool) {
    let (conns, rate, secs) = (env("BENCH_CONNS", 1000), env("BENCH_RATE", 50), env("BENCH_SECS", 5));
    let auth = AuthConfig::password().rate_limits(None);
    let t = TestApp::with_options(rxferrix::AppOptions { dev: false, auth, ..Default::default() }).await.unwrap();
    let addr = t.serve().await.unwrap();
    let tokens: Arc<Vec<String>> = Arc::new(if signed_in {
        sign_in(&t, &reqwest::Client::new(), &format!("http://{addr}"), env("BENCH_USERS", 8)).await
    } else {
        Vec::new()
    });
    for from in (0..items).step_by(100) {
        t.mutation::<bench_fns::bench_insert_many>((0, (items - from).min(100) as u32)).await.unwrap();
    }
    let subscriptions: Arc<Vec<(String, Value)>> =
        Arc::new((0..queries).map(|q| (path.clone(), json!({ "group": 0, "salt": q }))).collect());

    // Connect everyone (64 at a time) and wait for their first results.
    let started = Instant::now();
    let connecting = Arc::new(tokio::sync::Semaphore::new(64));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let seen: Arc<Vec<AtomicU64>> = Arc::new((0..conns).map(|_| AtomicU64::new(0)).collect());
    let received = Arc::new(AtomicU64::new(0));
    let mut ready = Vec::new();
    let mut clients = Vec::new();
    for c in 0..conns {
        let (subscriptions, connecting, mut stop) = (subscriptions.clone(), connecting.clone(), stop_rx.clone());
        let (seen, received, tokens) = (seen.clone(), received.clone(), tokens.clone());
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        ready.push(ready_rx);
        clients.push(tokio::spawn(async move {
            let permit = connecting.acquire_owned().await.unwrap();
            let token = (!tokens.is_empty()).then(|| tokens[c as usize % tokens.len()].as_str());
            let mut ws = subscribe_with(addr, c, &subscriptions, diffs, token).await;
            drop(permit);
            let _ = ready_tx.send(());
            let mut receipts = Vec::new();
            loop {
                tokio::select! {
                    msg = ws.next() => match msg {
                        Some(Ok(msg)) => {
                            received.fetch_add(msg.len() as u64, Ordering::Relaxed);
                            if let Some((_, ts)) = transition(&msg) {
                                receipts.push((ts, Instant::now()));
                                seen[c as usize].store(ts, Ordering::Relaxed);
                            }
                        }
                        other => panic!("socket {c} closed: {other:?}"),
                    },
                    _ = stop.changed() => return receipts,
                }
            }
        }));
    }
    for r in ready {
        r.await.unwrap();
    }
    let setup = started.elapsed();

    // Commit at a fixed rate; each commit invalidates every query.
    let mut commits = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / rate as f64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let started = Instant::now();
    for _ in 0..rate * secs {
        tick.tick().await;
        let ((), ts) = t.mutation_ts::<bench_fns::bench_insert>((0,)).await.unwrap();
        commits.push((ts, Instant::now()));
    }
    let elapsed = started.elapsed();

    // Wait until every socket has a Transition at or after the last commit, then collect.
    let final_ts = commits.last().unwrap().0;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && seen.iter().any(|s| s.load(Ordering::Relaxed) < final_ts) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop_tx.send(true);
    let mut receipts = Vec::new();
    for client in clients {
        receipts.push(client.await.unwrap());
    }
    let transitions = receipts.iter().map(Vec::len).sum::<usize>() as f64 / conns as f64;
    println!(
        "[{name}] {conns} sockets x {queries} queries, connected in {:.2}s; {} commits in {:.2}s ({:.0}/s), {transitions:.1} \
         Transitions per socket ({:.0} KiB received); {}",
        setup.as_secs_f64(),
        commits.len(),
        elapsed.as_secs_f64(),
        commits.len() as f64 / elapsed.as_secs_f64(),
        received.load(Ordering::Relaxed) as f64 / conns as f64 / 1024.0,
        report("", &receipts, &commits),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
async fn bench_ws_slow_reader() {
    let (items, rate, pause) = (env("BENCH_ITEMS", 2000), env("BENCH_RATE", 50), env("BENCH_PAUSE_SECS", 3));
    let t = TestApp::with_options(rxferrix::AppOptions { dev: false, ..Default::default() }).await.unwrap();
    let addr = t.serve().await.unwrap();
    let id = t.mutation::<bench_fns::bench_new_counter>(()).await.unwrap();
    let query = [(fns::net_blob::path(), json!({ "id": id, "n": items }))];
    let mut slow = subscribe(addr, 0, &query).await;
    let mut fast = subscribe(addr, 1, &query).await;
    let fast_reader = tokio::spawn(async move {
        let mut frames = 0u64;
        while let Some(Ok(msg)) = fast.next().await {
            frames += transition(&msg).is_some() as u64;
        }
        frames
    });

    // The slow socket reads nothing while the result changes `rate` times a second.
    let before = rss();
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / rate as f64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_ts = 0;
    for _ in 0..rate * pause {
        tick.tick().await;
        let ((), ts) = t.mutation_ts::<bench_fns::bench_increment>((id,)).await.unwrap();
        last_ts = ts;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let during = rss();

    // Then it reads until it has the last change.
    let (mut frames, mut bytes, started) = (0u64, 0u64, Instant::now());
    while let Some(Ok(msg)) = slow.next().await {
        bytes += msg.len() as u64;
        if let Some((_, ts)) = transition(&msg) {
            frames += 1;
            if ts >= last_ts {
                break;
            }
        }
    }
    let drained = started.elapsed();
    fast_reader.abort();
    let memory = match (before, during) {
        (Some(before), Some(during)) => {
            format!("resident memory +{:.1} MiB during the pause", mib(during.saturating_sub(before)))
        }
        _ => "resident memory not measured (no /proc)".to_string(),
    };
    println!(
        "[ws slow reader] result of {items} strings, {} commits during a {pause}s pause: {memory}; then {frames} frames \
         ({:.1} MiB) in {:.0} ms to catch up",
        rate * pause,
        mib(bytes),
        drained.as_secs_f64() * 1000.0,
    );
}

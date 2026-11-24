//! Money transfers over WebSockets, with the server and the load on different machines if wanted (ignored; run it in
//! release mode):
//!
//! ```sh
//! scripts/bench.sh bench_transfer                       # server and load in one process, over loopback
//! BENCH_LISTEN=0.0.0.0:4455 scripts/bench.sh bench_transfer   # only the server, until interrupted
//! BENCH_TARGET=host:4455 ./it-<hash> bench_transfer --ignored --nocapture   # only the load (the test binary, copied)
//! ```
//!
//! A transfer is one mutation: read two accounts by id, check the balance, write both. `BENCH_ACCOUNTS` (100000)
//! accounts with no index; the load picks the two accounts by rank from a power law (`BENCH_ALPHA`, a list: `0` is
//! uniform, `1.5` sends most transfers through a few accounts, which are spread over the table, not neighbours). `BENCH_CONNS` (64) sockets each keep `BENCH_INFLIGHT`
//! (a list, 40) mutations unanswered at a time (at most 64: the server's limit per socket), for `BENCH_SECS` (10)
//! after `BENCH_WARMUP` (2). Reported: transfers per second and latency from sending a mutation to its
//! `MutationResponse`, which the server sends once the commit is on disk. Every socket sends `Connect` first as the
//! JS client does, so every mutation carries an idempotency key (`BENCH_SESSION=0`: no `Connect`, no keys).
//!
//! The server prints each second the transfers it ran, the cores it kept busy and the time inside a transfer (its
//! reads and writes, without the commit); the load prints its own cores: a load
//! generator at its limit measures itself. `BENCH_FSYNC=0`: commits don't wait for the disk. `TMPDIR` must be on a
//! real disk for the fsync to mean anything. `BENCH_WORKERS`: runtime threads (default: one per core, as `serve`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use rxferrix::testing::TestApp;
use rxferrix::{FunctionRef, Identity};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::bench::{env, percentile};

/// Transfers the server has run (successful or not).
static TRANSFERS: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds spent inside `transfer` (its two reads and two writes), and in the reads alone.
static BODY_NANOS: AtomicU64 = AtomicU64::new(0);
static READ_NANOS: AtomicU64 = AtomicU64::new(0);

pub mod fns {
    use rxferrix::prelude::*;

    #[table(name = "bench_balances")]
    pub struct Account {
        pub balance: i64,
    }

    #[mutation]
    pub async fn transfer_seed(ctx: &MutationCtx, n: u32, balance: i64) -> Result<()> {
        for _ in 0..n {
            ctx.db.insert(Account { balance }).await?;
        }
        Ok(())
    }

    #[query]
    pub async fn transfer_accounts(ctx: &QueryCtx) -> Result<Vec<Id<Account>>> {
        Ok(ctx.db.query::<Account>().collect().await?.into_iter().map(|account| account.id).collect())
    }

    /// The sum of every balance: transfers must leave it unchanged.
    #[query]
    pub async fn transfer_total(ctx: &QueryCtx) -> Result<i64> {
        Ok(ctx.db.query::<Account>().collect().await?.iter().map(|account| account.value.balance).sum())
    }

    #[mutation]
    pub async fn transfer(ctx: &MutationCtx, from: Id<Account>, to: Id<Account>, amount: i64) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        super::TRANSFERS.fetch_add(1, Relaxed);
        if from == to {
            return Err(UserError::new("same_account", "from and to are the same account").into());
        }
        if amount <= 0 {
            return Err(UserError::new("non_positive_amount", "amount must be positive").into());
        }
        let source = ctx.db.get(from).await?.ok_or_else(|| UserError::new("account_missing", "no such account"))?;
        let target = ctx.db.get(to).await?.ok_or_else(|| UserError::new("account_missing", "no such account"))?;
        super::READ_NANOS.fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
        if source.value.balance < amount {
            return Err(UserError::new("insufficient_funds", "balance too low").into());
        }
        ctx.db.patch(from, json!({ "balance": source.value.balance - amount })).await?;
        ctx.db.patch(to, json!({ "balance": target.value.balance + amount })).await?;
        super::BODY_NANOS.fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
        Ok(())
    }
}

const INITIAL_BALANCE: i64 = 1_000_000_000;

fn env_list(name: &str, default: &str) -> Vec<f64> {
    let list = std::env::var(name).unwrap_or_else(|_| default.to_string());
    list.split(',').map(|v| v.trim().parse().unwrap_or_else(|_| panic!("{name}: {v:?} is not a number"))).collect()
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).map_or(true, |v| !matches!(v.to_lowercase().as_str(), "0" | "false" | "no" | "off"))
}

/// CPU time this process has used so far (user and system), from `/proc/self/stat` (Linux only; zero elsewhere).
fn cpu_time() -> Duration {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else { return Duration::ZERO };
    // The fields after the command name (which may contain spaces): utime and stime are the 12th and 13th, in ticks
    // of 10 ms.
    let fields: Vec<&str> = stat.rsplit(')').next().unwrap_or("").split_whitespace().collect();
    let ticks = |i: usize| fields.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    Duration::from_millis((ticks(11) + ticks(12)) * 10)
}

// ---------------------------------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------------------------------

/// Open an app with `BENCH_ACCOUNTS` accounts and serve it on `listen`.
async fn start_server(listen: &str) -> (TestApp, SocketAddr) {
    let fsync_commits = env_flag("BENCH_FSYNC");
    let options = rxferrix::AppOptions { dev: false, fsync_commits, ..Default::default() };
    let t = TestApp::with_options(options).await.unwrap();
    let accounts = env("BENCH_ACCOUNTS", 100_000);
    let started = Instant::now();
    let mut left = accounts;
    while left > 0 {
        let n = left.min(1000);
        let args = json!({ "n": n, "balance": INITIAL_BALANCE });
        t.app().execute_mutation(&fns::transfer_seed::path(), args, &Identity::default(), None).await.result.unwrap();
        left -= n;
    }
    let listener = tokio::net::TcpListener::bind(listen).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = t.app().clone();
    tokio::spawn(async move { rxferrix::serve(listener, app).await.unwrap() });
    println!(
        "[transfer server] {accounts} accounts seeded in {:.1?}, fsync {}, data in {}, listening on {addr}",
        started.elapsed(),
        if fsync_commits { "on" } else { "off" },
        t.dir().display(),
    );
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let (mut transfers, mut cpu, mut body, mut read, mut at) = (0, cpu_time(), 0, 0, Instant::now());
        loop {
            tick.tick().await;
            let (now_transfers, now_cpu) = (TRANSFERS.load(Ordering::Relaxed), cpu_time());
            let (now_body, now_read) = (BODY_NANOS.load(Ordering::Relaxed), READ_NANOS.load(Ordering::Relaxed));
            if now_transfers != transfers {
                let secs = at.elapsed().as_secs_f64();
                let (n, cores) = ((now_transfers - transfers) as f64 / secs, (now_cpu - cpu).as_secs_f64() / secs);
                println!(
                    "[transfer server] {n:.0} transfers/s, {cores:.1} cores busy, {:.1} µs in each transfer ({:.1} µs \
                     reading)",
                    (now_body - body) as f64 / (now_transfers - transfers) as f64 / 1e3,
                    (now_read - read) as f64 / (now_transfers - transfers) as f64 / 1e3,
                );
            }
            (transfers, cpu, body, read, at) = (now_transfers, now_cpu, now_body, now_read, Instant::now());
        }
    });
    (t, addr)
}

// ---------------------------------------------------------------------------------------------------------------------
// Load
// ---------------------------------------------------------------------------------------------------------------------

/// SplitMix64: small, fast, and good enough to pick accounts.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// In [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Ranks `0..n` with probability proportional to `1 / (rank + 1)^alpha` (uniform for `alpha <= 0`).
struct PowerLaw {
    n: usize,
    cdf: Vec<f64>,
}

impl PowerLaw {
    fn new(n: usize, alpha: f64) -> PowerLaw {
        if alpha <= 0.0 {
            return PowerLaw { n, cdf: Vec::new() };
        }
        let mut cdf: Vec<f64> = Vec::with_capacity(n);
        let mut sum = 0.0;
        for rank in 0..n {
            sum += 1.0 / ((rank + 1) as f64).powf(alpha);
            cdf.push(sum);
        }
        cdf.iter_mut().for_each(|c| *c /= sum);
        PowerLaw { n, cdf }
    }

    fn sample(&self, rng: &mut Rng) -> usize {
        if self.cdf.is_empty() {
            return (rng.unit() * self.n as f64) as usize;
        }
        let u = rng.unit();
        self.cdf.partition_point(|c| *c < u).min(self.n - 1)
    }

    fn pair(&self, rng: &mut Rng) -> (usize, usize) {
        let from = self.sample(rng);
        loop {
            let to = self.sample(rng);
            if to != from {
                return (from, to);
            }
        }
    }
}

/// The part of a server message the load reads.
#[derive(Deserialize)]
struct Frame<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(rename = "requestId")]
    request_id: Option<u64>,
    success: Option<bool>,
}

#[derive(Default)]
struct Load {
    /// Send-to-response times of the transfers answered inside the measured window.
    latencies: Vec<Duration>,
    /// Transfers answered with an error, inside the window or not.
    failed: u64,
}

/// What every socket of a run shares.
struct Plan {
    url: String,
    /// The prefix of this run's session ids (`None`: no `Connect`). Unique per run: a mutation with a session and
    /// request id the server has seen is answered from its record, not run.
    session: Option<String>,
    ids: Vec<String>,
    ranks: PowerLaw,
    inflight: usize,
    measure_from: Instant,
    until: Instant,
}

/// One socket: keep `plan.inflight` transfers unanswered until `plan.until`.
async fn socket(plan: Arc<Plan>, n: u64) -> Load {
    let (mut ws, _) = tokio_tungstenite::connect_async(&plan.url).await.unwrap();
    if let Some(session) = &plan.session {
        let connect = json!({ "type": "Connect", "sessionId": format!("{session}-{n}"), "connectionCount": 0,
            "lastCloseReason": null, "clientVersion": "0.1.0", "features": [] });
        ws.send(Message::text(connect.to_string())).await.unwrap();
    }
    let path = fns::transfer::path();
    let mut rng = Rng(0x7261_6e73_6665_7200 ^ n);
    let mut sent: HashMap<u64, Instant> = HashMap::with_capacity(plan.inflight * 2);
    let mut next_id = 0u64;
    let mut next = |sent: &mut HashMap<u64, Instant>| {
        let (from, to) = plan.ranks.pair(&mut rng);
        let (from, to) = (&plan.ids[from], &plan.ids[to]);
        let text = format!(
            r#"{{"type":"Mutation","requestId":{next_id},"udfPath":"{path}","args":{{"from":"{from}","to":"{to}","amount":1}}}}"#
        );
        sent.insert(next_id, Instant::now());
        next_id += 1;
        Message::text(text)
    };
    for _ in 0..plan.inflight {
        let msg = next(&mut sent);
        ws.feed(msg).await.unwrap();
    }
    ws.flush().await.unwrap();

    let mut load = Load::default();
    while let Some(msg) = ws.next().await {
        let Message::Text(text) = msg.unwrap() else { continue };
        let frame: Frame = serde_json::from_str(text.as_str()).unwrap();
        match frame.kind {
            "MutationResponse" => {
                let started = sent.remove(&frame.request_id.unwrap()).expect("a response to a request we sent");
                let now = Instant::now();
                if frame.success != Some(true) {
                    if load.failed == 0 {
                        eprintln!("[transfer] a transfer failed: {text}");
                    }
                    load.failed += 1;
                } else if now >= plan.measure_from && now < plan.until {
                    load.latencies.push(now - started);
                }
                if now >= plan.until {
                    break;
                }
                let msg = next(&mut sent);
                ws.send(msg).await.unwrap();
            }
            "Ping" => ws.send(Message::text(r#"{"type":"Pong"}"#)).await.unwrap(),
            "FatalError" => panic!("{text}"),
            _ => {}
        }
    }
    load
}

/// Call a query of the benchmark over HTTP.
async fn http_query(client: &reqwest::Client, target: &str, path: String) -> Value {
    let res = client.post(format!("http://{target}/api/query")).json(&json!({ "path": path, "args": {} }));
    let body: Value = res.send().await.unwrap().json().await.unwrap();
    assert_eq!(body["status"], "success", "{path}: {body}");
    body["value"].clone()
}

async fn run_load(target: &str) {
    let (conns, secs, warmup) = (env("BENCH_CONNS", 64), env("BENCH_SECS", 10), env("BENCH_WARMUP", 2));
    let session = env_flag("BENCH_SESSION");
    let client = reqwest::Client::new();
    let ids = http_query(&client, target, fns::transfer_accounts::path()).await;
    let mut ids: Vec<String> = serde_json::from_value(ids).unwrap();
    assert!(ids.len() >= 2, "the server has {} accounts", ids.len());
    // Ids come in creation order, which is how the accounts lie on disk: shuffled (the same way every run), the
    // most used accounts don't share a page.
    let mut rng = Rng(0x7368_7566_666c_6500);
    for i in (1..ids.len()).rev() {
        ids.swap(i, (rng.next() % (i as u64 + 1)) as usize);
    }
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let mut run = 0;
    let total = http_query(&client, target, fns::transfer_total::path()).await;

    for alpha in env_list("BENCH_ALPHA", "0,1.5") {
        for inflight in env_list("BENCH_INFLIGHT", "40") {
            let measure_from = Instant::now() + Duration::from_secs(warmup);
            run += 1;
            let plan = Arc::new(Plan {
                url: format!("ws://{target}/api/sync"),
                session: session.then(|| format!("transfer-{started:x}-{run}")),
                ids: ids.clone(),
                ranks: PowerLaw::new(ids.len(), alpha),
                inflight: inflight as usize,
                measure_from,
                until: measure_from + Duration::from_secs(secs),
            });
            let sockets: Vec<_> = (0..conns).map(|n| tokio::spawn(socket(plan.clone(), n))).collect();
            tokio::time::sleep_until(measure_from.into()).await;
            let cpu = cpu_time();
            tokio::time::sleep_until(plan.until.into()).await;
            let cores = (cpu_time() - cpu).as_secs_f64() / secs as f64;
            let (mut latencies, mut failed) = (Vec::new(), 0);
            for socket in sockets {
                let load = socket.await.unwrap();
                latencies.extend(load.latencies);
                failed += load.failed;
            }
            latencies.sort();
            println!(
                "[transfer] alpha {alpha}, {conns} sockets x {inflight} in flight{}: {:.0} transfers/s, latency p50 {:?} \
                 p99 {:?} max {:?}, {failed} failed; the load kept {cores:.1} cores busy",
                if session { "" } else { ", no idempotency keys" },
                latencies.len() as f64 / secs as f64,
                percentile(&latencies, 0.5),
                percentile(&latencies, 0.99),
                latencies.last().copied().unwrap_or_default(),
            );
        }
    }
    assert_eq!(http_query(&client, target, fns::transfer_total::path()).await, total, "transfers changed the total");
}

#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn bench_transfer() {
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    if let workers @ 1.. = env("BENCH_WORKERS", 0) {
        runtime.worker_threads(workers as usize);
    }
    runtime.enable_all().build().unwrap().block_on(async {
        let (listen, target) = (std::env::var("BENCH_LISTEN").ok(), std::env::var("BENCH_TARGET").ok());
        if let Some(target) = target {
            return run_load(&target).await;
        }
        let (_app, addr) = start_server(listen.as_deref().unwrap_or("127.0.0.1:0")).await;
        match listen {
            // Interrupted (ctrl-c), the app's directory is removed on the way out.
            Some(_) => tokio::signal::ctrl_c().await.unwrap(),
            None => run_load(&addr.to_string()).await,
        }
    });
}

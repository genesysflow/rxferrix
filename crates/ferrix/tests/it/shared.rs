//! The shared subscription manager: identical queries run once per invalidation for all their subscribers, per-identity
//! separation, reuse of cached results, unsubscribe/resubscribe, cleanup, and slow consumers.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use ferrix::auth::{AuthConfig, ClientInfo, SignUpResult};
use ferrix::sync::protocol::{ClientMessage, QuerySetModification, ServerMessage, StateModification, StateVersion};
use ferrix::sync::worker::{self, Inbound};
use ferrix::testing::TestApp;
use ferrix::{App, FunctionRef, Identity, LogFilter};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::common::todos;

/// Queries with different dependencies on `ctx.auth`.
pub mod fns {
    use ferrix::prelude::*;

    use crate::common::todos::Todo;

    /// Who is asking (the principal) and how many todos there are.
    #[query]
    pub async fn my_id(ctx: &QueryCtx) -> Result<String> {
        let n = ctx.db.query::<Todo>().collect().await?.len();
        Ok(format!("{}:{n}", ctx.auth.user_id().map(|u| u.to_string()).unwrap_or_else(|| "anon".into())))
    }

    /// The caller's session and how many todos there are.
    #[query]
    pub async fn my_session(ctx: &QueryCtx) -> Result<String> {
        let n = ctx.db.query::<Todo>().collect().await?.len();
        Ok(format!("{}:{n}", ctx.auth.session_id().map(|s| s.to_string()).unwrap_or_else(|| "none".into())))
    }

    /// The same for everyone.
    #[query]
    pub async fn everyone(ctx: &QueryCtx) -> Result<usize> {
        Ok(ctx.db.query::<Todo>().collect().await?.len())
    }

    /// Reads `ctx.auth` only once there are at least `threshold` todos: a rerun can depend on more of the identity than
    /// the run before it.
    #[query]
    pub async fn late_auth(ctx: &QueryCtx, threshold: u32) -> Result<String> {
        let n = ctx.db.query::<Todo>().collect().await?.len();
        if n < threshold as usize {
            return Ok(format!("few:{n}"));
        }
        Ok(format!("{}:{n}", ctx.auth.user_id().map(|u| u.to_string()).unwrap_or_else(|| "anon".into())))
    }

    /// How many todos there are, taking at least `ms` milliseconds (it depends on every commit).
    #[query]
    pub async fn slow_count(ctx: &QueryCtx, ms: u32) -> Result<usize> {
        tokio::time::sleep(std::time::Duration::from_millis(ms.into())).await;
        Ok(ctx.db.query::<Todo>().collect().await?.len())
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// An in-process client: a real sync worker driven through its channels.
// ---------------------------------------------------------------------------------------------------------------------

pub struct Client {
    tx: mpsc::Sender<Inbound>,
    rx: mpsc::Receiver<ServerMessage>,
    pub version: StateVersion,
    pub results: BTreeMap<u64, Result<Value, String>>,
    query_set: u64,
    identity: u64,
    pub transitions: usize,
}

impl Client {
    pub async fn new(app: &App) -> Client {
        let (tx, rx) = worker::spawn(app.clone());
        let c = Client { tx, rx, version: StateVersion::default(), results: BTreeMap::new(), query_set: 0, identity: 0, transitions: 0 };
        c.send(ClientMessage::Connect { session_id: "s".into(), connection_count: 0, last_close_reason: None, client_version: None })
            .await;
        c
    }

    pub async fn send(&self, m: ClientMessage) {
        self.tx.send(Inbound::Message(m)).await.unwrap();
    }

    /// Next message (Pings skipped); Transitions are checked and applied.
    pub async fn recv(&mut self) -> ServerMessage {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.rx.recv()).await.expect("timed out").expect("worker stopped");
            match &msg {
                ServerMessage::Ping => continue,
                ServerMessage::Transition { start_version, end_version, modifications } => {
                    assert_eq!(*start_version, self.version, "startVersion must equal the previous endVersion");
                    assert!(end_version.ts >= start_version.ts);
                    for m in modifications {
                        match m {
                            StateModification::QueryUpdated { query_id, value, .. } => {
                                self.results.insert(*query_id, Ok(value.clone()));
                            }
                            StateModification::QueryFailed { query_id, error, .. } => {
                                self.results.insert(*query_id, Err(error.message.clone()));
                            }
                            StateModification::QueryRemoved { query_id } => {
                                assert!(self.results.remove(query_id).is_some(), "removed a query without a result");
                            }
                        }
                    }
                    self.version = *end_version;
                    self.transitions += 1;
                }
                _ => {}
            }
            return msg;
        }
    }

    /// Receive until `cond` holds.
    pub async fn until(&mut self, cond: impl Fn(&Client) -> bool) {
        while !cond(self) {
            self.recv().await;
        }
    }

    pub async fn modify(&mut self, modifications: Vec<QuerySetModification>) -> Vec<StateModification> {
        let base = self.query_set;
        self.query_set += 1;
        self.send(ClientMessage::ModifyQuerySet { base_version: base, new_version: base + 1, modifications }).await;
        loop {
            if let ServerMessage::Transition { end_version, modifications, .. } = self.recv().await
                && end_version.query_set == base + 1
            {
                return modifications;
            }
        }
    }

    pub async fn subscribe(&mut self, query_id: u64, path: &str, args: Value) -> Result<Value, String> {
        self.modify(vec![QuerySetModification::Add { query_id, udf_path: path.into(), args }]).await;
        self.results[&query_id].clone()
    }

    pub async fn unsubscribe(&mut self, query_id: u64) {
        self.modify(vec![QuerySetModification::Remove { query_id }]).await;
    }

    pub async fn authenticate(&mut self, token: Option<&str>) {
        let base = self.identity;
        self.identity += 1;
        self.send(ClientMessage::Authenticate { base_version: base, token: token.map(Into::into) }).await;
        loop {
            if let ServerMessage::Transition { end_version, .. } = self.recv().await
                && end_version.identity == base + 1
            {
                return;
            }
        }
    }

    pub fn value(&self, query_id: u64) -> Value {
        self.results[&query_id].clone().expect("query succeeded")
    }
}

/// Executions of `path` recorded in the execution log, by caller identity.
async fn executions(app: &App, path: &str) -> BTreeMap<Option<String>, usize> {
    app.logs().flush().await.unwrap();
    let entries = app.logs().query(LogFilter { path_prefix: Some(path.into()), limit: Some(10_000), ..Default::default() }).await.unwrap();
    let mut by = BTreeMap::new();
    for e in entries.into_iter().filter(|e| e.path == path) {
        *by.entry(e.identity).or_default() += 1;
    }
    by
}

async fn total_executions(app: &App, path: &str) -> usize {
    executions(app, path).await.values().sum()
}

/// Wait until the manager has processed everything and holds nothing.
pub async fn assert_manager_empty(app: &App) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = app.subscription_stats();
        if s.connections == 0 && s.shared_queries == 0 && s.subscriptions == 0 && s.index_ranges == 0 {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "manager not empty: {s:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_stats(app: &App, what: &str, cond: impl Fn(&ferrix::sync::subscriptions::SubscriptionStats) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !cond(&app.subscription_stats()) {
        assert!(tokio::time::Instant::now() < deadline, "{what}: {:?}", app.subscription_stats());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn texts(value: &Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|d| d["text"].as_str().unwrap().to_string()).collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// Dedup over real sockets
// ---------------------------------------------------------------------------------------------------------------------

async fn ws_connect(addr: SocketAddr) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
    let connect = json!({ "type": "Connect", "sessionId": "x", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "t" });
    ws.send(Message::text(connect.to_string())).await.unwrap();
    ws
}

async fn ws_next_transition(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("timed out").unwrap().unwrap();
        let Message::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(text.as_str()).unwrap();
        if v["type"] == "Transition" {
            return v;
        }
    }
}

/// 50 sockets watch the same query; one commit reruns it exactly once, and all 50 get the new value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_sockets_one_execution_per_commit() {
    const N: usize = 50;
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let addr = t.serve().await.unwrap();
    let list = todos::list::path();

    let mut sockets = Vec::new();
    for i in 0..N {
        let mut ws = ws_connect(addr).await;
        let add = json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1,
            "modifications": [{ "type": "Add", "queryId": i, "udfPath": list, "args": {} }] });
        ws.send(Message::text(add.to_string())).await.unwrap();
        sockets.push(ws);
    }
    for ws in &mut sockets {
        let first = ws_next_transition(ws).await;
        assert_eq!(first["endVersion"]["querySet"], 1);
        assert_eq!(first["modifications"][0]["value"], json!([]));
    }
    let stats = app.subscription_stats();
    assert_eq!((stats.connections, stats.shared_queries, stats.subscriptions), (N as u64, 1, N as u64), "{stats:?}");
    let before = total_executions(&app, &list).await;
    let reruns_before = app.subscription_stats().reruns;
    assert_eq!(before, 1, "50 concurrent subscribers share the first execution too");

    app.run_mutation(&todos::add::path(), json!({ "text": "shared" }), &Identity::default()).await.unwrap();
    for ws in &mut sockets {
        let update = ws_next_transition(ws).await;
        assert_eq!(texts(&update["modifications"][0]["value"]), vec!["shared"]);
    }
    assert_eq!(total_executions(&app, &list).await - before, 1, "exactly one execution for the commit");
    assert_eq!(app.subscription_stats().reruns - reruns_before, 1);

    drop(sockets);
    assert_manager_empty(&app).await;
}

// ---------------------------------------------------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------------------------------------------------

async fn sign_up(app: &App, email: &str) -> (String, String) {
    let res = app.auth().sign_up(app, email, "correct horse".into(), None, ClientInfo::default()).await.unwrap();
    let SignUpResult::SignedIn(tokens) = res else { panic!("signed in") };
    (tokens.user_id.clone().unwrap(), tokens.access_token)
}

/// Different principals get separate executions and results; anonymous sockets share one; a query that never reads
/// `ctx.auth` is shared by everyone; one that reads the session id is separate per session.
#[tokio::test]
async fn identity_separation() {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let (alice, alice_token) = sign_up(&app, "alice@example.com").await;
    let alice_token2 = app.auth().sign_in_as(&app, alice.parse().unwrap()).await.unwrap().access_token;
    let (bob, bob_token) = sign_up(&app, "bob@example.com").await;

    let mut clients = Vec::new();
    for token in [None, None, None, Some(&alice_token), Some(&alice_token2), Some(&bob_token)] {
        let mut c = Client::new(&app).await;
        if token.is_some() {
            c.authenticate(token.map(|s| s.as_str())).await;
        }
        c.subscribe(1, &fns::my_id::path(), json!({})).await.unwrap();
        c.subscribe(2, &fns::my_session::path(), json!({})).await.unwrap();
        c.subscribe(3, &fns::everyone::path(), json!({})).await.unwrap();
        clients.push(c);
    }
    let (anon, a1, a2, b) = (0, 3, 4, 5);
    assert_eq!(clients[anon].value(1), json!("anon:0"));
    assert_eq!(clients[a1].value(1), json!(format!("{alice}:0")));
    assert_eq!(clients[a2].value(1), clients[a1].value(1), "same user, other session: same principal result");
    assert_eq!(clients[b].value(1), json!(format!("{bob}:0")));
    assert_ne!(clients[a1].value(2), clients[a2].value(2), "the session id differs per session");
    assert_eq!(clients[0].value(2), clients[1].value(2));
    // my_id: anonymous, alice, bob. my_session: anonymous, alice x2, bob. everyone: one.
    wait_stats(&app, "shared queries", |s| s.shared_queries == 3 + 4 + 1 && s.subscriptions == 18).await;

    let my_id = executions(&app, &fns::my_id::path()).await;
    let my_session = executions(&app, &fns::my_session::path()).await;
    let everyone = executions(&app, &fns::everyone::path()).await;
    t.mutation::<todos::add>(("x".to_string(),)).await.unwrap();
    for c in &mut clients {
        c.until(|c| c.value(3) == json!(1) && c.value(1).as_str().unwrap().ends_with(":1")).await;
        assert!(c.value(2).as_str().unwrap().ends_with(":1"));
    }
    let diff = |after: BTreeMap<Option<String>, usize>, before: BTreeMap<Option<String>, usize>| {
        after.into_iter().map(|(k, v)| (k.clone(), v - before.get(&k).copied().unwrap_or(0))).filter(|(_, v)| *v > 0).collect::<BTreeMap<_, _>>()
    };
    let user = |id: &str| Some(format!("user:{id}"));
    assert_eq!(
        diff(executions(&app, &fns::my_id::path()).await, my_id),
        BTreeMap::from([(None, 1), (user(&alice), 1), (user(&bob), 1)]),
        "one execution per principal; anonymous sockets share one"
    );
    assert_eq!(
        diff(executions(&app, &fns::my_session::path()).await, my_session),
        BTreeMap::from([(None, 1), (user(&alice), 2), (user(&bob), 1)]),
        "session-dependent: one per session"
    );
    assert_eq!(diff(executions(&app, &fns::everyone::path()).await, everyone).values().sum::<usize>(), 1, "auth-free: one for all");

    // Signing out on the socket moves its subscriptions to the anonymous shared queries (no execution needed).
    let before = total_executions(&app, &fns::my_id::path()).await;
    clients[b].authenticate(None).await;
    assert_eq!(clients[b].value(1), json!("anon:1"));
    assert_eq!(total_executions(&app, &fns::my_id::path()).await, before, "reused the anonymous result");

    drop(clients);
    assert_manager_empty(&app).await;
}

/// A shared query that starts reading `ctx.auth` on a rerun is split per principal in the same round.
#[tokio::test]
async fn query_that_starts_reading_auth_is_split() {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let (alice, alice_token) = sign_up(&app, "alice@example.com").await;
    let (bob, bob_token) = sign_up(&app, "bob@example.com").await;
    let path = fns::late_auth::path();

    let mut clients = Vec::new();
    for token in [None, Some(&alice_token), Some(&bob_token), Some(&alice_token)] {
        let mut c = Client::new(&app).await;
        if let Some(token) = token {
            c.authenticate(Some(token)).await;
        }
        assert_eq!(c.subscribe(7, &path, json!({ "threshold": 1 })).await, Ok(json!("few:0")));
        clients.push(c);
    }
    wait_stats(&app, "one shared query", |s| s.shared_queries == 1).await;

    t.mutation::<todos::add>(("x".to_string(),)).await.unwrap();
    let expected = [json!("anon:1"), json!(format!("{alice}:1")), json!(format!("{bob}:1")), json!(format!("{alice}:1"))];
    for (c, want) in clients.iter_mut().zip(&expected) {
        c.until(|c| c.value(7) != json!("few:0")).await;
        assert_eq!(&c.value(7), want);
    }
    wait_stats(&app, "split per principal", |s| s.shared_queries == 3).await;
    drop(clients);
    assert_manager_empty(&app).await;
}

// ---------------------------------------------------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------------------------------------------------

/// A late subscriber gets the cached result in its Transition without running the query.
#[tokio::test]
async fn late_subscriber_reuses_cached_result() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    t.mutation::<todos::add>(("a".to_string(),)).await.unwrap();
    let list = todos::list::path();

    let mut first = Client::new(&app).await;
    first.subscribe(1, &list, json!({})).await.unwrap();
    let executions = total_executions(&app, &list).await;
    let reruns = app.subscription_stats().reruns;

    let mut late = Client::new(&app).await;
    let value = late.subscribe(9, &list, json!({})).await.unwrap();
    assert_eq!(texts(&value), vec!["a"]);
    assert_eq!(late.version.ts, first.version.ts);
    // Other args are another shared query.
    let other = late.subscribe(10, &todos::list::path(), json!({ "done": null })).await.unwrap();
    assert_eq!(texts(&other), vec!["a"]);
    assert_eq!(total_executions(&app, &list).await, executions + 1, "only the new args ran");
    assert_eq!(app.subscription_stats().reruns, reruns + 1);
    assert_eq!(app.subscription_stats().shared_queries, 2);

    let mut third = Client::new(&app).await;
    third.subscribe(1, &list, json!({ "done": null })).await.unwrap();
    third.subscribe(2, &list, json!({})).await.unwrap();
    assert_eq!(total_executions(&app, &list).await, executions + 1, "both cached");
}

/// Unsubscribing drops the shared query once its last subscriber leaves; subscribing again runs it fresh and keeps
/// receiving updates.
#[tokio::test]
async fn unsubscribe_and_resubscribe() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let list = todos::list::path();
    let mut a = Client::new(&app).await;
    let mut b = Client::new(&app).await;
    a.subscribe(1, &list, json!({})).await.unwrap();
    b.subscribe(1, &list, json!({})).await.unwrap();
    assert_eq!(app.subscription_stats().shared_queries, 1);

    a.unsubscribe(1).await;
    assert!(a.results.is_empty());
    assert_eq!(app.subscription_stats().shared_queries, 1, "b still watches it");
    b.unsubscribe(1).await;
    assert_eq!(app.subscription_stats().shared_queries, 0, "dropped with its last subscriber");

    t.mutation::<todos::add>(("x".to_string(),)).await.unwrap();
    let value = a.subscribe(2, &list, json!({})).await.unwrap();
    assert_eq!(texts(&value), vec!["x"]);
    t.mutation::<todos::add>(("y".to_string(),)).await.unwrap();
    a.until(|c| c.results.get(&2).is_some_and(|r| texts(r.as_ref().unwrap()) == vec!["y", "x"])).await;
    assert_eq!(a.results.keys().copied().collect::<Vec<_>>(), vec![2]);

    // Add and remove in one ModifyQuerySet: the query never reaches the client, and nothing leaks.
    let mods = a
        .modify(vec![
            QuerySetModification::Add { query_id: 3, udf_path: todos::latest::path(), args: json!({ "n": 1 }) },
            QuerySetModification::Remove { query_id: 3 },
        ])
        .await;
    assert!(mods.is_empty(), "{mods:?}");
    wait_stats(&app, "only query 2", |s| s.shared_queries == 1 && s.subscriptions == 1).await;
}

/// Subscribe/unsubscribe churn across many connections with commits in between, then disconnects: the manager ends
/// with no connections, shared queries, subscriptions or index ranges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn churn_and_disconnect_leave_the_manager_empty() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let paths = [todos::list::path(), todos::latest::path(), fns::everyone::path(), fns::my_id::path()];
    let mut tasks = Vec::new();
    for i in 0..12u64 {
        let app = app.clone();
        let paths = paths.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = Client::new(&app).await;
            let mut live: Vec<u64> = Vec::new();
            for step in 0..30u64 {
                let qid = i * 1000 + step;
                let path = &paths[((i + step) % paths.len() as u64) as usize];
                let args = if path.ends_with("latest") { json!({ "n": 1 + step % 3 }) } else { json!({}) };
                let mut mods = vec![QuerySetModification::Add { query_id: qid, udf_path: path.clone(), args }];
                if step % 3 == 2 && !live.is_empty() {
                    mods.push(QuerySetModification::Remove { query_id: live.remove(0) });
                }
                c.modify(mods).await;
                live.push(qid);
                if step % 5 == 0 {
                    app.run_mutation(&todos::add::path(), json!({ "text": format!("c{i}s{step}") }), &Identity::default()).await.unwrap();
                }
            }
            // Half unsubscribe everything first; the other half just disconnect.
            if i % 2 == 0 {
                let mods = live.iter().map(|q| QuerySetModification::Remove { query_id: *q }).collect();
                c.modify(mods).await;
                assert!(c.results.is_empty());
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_manager_empty(&app).await;
    // The manager still works afterwards.
    let mut c = Client::new(&app).await;
    let n = c.subscribe(1, &fns::everyone::path(), json!({})).await.unwrap();
    assert_eq!(n, json!(app.run_query(&fns::everyone::path(), json!({}), &Identity::default()).await.result.unwrap()));
    drop(c);
    assert_manager_empty(&app).await;
}

// ---------------------------------------------------------------------------------------------------------------------
// Backpressure
// ---------------------------------------------------------------------------------------------------------------------

/// A client that stops reading doesn't hold up the manager or other clients; when it resumes it gets the latest state,
/// with the intermediate updates coalesced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_consumer_is_coalesced_and_does_not_stall_others() {
    const COMMITS: usize = 700; // well past the 256-message socket buffer
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let path = fns::everyone::path();

    let mut slow = Client::new(&app).await;
    slow.subscribe(1, &path, json!({})).await.unwrap();
    let mut fast = Client::new(&app).await;
    fast.subscribe(1, &path, json!({})).await.unwrap();

    // `slow` is not read from while all the commits happen; `fast` sees each one promptly.
    let mut worst = Duration::ZERO;
    for i in 0..COMMITS {
        let (_, ts) = t.mutation_ts::<todos::add>((format!("t{i}"),)).await.unwrap();
        let started = std::time::Instant::now();
        fast.until(|c| c.version.ts >= ts).await;
        worst = worst.max(started.elapsed());
        assert_eq!(fast.value(1), json!(i + 1));
    }
    assert!(worst < Duration::from_secs(2), "fast client was held up: {worst:?}");

    // Now drain the slow one: it ends at the latest state with far fewer Transitions than commits.
    let final_ts = app.db().write_log().latest();
    slow.until(|c| c.version.ts >= final_ts).await;
    assert_eq!(slow.value(1), json!(COMMITS));
    assert!(slow.transitions < COMMITS, "updates were coalesced: {} transitions for {COMMITS} commits", slow.transitions);
    println!("slow consumer: {} transitions for {COMMITS} commits; fast consumer worst latency {worst:?}", slow.transitions);
}

// ---------------------------------------------------------------------------------------------------------------------
// Slow queries
// ---------------------------------------------------------------------------------------------------------------------

/// A query that takes long to run holds up neither the next generation nor the connections that don't watch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_query_does_not_hold_up_others() {
    const SLOW_MS: u32 = 2500;
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let mut slow = Client::new(&app).await;
    assert_eq!(slow.subscribe(1, &fns::slow_count::path(), json!({ "ms": SLOW_MS })).await, Ok(json!(0)));
    let mut fast = Client::new(&app).await;
    assert_eq!(fast.subscribe(1, &fns::everyone::path(), json!({})).await, Ok(json!(0)));

    // Each commit starts a rerun of the slow query; the fast one still follows every commit promptly.
    let started = std::time::Instant::now();
    for i in 1..=5 {
        let (_, ts) = t.mutation_ts::<todos::add>((format!("t{i}"),)).await.unwrap();
        fast.until(|c| c.version.ts >= ts).await;
        assert_eq!(fast.value(1), json!(i));
    }
    let fast_done = started.elapsed();
    assert!(fast_done < Duration::from_millis(SLOW_MS as u64 / 2), "the fast query waited for the slow one: {fast_done:?}");

    // The slow subscriber catches up, with a result valid at the ts it is sent at.
    let final_ts = app.db().write_log().latest();
    slow.until(|c| c.version.ts >= final_ts).await;
    assert_eq!(slow.value(1), json!(5));
}

/// A connection watching two slow queries that commits keep invalidating while they run (so their runs never finish
/// at the same ts on their own) still gets consistent updates while the commits go on: all three of its queries count
/// the same table, so every Transition must show one number three times.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_with_misaligned_slow_queries_still_gets_updates() {
    let t = TestApp::new().await.unwrap();
    let app = t.app().clone();
    let mut c = Client::new(&app).await;
    c.subscribe(1, &fns::slow_count::path(), json!({ "ms": 25 })).await.unwrap();
    c.subscribe(2, &fns::slow_count::path(), json!({ "ms": 60 })).await.unwrap();
    c.subscribe(3, &fns::everyone::path(), json!({})).await.unwrap();
    let start_ts = c.version.ts;

    let writer = {
        let app = app.clone();
        tokio::spawn(async move {
            let deadline = std::time::Instant::now() + Duration::from_millis(1500);
            while std::time::Instant::now() < deadline {
                app.run_mutation(&todos::add::path(), json!({ "text": "x" }), &Identity::default()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
        })
    };
    let mut during = 0;
    while !writer.is_finished() {
        if let Ok(msg) = tokio::time::timeout(Duration::from_millis(50), c.recv()).await
            && matches!(msg, ServerMessage::Transition { .. })
        {
            assert_eq!(c.value(1), c.value(3), "torn transition at ts {}", c.version.ts);
            assert_eq!(c.value(2), c.value(3), "torn transition at ts {}", c.version.ts);
            during += 1;
        }
    }
    writer.await.unwrap();
    assert!(during >= 5, "only {during} transitions while commits went on (from ts {start_ts} to {}; latest {}; {:?})", c.version.ts, app.db().write_log().latest(), app.subscription_stats());
    let final_ts = app.db().write_log().latest();
    c.until(|c| c.version.ts >= final_ts).await;
    let n = app.run_query(&fns::everyone::path(), json!({}), &Identity::default()).await.result.unwrap();
    assert_eq!((c.value(1), c.value(2), c.value(3)), (n.clone(), n.clone(), n));
    let stats = app.subscription_stats();
    assert!(stats.stale_runs > 0, "{stats:?}");
    println!("{during} transitions during the commits; {stats:?}");
}

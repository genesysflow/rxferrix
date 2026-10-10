//! Randomized robustness tests of everything a client can send: sync protocol messages (to real workers), raw WebSocket
//! frames, and HTTP requests to every API route. Nothing may crash a worker, the subscription manager or a request
//! handler, every HTTP request gets a response (never a dropped connection), and afterwards the server is clean and
//! works. Seeded and repeatable: `FUZZ_SEED` picks the seed, `FUZZ_SCALE` multiplies the amount of input (1 by default,
//! a few seconds; try 20 for a longer run).

use std::time::Duration;

use ferrix::sync::protocol::{ClientMessage, QuerySetModification, ServerMessage};
use ferrix::sync::worker::{self, Inbound};
use ferrix::testing::TestApp;
use ferrix::{FunctionRef, Identity};
use futures::{SinkExt, StreamExt};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde_json::{Map, Value, json};

use crate::common::todos;
use crate::shared::{Client, assert_manager_empty};

fn rng() -> (u64, StdRng) {
    let seed = std::env::var("FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5eed);
    (seed, StdRng::seed_from_u64(seed))
}

fn scale() -> usize {
    std::env::var("FUZZ_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

fn pick<'a, T>(rng: &mut StdRng, xs: &'a [T]) -> &'a T {
    &xs[rng.random_range(0..xs.len())]
}

fn string(rng: &mut StdRng) -> String {
    match rng.random_range(0..8) {
        0 => String::new(),
        1 => "x".repeat(rng.random_range(1000..20_000)),
        2 => "é✓🦀\u{0}\u{202e}\"\\".into(),
        3 => format!("{}", rng.random::<u64>()),
        4 => "../../etc/passwd".into(),
        5 => "_system/dashboard:documents".into(),
        6 => "00000000000000000000000000".into(),
        _ => format!("s{}", rng.random_range(0..50)),
    }
}

fn number(rng: &mut StdRng) -> Value {
    match rng.random_range(0..7) {
        0 => json!(i64::MIN),
        1 => json!(u64::MAX),
        2 => json!(f64::MAX),
        3 => json!(-0.0),
        4 => json!(1e-300),
        5 => json!(rng.random_range(-5i64..5)),
        _ => json!(rng.random::<f64>() * 1e6),
    }
}

/// Random JSON, biased towards the field names and shapes real functions take.
fn value(rng: &mut StdRng, depth: u32) -> Value {
    let keys = [
        "text",
        "id",
        "done",
        "n",
        "limit",
        "ms",
        "group",
        "marker",
        "fail",
        "paginationOpts",
        "numItems",
        "cursor",
        "$a",
        "_id",
    ];
    match rng.random_range(0..if depth == 0 { 5 } else { 8 }) {
        0 => Value::Null,
        1 => json!(rng.random_bool(0.5)),
        2 => number(rng),
        3 | 4 => json!(string(rng)),
        5 => Value::Array((0..rng.random_range(0..6)).map(|_| value(rng, depth - 1)).collect()),
        _ => {
            let mut m = Map::new();
            for _ in 0..rng.random_range(0..5) {
                let key = if rng.random_bool(0.8) { (*pick(rng, &keys)).to_string() } else { string(rng) };
                m.insert(key, value(rng, depth - 1));
            }
            Value::Object(m)
        }
    }
}

fn args(rng: &mut StdRng) -> Value {
    if rng.random_bool(0.8) {
        let mut m = Map::new();
        for _ in 0..rng.random_range(0..4) {
            let key = (*pick(rng, &["text", "id", "done", "n", "limit", "ms", "fail", "marker"])).to_string();
            m.insert(key, value(rng, 2));
        }
        Value::Object(m)
    } else {
        value(rng, 3)
    }
}

/// Paths of every registered function (public, internal, admin-only), plus made-up ones.
fn paths(t: &TestApp) -> Vec<String> {
    let mut paths: Vec<String> = t.app().registry().list().iter().map(|f| f.path.clone()).collect();
    paths.extend(["nope:nope", "", ":", "todos", "todos:", "_system/", "../x:y"].map(String::from));
    paths
}

/// What a session believes the server's state is (so it can follow the protocol, or break it on purpose).
#[derive(Default)]
struct Session {
    query_set: u64,
    identity: u64,
    live: std::collections::BTreeSet<u64>,
}

/// A random message. With probability `hostility` per message it breaks the protocol (wrong versions, duplicate or
/// unknown query ids, garbage tokens); otherwise it is valid, with random function paths and args.
fn client_message(rng: &mut StdRng, paths: &[String], session: &mut Session, hostility: f64) -> ClientMessage {
    let hostile = rng.random_bool(hostility);
    match rng.random_range(0..10) {
        0..=4 => {
            let (base, new) = if hostile {
                (rng.random_range(0..5), rng.random_range(0..5))
            } else {
                (session.query_set, session.query_set + 1)
            };
            let mut live = session.live.clone();
            let modifications = (0..rng.random_range(0..6))
                .filter_map(|_| {
                    let add = rng.random_bool(0.6) || live.is_empty();
                    let query_id = if add {
                        let id = rng.random_range(0..40);
                        if live.contains(&id) && !hostile {
                            return None;
                        }
                        live.insert(id);
                        id
                    } else {
                        let id = if hostile {
                            rng.random_range(0..40)
                        } else {
                            *live.iter().nth(rng.random_range(0..live.len()))?
                        };
                        live.remove(&id);
                        id
                    };
                    Some(if add {
                        QuerySetModification::Add { query_id, udf_path: pick(rng, paths).clone(), args: args(rng) }
                    } else {
                        QuerySetModification::Remove { query_id }
                    })
                })
                .collect();
            if base == session.query_set && new == base + 1 {
                session.query_set = new;
                session.live = live;
            }
            ClientMessage::ModifyQuerySet { base_version: base, new_version: new, modifications }
        }
        5 | 6 => {
            ClientMessage::Mutation { request_id: rng.random(), udf_path: pick(rng, paths).clone(), args: args(rng) }
        }
        7 => ClientMessage::Action { request_id: rng.random(), udf_path: pick(rng, paths).clone(), args: args(rng) },
        8 if hostile => {
            let token = Some(if rng.random_bool(0.5) { string(rng) } else { "eyJhbGciOiJIUzI1NiJ9.e30.x".into() });
            ClientMessage::Authenticate { base_version: rng.random_range(0..5), token }
        }
        8 => {
            // Valid: back to anonymous (re-keys every subscription).
            session.identity += 1;
            ClientMessage::Authenticate { base_version: session.identity - 1, token: None }
        }
        _ if hostile => ClientMessage::Connect {
            session_id: string(rng),
            connection_count: rng.random(),
            last_close_reason: rng.random_bool(0.5).then(|| string(rng)),
            client_version: Some(string(rng)),
            features: vec![string(rng), "diffs".into()],
        },
        _ => ClientMessage::Pong,
    }
}

/// After fuzzing: nothing left behind, indexes intact, and a well-behaved client works.
async fn healthy(t: &TestApp) {
    assert_manager_empty(t.app()).await;
    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new());
    let mut c = Client::new(t.app()).await;
    c.subscribe(1, &todos::list::path(), json!({})).await.unwrap();
    t.mutation::<todos::add>(("still works".to_string(),)).await.unwrap();
    c.until(|c| c.results.get(&1).is_some_and(|r| r.as_ref().is_ok_and(|v| v.to_string().contains("still works"))))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_protocol_sessions_never_crash_a_worker() {
    let t = TestApp::new().await.unwrap();
    let (seed, mut rng) = rng();
    let paths = paths(&t);
    let (mut fatal, mut results) = (0, 0);
    for session in 0..40 * scale() {
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(64);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(256);
        let worker = tokio::spawn(worker::run(t.app().clone(), in_rx, out_tx));
        let drain = tokio::spawn(async move {
            let (mut fatal, mut results) = (0, 0);
            while let Some(msg) = out_rx.recv().await {
                match msg {
                    ServerMessage::FatalError { .. } | ServerMessage::AuthError { .. } => fatal += 1,
                    ServerMessage::Transition { modifications, .. } => results += modifications.len(),
                    _ => {}
                }
            }
            (fatal, results)
        });
        let mut state = Session::default();
        // Most sessions follow the protocol (so queries run, fail, rerun and get re-keyed); some break it now and then.
        let hostility = *pick(&mut rng, &[0.0, 0.0, 0.0, 0.01, 0.05, 0.3]);
        let connect = ClientMessage::Connect {
            session_id: format!("fuzz-{session}"),
            connection_count: 0,
            last_close_reason: None,
            client_version: None,
            features: if rng.random_bool(0.5) { vec!["diffs".into()] } else { vec![] },
        };
        let _ = in_tx.send(Inbound::Message(connect)).await;
        for _ in 0..rng.random_range(1..80) {
            let msg = if rng.random_bool(hostility / 5.0) {
                Inbound::Malformed("fuzz".into())
            } else {
                Inbound::Message(client_message(&mut rng, &paths, &mut state, hostility))
            };
            if in_tx.send(msg).await.is_err() {
                break; // the worker closed the connection
            }
        }
        // Let results arrive (closing the input ends the worker at once), then hang up.
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(in_tx);
        let ended = tokio::time::timeout(Duration::from_secs(30), worker).await;
        let joined = ended.unwrap_or_else(|_| panic!("seed {seed}, session {session}: the worker hung"));
        assert!(joined.is_ok(), "seed {seed}, session {session}: the worker panicked: {joined:?}");
        let (f, r) = drain.await.unwrap();
        fatal += f;
        results += r;
    }
    // Both ways out were exercised: sessions that broke the protocol, and queries that ran and sent results.
    assert!(fatal > 0, "no session hit a protocol error: the fuzzer is too tame");
    assert!(
        results > 100 * scale(),
        "only {results} query results: the fuzzer hardly reached the subscription manager"
    );
    println!("fuzz: {results} query results, {fatal} sessions ended by a protocol error");
    healthy(&t).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_websocket_frames_never_crash_the_server() {
    use tokio_tungstenite::tungstenite::Message;

    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    let (seed, mut rng) = rng();
    let paths = paths(&t);
    for socket in 0..30 * scale() {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
        for _ in 0..rng.random_range(1..30) {
            let frame = match rng.random_range(0..7) {
                0 => Message::Binary(
                    (0..rng.random_range(0..512)).map(|_| rng.random::<u8>()).collect::<Vec<u8>>().into(),
                ),
                1 => Message::Text("[".repeat(rng.random_range(1..5000)).into()),
                2 => Message::Text(value(&mut rng, 4).to_string().into()),
                3 => {
                    let mut text =
                        serde_json::to_string(&client_message(&mut rng, &paths, &mut Session::default(), 0.5)).unwrap();
                    let cut = text.floor_char_boundary(rng.random_range(0..text.len().max(1)));
                    text.truncate(cut);
                    Message::Text(text.into())
                }
                4 => Message::Text(json!({ "type": string(&mut rng), "x": value(&mut rng, 2) }).to_string().into()),
                5 => Message::Ping(vec![1, 2, 3].into()),
                _ => Message::Text(
                    serde_json::to_string(&client_message(&mut rng, &paths, &mut Session::default(), 0.5))
                        .unwrap()
                        .into(),
                ),
            };
            if ws.send(frame).await.is_err() {
                break; // closed by the server
            }
        }
        // Read until the server closes or goes quiet.
        while let Ok(Some(Ok(_))) = tokio::time::timeout(Duration::from_millis(200), ws.next()).await {}
        let _ = ws.close(None).await;
        // The server is still up after each socket.
        let res = reqwest::Client::new()
            .post(format!("http://{addr}/api/query"))
            .json(&json!({ "path": todos::list::path(), "args": {} }))
            .send()
            .await
            .unwrap_or_else(|e| panic!("seed {seed}, socket {socket}: the server stopped answering: {e}"));
        assert_eq!(res.status(), 200);
    }
    healthy(&t).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_http_requests_always_get_an_answer() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    let (seed, mut rng) = rng();
    let paths = paths(&t);
    let id = t.mutation::<todos::add>(("x".to_string(),)).await.unwrap();
    let routes = [
        "/api/query",
        "/api/mutation",
        "/api/action",
        "/api/storage/upload",
        "/api/storage/",
        "/api/auth/signin",
        "/api/auth/signup",
        "/api/auth/refresh",
        "/api/auth/signout",
        "/api/auth/magic/verify",
        "/api/auth/providers",
        "/api/auth/oauth/github",
        "/api/auth/oauth/github/callback",
        "/api/admin/stats",
        "/api/admin/logs",
        "/api/admin/backups",
        "/api/admin/export/todos",
        "/api/admin/import/todos",
        "/api/admin/replication",
        "/api/admin/restore/point-in-time",
        "/api/sync",
        "/_/",
        "/_/assets/x.js",
        "/metrics",
        "/",
        "/nope",
    ];
    // A fresh connection per request: reusing one the server is closing (it may answer before reading a body it
    // won't use, then close) fails on the client's side, and isn't the server not answering.
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).pool_max_idle_per_host(0).build().unwrap();
    let mut server_errors = Vec::new();
    for i in 0..400 * scale() {
        let route = *pick(&mut rng, &routes);
        let mut url = format!("http://{addr}{route}");
        match rng.random_range(0..6) {
            0 => url.push_str(&id.to_string()),
            1 => {
                url.push_str(&format!("{}?token={}&filename={}", string(&mut rng), string(&mut rng), string(&mut rng)))
            }
            2 => url.push_str(&format!(
                "?thumb={}&fit={}&exp={}&sig={}",
                string(&mut rng),
                string(&mut rng),
                number(&mut rng),
                string(&mut rng)
            )),
            _ => {}
        }
        let method = pick(
            &mut rng,
            &[
                reqwest::Method::GET,
                reqwest::Method::POST,
                reqwest::Method::PUT,
                reqwest::Method::DELETE,
                reqwest::Method::HEAD,
            ],
        )
        .clone();
        let mut req = client.request(method.clone(), &url);
        req = match rng.random_range(0..5) {
            0 => req.json(&json!({ "path": pick(&mut rng, &paths), "args": args(&mut rng) })),
            1 => req.body((0..rng.random_range(0..2048)).map(|_| rng.random::<u8>()).collect::<Vec<u8>>()),
            2 => req.json(&value(&mut rng, 4)),
            3 => req
                .header("content-type", string(&mut rng).chars().filter(|c| c.is_ascii_graphic()).collect::<String>())
                .body(string(&mut rng)),
            _ => req,
        };
        if rng.random_bool(0.3) {
            req = req.header(
                "authorization",
                format!("Bearer {}", string(&mut rng).chars().filter(|c| c.is_ascii_graphic()).collect::<String>()),
            );
        }
        if rng.random_bool(0.2) {
            req = req.header("range", format!("bytes={}-{}", number(&mut rng), number(&mut rng)));
        }
        let res =
            req.send().await.unwrap_or_else(|e| panic!("seed {seed}, request {i}: {method} {url}: no answer: {e}"));
        if res.status().is_server_error() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            // A function that fails is a server error by design (the test registry has functions that misuse the API
            // on purpose): fine, if it is the protocol's JSON error. Anything else is ferrix's own fault.
            let function_error = ["/api/query", "/api/mutation", "/api/action"].contains(&route)
                && serde_json::from_str::<Value>(&body).is_ok_and(|b| b["status"] == "error");
            if !function_error {
                server_errors.push(format!("{method} {url} -> {status}: {body}"));
            }
        }
    }
    assert!(server_errors.is_empty(), "seed {seed}: server errors:\n{}", server_errors.join("\n"));
    healthy(&t).await;
    // And the function behind the routes still answers normally.
    let run = t.app().run_query(&todos::list::path(), json!({}), &Identity::default()).await;
    assert!(run.result.is_ok());
}

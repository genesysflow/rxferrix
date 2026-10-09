//! End to end over a real socket: tokio-tungstenite clients against a server on 127.0.0.1:0, plus the HTTP API.

use std::net::SocketAddr;
use std::time::Duration;

use ferrix::AppOptions;
use ferrix::testing::TestApp;
use ferrix::FunctionRef;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::common::todos;

struct Ws {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// Current StateVersion, checked against every Transition's startVersion.
    version: Value,
}

impl Ws {
    async fn connect(addr: SocketAddr) -> Ws {
        let (stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
        let mut ws = Ws { stream, version: json!({ "querySet": 0, "identity": 0, "ts": 0 }) };
        ws.send(json!({ "type": "Connect", "sessionId": "s", "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0" })).await;
        ws
    }

    async fn send(&mut self, v: Value) {
        self.stream.send(Message::text(v.to_string())).await.unwrap();
    }

    async fn send_raw(&mut self, s: &str) {
        self.stream.send(Message::text(s.to_string())).await.unwrap();
    }

    /// Next message (Pings skipped). Transitions are checked for version continuity.
    async fn recv(&mut self) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.stream.next())
                .await
                .expect("timed out waiting for a message")
                .expect("socket closed")
                .unwrap();
            let Message::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            match v["type"].as_str().unwrap() {
                "Ping" => continue,
                "Transition" => {
                    assert_eq!(v["startVersion"], self.version, "startVersion must equal the previous endVersion");
                    assert!(v["endVersion"]["ts"].as_u64() >= v["startVersion"]["ts"].as_u64());
                    self.version = v["endVersion"].clone();
                }
                _ => {}
            }
            return v;
        }
    }

    async fn recv_type(&mut self, ty: &str) -> Value {
        let v = self.recv().await;
        assert_eq!(v["type"], ty, "unexpected message {v}");
        v
    }

    /// Expect the server to close the socket (after an optional FatalError already consumed).
    async fn expect_closed(&mut self) {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.stream.next()).await.expect("socket not closed") {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => continue,
            }
        }
    }

    fn ts(&self) -> u64 {
        self.version["ts"].as_u64().unwrap()
    }

    async fn subscribe(&mut self, base: u64, query_id: u64, path: &str, args: Value) -> Value {
        self.send(json!({ "type": "ModifyQuerySet", "baseVersion": base, "newVersion": base + 1,
            "modifications": [{ "type": "Add", "queryId": query_id, "udfPath": path, "args": args }] }))
        .await;
        let t = self.recv_type("Transition").await;
        assert_eq!(t["endVersion"]["querySet"], base + 1);
        t
    }

    /// Run a mutation and wait until it is "resolved" the way the client resolves it: its MutationResponse AND a
    /// Transition with ts >= the commit ts, in either order. Returns (response, order) where order is
    /// "response_first" or "transition_first". Other Transitions are collected into `seen`.
    async fn mutate(&mut self, request_id: u64, path: &str, args: Value, seen: &mut Vec<Value>) -> (Value, &'static str) {
        self.send(json!({ "type": "Mutation", "requestId": request_id, "udfPath": path, "args": args })).await;
        loop {
            let v = self.recv().await;
            match v["type"].as_str().unwrap() {
                "Transition" => seen.push(v),
                "MutationResponse" => {
                    assert_eq!(v["requestId"], request_id);
                    if v["success"] != true {
                        return (v, "failed");
                    }
                    let m = v["ts"].as_u64().unwrap();
                    if self.ts() >= m {
                        return (v, "transition_first");
                    }
                    loop {
                        let t = self.recv_type("Transition").await;
                        seen.push(t);
                        if self.ts() >= m {
                            return (v, "response_first");
                        }
                    }
                }
                other => panic!("unexpected {other}: {v}"),
            }
        }
    }
}

fn texts(value: &Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|d| d["text"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn subscribe_mutate_and_receive_updates() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    let list = todos::list::path();

    let mut a = Ws::connect(addr).await;
    let first = a.subscribe(0, 0, &list, json!({})).await;
    assert_eq!(first["startVersion"], json!({ "querySet": 0, "identity": 0, "ts": 0 }));
    assert_eq!(first["modifications"], json!([{ "type": "QueryUpdated", "queryId": 0, "value": [], "logLines": [] }]));

    // B has no subscriptions, so its Transition can only come from the read-your-writes guarantee: the response
    // arrives first, then an empty Transition with ts >= M.
    let mut b = Ws::connect(addr).await;
    let mut seen = Vec::new();
    let (resp, order) = b.mutate(0, &todos::add::path(), json!({ "text": "hi" }), &mut seen).await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["logLines"], json!([]));
    let m = resp["ts"].as_u64().unwrap();
    assert!(resp["result"].as_str().unwrap().len() == 26, "result is the new id");
    assert_eq!(order, "response_first");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["modifications"], json!([]));
    assert!(seen[0]["endVersion"]["ts"].as_u64().unwrap() >= m);

    // A gets the new item, at ts >= M.
    let update = a.recv_type("Transition").await;
    assert!(update["endVersion"]["ts"].as_u64().unwrap() >= m);
    let mods = update["modifications"].as_array().unwrap();
    assert_eq!(mods.len(), 1);
    assert_eq!(mods[0]["queryId"], 0);
    assert_eq!(texts(&mods[0]["value"]), vec!["hi"]);

    // A mutation that writes nothing returns the current ts, which A has already seen: the covering Transition
    // came first, and nothing more is sent.
    let (resp, order) = a.mutate(1, &todos::noop::path(), json!({}), &mut seen).await;
    assert_eq!((resp["result"].clone(), order), (json!(7), "transition_first"));

    // A subscriber mutating what it watches: resolved in whichever order; the Transition carries the update.
    let mut seen = Vec::new();
    let (resp, _order) = a.mutate(2, &todos::add::path(), json!({ "text": "second" }), &mut seen).await;
    assert_eq!(resp["success"], true);
    let latest = seen.iter().rev().flat_map(|t| t["modifications"].as_array().unwrap()).next().expect("an update");
    assert_eq!(texts(&latest["value"]), vec!["second", "hi"]);

    // Actions over the socket.
    a.send(json!({ "type": "Action", "requestId": 3, "udfPath": todos::add_many::path(), "args": { "n": 2 } })).await;
    loop {
        let v = a.recv().await;
        if v["type"] == "ActionResponse" {
            assert_eq!(v, json!({ "type": "ActionResponse", "requestId": 3, "success": true, "result": null, "logLines": [] }));
            break;
        }
    }
    a.send(json!({ "type": "Action", "requestId": 4, "udfPath": "nope:nothing", "args": {} })).await;
    loop {
        let v = a.recv().await;
        if v["type"] == "ActionResponse" {
            assert_eq!(v["success"], false);
            assert!(v["errorMessage"].as_str().unwrap().contains("Could not find public function"));
            break;
        }
    }
}

#[tokio::test]
async fn modify_query_set_versions_and_remove() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    let mut a = Ws::connect(addr).await;
    // Arbitrary, non-contiguous queryIds (as after a reconnect) are fine.
    a.subscribe(0, 42, &todos::list::path(), json!({})).await;
    let t2 = a.subscribe(1, 1000, &todos::latest::path(), json!({ "n": 1 })).await;
    assert_eq!(t2["modifications"].as_array().unwrap().len(), 1, "only the new query is sent; the other is unchanged");

    a.send(json!({ "type": "ModifyQuerySet", "baseVersion": 2, "newVersion": 3,
        "modifications": [{ "type": "Remove", "queryId": 42 }] }))
    .await;
    let removed = a.recv_type("Transition").await;
    assert_eq!(removed["endVersion"]["querySet"], 3);
    assert_eq!(removed["modifications"], json!([{ "type": "QueryRemoved", "queryId": 42 }]));

    // Removed queries receive no further updates; live ones do.
    let mut b = Ws::connect(addr).await;
    b.mutate(0, &todos::add::path(), json!({ "text": "x" }), &mut Vec::new()).await;
    let update = a.recv_type("Transition").await;
    let mods = update["modifications"].as_array().unwrap();
    assert_eq!(mods.len(), 1);
    assert_eq!(mods[0]["queryId"], 1000);

    // Wrong baseVersion: FatalError, then the server closes the socket.
    a.send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [] })).await;
    let fatal = a.recv_type("FatalError").await;
    assert!(fatal["error"].as_str().unwrap().contains("baseVersion"), "{fatal}");
    a.expect_closed().await;

    // newVersion must be baseVersion + 1.
    let mut c = Ws::connect(addr).await;
    c.send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 2, "modifications": [] })).await;
    c.recv_type("FatalError").await;
    c.expect_closed().await;

    // Duplicate live queryId.
    let mut d = Ws::connect(addr).await;
    d.subscribe(0, 1, &todos::list::path(), json!({})).await;
    d.send(json!({ "type": "ModifyQuerySet", "baseVersion": 1, "newVersion": 2,
        "modifications": [{ "type": "Add", "queryId": 1, "udfPath": todos::list::path(), "args": {} }] }))
    .await;
    d.recv_type("FatalError").await;
    d.expect_closed().await;
}

#[tokio::test]
async fn query_failed_then_recovers() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    let mut a = Ws::connect(addr).await;
    let first = a.subscribe(0, 5, &todos::find::path(), json!({ "text": "later" })).await;
    assert_eq!(
        first["modifications"],
        json!([{ "type": "QueryFailed", "queryId": 5, "errorMessage": "no todo \"later\"",
                 "errorData": { "code": "not_found", "data": null }, "logLines": [] }])
    );

    // An unrelated write doesn't re-send the (unchanged) failure.
    let mut b = Ws::connect(addr).await;
    b.mutate(0, &todos::add::path(), json!({ "text": "other" }), &mut Vec::new()).await;
    b.mutate(1, &todos::add::path(), json!({ "text": "later" }), &mut Vec::new()).await;
    let update = a.recv_type("Transition").await;
    let mods = update["modifications"].as_array().unwrap();
    assert_eq!(mods.len(), 1, "{update}");
    assert_eq!(mods[0]["type"], "QueryUpdated");
    assert_eq!(mods[0]["value"]["text"], "later");
}

#[tokio::test]
async fn mutation_user_error_and_bad_messages() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();
    let mut a = Ws::connect(addr).await;

    let (resp, _) = a.mutate(9, &todos::add::path(), json!({ "text": "" }), &mut Vec::new()).await;
    assert_eq!(
        resp,
        json!({ "type": "MutationResponse", "requestId": 9, "success": false, "errorMessage": "text is empty",
                "errorData": { "code": "empty", "data": { "min": 1 } }, "logLines": [] })
    );
    let (resp, _) = a.mutate(10, &todos::add::path(), json!({ "text": 5 }), &mut Vec::new()).await;
    assert!(resp["errorMessage"].as_str().unwrap().starts_with("Invalid args for add: "), "{resp}");
    assert_eq!(resp["errorData"], Value::Null);

    // Authenticate bumps the identity version; a stale baseVersion is fatal. (Real tokens: tests/it/auth.rs.)
    a.subscribe(0, 1, &todos::list::path(), json!({})).await;
    a.send(json!({ "type": "Authenticate", "baseVersion": 0, "token": null })).await;
    let t1 = a.recv_type("Transition").await;
    assert_eq!(t1["endVersion"]["identity"], 1);
    assert_eq!(t1["modifications"], json!([]), "results did not change");
    a.send(json!({ "type": "Authenticate", "baseVersion": 1, "token": null })).await;
    assert_eq!(a.recv_type("Transition").await["endVersion"]["identity"], 2);
    a.send(json!({ "type": "Authenticate", "baseVersion": 0, "token": null })).await;
    a.recv_type("FatalError").await;
    a.expect_closed().await;

    // Malformed JSON and unknown message types are fatal.
    let mut b = Ws::connect(addr).await;
    b.send_raw("{not json").await;
    assert!(b.recv_type("FatalError").await["error"].as_str().unwrap().contains("malformed"));
    b.expect_closed().await;
    let mut c = Ws::connect(addr).await;
    c.send(json!({ "type": "Bogus" })).await;
    c.recv_type("FatalError").await;
    c.expect_closed().await;
}

#[tokio::test]
async fn server_errors_are_hidden_outside_dev() {
    let t = TestApp::with_options(AppOptions { dev: false, ..Default::default() }).await.unwrap();
    let addr = t.serve().await.unwrap();
    let mut a = Ws::connect(addr).await;
    let first = a.subscribe(0, 0, &todos::server_error::path(), json!({})).await;
    assert_eq!(first["modifications"][0]["errorMessage"], "Server Error");
    assert_eq!(first["modifications"][0]["errorData"], Value::Null);
    let (status, body) = http(addr, "POST", "/api/query", Some(json!({ "path": todos::server_error::path() }))).await;
    assert_eq!((status, body["errorMessage"].clone()), (500, json!("Server Error")));

    let dev = TestApp::new().await.unwrap();
    let addr = dev.serve().await.unwrap();
    let (status, body) = http(addr, "POST", "/api/query", Some(json!({ "path": todos::server_error::path(), "args": {} }))).await;
    assert_eq!(status, 500);
    assert!(body["errorMessage"].as_str().unwrap().contains("secret internal detail"));
}

/// A minimal HTTP/1.1 client (no extra dependency): returns (status, JSON body). Anonymous (no Authorization).
async fn http(addr: SocketAddr, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8(buf).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn http_fallback_endpoints() {
    let t = TestApp::new().await.unwrap();
    let addr = t.serve().await.unwrap();

    assert_eq!(http(addr, "GET", "/api/health", None).await, (200, json!({ "status": "ok" })));

    let (status, body) = http(addr, "POST", "/api/mutation", Some(json!({ "path": todos::add::path(), "args": { "text": "h" } }))).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "success");
    assert_eq!(body["ts"], 1);
    assert_eq!(body["logLines"], json!([]));

    let (status, body) = http(addr, "POST", "/api/query", Some(json!({ "path": todos::list::path(), "args": {} }))).await;
    assert_eq!(status, 200);
    assert_eq!(texts(&body["value"]), vec!["h"]);

    let (status, body) = http(addr, "POST", "/api/action", Some(json!({ "path": todos::count_via_action::path(), "args": {} }))).await;
    assert_eq!((status, body["value"].clone()), (200, json!(1)));

    // User error: 400 with code and data.
    let (status, body) = http(addr, "POST", "/api/mutation", Some(json!({ "path": todos::add::path(), "args": { "text": "" } }))).await;
    assert_eq!(status, 400);
    assert_eq!(
        body,
        json!({ "status": "error", "errorMessage": "text is empty", "errorData": { "code": "empty", "data": { "min": 1 } }, "logLines": [] })
    );
    // Bad args: 400.
    let (status, body) = http(addr, "POST", "/api/mutation", Some(json!({ "path": todos::add::path(), "args": {} }))).await;
    assert_eq!(status, 400);
    assert!(body["errorMessage"].as_str().unwrap().starts_with("Invalid args for add: "));
    // Unknown and internal functions: 404.
    let (status, _) = http(addr, "POST", "/api/query", Some(json!({ "path": "nope:x", "args": {} }))).await;
    assert_eq!(status, 404);
    let (status, _) = http(addr, "POST", "/api/query", Some(json!({ "path": todos::count::path(), "args": {} }))).await;
    assert_eq!(status, 404);
    // Malformed body: 400.
    let (status, body) = http(addr, "POST", "/api/query", None).await;
    assert_eq!((status, body["status"].clone()), (400, json!("error")));
}

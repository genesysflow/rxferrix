//! Shared helpers for the auth-adjacent tests (revocation, external JWTs, settings, audit, impersonation): an HTTP
//! client for the auth and function APIs, and a raw sync-protocol WebSocket.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub struct Http {
    pub client: reqwest::Client,
    pub base: String,
}

impl Http {
    pub fn new(addr: SocketAddr) -> Http {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Http { client, base: format!("http://{addr}") }
    }

    pub async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self.client.post(format!("{}{path}", self.base)).json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    pub async fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        let mut req = self.client.get(format!("{}{path}", self.base));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    /// Call a function over the HTTP API.
    pub async fn call(&self, kind: &str, path: &str, args: Value, token: Option<&str>) -> (u16, Value) {
        self.post(&format!("/api/{kind}"), token, json!({ "path": path, "args": args })).await
    }

    /// Call a function that must succeed; returns its value.
    pub async fn ok(&self, kind: &str, path: &str, args: Value, token: &str) -> Value {
        let (s, b) = self.call(kind, path, args, Some(token)).await;
        assert_eq!(s, 200, "{path}: {b}");
        b["value"].clone()
    }

    /// Sign up with a password; returns the tokens.
    pub async fn sign_up(&self, email: &str, password: &str) -> Value {
        let (s, b) = self.post("/api/auth/signup", None, json!({ "email": email, "password": password })).await;
        assert_eq!(s, 200, "{b}");
        b
    }

    pub async fn sign_in(&self, email: &str, password: &str) -> Value {
        let (s, b) = self.post("/api/auth/signin", None, json!({ "email": email, "password": password })).await;
        assert_eq!(s, 200, "{b}");
        b
    }

    pub async fn admin_sign_in(&self, email: &str, password: &str) -> Value {
        let (s, b) = self.post("/api/auth/admin/signin", None, json!({ "email": email, "password": password })).await;
        assert_eq!(s, 200, "{b}");
        b
    }
}

pub fn token(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_else(|| panic!("no {key} in {v}")).to_string()
}

pub fn code_of(v: &Value) -> &str {
    v["errorData"]["code"].as_str().unwrap_or("")
}

pub struct Ws {
    pub stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_query: u64,
    query_set: u64,
    identity: u64,
}

impl Ws {
    pub async fn connect(addr: SocketAddr) -> Ws {
        let (stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
        let mut ws = Ws { stream, next_query: 0, query_set: 0, identity: 0 };
        let session = format!("s-{}", rand::random::<u64>());
        ws.send(json!({ "type": "Connect", "sessionId": session, "connectionCount": 0, "lastCloseReason": null })).await;
        ws
    }

    /// Authenticate and wait for the confirming Transition (panics on AuthError).
    pub async fn authenticate(&mut self, token: Option<&str>) {
        let base = self.identity;
        self.send(json!({ "type": "Authenticate", "baseVersion": base, "token": token })).await;
        self.identity += 1;
        loop {
            let t = self.recv().await;
            assert_ne!(t["type"], "AuthError", "{t}");
            if t["type"] == "Transition" && t["endVersion"]["identity"] == base + 1 {
                return;
            }
        }
    }

    /// Send `Authenticate` and return the first AuthError or confirming Transition.
    pub async fn try_authenticate(&mut self, token: &str) -> Value {
        let base = self.identity;
        self.send(json!({ "type": "Authenticate", "baseVersion": base, "token": token })).await;
        self.identity += 1;
        loop {
            let t = self.recv().await;
            if t["type"] == "AuthError" || (t["type"] == "Transition" && t["endVersion"]["identity"] == base + 1) {
                return t;
            }
        }
    }

    /// Subscribe to a query; returns its query id and first value.
    pub async fn subscribe(&mut self, path: &str, args: Value) -> (u64, Value) {
        let query_id = self.next_query;
        self.next_query += 1;
        let base = self.query_set;
        self.query_set += 1;
        self.send(json!({ "type": "ModifyQuerySet", "baseVersion": base, "newVersion": base + 1, "modifications": [
            { "type": "Add", "queryId": query_id, "udfPath": path, "args": args }
        ] }))
        .await;
        (query_id, self.next_value(query_id).await)
    }

    pub async fn send(&mut self, v: Value) {
        self.stream.send(Message::text(v.to_string())).await.unwrap();
    }

    pub async fn recv(&mut self) -> Value {
        self.recv_within(Duration::from_secs(10)).await.expect("timed out waiting for a message")
    }

    /// The next non-Ping message within `within`, or None. Panics if the socket closes.
    pub async fn recv_within(&mut self, within: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let msg = match tokio::time::timeout_at(deadline, self.stream.next()).await {
                Err(_) => return None,
                Ok(m) => m.expect("socket closed").expect("socket error"),
            };
            let Message::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            if v["type"] != "Ping" {
                return Some(v);
            }
        }
    }

    pub fn value_of(t: &Value, query_id: u64) -> Option<Value> {
        t["modifications"]
            .as_array()?
            .iter()
            .find(|m| m["queryId"] == query_id && m["type"] == "QueryUpdated")
            .map(|m| m["value"].clone())
    }

    /// Receive until a Transition updates `query_id`; returns the value.
    pub async fn next_value(&mut self, query_id: u64) -> Value {
        loop {
            let t = self.recv().await;
            assert!(matches!(t["type"].as_str(), Some("Transition" | "MutationResponse")), "unexpected {t}");
            if let Some(v) = Self::value_of(&t, query_id) {
                return v;
            }
        }
    }

    /// Expect the socket to close, returning every message received before it did.
    pub async fn drain_until_closed(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.stream.next()).await.expect("socket not closed") {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return out,
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["type"] != "Ping" {
                        out.push(v);
                    }
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

/// Read Server-Sent Events from a streaming response.
pub struct Sse {
    res: reqwest::Response,
    buf: String,
}

impl Sse {
    pub fn new(res: reqwest::Response) -> Sse {
        Sse { res, buf: String::new() }
    }

    /// The next `(event, data)`, or None when the stream ends.
    pub async fn next(&mut self) -> Option<(String, String)> {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let raw: String = self.buf.drain(..end + 2).collect();
                let (mut event, mut data) = (String::new(), String::new());
                for line in raw.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        event = e.to_string();
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push_str(d);
                    }
                }
                if !event.is_empty() {
                    return Some((event, data));
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.res.chunk()).await.expect("sse timeout").unwrap()?;
            self.buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }
}

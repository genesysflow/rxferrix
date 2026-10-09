//! Auth end to end over real HTTP and WebSockets: password sign-up/sign-in, refresh rotation and reuse detection,
//! sign-out, expiry, `ctx.auth` in queries/mutations/actions, email flows (captured with `MemoryMailer`), OAuth
//! against a mock OIDC provider, admins and the setup token, and rate limiting.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrix::FunctionRef;
use ferrix::auth::{AuthConfig, Identity, MemoryMailer, Oidc, RateLimits};
use ferrix::testing::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// App functions that depend on the caller's identity.
mod app {
    use ferrix::auth::User;
    use ferrix::prelude::*;

    /// A note only its owner can see.
    #[table(name = "private_notes")]
    #[index(by_owner = [owner])]
    pub struct PrivateNote {
        pub owner: Id<User>,
        pub text: String,
    }

    /// The signed-in user's email (read through the db, so it reacts to profile changes).
    #[query]
    pub async fn whoami(ctx: &QueryCtx) -> Result<Option<String>> {
        Ok(ctx.auth.user(&ctx.db).await?.and_then(|u| u.value.email))
    }

    #[query]
    pub async fn my_notes(ctx: &QueryCtx) -> Result<Vec<String>> {
        let Some(me) = ctx.auth.user_id() else { return Ok(Vec::new()) };
        let mut notes =
            ctx.db.query::<PrivateNote>().with_index(PrivateNote::by_owner, |q| q.eq(PrivateNote::owner, me)).collect().await?;
        notes.sort_by(|a, b| a.creation_time.total_cmp(&b.creation_time));
        Ok(notes.into_iter().map(|n| n.value.text).collect())
    }

    #[mutation]
    pub async fn add_note(ctx: &MutationCtx, text: String) -> Result<Id<PrivateNote>> {
        let owner = ctx.auth.require_user()?;
        ctx.db.insert(PrivateNote { owner, text }).await
    }

    /// Identity flows from an action into `run_mutation`.
    #[action]
    pub async fn add_note_via_action(ctx: &ActionCtx, text: String) -> Result<Id<PrivateNote>> {
        ctx.run_mutation::<add_note>((text,)).await
    }

    #[query]
    pub async fn admin_only(ctx: &QueryCtx) -> Result<String> {
        let admin = ctx.auth.require_admin()?;
        Ok(admin.to_string())
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------------------------------

struct Http {
    client: reqwest::Client,
    base: String,
}

impl Http {
    fn new(addr: SocketAddr) -> Http {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Http { client, base: format!("http://{addr}") }
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.post_with(path, None, body).await
    }

    async fn post_with(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self.client.post(format!("{}{path}", self.base)).json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    /// Call a function over the HTTP API.
    async fn call(&self, kind: &str, path: &str, args: Value, token: Option<&str>) -> (u16, Value) {
        self.post_with(&format!("/api/{kind}"), token, json!({ "path": path, "args": args })).await
    }

    async fn sign_up(&self, email: &str, password: &str) -> Value {
        let (status, body) = self.post("/api/auth/signup", json!({ "email": email, "password": password })).await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn sign_in(&self, email: &str, password: &str) -> (u16, Value) {
        self.post("/api/auth/signin", json!({ "email": email, "password": password })).await
    }
}

fn token(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_else(|| panic!("no {key} in {v}")).to_string()
}

fn code_of(v: &Value) -> &str {
    v["errorData"]["code"].as_str().unwrap_or("")
}

fn config(mailer: &Arc<MemoryMailer>) -> AuthConfig {
    AuthConfig::password()
        .magic_link()
        .mailer(mailer.clone())
        .rate_limits(None)
        .redirect_url("http://app.test")
}

async fn wait_for_email(mailer: &MemoryMailer, to: &str, count: usize) -> ferrix::auth::Email {
    for _ in 0..200 {
        let mine: Vec<_> = mailer.emails().into_iter().filter(|e| e.to == to).collect();
        if mine.len() >= count {
            return mine[count - 1].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no email #{count} to {to}");
}

/// The 6-digit code in an email body.
fn email_code(body: &str) -> String {
    let at = body.find("Your code: ").expect("code in email") + "Your code: ".len();
    body[at..at + 6].to_string()
}

/// The link token (`#<key>=<token>`) in an email body.
fn email_token(body: &str, key: &str) -> String {
    let marker = format!("#{key}=");
    let at = body.find(&marker).unwrap_or_else(|| panic!("no {key} link in {body}")) + marker.len();
    body[at..].split_whitespace().next().unwrap().to_string()
}

struct Ws {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Ws {
    async fn connect(addr: SocketAddr) -> Ws {
        let (stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
        let mut ws = Ws { stream };
        ws.send(json!({ "type": "Connect", "sessionId": "s", "connectionCount": 0, "lastCloseReason": null })).await;
        ws
    }

    async fn send(&mut self, v: Value) {
        self.stream.send(Message::text(v.to_string())).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.stream.next())
                .await
                .expect("timed out waiting for a message")
                .expect("socket closed")
                .unwrap();
            let Message::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            if v["type"] != "Ping" {
                return v;
            }
        }
    }

    async fn recv_type(&mut self, ty: &str) -> Value {
        let v = self.recv().await;
        assert_eq!(v["type"], ty, "unexpected message {v}");
        v
    }

    async fn expect_closed(&mut self) {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.stream.next()).await.expect("socket not closed") {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => continue,
            }
        }
    }

    /// Value of query `query_id` in a Transition, if it was updated (None for other messages).
    fn value_of(t: &Value, query_id: u64) -> Option<Value> {
        t["modifications"]
            .as_array()?
            .iter()
            .find(|m| m["queryId"] == query_id && m["type"] == "QueryUpdated")
            .map(|m| m["value"].clone())
    }

    /// Receive messages until a Transition gives query `query_id` a new value; returns it.
    async fn next_value(&mut self, query_id: u64) -> Value {
        loop {
            let t = self.recv().await;
            assert!(matches!(t["type"].as_str(), Some("Transition" | "MutationResponse")), "unexpected {t}");
            if let Some(v) = Self::value_of(&t, query_id) {
                return v;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Password, refresh, sign-out
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn password_sign_up_sign_in_and_identity_in_functions() {
    let mailer = MemoryMailer::new();
    let t = TestApp::with_auth(config(&mailer)).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());

    let up = http.sign_up(" Ada@Example.com ", "correct horse").await;
    assert_eq!(up["expiresIn"], 900);
    let user_id = token(&up, "userId");
    assert!(up.get("adminId").is_none());

    // Duplicate (case-insensitive), weak password, bad email.
    let (s, b) = http.post("/api/auth/signup", json!({ "email": "ada@example.com", "password": "another one" })).await;
    assert_eq!((s, code_of(&b)), (400, "email_in_use"));
    let (s, b) = http.post("/api/auth/signup", json!({ "email": "bob@example.com", "password": "short" })).await;
    assert_eq!((s, code_of(&b)), (400, "weak_password"));
    let (s, b) = http.post("/api/auth/signup", json!({ "email": "not an email", "password": "long enough" })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_email"));

    // Wrong password and unknown account give the same answer.
    let (s1, b1) = http.sign_in("ada@example.com", "wrong password").await;
    let (s2, b2) = http.sign_in("nobody@example.com", "wrong password").await;
    assert_eq!((s1, code_of(&b1)), (401, "invalid_credentials"));
    assert_eq!((s1, &b1), (s2, &b2));

    let (s, signed_in) = http.sign_in("ADA@example.com", "correct horse").await;
    assert_eq!(s, 200, "{signed_in}");
    assert_eq!(token(&signed_in, "userId"), user_id);
    let access = token(&signed_in, "accessToken");

    // ctx.auth through the HTTP API.
    let whoami = app::whoami::path();
    assert_eq!(http.call("query", &whoami, json!({}), None).await, (200, json!({ "status": "success", "value": null, "logLines": [] })));
    let (s, b) = http.call("query", &whoami, json!({}), Some(&access)).await;
    assert_eq!((s, &b["value"]), (200, &json!("ada@example.com")));
    let (s, b) = http.call("mutation", &app::add_note::path(), json!({ "text": "anon" }), None).await;
    assert_eq!((s, code_of(&b)), (400, "unauthenticated"));
    let (s, b) = http.call("query", &whoami, json!({}), Some("not.a.token")).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_token"));
    assert_eq!(http.call("mutation", &app::add_note::path(), json!({ "text": "one" }), Some(&access)).await.0, 200);
    assert_eq!(http.call("action", &app::add_note_via_action::path(), json!({ "text": "two" }), Some(&access)).await.0, 200);
    let (_, b) = http.call("query", &app::my_notes::path(), json!({}), Some(&access)).await;
    assert_eq!(b["value"], json!(["one", "two"]));
    let (_, b) = http.call("query", &app::my_notes::path(), json!({}), None).await;
    assert_eq!(b["value"], json!([]));

    // Built-in system functions.
    let (_, me) = http.call("query", "_system/auth:me", json!({}), Some(&access)).await;
    assert_eq!(me["value"]["_id"], json!(user_id));
    assert_eq!(me["value"]["email"], "ada@example.com");
    assert_eq!(me["value"]["emailVerified"], false);
    let text = me.to_string();
    assert!(!text.contains("argon2") && !text.contains("passwordHash"), "no secrets in {text}");
    let (s, _) = http.call("mutation", "_system/auth:update_profile", json!({ "name": "Ada" }), Some(&access)).await;
    assert_eq!(s, 200);
    let (_, me) = http.call("query", "_system/auth:me", json!({}), Some(&access)).await;
    assert_eq!(me["value"]["name"], "Ada");
    let (s, b) = http.call("mutation", "_system/auth:update_profile", json!({ "name": "x" }), None).await;
    assert_eq!((s, code_of(&b)), (400, "unauthenticated"));

    // TestApp helpers: identity in direct calls, and actions pass it on to run_mutation.
    let me: ferrix::Id<ferrix::auth::User> = user_id.parse().unwrap();
    let ident = Identity::user(me, None);
    t.action_as::<app::add_note_via_action>(&ident, ("three".to_string(),)).await.unwrap();
    assert_eq!(t.query_as::<app::my_notes>(&ident, ()).await.unwrap(), vec!["one", "two", "three"]);
    let err = t.mutation::<app::add_note>(("x".to_string(),)).await.unwrap_err();
    assert_eq!(err.downcast_ref::<ferrix::UserError>().unwrap().code, "unauthenticated");

    // A disabled method answers 404.
    let plain = TestApp::new().await.unwrap();
    let http2 = Http::new(plain.serve().await.unwrap());
    let (s, b) = http2.post("/api/auth/signup", json!({ "email": "a@b.co", "password": "long enough" })).await;
    assert_eq!((s, code_of(&b)), (404, "not_enabled"));
    let (s, b) = http2.post("/api/auth/magic/request", json!({ "email": "a@b.co" })).await;
    assert_eq!((s, code_of(&b)), (404, "not_enabled"));
}

#[tokio::test]
async fn refresh_rotation_reuse_detection_and_sign_out() {
    let mailer = MemoryMailer::new();
    let t = TestApp::with_auth(config(&mailer).refresh_reuse_grace(Duration::ZERO)).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let whoami = app::whoami::path();

    let first = http.sign_up("carol@example.com", "correct horse").await;
    let r1 = token(&first, "refreshToken");
    let (s, second) = http.post("/api/auth/refresh", json!({ "refreshToken": r1 })).await;
    assert_eq!(s, 200, "{second}");
    let r2 = token(&second, "refreshToken");
    assert_ne!(r1, r2, "refresh tokens rotate");
    assert_eq!(http.call("query", &whoami, json!({}), Some(&token(&second, "accessToken"))).await.1["value"], "carol@example.com");

    // Reusing r1 (already rotated) revokes the whole session: r2 and the access tokens stop working.
    let (s, b) = http.post("/api/auth/refresh", json!({ "refreshToken": r1 })).await;
    assert_eq!((s, code_of(&b)), (401, "refresh_token_reused"));
    let (s, b) = http.post("/api/auth/refresh", json!({ "refreshToken": r2 })).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_refresh_token"));
    let (s, b) = http.call("query", &whoami, json!({}), Some(&token(&second, "accessToken"))).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_token"));
    assert!(b["errorMessage"].as_str().unwrap().contains("session"));
    let (s, b) = http.post("/api/auth/refresh", json!({ "refreshToken": "garbage" })).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_refresh_token"));

    // Sign-out with the refresh token ends the session.
    let (_, session) = http.sign_in("carol@example.com", "correct horse").await;
    let (s, _) = http.post("/api/auth/signout", json!({ "refreshToken": token(&session, "refreshToken") })).await;
    assert_eq!(s, 200);
    let (s, _) = http.post("/api/auth/refresh", json!({ "refreshToken": token(&session, "refreshToken") })).await;
    assert_eq!(s, 401);
    assert_eq!(http.call("query", &whoami, json!({}), Some(&token(&session, "accessToken"))).await.0, 401);

    // Sign-out with only the access token works too; signing out twice is harmless.
    let (_, session) = http.sign_in("carol@example.com", "correct horse").await;
    let access = token(&session, "accessToken");
    assert_eq!(http.post_with("/api/auth/signout", Some(&access), json!({})).await.0, 200);
    assert_eq!(http.post_with("/api/auth/signout", Some(&access), json!({})).await.0, 200);
    assert_eq!(http.call("query", &whoami, json!({}), Some(&access)).await.0, 401);

    // Within the grace window a just-rotated token is "stale" and does NOT revoke the session (two tabs racing).
    let t2 = TestApp::with_auth(config(&mailer)).await.unwrap();
    let http2 = Http::new(t2.serve().await.unwrap());
    let a = http2.sign_up("dave@example.com", "correct horse").await;
    let (_, b) = http2.post("/api/auth/refresh", json!({ "refreshToken": token(&a, "refreshToken") })).await;
    let (s, stale) = http2.post("/api/auth/refresh", json!({ "refreshToken": token(&a, "refreshToken") })).await;
    assert_eq!((s, code_of(&stale)), (401, "stale_refresh_token"));
    let (s, _) = http2.post("/api/auth/refresh", json!({ "refreshToken": token(&b, "refreshToken") })).await;
    assert_eq!(s, 200, "the session survives");
}

#[tokio::test]
async fn access_token_and_session_expiry() {
    let mailer = MemoryMailer::new();
    // Expiry has second granularity, so a 2s token is valid for 1-2s.
    let auth = config(&mailer).access_token_ttl(Duration::from_secs(2)).session_ttl(Duration::from_millis(1500));
    let t = TestApp::with_auth(auth).await.unwrap();
    let addr = t.serve().await.unwrap();
    let http = Http::new(addr);
    let up = http.sign_up("erin@example.com", "correct horse").await;
    let access = token(&up, "accessToken");

    // The socket's identity ends with AuthError when the token expires.
    let mut ws = Ws::connect(addr).await;
    ws.send(json!({ "type": "Authenticate", "baseVersion": 0, "token": access })).await;
    ws.recv_type("Transition").await;
    let err = ws.recv_type("AuthError").await;
    assert_eq!(err["error"], "access token expired");
    ws.expect_closed().await;

    // By now (>= 1s) wait until the 1.5s session is over too.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (s, b) = http.call("query", &app::whoami::path(), json!({}), Some(&access)).await;
    assert_eq!((s, b["errorMessage"].as_str().unwrap()), (401, "access token expired"));
    let (s, b) = http.post("/api/auth/refresh", json!({ "refreshToken": token(&up, "refreshToken") })).await;
    assert_eq!((s, code_of(&b)), (401, "session_expired"));
}

// ---------------------------------------------------------------------------------------------------------------------
// WebSocket Authenticate
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn websocket_authenticate_reruns_identity_dependent_queries() {
    let mailer = MemoryMailer::new();
    let t = TestApp::with_auth(config(&mailer)).await.unwrap();
    let addr = t.serve().await.unwrap();
    let http = Http::new(addr);
    let up = http.sign_up("frank@example.com", "correct horse").await;
    let access = token(&up, "accessToken");

    let mut ws = Ws::connect(addr).await;
    ws.send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
        { "type": "Add", "queryId": 0, "udfPath": app::whoami::path(), "args": {} },
        { "type": "Add", "queryId": 1, "udfPath": app::my_notes::path(), "args": {} },
        { "type": "Add", "queryId": 2, "udfPath": "_system/auth:me", "args": {} },
    ] }))
    .await;
    let first = ws.recv_type("Transition").await;
    assert_eq!(Ws::value_of(&first, 0), Some(Value::Null));
    assert_eq!(Ws::value_of(&first, 2), Some(Value::Null));

    // A valid token: identity version 1, and the identity-dependent queries rerun.
    ws.send(json!({ "type": "Authenticate", "baseVersion": 0, "token": access })).await;
    let t1 = ws.recv_type("Transition").await;
    assert_eq!(t1["endVersion"]["identity"], 1);
    assert_eq!(Ws::value_of(&t1, 0), Some(json!("frank@example.com")));
    assert_eq!(Ws::value_of(&t1, 2).unwrap()["email"], "frank@example.com");
    assert_eq!(Ws::value_of(&t1, 1), None, "unchanged results are not resent");

    // Mutations on this socket run as the user; `user()` reads are tracked, so a profile change pushes `me`.
    ws.send(json!({ "type": "Mutation", "requestId": 0, "udfPath": app::add_note::path(), "args": { "text": "hi" } })).await;
    assert_eq!(ws.next_value(1).await, json!(["hi"]));
    http.call("mutation", "_system/auth:update_profile", json!({ "name": "Frank" }), Some(&access)).await;
    assert_eq!(ws.next_value(2).await["name"], "Frank");

    // Clearing the token makes the socket anonymous again.
    ws.send(json!({ "type": "Authenticate", "baseVersion": 1, "token": null })).await;
    let t2 = loop {
        let t = ws.recv().await;
        if t["type"] == "Transition" && t["endVersion"]["identity"] == 2 {
            break t;
        }
    };
    assert_eq!(Ws::value_of(&t2, 0), Some(Value::Null));
    assert_eq!(Ws::value_of(&t2, 1), Some(json!([])));

    // An invalid token: AuthError, then the server closes the socket.
    let mut bad = Ws::connect(addr).await;
    bad.send(json!({ "type": "Authenticate", "baseVersion": 0, "token": "eyJ.bad.token" })).await;
    assert_eq!(bad.recv_type("AuthError").await["error"], "malformed access token");
    bad.expect_closed().await;

    // After sign-out, the (unexpired) access token is rejected on new sockets.
    http.post_with("/api/auth/signout", Some(&access), json!({})).await;
    let mut after = Ws::connect(addr).await;
    after.send(json!({ "type": "Authenticate", "baseVersion": 0, "token": access })).await;
    assert_eq!(after.recv_type("AuthError").await["error"], "session has ended");
    after.expect_closed().await;
}

// ---------------------------------------------------------------------------------------------------------------------
// Email flows
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn email_verification_password_reset_and_magic_link() {
    let mailer = MemoryMailer::new();
    let t = TestApp::with_auth(config(&mailer)).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let email = "gina@example.com";

    // Sign-up sends a verification email with a link to the default app URL and a code.
    let up = http.sign_up(email, "correct horse").await;
    let access = token(&up, "accessToken");
    let verify = wait_for_email(&mailer, email, 1).await;
    assert_eq!(verify.subject, "Verify your email address");
    assert!(verify.body.contains("http://app.test/#ferrix_verify_token="), "{}", verify.body);
    let (s, b) = http.post("/api/auth/verify-email/confirm", json!({ "token": "wrong" })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_code"));
    let vt = email_token(&verify.body, "ferrix_verify_token");
    assert_eq!(http.post("/api/auth/verify-email/confirm", json!({ "token": vt })).await.0, 200);
    let (_, me) = http.call("query", "_system/auth:me", json!({}), Some(&access)).await;
    assert_eq!(me["value"]["emailVerified"], true);
    assert_eq!(http.post("/api/auth/verify-email/confirm", json!({ "token": vt })).await.0, 400, "one use only");

    // Password reset: same answer for unknown emails, and no email is sent for them.
    let (s, b) = http.post("/api/auth/reset/request", json!({ "email": "nobody@example.com" })).await;
    assert_eq!((s, b), (200, json!({})));
    let (s, b) = http.post("/api/auth/reset/request", json!({ "email": email, "redirectTo": "http://app.test/reset" })).await;
    assert_eq!((s, b), (200, json!({})));
    let reset = wait_for_email(&mailer, email, 2).await;
    assert_eq!(reset.subject, "Reset your password");
    assert!(reset.body.contains("http://app.test/reset#ferrix_reset_token="), "{}", reset.body);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(mailer.emails().iter().all(|e| e.to != "nobody@example.com"));
    // A redirect outside the allowlist is refused outright.
    let (s, b) = http.post("/api/auth/reset/request", json!({ "email": email, "redirectTo": "https://evil.test/" })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_redirect"));

    // Confirm with the numeric code; earlier sessions end, the new password works, the old one doesn't.
    let code = email_code(&reset.body);
    let (s, b) = http.post("/api/auth/reset/confirm", json!({ "email": email, "code": code, "password": "short" })).await;
    assert_eq!((s, code_of(&b)), (400, "weak_password"), "policy is checked before the code is used up");
    let (s, b) = http.post("/api/auth/reset/confirm", json!({ "email": email, "code": code, "password": "new password!" })).await;
    assert_eq!(s, 200, "{b}");
    assert!(b["accessToken"].is_string());
    assert_eq!(http.call("query", &app::whoami::path(), json!({}), Some(&access)).await.0, 401, "old sessions ended");
    assert_eq!(http.sign_in(email, "correct horse").await.0, 401);
    assert_eq!(http.sign_in(email, "new password!").await.0, 200);

    // Wrong codes burn the code after 5 attempts.
    http.post("/api/auth/reset/request", json!({ "email": email })).await;
    let reset2 = wait_for_email(&mailer, email, 3).await;
    let good = email_code(&reset2.body);
    let wrong = if good == "000000" { "000001" } else { "000000" };
    for _ in 0..5 {
        let (s, b) = http.post("/api/auth/reset/confirm", json!({ "email": email, "code": wrong, "password": "x password 2" })).await;
        assert_eq!((s, code_of(&b)), (400, "invalid_code"));
    }
    let (s, _) = http.post("/api/auth/reset/confirm", json!({ "email": email, "code": good, "password": "x password 2" })).await;
    assert_eq!(s, 400, "the code is burned");

    // Magic link: signs in an existing user by link, and creates a new (verified) user by code.
    http.post("/api/auth/magic/request", json!({ "email": email })).await;
    let magic = wait_for_email(&mailer, email, 4).await;
    let mt = email_token(&magic.body, "ferrix_magic_token");
    let (s, b) = http.post("/api/auth/magic/verify", json!({ "token": mt })).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(token(&b, "userId"), token(&up, "userId"));

    let newbie = "hank@example.com";
    assert_eq!(http.post("/api/auth/magic/request", json!({ "email": newbie })).await.0, 200);
    let magic = wait_for_email(&mailer, newbie, 1).await;
    let (s, b) = http.post("/api/auth/magic/verify", json!({ "email": "HANK@example.com", "code": email_code(&magic.body) })).await;
    assert_eq!(s, 200, "{b}");
    let (_, me) = http.call("query", "_system/auth:me", json!({}), Some(&token(&b, "accessToken"))).await;
    assert_eq!((me["value"]["email"].clone(), me["value"]["emailVerified"].clone()), (json!(newbie), json!(true)));
}

#[tokio::test]
async fn required_email_verification_and_disabled_signups() {
    let mailer = MemoryMailer::new();
    let t = TestApp::with_auth(config(&mailer).require_email_verification(true)).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let (s, b) = http.post("/api/auth/signup", json!({ "email": "ivy@example.com", "password": "correct horse" })).await;
    assert_eq!((s, b), (200, json!({ "verificationRequired": true })));
    let (s, b) = http.sign_in("ivy@example.com", "correct horse").await;
    assert_eq!((s, code_of(&b)), (403, "email_not_verified"));
    let mail = wait_for_email(&mailer, "ivy@example.com", 1).await;
    http.post("/api/auth/verify-email/confirm", json!({ "email": "ivy@example.com", "code": email_code(&mail.body) })).await;
    assert_eq!(http.sign_in("ivy@example.com", "correct horse").await.0, 200);

    let closed = TestApp::with_auth(config(&mailer).signups(false)).await.unwrap();
    let http = Http::new(closed.serve().await.unwrap());
    let (s, b) = http.post("/api/auth/signup", json!({ "email": "jo@example.com", "password": "correct horse" })).await;
    assert_eq!((s, code_of(&b)), (403, "signups_disabled"));
    // Magic links for unknown emails are silently not sent.
    assert_eq!(http.post("/api/auth/magic/request", json!({ "email": "jo@example.com" })).await.0, 200);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(mailer.last_to("jo@example.com").is_none());
}

// ---------------------------------------------------------------------------------------------------------------------
// OAuth against a mock OIDC provider
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct MockOidc {
    /// code → (PKCE challenge, redirect_uri, profile)
    codes: HashMap<String, (String, String, Value)>,
    /// access token → profile
    tokens: HashMap<String, Value>,
    token_requests: usize,
}

type Mock = Arc<Mutex<MockOidc>>;

async fn start_mock_oidc() -> (SocketAddr, Mock) {
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let mock: Mock = Arc::default();
    let discovery = json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "userinfo_endpoint": format!("{base}/userinfo"),
    });
    let router = axum::Router::new()
        .route("/.well-known/openid-configuration", get(move || async move { axum::Json(discovery) }))
        .route(
            "/token",
            post(|State(mock): State<Mock>, axum::Form(form): axum::Form<HashMap<String, String>>| async move {
                let mut m = mock.lock().unwrap();
                m.token_requests += 1;
                let ok = form.get("grant_type").map(String::as_str) == Some("authorization_code")
                    && form.get("client_id").map(String::as_str) == Some("client-1")
                    && form.get("client_secret").map(String::as_str) == Some("secret-1");
                let entry = form.get("code").and_then(|c| m.codes.remove(c));
                let Some((challenge, redirect_uri, profile)) = entry.filter(|_| ok) else {
                    return axum::Json(json!({ "error": "invalid_grant" }));
                };
                // PKCE: the verifier must hash to the challenge sent with the authorization request.
                let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                use sha2::Digest;
                let computed = data_encoding::BASE64URL_NOPAD.encode(&sha2::Sha256::digest(verifier.as_bytes()));
                if computed != challenge || form.get("redirect_uri") != Some(&redirect_uri) {
                    return axum::Json(json!({ "error": "invalid_grant" }));
                }
                let access = format!("at-{}", m.tokens.len());
                m.tokens.insert(access.clone(), profile);
                axum::Json(json!({ "access_token": access, "token_type": "Bearer" }))
            }),
        )
        .route(
            "/userinfo",
            get(|State(mock): State<Mock>, headers: HeaderMap| async move {
                let token = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
                let profile = token.and_then(|t| mock.lock().unwrap().tokens.get(t).cloned());
                match profile {
                    Some(p) => (axum::http::StatusCode::OK, axum::Json(p)),
                    None => (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({}))),
                }
            }),
        )
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (addr, mock)
}

/// Run the whole browser flow for `profile`; returns the final redirect URL (to the app).
async fn oauth_flow(http: &Http, mock: &Mock, profile: Value, redirect_to: &str, send_cookie: bool) -> url::Url {
    let start = http
        .client
        .get(format!("{}/api/auth/oauth/mock", http.base))
        .query(&[("redirect_to", redirect_to)])
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 302);
    let cookie = start.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"), "{cookie}");
    let authorize = url::Url::parse(start.headers()["location"].to_str().unwrap()).unwrap();
    let q: HashMap<String, String> = authorize.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "client-1");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["scope"], "openid email profile");
    assert!(q["redirect_uri"].ends_with("/api/auth/oauth/mock/callback"));

    // The "user" approves at the provider, which redirects back with a code.
    let code = format!("code-{}", q["state"]);
    mock.lock().unwrap().codes.insert(code.clone(), (q["code_challenge"].clone(), q["redirect_uri"].clone(), profile));
    let mut req = http.client.get(&q["redirect_uri"]).query(&[("code", code.as_str()), ("state", q["state"].as_str())]);
    if send_cookie {
        req = req.header("cookie", cookie.split(';').next().unwrap());
    }
    let back = req.send().await.unwrap();
    assert_eq!(back.status(), 302);
    assert_eq!(back.headers()["cache-control"], "no-store");
    url::Url::parse(back.headers()["location"].to_str().unwrap()).unwrap()
}

fn fragment(url: &url::Url) -> HashMap<String, String> {
    url::form_urlencoded::parse(url.fragment().unwrap_or_default().as_bytes()).into_owned().collect()
}

#[tokio::test]
async fn oauth_with_mock_oidc_provider() {
    let (mock_addr, mock) = start_mock_oidc().await;
    let mailer = MemoryMailer::new();
    let auth = config(&mailer).oauth(Oidc::new("mock", format!("http://{mock_addr}"), "client-1", "secret-1"));
    let t = TestApp::with_auth(auth).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());

    let res = http.client.get(format!("{}/api/auth/providers", http.base)).send().await.unwrap();
    let providers: Value = res.json().await.unwrap();
    assert_eq!(providers["oauth"], json!(["mock"]));

    let profile = json!({ "sub": "u-1", "email": "Kim@Example.com", "email_verified": true, "name": "Kim", "picture": "https://img.test/k.png" });
    let landed = oauth_flow(&http, &mock, profile.clone(), "http://app.test/after?x=1", true).await;
    assert_eq!(landed.as_str().split('#').next().unwrap(), "http://app.test/after?x=1");
    let f = fragment(&landed);
    assert_eq!(f["ferrix_expires_in"], "900");
    let access = f["ferrix_access_token"].clone();
    let (_, me) = http.call("query", "_system/auth:me", json!({}), Some(&access)).await;
    assert_eq!(me["value"]["email"], "kim@example.com");
    assert_eq!(me["value"]["emailVerified"], true);
    assert_eq!(me["value"]["name"], "Kim");
    assert_eq!(me["value"]["image"], "https://img.test/k.png");
    // The refresh token from the fragment works.
    let (s, _) = http.post("/api/auth/refresh", json!({ "refreshToken": f["ferrix_refresh_token"] })).await;
    assert_eq!(s, 200);

    // Signing in again finds the same user through the linked account.
    let again = fragment(&oauth_flow(&http, &mock, profile, "http://app.test/", true).await);
    assert_eq!(again["ferrix_user_id"], f["ferrix_user_id"]);

    // Without the state cookie (a callback URL replayed into another browser), sign-in is refused.
    let replayed = oauth_flow(&http, &mock, json!({ "sub": "u-2" }), "http://app.test/", false).await;
    assert_eq!(fragment(&replayed)["ferrix_error"], "state_mismatch");
    assert!(!fragment(&replayed).contains_key("ferrix_access_token"));

    // Redirects outside the allowlist are refused before leaving for the provider.
    let res = http.client.get(format!("{}/api/auth/oauth/mock?redirect_to=https://evil.test/", http.base)).send().await.unwrap();
    assert_eq!(res.status(), 400);
    // Unknown provider; unknown state.
    assert_eq!(http.client.get(format!("{}/api/auth/oauth/nope", http.base)).send().await.unwrap().status(), 404);
    let res = http.client.get(format!("{}/api/auth/oauth/mock/callback?code=x&state=y", http.base)).send().await.unwrap();
    assert_eq!(res.status(), 400);

    // Linking by verified email takes over an unverified password account (and removes its password).
    http.sign_up("lee@example.com", "attacker pass").await;
    let profile = json!({ "sub": "u-3", "email": "lee@example.com", "email_verified": "true" });
    let linked = fragment(&oauth_flow(&http, &mock, profile, "http://app.test/", true).await);
    assert!(linked.contains_key("ferrix_access_token"), "{linked:?}");
    assert_eq!(http.sign_in("lee@example.com", "attacker pass").await.0, 401);
    // An unverified provider email can't claim an existing account.
    http.sign_up("max@example.com", "correct horse").await;
    let profile = json!({ "sub": "u-4", "email": "max@example.com", "email_verified": false });
    let refused = fragment(&oauth_flow(&http, &mock, profile, "http://app.test/", true).await);
    assert_eq!(refused["ferrix_error"], "email_in_use");
    assert_eq!(mock.lock().unwrap().token_requests, 4, "the replayed callback never reached the provider");
}

// ---------------------------------------------------------------------------------------------------------------------
// Admins
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn admins_setup_token_and_admin_identity() {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let http = Http::new(t.serve().await.unwrap());

    // No admin yet: a setup token exists, and only it can create the first admin.
    let res = http.client.get(format!("{}/api/auth/admin/setup", http.base)).send().await.unwrap();
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "needsSetup": true }));
    let setup = app.auth().create_setup_token(&app).await.unwrap().expect("no admin yet");
    let page = http.client.get(format!("{}/_/setup", http.base)).send().await.unwrap();
    assert!(page.text().await.unwrap().contains("/api/auth/admin/setup"));
    let (s, b) = http.post("/api/auth/admin/setup", json!({ "token": "wrong", "email": "root@example.com", "password": "admin password" })).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_setup_token"));
    let (s, b) = http.post("/api/auth/admin/setup", json!({ "token": setup, "email": "Root@example.com", "password": "admin password" })).await;
    assert_eq!(s, 200, "{b}");
    assert!(b["adminId"].is_string() && b.get("userId").is_none());
    let (s, _) = http.post("/api/auth/admin/setup", json!({ "token": setup, "email": "x@example.com", "password": "admin password" })).await;
    assert_eq!(s, 401, "the setup token works once");
    assert!(app.auth().create_setup_token(&app).await.unwrap().is_none());

    // Admin sign-in issues tokens with the admin claim; functions see is_admin().
    app.auth().create_admin(&app, "ops@example.com", "ops password".into()).await.unwrap();
    assert!(app.auth().create_admin(&app, "ops@example.com", "ops password".into()).await.is_err());
    let (s, b) = http.post("/api/auth/admin/signin", json!({ "email": "ops@example.com", "password": "wrong password" })).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_credentials"));
    let (s, admin) = http.post("/api/auth/admin/signin", json!({ "email": "ops@example.com", "password": "ops password" })).await;
    assert_eq!(s, 200);
    let admin_access = token(&admin, "accessToken");
    let (s, b) = http.call("query", &app::admin_only::path(), json!({}), Some(&admin_access)).await;
    assert_eq!((s, &b["value"]), (200, &admin["adminId"]));
    // Admins are not users.
    assert_eq!(http.call("query", &app::whoami::path(), json!({}), Some(&admin_access)).await.1["value"], Value::Null);
    // Admin refresh works like user refresh.
    let (s, refreshed) = http.post("/api/auth/refresh", json!({ "refreshToken": token(&admin, "refreshToken") })).await;
    assert_eq!((s, &refreshed["adminId"]), (200, &admin["adminId"]));

    // Users and anonymous callers are refused.
    let user = http.sign_up("user@example.com", "user password").await;
    let (s, b) = http.call("query", &app::admin_only::path(), json!({}), Some(&token(&user, "accessToken"))).await;
    assert_eq!((s, code_of(&b)), (400, "forbidden"));
    let (_, b) = http.call("query", &app::admin_only::path(), json!({}), None).await;
    assert_eq!(code_of(&b), "unauthenticated");
    // A user can't use the admin sign-in.
    let (s, _) = http.post("/api/auth/admin/signin", json!({ "email": "user@example.com", "password": "user password" })).await;
    assert_eq!(s, 401);

    // reset-password (the CLI path) ends the admin's sessions.
    app.auth().reset_admin_password(&app, "ops@example.com", "new ops password".into()).await.unwrap();
    assert_eq!(http.call("query", &app::admin_only::path(), json!({}), Some(&token(&refreshed, "accessToken"))).await.0, 401);
    let (s, _) = http.post("/api/auth/admin/signin", json!({ "email": "ops@example.com", "password": "new ops password" })).await;
    assert_eq!(s, 200);
    assert!(app.auth().reset_admin_password(&app, "nobody@example.com", "whatever pw".into()).await.is_err());
}

// ---------------------------------------------------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn auth_endpoints_are_rate_limited() {
    let limits = RateLimits { credential_burst: 3, credential_per_minute: 1, ..Default::default() };
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(Some(limits))).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    for _ in 0..3 {
        assert_eq!(http.sign_in("a@example.com", "whatever pw").await.0, 401);
    }
    let res = http
        .client
        .post(format!("{}/api/auth/signin", http.base))
        .json(&json!({ "email": "a@example.com", "password": "whatever pw" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers()["retry-after"].to_str().unwrap().parse::<u64>().unwrap() >= 1);
    let body: Value = res.json().await.unwrap();
    assert_eq!(code_of(&body), "rate_limited");
    // Other endpoint groups have their own buckets.
    assert_eq!(http.post("/api/auth/refresh", json!({ "refreshToken": "x" })).await.0, 401);
}

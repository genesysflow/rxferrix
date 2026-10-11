//! Live revocation: sockets (and the admin log stream) authenticated with a session react the moment the session
//! ends (sign-out, `revoke_session`, `delete_user`, refresh-token reuse), not when the access token expires.

use std::time::{Duration, Instant};

use ferrix::FunctionRef;
use ferrix::auth::AuthConfig;
use ferrix::testing::TestApp;
use serde_json::{Value, json};

use crate::authkit::{Http, Sse, Ws, token};

mod app {
    use ferrix::auth::User;
    use ferrix::prelude::*;

    #[table(name = "revocation_notes")]
    #[index(by_owner = [owner])]
    pub struct RevocationNote {
        pub owner: Id<User>,
        pub text: String,
    }

    /// The caller's notes (identity-dependent).
    #[query]
    pub async fn my_notes(ctx: &QueryCtx) -> Result<Vec<String>> {
        let Some(me) = ctx.auth.user_id() else { return Ok(Vec::new()) };
        let notes = ctx
            .db
            .query::<RevocationNote>()
            .with_index(RevocationNote::by_owner, |q| q.eq(RevocationNote::owner, me))
            .collect()
            .await?;
        Ok(notes.into_iter().map(|n| n.value.text).collect())
    }

    #[mutation]
    pub async fn add_note(ctx: &MutationCtx, text: String) -> Result<()> {
        let owner = ctx.auth.require_user()?;
        ctx.db.insert(RevocationNote { owner, text }).await?;
        Ok(())
    }
}

const BUDGET: Duration = Duration::from_millis(100);

async fn setup() -> (TestApp, Http, std::net::SocketAddr) {
    let t =
        TestApp::with_auth(AuthConfig::password().rate_limits(None).refresh_reuse_grace(Duration::ZERO)).await.unwrap();
    let addr = t.serve().await.unwrap();
    (t, Http::new(addr), addr)
}

/// A socket authenticated with `access`, subscribed to `my_notes`.
async fn watching_socket(addr: std::net::SocketAddr, access: &str) -> (Ws, u64) {
    let mut ws = Ws::connect(addr).await;
    ws.authenticate(Some(access)).await;
    let (qid, _) = ws.subscribe(&app::my_notes::path(), json!({})).await;
    (ws, qid)
}

/// The socket must get `AuthError("session has ended")` within the budget (measured from `since`), then close
/// without sending any further query result.
async fn expect_revoked(ws: &mut Ws, since: Instant) {
    let msg = loop {
        let m = ws.recv_within(Duration::from_secs(5)).await.expect("no AuthError");
        if m["type"] != "Transition" {
            break m;
        }
    };
    let elapsed = since.elapsed();
    assert_eq!(msg["type"], "AuthError", "{msg}");
    assert_eq!(msg["error"], "session has ended");
    assert!(elapsed < BUDGET, "AuthError took {elapsed:?}");
    let after = ws.drain_until_closed().await;
    assert!(after.iter().all(|m| m["type"] != "Transition"), "no results after AuthError: {after:?}");
}

#[tokio::test]
async fn sign_out_ends_the_sockets_of_that_session_only() {
    let (t, http, addr) = setup().await;
    let a = http.sign_up("ann@example.com", "correct horse").await;
    let b = http.sign_in("ann@example.com", "correct horse").await;
    let (mut ws_a, qa) = watching_socket(addr, &token(&a, "accessToken")).await;
    let (mut ws_a2, _) = watching_socket(addr, &token(&a, "accessToken")).await;
    let (mut ws_b, qb) = watching_socket(addr, &token(&b, "accessToken")).await;
    assert_eq!(t.app().auth().revocations().watched(), 2, "two sessions watched");

    let (s, _) = http.post("/api/auth/signout", None, json!({ "refreshToken": token(&a, "refreshToken") })).await;
    let signed_out = Instant::now();
    assert_eq!(s, 200);
    expect_revoked(&mut ws_a, signed_out).await;
    expect_revoked(&mut ws_a2, signed_out).await;

    // The other session's socket keeps its identity and its live results.
    http.ok("mutation", &app::add_note::path(), json!({ "text": "still here" }), &token(&b, "accessToken")).await;
    assert_eq!(ws_b.next_value(qb).await, json!(["still here"]));
    let _ = qa;
    drop((ws_a, ws_a2));
    // Sockets that ended unregister their watches.
    for _ in 0..100 {
        if t.app().auth().revocations().watched() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(t.app().auth().revocations().watched(), 1);
}

#[tokio::test]
async fn sign_out_with_the_access_token_and_refresh_token_reuse() {
    let (_t, http, addr) = setup().await;
    let a = http.sign_up("bo@example.com", "correct horse").await;
    let (mut ws, _) = watching_socket(addr, &token(&a, "accessToken")).await;
    let (s, _) = http.post("/api/auth/signout", Some(&token(&a, "accessToken")), json!({})).await;
    let at = Instant::now();
    assert_eq!(s, 200);
    expect_revoked(&mut ws, at).await;

    // Reuse detection: presenting a rotated refresh token revokes the session, and its sockets with it.
    let b = http.sign_in("bo@example.com", "correct horse").await;
    let (_, rotated) = http.post("/api/auth/refresh", None, json!({ "refreshToken": token(&b, "refreshToken") })).await;
    let (mut ws, _) = watching_socket(addr, &token(&rotated, "accessToken")).await;
    let (s, body) = http.post("/api/auth/refresh", None, json!({ "refreshToken": token(&b, "refreshToken") })).await;
    let at = Instant::now();
    assert_eq!((s, body["errorData"]["code"].clone()), (401, json!("refresh_token_reused")));
    expect_revoked(&mut ws, at).await;
}

#[tokio::test]
async fn refreshing_keeps_the_socket() {
    let (_t, http, addr) = setup().await;
    let a = http.sign_up("cy@example.com", "correct horse").await;
    let (mut ws, qid) = watching_socket(addr, &token(&a, "accessToken")).await;
    // A refresh rewrites the session document; the watch must not fire.
    let (s, r) = http.post("/api/auth/refresh", None, json!({ "refreshToken": token(&a, "refreshToken") })).await;
    assert_eq!(s, 200, "{r}");
    assert!(ws.recv_within(Duration::from_millis(150)).await.is_none(), "nothing happens on refresh");
    http.ok("mutation", &app::add_note::path(), json!({ "text": "n" }), &token(&r, "accessToken")).await;
    assert_eq!(ws.next_value(qid).await, json!(["n"]));
}

#[tokio::test]
async fn dashboard_revoke_and_delete_user_end_sockets_and_admin_sign_out_ends_admin_connections() {
    let (t, http, addr) = setup().await;
    let app = t.app().clone();
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let admin = http.admin_sign_in("admin@example.com", "admin password").await;
    let admin_token = token(&admin, "accessToken");
    let user = http.sign_up("dee@example.com", "correct horse").await;
    let second = http.sign_in("dee@example.com", "correct horse").await;
    let user_id = token(&user, "userId");

    // revoke_session from the dashboard.
    let (mut ws, _) = watching_socket(addr, &token(&user, "accessToken")).await;
    let sessions = http.ok("query", "_system/dashboard:user_sessions", json!({ "user": user_id }), &admin_token).await;
    let sid = sessions
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["_id"].as_str().unwrap().to_string())
        .find(|sid| {
            // The first sign-up's session: its access token's `sid`.
            let claims = token(&user, "accessToken");
            let payload = claims.split('.').nth(1).unwrap().to_string();
            let decoded = data_encoding::BASE64URL_NOPAD.decode(payload.as_bytes()).unwrap();
            serde_json::from_slice::<Value>(&decoded).unwrap()["sid"] == sid.as_str()
        })
        .unwrap();
    http.ok("mutation", "_system/dashboard:revoke_session", json!({ "session": sid }), &admin_token).await;
    expect_revoked(&mut ws, Instant::now()).await;

    // delete_user ends every remaining session of the user.
    let (mut ws, _) = watching_socket(addr, &token(&second, "accessToken")).await;
    http.ok("mutation", "_system/dashboard:delete_user", json!({ "user": user_id }), &admin_token).await;
    expect_revoked(&mut ws, Instant::now()).await;

    // The admin's own socket and SSE log stream end when the admin signs out.
    let mut admin_ws = Ws::connect(addr).await;
    admin_ws.authenticate(Some(&admin_token)).await;
    let res =
        http.client.get(format!("{}/api/admin/logs/stream", http.base)).bearer_auth(&admin_token).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let mut sse = Sse::new(res);
    assert_eq!(sse.next().await.unwrap().0, "ready");
    let (s, _) = http.post("/api/auth/signout", None, json!({ "refreshToken": token(&admin, "refreshToken") })).await;
    let at = Instant::now();
    assert_eq!(s, 200);
    expect_revoked(&mut admin_ws, at).await;
    loop {
        let (event, data) = sse.next().await.expect("stream ended without `revoked`");
        if event == "revoked" {
            assert_eq!(data, "session has ended");
            assert!(at.elapsed() < BUDGET * 3, "revoked after {:?}", at.elapsed());
            break;
        }
    }
    assert!(sse.next().await.is_none(), "the stream ends after `revoked`");
}

#[tokio::test]
async fn revoked_between_check_and_watch_is_not_missed() {
    // Many sign-in / authenticate / sign-out races: every socket must end, none may keep its identity.
    let (_t, http, addr) = setup().await;
    http.sign_up("eve@example.com", "correct horse").await;
    for _ in 0..10 {
        let s = http.sign_in("eve@example.com", "correct horse").await;
        let access = token(&s, "accessToken");
        let mut ws = Ws::connect(addr).await;
        let signout = http.post("/api/auth/signout", None, json!({ "refreshToken": token(&s, "refreshToken") }));
        let auth = ws.try_authenticate(&access);
        let (_, first) = tokio::join!(signout, auth);
        if first["type"] == "AuthError" {
            continue;
        }
        let rest = ws.drain_until_closed().await;
        assert!(rest.iter().any(|m| m["type"] == "AuthError"), "socket kept a signed-out session: {rest:?}");
    }
}

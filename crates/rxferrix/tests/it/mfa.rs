//! Two-factor authentication end to end over HTTP: enrolling a TOTP factor (password required, first code confirms,
//! recovery codes shown once), sign-ins that stop at a second-factor token until a code is presented, used codes
//! and lockout, recovery codes, disabling, and what administrators see and can reset. For users and for admins.

use std::net::SocketAddr;
use std::sync::Arc;

use data_encoding::BASE32_NOPAD;
use rxferrix::auth::totp::{code_at, step_at};
use rxferrix::auth::{AuthConfig, MemoryMailer};
use rxferrix::testing::TestApp;
use serde_json::{Value, json};

struct Http {
    client: reqwest::Client,
    base: String,
}

impl Http {
    fn new(addr: SocketAddr) -> Http {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Http { client, base: format!("http://{addr}") }
    }

    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self.client.post(format!("{}{path}", self.base)).json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        let mut req = self.client.get(format!("{}{path}", self.base));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
    }

    async fn call(&self, kind: &str, path: &str, args: Value, token: Option<&str>) -> (u16, Value) {
        self.post(&format!("/api/{kind}"), token, json!({ "path": path, "args": args })).await
    }

    async fn ok(&self, kind: &str, path: &str, args: Value, token: &str) -> Value {
        let (status, body) = self.call(kind, path, args, Some(token)).await;
        assert_eq!(status, 200, "{path}: {body}");
        body["value"].clone()
    }

    /// Enroll the account of `token` (password `password`) and return its secret and recovery codes.
    async fn enroll(&self, token: &str, password: &str) -> (Vec<u8>, Vec<String>) {
        let (s, setup) = self.post("/api/auth/mfa/totp/setup", Some(token), json!({ "password": password })).await;
        assert_eq!(s, 200, "{setup}");
        let secret = BASE32_NOPAD.decode(setup["secret"].as_str().unwrap().as_bytes()).unwrap();
        let (s, confirmed) =
            self.post("/api/auth/mfa/totp/confirm", Some(token), json!({ "code": code_now(&secret, 0) })).await;
        assert_eq!(s, 200, "{confirmed}");
        let codes = confirmed["recoveryCodes"].as_array().unwrap().iter().map(str).collect();
        (secret, codes)
    }
}

fn str(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

fn code_of(v: &Value) -> &str {
    v["errorData"]["code"].as_str().unwrap_or("")
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// The code of the current step plus `offset` (codes within one step of now are accepted; a used step is not).
fn code_now(secret: &[u8], offset: i64) -> String {
    code_at(secret, (step_at(now_s()) as i64 + offset) as u64)
}

async fn setup() -> (TestApp, Http, Arc<MemoryMailer>) {
    let mailer = MemoryMailer::new();
    let config = AuthConfig::password()
        .magic_link()
        .mailer(mailer.clone())
        .rate_limits(None)
        .redirect_url("http://app.test")
        .totp_issuer("Test App");
    let t = TestApp::with_auth(config).await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    (t, http, mailer)
}

async fn admin_token(t: &TestApp, http: &Http) -> String {
    let app = t.app().clone();
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let (s, body) = http
        .post("/api/auth/admin/signin", None, json!({ "email": "admin@example.com", "password": "admin password" }))
        .await;
    assert_eq!(s, 200, "{body}");
    str(&body["accessToken"])
}

#[tokio::test]
async fn user_enrolls_and_signs_in_with_a_code_or_a_recovery_code() {
    let (_t, http, _) = setup().await;
    let (s, signed_up) =
        http.post("/api/auth/signup", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    assert_eq!(s, 200, "{signed_up}");
    let token = str(&signed_up["accessToken"]);

    // Nothing yet.
    let (s, status) = http.get("/api/auth/mfa", Some(&token)).await;
    assert_eq!(
        (s, &status),
        (200, &json!({ "enabled": false, "pending": false, "recoveryCodesLeft": 0, "methods": [] }))
    );
    assert_eq!(http.get("/api/auth/mfa", None).await.0, 401);

    // Setup needs the password (a stolen access token must not be enough), then returns the secret three ways.
    let (s, b) = http.post("/api/auth/mfa/totp/setup", Some(&token), json!({})).await;
    assert_eq!((s, code_of(&b)), (400, "password_required"));
    let (s, b) = http.post("/api/auth/mfa/totp/setup", Some(&token), json!({ "password": "wrong" })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_current_password"));
    let (s, setup) = http.post("/api/auth/mfa/totp/setup", Some(&token), json!({ "password": "correct horse" })).await;
    assert_eq!(s, 200, "{setup}");
    let secret_b32 = str(&setup["secret"]);
    let uri = str(&setup["uri"]);
    assert!(uri.starts_with("otpauth://totp/Test%20App:u%40example.com?secret="), "{uri}");
    assert!(uri.contains(&format!("secret={secret_b32}&issuer=Test%20App")), "{uri}");
    assert!(str(&setup["qrSvg"]).starts_with("<svg"));
    let secret = BASE32_NOPAD.decode(secret_b32.as_bytes()).unwrap();
    let (_, status) = http.get("/api/auth/mfa", Some(&token)).await;
    assert_eq!((&status["enabled"], &status["pending"]), (&json!(false), &json!(true)));

    // Not confirmed yet: sign-ins still go straight through.
    let (s, b) =
        http.post("/api/auth/signin", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    assert_eq!(s, 200);
    assert!(b["accessToken"].is_string(), "{b}");

    // A wrong first code doesn't confirm; the right one does and hands out the recovery codes.
    let (s, b) = http.post("/api/auth/mfa/totp/confirm", Some(&token), json!({ "code": "000000" })).await;
    assert!(s == 400 && code_of(&b) == "invalid_mfa_code" || code_now(&secret, 0) == "000000", "{s} {b}");
    let (s, confirmed) =
        http.post("/api/auth/mfa/totp/confirm", Some(&token), json!({ "code": code_now(&secret, 0) })).await;
    assert_eq!(s, 200, "{confirmed}");
    let recovery: Vec<String> = confirmed["recoveryCodes"].as_array().unwrap().iter().map(str).collect();
    assert_eq!(recovery.len(), 8);
    assert!(recovery.iter().all(|c| c.len() == 11 && c.as_bytes()[5] == b'-'), "{recovery:?}");
    let (_, status) = http.get("/api/auth/mfa", Some(&token)).await;
    assert_eq!(status, json!({ "enabled": true, "pending": false, "recoveryCodesLeft": 8, "methods": ["totp"] }));
    let (s, b) = http.post("/api/auth/mfa/totp/setup", Some(&token), json!({ "password": "correct horse" })).await;
    assert_eq!((s, code_of(&b)), (400, "mfa_already_enabled"));

    // Signing in now stops at a second-factor token, which is no access token.
    let (s, b) =
        http.post("/api/auth/signin", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!((&b["mfaRequired"], &b["methods"]), (&json!(true), &json!(["totp"])));
    assert!(b["accessToken"].is_null() && b["refreshToken"].is_null(), "{b}");
    let mfa_token = str(&b["mfaToken"]);
    let (s, b) = http.call("query", "_system/auth:me", json!({}), Some(&mfa_token)).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_token"));

    // The code that confirmed the factor was used; the next step's code works (within the accepted window).
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token, "code": "123456" })).await;
    assert!(s == 400 && code_of(&b) == "invalid_mfa_code" || code_now(&secret, 1) == "123456", "{s} {b}");
    let (s, b) =
        http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token, "code": code_now(&secret, 0) })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_mfa_code"), "a code works once: {b}");
    let next = code_now(&secret, 1);
    let (s, signed_in) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token, "code": next })).await;
    assert_eq!(s, 200, "{signed_in}");
    let session_token = str(&signed_in["accessToken"]);
    assert_eq!(signed_in["userId"], signed_up["userId"]);
    let (s, me) = http.call("query", "_system/auth:me", json!({}), Some(&session_token)).await;
    assert_eq!((s, &me["value"]["email"]), (200, &json!("u@example.com")));
    // The same code again, with a fresh first step: refused.
    let (_, b) =
        http.post("/api/auth/signin", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    let mfa_token2 = str(&b["mfaToken"]);
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token2, "code": next })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_mfa_code"));
    // A garbage token is refused outright.
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": "nope", "code": next })).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_mfa_token"));

    // A recovery code signs in too (spaces, dashes and case don't matter) and is consumed.
    let messy = recovery[0].replace('-', " ").to_uppercase();
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token2, "code": messy })).await;
    assert_eq!(s, 200, "{b}");
    let (_, status) = http.get("/api/auth/mfa", Some(&token)).await;
    assert_eq!(status["recoveryCodesLeft"], 7);
    let (_, b) =
        http.post("/api/auth/signin", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    let mfa_token3 = str(&b["mfaToken"]);
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token3, "code": recovery[0] })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_mfa_code"), "used: {b}");

    // Regenerating the codes needs a code; the old ones stop working.
    let (s, b) = http.post("/api/auth/mfa/recovery-codes", Some(&token), json!({ "code": "000000" })).await;
    assert!(s == 400 || code_now(&secret, 0) == "000000", "{b}");
    let (s, b) = http.post("/api/auth/mfa/recovery-codes", Some(&token), json!({ "code": recovery[1] })).await;
    assert_eq!(s, 200, "{b}");
    let fresh: Vec<String> = b["recoveryCodes"].as_array().unwrap().iter().map(str).collect();
    assert_eq!(fresh.len(), 8);
    assert!(fresh.iter().all(|c| !recovery.contains(c)));
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token3, "code": recovery[2] })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_mfa_code"), "old recovery codes are gone: {b}");
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token3, "code": fresh[0] })).await;
    assert_eq!(s, 200, "{b}");

    // Disabling needs a code; afterwards sign-ins go straight through again and setup can start over.
    let (s, b) = http.post("/api/auth/mfa/disable", Some(&token), json!({})).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_mfa_code"));
    let (s, b) = http.post("/api/auth/mfa/disable", Some(&token), json!({ "code": fresh[1] })).await;
    assert_eq!(s, 200, "{b}");
    let (_, status) = http.get("/api/auth/mfa", Some(&token)).await;
    assert_eq!(status["enabled"], false);
    let (s, b) = http.post("/api/auth/mfa/disable", Some(&token), json!({ "code": fresh[2] })).await;
    assert_eq!((s, code_of(&b)), (400, "mfa_not_enabled"));
    let (s, b) =
        http.post("/api/auth/signin", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    assert_eq!(s, 200);
    assert!(b["accessToken"].is_string(), "{b}");
    // A second-factor token issued before the factor was removed can't be completed.
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token3, "code": fresh[3] })).await;
    assert_eq!((s, code_of(&b)), (401, "invalid_mfa_token"));
    // A pending setup is dropped without a code.
    http.post("/api/auth/mfa/totp/setup", Some(&token), json!({ "password": "correct horse" })).await;
    let (s, _) = http.post("/api/auth/mfa/disable", Some(&token), json!({})).await;
    assert_eq!(s, 200);
    let (_, status) = http.get("/api/auth/mfa", Some(&token)).await;
    assert_eq!((&status["enabled"], &status["pending"]), (&json!(false), &json!(false)));
}

#[tokio::test]
async fn wrong_codes_lock_the_factor() {
    let (_t, http, _) = setup().await;
    let (_, signed_up) =
        http.post("/api/auth/signup", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    let token = str(&signed_up["accessToken"]);
    let (secret, recovery) = http.enroll(&token, "correct horse").await;
    let (_, b) =
        http.post("/api/auth/signin", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    let mfa_token = str(&b["mfaToken"]);
    let wrong = |i: u32| format!("{:06}", (900_000 + i) % 1_000_000);
    for i in 0..5 {
        let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token, "code": wrong(i) })).await;
        assert!(s == 400 || code_now(&secret, 0) == wrong(i) || code_now(&secret, 1) == wrong(i), "#{i}: {s} {b}");
    }
    // Locked: even the right codes are refused for a while, with Retry-After.
    let res = http
        .client
        .post(format!("{}/api/auth/mfa/verify", http.base))
        .json(&json!({ "mfaToken": mfa_token, "code": code_now(&secret, 1) }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 429);
    assert!(res.headers().get("retry-after").is_some());
    let b: Value = res.json().await.unwrap();
    assert_eq!(code_of(&b), "mfa_locked");
    let (s, b) = http.post("/api/auth/mfa/verify", None, json!({ "mfaToken": mfa_token, "code": recovery[0] })).await;
    assert_eq!((s, code_of(&b)), (429, "mfa_locked"), "{b}");
    // Managing the factor is locked the same way (same code check).
    let (s, b) = http.post("/api/auth/mfa/disable", Some(&token), json!({ "code": recovery[0] })).await;
    assert_eq!((s, code_of(&b)), (429, "mfa_locked"), "{b}");
}

#[tokio::test]
async fn every_sign_in_method_goes_through_the_second_factor() {
    let (t, http, mailer) = setup().await;
    let (_, signed_up) =
        http.post("/api/auth/signup", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    let token = str(&signed_up["accessToken"]);
    let (secret, _) = http.enroll(&token, "correct horse").await;

    // A password reset proves the email, not the second factor.
    let (s, _) = http.post("/api/auth/reset/request", None, json!({ "email": "u@example.com" })).await;
    assert_eq!(s, 200);
    let email = wait_for_email(&mailer, "u@example.com", 2).await; // #1 is the sign-up's verification
    let reset_token = fragment_token(&email.body, "rxferrix_reset_token");
    let (s, b) =
        http.post("/api/auth/reset/confirm", None, json!({ "token": reset_token, "password": "battery staple" })).await;
    assert_eq!((s, &b["mfaRequired"]), (200, &json!(true)), "{b}");
    let (s, b) = http
        .post("/api/auth/mfa/verify", None, json!({ "mfaToken": b["mfaToken"], "code": code_now(&secret, 1) }))
        .await;
    assert_eq!(s, 200, "{b}");
    // The reset ended the other sessions as usual.
    let (s, _) = http.call("query", "_system/auth:me", json!({}), Some(&token)).await;
    assert_eq!(s, 401);

    // A magic link too.
    let (s, _) = http.post("/api/auth/magic/request", None, json!({ "email": "u@example.com" })).await;
    assert_eq!(s, 200);
    let email = wait_for_email(&mailer, "u@example.com", 3).await;
    let magic = fragment_token(&email.body, "rxferrix_magic_token");
    let (s, b) = http.post("/api/auth/magic/verify", None, json!({ "token": magic })).await;
    assert_eq!((s, &b["mfaRequired"]), (200, &json!(true)), "{b}");
    assert!(b["accessToken"].is_null());

    // `sign_in_as` and impersonation bypass it (the caller is the server or an audited administrator), and an
    // impersonation session can't touch the factor.
    let user: rxferrix::Id<rxferrix::auth::User> = str(&signed_up["userId"]).parse().unwrap();
    let app = t.app();
    let as_user = app.auth().sign_in_as(app, user).await.unwrap();
    let (s, _) = http.call("query", "_system/auth:me", json!({}), Some(&as_user.access_token)).await;
    assert_eq!(s, 200);
    let admin = admin_token(&t, &http).await;
    let (s, imp) = http.post("/api/admin/impersonate", Some(&admin), json!({ "user": user.to_string() })).await;
    assert_eq!(s, 200, "{imp}");
    let imp_token = str(&imp["accessToken"]);
    let (s, b) = http.post("/api/auth/mfa/disable", Some(&imp_token), json!({ "code": code_now(&secret, 1) })).await;
    assert_eq!((s, code_of(&b)), (403, "forbidden"), "{b}");
    let (s, b) = http.post("/api/auth/mfa/totp/setup", Some(&imp_token), json!({ "password": "battery staple" })).await;
    assert_eq!((s, code_of(&b)), (403, "forbidden"), "{b}");
}

#[tokio::test]
async fn admins_enroll_too_and_the_dashboard_shows_and_resets_factors() {
    let (t, http, _) = setup().await;
    let admin = admin_token(&t, &http).await;
    let (_, signed_up) =
        http.post("/api/auth/signup", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    let user_token = str(&signed_up["accessToken"]);
    let user_id = str(&signed_up["userId"]);

    // The admin's own factor, through the same endpoints.
    let me = http.ok("query", "_system/dashboard:me", json!({}), &admin).await;
    assert_eq!(me["mfaEnabled"], false);
    let (admin_secret, admin_recovery) = http.enroll(&admin, "admin password").await;
    let me = http.ok("query", "_system/dashboard:me", json!({}), &admin).await;
    assert_eq!(me["mfaEnabled"], true);
    let (s, b) = http
        .post("/api/auth/admin/signin", None, json!({ "email": "admin@example.com", "password": "admin password" }))
        .await;
    assert_eq!((s, &b["mfaRequired"]), (200, &json!(true)), "{b}");
    let (s, b) = http
        .post("/api/auth/mfa/verify", None, json!({ "mfaToken": b["mfaToken"], "code": code_now(&admin_secret, 1) }))
        .await;
    assert_eq!(s, 200, "{b}");
    assert!(b["adminId"].is_string(), "{b}");
    let admin2 = str(&b["accessToken"]);
    http.ok("query", "_system/dashboard:tables", json!({}), &admin2).await;
    // Enabling and disabling an admin's factor is audited.
    let log = http
        .ok(
            "query",
            "_system/audit:list",
            json!({ "action": "mfa_enable", "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await;
    assert_eq!(log["page"].as_array().unwrap().len(), 1, "{log}");
    assert_eq!(log["page"][0]["targetTable"], "_admins");

    // The Users page sees who has a factor; the factor row itself is masked in the data browser.
    let (user_secret, _) = http.enroll(&user_token, "correct horse").await;
    let users = http
        .ok("query", "_system/dashboard:users", json!({ "paginationOpts": { "numItems": 10, "cursor": null } }), &admin)
        .await;
    assert_eq!((&users["page"][0]["email"], &users["page"][0]["mfaEnabled"]), (&json!("u@example.com"), &json!(true)));
    let rows = http
        .ok(
            "query",
            "_system/dashboard:documents",
            json!({ "table": "_mfa_factors", "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await;
    let rows = rows["page"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for r in rows {
        assert_eq!((&r["secretEncrypted"], &r["recoveryCodeHashes"]), (&json!("••••"), &json!("••••")), "{r}");
        assert_eq!(r["confirmed"], true);
    }
    let (s, b) = http
        .call("mutation", "_system/dashboard:delete_documents", json!({ "ids": [rows[0]["_id"]] }), Some(&admin))
        .await;
    assert_eq!((s, code_of(&b)), (400, "read_only_table"));

    // An administrator resets a user's factor (audited); the user then signs in with the password alone.
    let (s, b) = http.call("mutation", "_system/dashboard:reset_mfa", json!({ "user": "nope" }), Some(&admin)).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_id"), "{b}");
    http.ok("mutation", "_system/dashboard:reset_mfa", json!({ "user": user_id }), &admin).await;
    let (s, b) = http.call("mutation", "_system/dashboard:reset_mfa", json!({ "user": user_id }), Some(&admin)).await;
    assert_eq!((s, code_of(&b)), (400, "mfa_not_enabled"));
    let (s, b) =
        http.post("/api/auth/signin", None, json!({ "email": "u@example.com", "password": "correct horse" })).await;
    assert_eq!(s, 200);
    assert!(b["accessToken"].is_string(), "{b}");
    let log = http
        .ok(
            "query",
            "_system/audit:list",
            json!({ "action": "reset_mfa", "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await;
    assert_eq!(log["page"][0]["targetId"], user_id);
    let _ = user_secret;

    // Users can't reset anything; deleting a user removes their factor.
    let (s, _) =
        http.call("mutation", "_system/dashboard:reset_mfa", json!({ "user": user_id }), Some(&user_token)).await;
    assert_eq!(s, 400);
    http.enroll(&user_token, "correct horse").await;
    let deleted = http.ok("mutation", "_system/dashboard:delete_user", json!({ "user": user_id }), &admin).await;
    assert_eq!(deleted["mfaFactors"], 1, "{deleted}");
    let rows = http
        .ok(
            "query",
            "_system/dashboard:documents",
            json!({ "table": "_mfa_factors", "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await;
    assert_eq!(rows["page"].as_array().unwrap().len(), 1, "only the admin's: {rows}");

    // `admin disable-2fa` for an administrator who lost everything.
    let app = t.app();
    assert!(!app.auth().disable_admin_mfa(app, "nobody@example.com").await.unwrap());
    assert!(app.auth().disable_admin_mfa(app, "Admin@Example.com").await.unwrap());
    let (s, b) = http
        .post("/api/auth/admin/signin", None, json!({ "email": "admin@example.com", "password": "admin password" }))
        .await;
    assert_eq!(s, 200);
    assert!(b["accessToken"].is_string(), "{b}");
    let _ = admin_recovery;
    assert_eq!(app.db().verify_indexes().await.unwrap(), Vec::<String>::new());
}

/// The `count`-th email sent to `to`.
async fn wait_for_email(mailer: &MemoryMailer, to: &str, count: usize) -> rxferrix::auth::Email {
    for _ in 0..200 {
        let mine: Vec<_> = mailer.emails().into_iter().filter(|e| e.to == to).collect();
        if mine.len() >= count {
            return mine[count - 1].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no email #{count} to {to}");
}

fn fragment_token(body: &str, key: &str) -> String {
    let marker = format!("#{key}=");
    let at = body.find(&marker).unwrap_or_else(|| panic!("no {key} in {body}")) + marker.len();
    body[at..].split_whitespace().next().unwrap().to_string()
}

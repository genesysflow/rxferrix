//! Runtime settings (`_settings`, `_system/settings:*`): admin-only, validated, secrets encrypted at rest and never
//! returned (nor logged), applied without a restart, config wins over settings, audited, CORS, test emails.

use std::time::Duration;

use rxferrix::FunctionRef;
use rxferrix::auth::{AuthConfig, MemoryMailer};
use rxferrix::testing::TestApp;
use serde_json::{Value, json};

use crate::authkit::{Http, code_of, token};

const MASK: &str = "••••";
const SECRET: &str = "gh-client-secret-0123456789";

async fn setup(config: AuthConfig) -> (TestApp, Http, String) {
    let t = TestApp::with_auth(config.rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let http = Http::new(t.serve().await.unwrap());
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let admin = token(&http.admin_sign_in("admin@example.com", "admin password").await, "accessToken");
    (t, http, admin)
}

async fn update(http: &Http, admin: &str, args: Value) -> (u16, Value) {
    http.call("action", "_system/settings:update", args, Some(admin)).await
}

fn github(enabled: bool, secret: Option<&str>) -> Value {
    json!({ "id": "gh", "kind": "github", "clientId": "gh-client-id", "clientSecret": secret, "enabled": enabled })
}

#[tokio::test]
async fn settings_are_admin_only_validated_and_applied_immediately() {
    let (_t, http, admin) = setup(AuthConfig::password()).await;
    let user = token(&http.sign_up("u@example.com", "correct horse").await, "accessToken");
    for (name, args) in
        [("get", json!({})), ("update", json!({ "signups": false })), ("send_test_email", json!({ "to": "a@b.co" }))]
    {
        let path = format!("_system/settings:{name}");
        assert_eq!(code_of(&http.call("action", &path, args.clone(), None).await.1), "unauthenticated");
        assert_eq!(code_of(&http.call("action", &path, args, Some(&user)).await.1), "forbidden");
    }

    let view = http.ok("action", "_system/settings:get", json!({}), &admin).await;
    assert_eq!((&view["oauth"], &view["signups"], &view["smtp"]), (&json!([]), &Value::Null, &Value::Null));
    assert_eq!(view["effective"]["signups"], true);
    assert_eq!(view["effective"]["accessTokenTtlSecs"], 900);

    // Validation: nothing is written when anything is wrong.
    let bad = [
        json!({ "redirectUrls": ["not a url"] }),
        json!({ "accessTokenTtlSecs": 5 }),
        json!({ "sessionTtlSecs": 10 }),
        json!({ "corsOrigins": ["https://app.example.com/path"] }),
        json!({ "oauth": [github(true, None)] }),
        json!({ "oauth": [{ "id": "Bad Id", "kind": "github", "clientId": "x", "clientSecret": "y", "enabled": true }] }),
        json!({ "oauth": [{ "id": "corp", "kind": "oidc", "clientId": "x", "clientSecret": "y", "enabled": true }] }),
        json!({ "oauth": [{ "id": "corp", "kind": "oidc", "issuer": "ftp://idp", "clientId": "x", "clientSecret": "y", "enabled": true }] }),
        json!({ "oauth": [github(true, Some("a")), github(true, Some("b"))] }),
        json!({ "smtp": { "url": "http://mail.example.com", "from": "a@b.co" } }),
        json!({ "smtp": { "url": "smtp://mail.example.com", "from": "not an address" } }),
        json!({ "clear": ["nope"] }),
        json!({ "signups": false, "accessTokenTtlSecs": 1 }),
    ];
    for args in bad {
        let (s, b) = update(&http, &admin, args.clone()).await;
        assert_eq!((s, code_of(&b)), (400, "invalid_settings"), "{args}: {b}");
    }
    assert_eq!(http.ok("action", "_system/settings:get", json!({}), &admin).await["signups"], Value::Null);

    // Sign-ups off, short tokens, a GitHub provider: all in effect at once.
    let view = http
        .ok(
            "action",
            "_system/settings:update",
            json!({ "signups": false, "accessTokenTtlSecs": 60, "oauth": [github(true, Some(SECRET))],
                    "redirectUrls": ["https://app.example.com"] }),
            &admin,
        )
        .await;
    assert_eq!(view["effective"]["signups"], false);
    assert_eq!(view["effective"]["oauthProviders"], json!(["gh"]));
    assert_eq!(view["oauth"][0]["clientSecret"], MASK, "never returned in clear");
    assert!(!view.to_string().contains(SECRET));
    let (s, b) =
        http.post("/api/auth/signup", None, json!({ "email": "new@example.com", "password": "correct horse" })).await;
    assert_eq!((s, code_of(&b)), (403, "signups_disabled"));
    let (_, providers) = http.get("/api/auth/providers", None).await;
    assert_eq!((&providers["signups"], &providers["oauth"]), (&json!(false), &json!(["gh"])));
    assert_eq!(http.sign_in("u@example.com", "correct horse").await["expiresIn"], 60);
    let res = http
        .client
        .get(format!("{}/api/auth/oauth/gh?redirect_to=https://app.example.com/cb", http.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 302);
    let location = res.headers()["location"].to_str().unwrap();
    assert!(
        location.starts_with("https://github.com/login/oauth/authorize") && location.contains("client_id=gh-client-id")
    );

    // Sending the mask (or nothing) back keeps the secret; disabling keeps it too; clear removes everything.
    let view =
        http.ok("action", "_system/settings:update", json!({ "oauth": [github(false, Some(MASK))] }), &admin).await;
    assert_eq!((&view["oauth"][0]["clientSecret"], &view["effective"]["oauthProviders"]), (&json!(MASK), &json!([])));
    let view = http.ok("action", "_system/settings:update", json!({ "oauth": [github(true, None)] }), &admin).await;
    assert_eq!(view["effective"]["oauthProviders"], json!(["gh"]), "the stored secret was kept");
    let view = http
        .ok("action", "_system/settings:update", json!({ "clear": ["oauth", "signups", "accessTokenTtlSecs"] }), &admin)
        .await;
    assert_eq!((&view["oauth"], &view["effective"]["signups"]), (&json!([]), &json!(true)));
    assert_eq!(http.sign_up("new@example.com", "correct horse").await["expiresIn"], 900);
}

#[tokio::test]
async fn secrets_are_encrypted_at_rest_masked_everywhere_and_survive_a_restart() {
    let (t, http, admin) = setup(AuthConfig::password()).await;
    let smtp_password = "smtp-pass-ABCDEFGH";
    http.ok(
        "action",
        "_system/settings:update",
        json!({ "oauth": [github(true, Some(SECRET))],
                "smtp": { "url": format!("smtps://mailer:{smtp_password}@mail.example.com:465"), "from": "App <app@example.com>" } }),
        &admin,
    )
    .await;
    let view = http.ok("action", "_system/settings:get", json!({}), &admin).await;
    assert_eq!(
        view["smtp"],
        json!({ "url": "smtps://mailer@mail.example.com:465", "from": "App <app@example.com>", "password": MASK })
    );
    assert_eq!(view["effective"]["emailConfigured"], true);

    // The data browser masks the sealed values; generic writes to `_settings` are refused.
    let page = json!({ "table": "_settings", "paginationOpts": { "numItems": 20, "cursor": null } });
    let rows = http.ok("query", "_system/dashboard:documents", page, &admin).await;
    let rows = rows["page"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["encrypted"] == MASK), "{rows:?}");
    let (_, b) = http
        .call("mutation", "_system/dashboard:delete_documents", json!({ "ids": [rows[0]["_id"]] }), Some(&admin))
        .await;
    assert_eq!(code_of(&b), "read_only_table");

    // Not in the database file, not in the execution log (arguments of system functions are redacted).
    t.app().logs().flush().await.unwrap();
    for file in ["data.db", "data.db-wal", "aux.db", "aux.db-wal"] {
        let bytes = std::fs::read(t.dir().join(file)).unwrap_or_default();
        for secret in [SECRET, smtp_password] {
            assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} found in {file}");
        }
    }
    let (_, logs) = http.get("/api/admin/logs?pathPrefix=_system/settings:update", Some(&admin)).await;
    let logged = logs["entries"][0]["args"].as_str().unwrap().to_string();
    assert!(logged.contains(MASK) && !logged.contains(SECRET), "{logged}");

    // The audit log has the change, with secrets masked.
    let entries = http
        .ok(
            "query",
            "_system/audit:list",
            json!({ "action": "settings_update", "paginationOpts": { "numItems": 5, "cursor": null } }),
            &admin,
        )
        .await;
    let entry = &entries["page"][0];
    assert_eq!(entry["after"]["oauth"][0]["clientSecret"], MASK);
    assert_eq!(entry["after"]["smtp"]["password"], MASK);
    assert!(!entries.to_string().contains(SECRET));

    // A restart decrypts them again (same auth_secret.key).
    let t = t.restart().await.unwrap();
    let http = Http::new(t.serve().await.unwrap());
    let admin = token(&http.admin_sign_in("admin@example.com", "admin password").await, "accessToken");
    let view = http.ok("action", "_system/settings:get", json!({}), &admin).await;
    assert_eq!(
        (&view["effective"]["oauthProviders"], &view["effective"]["emailConfigured"]),
        (&json!(["gh"]), &json!(true))
    );

    // With another server secret, the stored secrets are unreadable: the provider is off rather than broken.
    let dir = t.dir().to_path_buf();
    drop(http);
    let app = t.app().clone();
    drop(app);
    std::fs::write(dir.join("auth_secret.key"), "ab".repeat(64)).unwrap();
    let t = t.restart().await.unwrap();
    assert_eq!(t.app().auth().oauth_provider_ids(), Vec::<String>::new());
}

#[tokio::test]
async fn configuration_wins_over_settings() {
    let mailer = MemoryMailer::new();
    let config = AuthConfig::password()
        .signups(true)
        .access_token_ttl(Duration::from_secs(120))
        .redirect_url("https://configured.example.com")
        .mailer(mailer.clone())
        .oauth(rxferrix::auth::GitHub::new("cfg-id", "cfg-secret"));
    let (_t, http, admin) = setup(config).await;
    let view = http
        .ok(
            "action",
            "_system/settings:update",
            json!({ "signups": false, "accessTokenTtlSecs": 60, "sessionTtlSecs": 3600,
                    "redirectUrls": ["https://settings.example.com"],
                    "smtp": { "url": "smtp://mail.example.com:25", "from": "a@b.co" },
                    "oauth": [{ "id": "github", "kind": "github", "clientId": "settings-id", "clientSecret": "s", "enabled": true },
                              { "id": "google", "kind": "google", "clientId": "g-id", "clientSecret": "g", "enabled": true }] }),
            &admin,
        )
        .await;
    let locked: Vec<&str> = view["locked"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(locked, ["signups", "accessTokenTtlSecs", "redirectUrls", "smtp", "oauth.github"]);
    let e = &view["effective"];
    assert_eq!(
        (&e["signups"], &e["accessTokenTtlSecs"], &e["sessionTtlSecs"]),
        (&json!(true), &json!(120), &json!(3600))
    );
    assert_eq!(e["redirectUrls"], json!(["https://configured.example.com/"]));
    assert_eq!(e["oauthProviders"], json!(["github", "google"]), "settings add providers with other ids");
    assert_eq!(view["oauth"][0]["locked"], true);
    let res = http.client.get(format!("{}/api/auth/oauth/github", http.base)).send().await.unwrap();
    assert!(
        res.headers()["location"].to_str().unwrap().contains("client_id=cfg-id"),
        "the configured GitHub app is used"
    );

    // The configured mailer stays in use: a test email goes through it.
    http.ok("action", "_system/settings:send_test_email", json!({ "to": "ops@example.com" }), &admin).await;
    assert!(mailer.emails().iter().any(|m| m.to == "ops@example.com" && m.subject.contains("test")));
}

#[tokio::test]
async fn test_emails_report_failures_and_cors_origins_apply() {
    let (_t, http, admin) = setup(AuthConfig::password()).await;
    // Nothing listens on port 9 here: sending fails, with the error (and never the password).
    let (s, b) = http
        .call(
            "action",
            "_system/settings:send_test_email",
            json!({ "to": "ops@example.com", "smtp": { "url": "smtp://127.0.0.1:9", "from": "a@b.co", "password": "pw-XYZ" } }),
            Some(&admin),
        )
        .await;
    assert_eq!((s, code_of(&b)), (400, "email_failed"), "{b}");
    assert!(!b.to_string().contains("pw-XYZ"));

    let origin = |o: &'static str| {
        let http = &http;
        async move {
            let res = http.client.get(format!("{}/api/health", http.base)).header("Origin", o).send().await.unwrap();
            res.headers().get("access-control-allow-origin").map(|v| v.to_str().unwrap().to_string())
        }
    };
    assert!(origin("https://anywhere.example.com").await.is_some(), "any origin by default");
    http.ok("action", "_system/settings:update", json!({ "corsOrigins": ["https://app.example.com"] }), &admin).await;
    assert_eq!(origin("https://app.example.com").await.as_deref(), Some("https://app.example.com"));
    assert_eq!(origin("https://evil.example.com").await, None);
    http.ok("action", "_system/settings:update", json!({ "clear": ["corsOrigins"] }), &admin).await;
    assert!(origin("https://evil.example.com").await.is_some());
}

#[tokio::test]
async fn trusted_proxy_headers_decide_the_client_ip() {
    let (_t, http, admin) = setup(AuthConfig::password()).await;
    let client_ip = |pairs: &'static [(&'static str, &'static str)]| {
        let (http, admin) = (&http, &admin);
        async move {
            let mut req = http.client.get(format!("{}/api/admin/client-ip", http.base)).bearer_auth(admin);
            for (k, v) in pairs {
                req = req.header(*k, *v);
            }
            let res = req.send().await.unwrap();
            assert_eq!(res.status(), 200);
            res.json::<Value>().await.unwrap()
        }
    };
    let proxied: &[(&str, &str)] =
        &[("cf-connecting-ip", "198.51.100.7"), ("x-forwarded-for", "203.0.113.9, 203.0.113.10")];

    // Not behind a proxy by default: the peer address, whatever the headers say.
    let r = client_ip(proxied).await;
    assert_eq!((&r["ip"], &r["header"], &r["peer"]), (&json!("127.0.0.1"), &Value::Null, &json!("127.0.0.1")));
    assert_eq!(r["proxyHeaders"][0], json!({ "name": "cf-connecting-ip", "value": "198.51.100.7" }));
    assert_eq!(r["proxyHeaders"][1], json!({ "name": "x-forwarded-for", "value": "203.0.113.9, 203.0.113.10" }));

    let (s, b) = update(&http, &admin, json!({ "trustedProxy": { "headers": ["X Forwarded"] } })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_settings"), "{b}");

    let view = http
        .ok("action", "_system/settings:update", json!({ "trustedProxy": { "headers": ["CF-Connecting-IP"] } }), &admin)
        .await;
    assert_eq!(view["trustedProxy"], json!({ "headers": ["CF-Connecting-IP"], "useLeftmostIp": false }));
    assert_eq!(view["effective"]["trustedProxy"]["headers"], json!(["cf-connecting-ip"]));
    let r = client_ip(proxied).await;
    assert_eq!((&r["ip"], &r["header"]), (&json!("198.51.100.7"), &json!("cf-connecting-ip")));
    // A request without the header falls back to the peer.
    assert_eq!(client_ip(&[]).await["ip"], "127.0.0.1");

    let both = json!({ "trustedProxy": { "headers": ["Fly-Client-IP", "X-Forwarded-For"], "useLeftmostIp": true } });
    http.ok("action", "_system/settings:update", both, &admin).await;
    let r = client_ip(proxied).await;
    assert_eq!((&r["ip"], &r["header"]), (&json!("203.0.113.9"), &json!("x-forwarded-for")));

    http.ok("action", "_system/settings:update", json!({ "clear": ["trustedProxy"] }), &admin).await;
    assert_eq!(client_ip(proxied).await["ip"], "127.0.0.1");
}

#[tokio::test]
async fn trust_proxy_in_the_configuration_locks_the_trusted_headers() {
    let (_t, http, admin) = setup(AuthConfig::password().trust_proxy(true)).await;
    let view = http.ok("action", "_system/settings:get", json!({}), &admin).await;
    assert!(view["locked"].as_array().unwrap().contains(&json!("trustedProxy")));
    assert_eq!(view["effective"]["trustedProxy"], json!({ "headers": ["x-forwarded-for"], "useLeftmostIp": false }));
    // A stored setting doesn't override it.
    http.ok(
        "action",
        "_system/settings:update",
        json!({ "trustedProxy": { "headers": ["CF-Connecting-IP"] } }),
        &admin,
    )
    .await;
    let r: Value = http
        .client
        .get(format!("{}/api/admin/client-ip", http.base))
        .bearer_auth(&admin)
        .header("cf-connecting-ip", "198.51.100.7")
        .header("x-forwarded-for", "203.0.113.9")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((&r["ip"], &r["header"]), (&json!("203.0.113.9"), &json!("x-forwarded-for")));
}

fn rate_limits(enabled: bool, rules: Value) -> Value {
    json!({ "rateLimits": { "enabled": enabled, "rules": rules } })
}

#[tokio::test]
async fn rate_limits_apply_to_function_calls_over_http_and_websocket() {
    use crate::authkit::Ws;
    use crate::common::todos;
    let (_t, http, admin) = setup(AuthConfig::password()).await;
    let noop = todos::noop::path();

    for rules in [
        json!([{ "label": "/api/*", "maxRequests": 1, "intervalSecs": 1 }]),
        json!([{ "label": "noop", "maxRequests": 1, "intervalSecs": 1 }]),
        json!([{ "label": "_system/settings:update", "maxRequests": 1, "intervalSecs": 1 }]),
        json!([{ "label": "*", "maxRequests": 0, "intervalSecs": 1 }]),
        json!([{ "label": "*", "maxRequests": 1, "intervalSecs": 0 }]),
        json!([{ "label": "*", "maxRequests": 1, "intervalSecs": 1 }, { "label": "*", "maxRequests": 2, "intervalSecs": 1 }]),
    ] {
        let (s, b) = update(&http, &admin, rate_limits(true, rules.clone())).await;
        assert_eq!((s, code_of(&b)), (400, "invalid_settings"), "{rules}: {b}");
    }

    let rule = json!([{ "label": noop, "maxRequests": 2, "intervalSecs": 3600, "audience": "all" }]);
    let view = http.ok("action", "_system/settings:update", rate_limits(true, rule.clone()), &admin).await;
    assert_eq!(view["rateLimits"], json!({ "enabled": true, "rules": rule }));

    for _ in 0..2 {
        assert_eq!(http.call("mutation", &noop, json!({}), None).await.0, 200);
    }
    let res = http
        .client
        .post(format!("{}/api/mutation", http.base))
        .json(&json!({ "path": noop, "args": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    let retry: u64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=1800).contains(&retry), "{retry}");
    let body: Value = res.json().await.unwrap();
    assert_eq!(code_of(&body), "rate_limited");
    // Other functions aren't limited, and neither are administrators.
    assert_eq!(http.call("query", &todos::list::path(), json!({}), None).await.0, 200);
    assert_eq!(http.call("mutation", &noop, json!({}), Some(&admin)).await.0, 200);

    // The WebSocket shares the client's bucket: the mutation is answered with the error, the connection stays up.
    let mut ws = Ws::connect(http.base.trim_start_matches("http://").parse().unwrap()).await;
    ws.send(json!({ "type": "Mutation", "requestId": 7, "udfPath": noop, "args": {} })).await;
    let reply = loop {
        let m = ws.recv().await;
        if m["type"] == "MutationResponse" {
            break m;
        }
    };
    assert_eq!((&reply["requestId"], &reply["success"]), (&json!(7), &json!(false)), "{reply}");
    assert_eq!(reply["errorData"]["code"], "rate_limited");
    let (_, list) = ws.subscribe(&todos::list::path(), json!({})).await;
    assert_eq!(list, json!([]));

    // Turned off: the rules are kept, none applies.
    let view = http.ok("action", "_system/settings:update", rate_limits(false, rule.clone()), &admin).await;
    assert_eq!(view["rateLimits"]["rules"], rule);
    assert_eq!(http.call("mutation", &noop, json!({}), None).await.0, 200);
    http.ok("action", "_system/settings:update", json!({ "clear": ["rateLimits"] }), &admin).await;
    assert_eq!(http.ok("action", "_system/settings:get", json!({}), &admin).await["rateLimits"], Value::Null);
}

#[tokio::test]
async fn path_rate_limits_tell_guests_from_users_and_spare_the_dashboard() {
    use crate::common::todos;
    let (_t, http, admin) = setup(AuthConfig::password()).await;
    let user = token(&http.sign_up("u@example.com", "correct horse").await, "accessToken");
    let rules = json!([
        { "label": "/api/query", "maxRequests": 1, "intervalSecs": 3600, "audience": "guest" },
        { "label": "/api/", "maxRequests": 3, "intervalSecs": 3600 },
        { "label": "/", "maxRequests": 1, "intervalSecs": 3600 },
    ]);
    http.ok("action", "_system/settings:update", rate_limits(true, rules), &admin).await;
    let list = todos::list::path();

    // Guests: the guest rule decides for /api/query.
    assert_eq!(http.call("query", &list, json!({}), None).await.0, 200);
    let (s, b) = http.call("query", &list, json!({}), None).await;
    assert_eq!((s, code_of(&b)), (429, "rate_limited"));
    // Signed-in users fall through to /api/: three requests.
    for _ in 0..3 {
        assert_eq!(http.call("query", &list, json!({}), Some(&user)).await.0, 200);
    }
    assert_eq!(http.call("query", &list, json!({}), Some(&user)).await.0, 429);
    // A forged token is a guest's.
    assert_eq!(http.call("query", &list, json!({}), Some("not.a.token")).await.0, 429);

    // Health checks and the dashboard's API are never limited, nor are administrators over the limit.
    for _ in 0..5 {
        assert_eq!(http.get("/api/health", None).await.0, 200);
        assert_eq!(http.get("/api/admin/client-ip", Some(&admin)).await.0, 200);
        assert_eq!(http.call("query", &list, json!({}), Some(&admin)).await.0, 200);
    }
    // Saving other settings keeps the counts.
    http.ok("action", "_system/settings:update", json!({ "signups": true }), &admin).await;
    assert_eq!(http.call("query", &list, json!({}), None).await.0, 429);
}

#[tokio::test]
async fn a_strict_api_rule_does_not_lock_the_dashboard_out() {
    use crate::authkit::Ws;
    let (_t, http, admin) = setup(AuthConfig::password()).await;
    let rules = json!([{ "label": "/api/", "maxRequests": 1, "intervalSecs": 3600 }]);
    http.ok("action", "_system/settings:update", rate_limits(true, rules), &admin).await;
    let addr = http.base.trim_start_matches("http://").parse().unwrap();
    // WebSocket connections (the dashboard's too) and admin sign-ins are not counted by `/api/`.
    for _ in 0..5 {
        let mut ws = Ws::connect(addr).await;
        ws.authenticate(Some(&admin)).await;
        assert!(http.admin_sign_in("admin@example.com", "admin password").await["accessToken"].is_string());
    }
    // A rule naming `/api/sync` does count them.
    let rules = json!([{ "label": "/api/sync", "maxRequests": 1, "intervalSecs": 3600 }]);
    http.ok("action", "_system/settings:update", rate_limits(true, rules), &admin).await;
    let url = format!("ws://{addr}/api/sync");
    assert!(tokio_tungstenite::connect_async(&url).await.is_ok());
    let err = tokio_tungstenite::connect_async(&url).await.unwrap_err().to_string();
    assert!(err.contains("429"), "{err}");
}

/// The latest email to `to`, once at least `count` were sent to it (auth emails go out in the background).
async fn nth_email(mailer: &MemoryMailer, to: &str, count: usize) -> rxferrix::auth::Email {
    for _ in 0..200 {
        let mine: Vec<_> = mailer.emails().into_iter().filter(|e| e.to == to).collect();
        if mine.len() >= count {
            return mine[count - 1].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no email #{count} to {to}");
}

#[tokio::test]
async fn email_templates_shape_the_auth_emails() {
    let mailer = MemoryMailer::new();
    let config = AuthConfig::password().magic_link().mailer(mailer.clone()).redirect_url("https://app.example.com");
    let (_t, http, admin) = setup(config).await;
    let view = http.ok("action", "_system/settings:get", json!({}), &admin).await;
    assert_eq!((&view["emailTemplates"], &view["effective"]["appName"]), (&Value::Null, &json!("")));
    assert_eq!(view["emailTemplateDefaults"]["signIn"]["subject"], "Your sign-in link");
    assert!(view["emailTemplateDefaults"]["resetPassword"]["html"].as_str().unwrap().contains("{code}"));

    let sign_in =
        |subject: &str, html: &str| json!({ "emailTemplates": { "signIn": { "subject": subject, "html": html } } });
    for args in [
        sign_in("", "<p>{code}</p>"),
        sign_in("Hi", "<p>no code</p>"),
        sign_in("Hi {name}", "<p>{code}</p>"),
        sign_in("Two\nlines", "<p>{code}</p>"),
    ] {
        let (s, b) = update(&http, &admin, args.clone()).await;
        assert_eq!((s, code_of(&b)), (400, "invalid_settings"), "{args}: {b}");
    }

    let templates = json!({
        "appName": "Acme <Shop>",
        "signIn": { "subject": "Sign in to {app_name}", "html": "<p>Hi {email}, <a href=\"{link}\">sign in</a> with <b>{code}</b> within {expires}. {app_name}</p>" },
    });
    let view = http.ok("action", "_system/settings:update", json!({ "emailTemplates": templates }), &admin).await;
    assert_eq!(view["emailTemplates"]["signIn"]["subject"], "Sign in to {app_name}");
    assert_eq!(
        (&view["emailTemplates"]["verifyEmail"], &view["effective"]["appName"]),
        (&Value::Null, &json!("Acme <Shop>"))
    );

    let email = "ana@example.com";
    assert_eq!(http.post("/api/auth/magic/request", None, json!({ "email": email })).await.0, 200);
    let sent = nth_email(&mailer, email, 1).await;
    assert_eq!(sent.subject, "Sign in to Acme <Shop>", "subjects are not HTML-escaped");
    let html = sent.html.expect("an HTML part");
    assert!(html.contains("Hi ana@example.com, <a href=\"https://app.example.com/#rxferrix_magic_token="), "{html}");
    assert!(html.contains("within 15 minutes. Acme &lt;Shop&gt;</p>"), "{html}");
    let token = html.split("rxferrix_magic_token=").nth(1).unwrap().split('"').next().unwrap();
    assert!(
        sent.body.contains(&format!("sign in (https://app.example.com/#rxferrix_magic_token={token})")),
        "{}",
        sent.body
    );
    let (s, _) = http.post("/api/auth/magic/verify", None, json!({ "token": token })).await;
    assert_eq!(s, 200, "the link in the templated email signs in");

    // Emails without a template are the built-in ones; clearing the setting brings back all of them.
    http.post("/api/auth/reset/request", None, json!({ "email": email })).await;
    let reset = nth_email(&mailer, email, 2).await;
    assert_eq!((reset.subject.as_str(), reset.html.is_none()), ("Reset your password", true));
    http.ok("action", "_system/settings:update", json!({ "clear": ["emailTemplates"] }), &admin).await;
    http.post("/api/auth/magic/request", None, json!({ "email": email })).await;
    let built_in = nth_email(&mailer, email, 3).await;
    assert_eq!((built_in.subject.as_str(), built_in.html.is_none()), ("Your sign-in link", true));
}

#[tokio::test]
async fn the_app_name_defaults_to_the_smtp_sender_name() {
    let (_t, http, admin) = setup(AuthConfig::password()).await;
    let smtp = json!({ "url": "smtp://mail.example.com:25", "from": "Acme Mail <app@example.com>" });
    let view = http.ok("action", "_system/settings:update", json!({ "smtp": smtp }), &admin).await;
    assert_eq!(view["effective"]["appName"], "Acme Mail");
    let view =
        http.ok("action", "_system/settings:update", json!({ "emailTemplates": { "appName": "Acme" } }), &admin).await;
    assert_eq!((&view["effective"]["appName"], &view["emailTemplates"]["signIn"]), (&json!("Acme"), &Value::Null));
}

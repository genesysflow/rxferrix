//! Runtime settings (`_settings`, `_system/settings:*`): admin-only, validated, secrets encrypted at rest and never
//! returned (nor logged), applied without a restart, config wins over settings, audited, CORS, test emails.

use std::time::Duration;

use ferrix::auth::{AuthConfig, MemoryMailer};
use ferrix::testing::TestApp;
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
        .oauth(ferrix::auth::GitHub::new("cfg-id", "cfg-secret"));
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

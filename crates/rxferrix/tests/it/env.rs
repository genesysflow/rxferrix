//! Environment variables (`_env`, `ctx.env`, `_system/env:*`): admin-only and validated; queries that read a
//! variable rerun when it changes (and only those); mutations and actions read them too; values are encrypted at rest,
//! survive a restart, and never reach the execution log or the audit log.

use rxferrix::FunctionRef;
use rxferrix::auth::AuthConfig;
use rxferrix::testing::TestApp;
use serde_json::{Value, json};

use crate::authkit::{Http, Ws, code_of, token};

const MASK: &str = "••••";

pub mod fns {
    use rxferrix::prelude::*;

    #[query]
    pub async fn env_greeting(ctx: &QueryCtx) -> Result<Option<String>> {
        ctx.env.get("GREETING").await
    }

    #[query]
    pub async fn env_other(ctx: &QueryCtx) -> Result<Option<String>> {
        ctx.env.get("OTHER").await
    }

    #[query]
    pub async fn env_required(ctx: &QueryCtx) -> Result<String> {
        ctx.env.require("API_URL").await
    }

    #[mutation]
    pub async fn env_in_mutation(ctx: &MutationCtx) -> Result<Option<String>> {
        ctx.env.get("GREETING").await
    }

    #[action]
    pub async fn env_in_action(ctx: &ActionCtx) -> Result<Option<String>> {
        ctx.env.get("GREETING").await
    }
}

async fn setup() -> (TestApp, Http, String) {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let http = Http::new(t.serve().await.unwrap());
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let admin = token(&http.admin_sign_in("admin@example.com", "admin password").await, "accessToken");
    (t, http, admin)
}

async fn set(http: &Http, admin: &str, name: &str, value: &str) -> (u16, Value) {
    http.call("mutation", "_system/env:set", json!({ "name": name, "value": value }), Some(admin)).await
}

#[tokio::test]
async fn variables_are_admin_only_validated_and_reactive() {
    let (t, http, admin) = setup().await;
    let user = token(&http.sign_up("u@example.com", "correct horse").await, "accessToken");
    for (kind, name, args) in [
        ("query", "list", json!({})),
        ("mutation", "set", json!({ "name": "A", "value": "1" })),
        ("mutation", "remove", json!({ "name": "A" })),
    ] {
        let path = format!("_system/env:{name}");
        assert_eq!(code_of(&http.call(kind, &path, args.clone(), None).await.1), "unauthenticated");
        assert_eq!(code_of(&http.call(kind, &path, args, Some(&user)).await.1), "forbidden");
    }
    // `put` is internal: not callable over HTTP at all.
    assert_eq!(
        http.call("mutation", "_system/env:put", json!({ "name": "A", "value": "1" }), Some(&admin)).await.0,
        404
    );

    for bad in ["", "1ABC", "has space", "dash-ed", &"X".repeat(65)] {
        assert_eq!(code_of(&set(&http, &admin, bad, "v").await.1), "invalid_name", "{bad:?}");
    }
    assert_eq!(code_of(&set(&http, &admin, "BIG", &"x".repeat(8 * 1024 + 1)).await.1), "invalid_value");

    // Two subscriptions, each reading one variable.
    let mut ws = Ws::connect(t.serve().await.unwrap()).await;
    let (greeting, first) = ws.subscribe(&fns::env_greeting::path(), json!({})).await;
    assert_eq!(first, Value::Null);
    let (other, first) = ws.subscribe(&fns::env_other::path(), json!({})).await;
    assert_eq!(first, Value::Null);

    // Setting, changing and removing GREETING reruns its query, not OTHER's.
    for (value, expect) in [(Some("hello"), json!("hello")), (Some("hi"), json!("hi")), (None, Value::Null)] {
        match value {
            Some(v) => assert_eq!(set(&http, &admin, "GREETING", v).await.0, 200),
            None => {
                http.ok("mutation", "_system/env:remove", json!({ "name": "GREETING" }), &admin).await;
            }
        }
        loop {
            let t = ws.recv().await;
            assert!(Ws::value_of(&t, other).is_none(), "OTHER's query reran for a change to GREETING: {t}");
            if let Some(v) = Ws::value_of(&t, greeting) {
                assert_eq!(v, expect);
                break;
            }
        }
    }
    set(&http, &admin, "OTHER", "x").await;
    assert_eq!(ws.next_value(other).await, json!("x"));

    // Mutations and actions read them too; `require` names a missing variable.
    set(&http, &admin, "GREETING", "from env").await;
    assert_eq!(t.mutation::<fns::env_in_mutation>(()).await.unwrap().as_deref(), Some("from env"));
    assert_eq!(t.action::<fns::env_in_action>(()).await.unwrap().as_deref(), Some("from env"));
    let err = t.query::<fns::env_required>(()).await.unwrap_err();
    assert!(format!("{err:#}").contains("`API_URL` is not set"), "{err:#}");
    set(&http, &admin, "API_URL", "https://api.example.com").await;
    assert_eq!(t.query::<fns::env_required>(()).await.unwrap(), "https://api.example.com");

    // The list, by name, with values.
    let list = http.ok("query", "_system/env:list", json!({}), &admin).await;
    let names: Vec<_> = list.as_array().unwrap().iter().map(|v| (v["name"].clone(), v["value"].clone())).collect();
    assert_eq!(
        names,
        vec![
            (json!("API_URL"), json!("https://api.example.com")),
            (json!("GREETING"), json!("from env")),
            (json!("OTHER"), json!("x"))
        ]
    );
}

#[tokio::test]
async fn values_are_encrypted_at_rest_kept_across_restarts_and_never_logged() {
    let (t, http, admin) = setup().await;
    const VALUE: &str = "sk-live-0123456789-very-secret";
    assert_eq!(set(&http, &admin, "PAYMENT_KEY", VALUE).await.0, 200);
    // Setting the same value again changes nothing (no write, no audit entry).
    assert_eq!(set(&http, &admin, "PAYMENT_KEY", VALUE).await.0, 200);

    // The data browser masks the sealed value.
    let docs = http
        .ok(
            "query",
            "_system/dashboard:documents",
            json!({ "table": "_env", "index": null, "filters": [], "order": "asc",
                    "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await;
    assert_eq!(docs["page"][0]["name"], "PAYMENT_KEY");
    assert_eq!(docs["page"][0]["valueEncrypted"], MASK);

    // The execution log has `set`'s value masked; the audit log names the variable once, without the value.
    let (_, logs) = http.get("/api/admin/logs?path=_system/env:set", Some(&admin)).await;
    let entries = logs["entries"].as_array().unwrap();
    assert!(!entries.is_empty());
    let args = |e: &Value| serde_json::from_str::<Value>(e["args"].as_str().unwrap()).unwrap();
    assert!(entries.iter().all(|e| args(e)["value"] == MASK), "{logs}");
    let audit = http
        .ok("query", "_system/audit:list", json!({ "paginationOpts": { "numItems": 50, "cursor": null } }), &admin)
        .await;
    let env_entries: Vec<_> = audit["page"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"].as_str().unwrap().starts_with("env_"))
        .collect();
    assert_eq!(env_entries.len(), 1, "{audit}");
    assert_eq!(
        (&env_entries[0]["action"], &env_entries[0]["details"]["name"]),
        (&json!("env_create"), &json!("PAYMENT_KEY"))
    );
    assert!(!audit.to_string().contains(VALUE));

    // Nothing in the data directory holds the value in clear.
    t.app().logs().flush().await.unwrap();
    let t = t.restart().await.unwrap();
    for entry in std::fs::read_dir(t.dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(!bytes.windows(VALUE.len()).any(|w| w == VALUE.as_bytes()), "{} holds the value", path.display());
        }
    }

    // After a restart the value is still there (the key is in the data directory).
    let app = t.app().clone();
    let http = Http::new(t.serve().await.unwrap());
    let admin = token(&http.admin_sign_in("admin@example.com", "admin password").await, "accessToken");
    assert_eq!(http.ok("query", "_system/env:list", json!({}), &admin).await[0]["value"], VALUE);
    drop(app);
}

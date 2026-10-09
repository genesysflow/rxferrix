//! The admin audit log (`_audit_log`, `_system/audit:list`) and impersonation through real sessions
//! (`run_function` "as user", `POST /api/admin/impersonate`).

use std::time::Duration;

use ferrix::FunctionRef;
use ferrix::auth::AuthConfig;
use ferrix::testing::TestApp;
use serde_json::{Value, json};

use crate::authkit::{Http, Ws, code_of, token};

mod app {
    use ferrix::prelude::*;

    #[table(name = "audited_items")]
    pub struct AuditedItem {
        pub title: String,
        pub rank: i64,
    }

    /// Who is calling, including the session (so its shared-query scope is the session).
    #[query]
    /// `[user id, session id, impersonated by]`.
    pub async fn audit_whoami(ctx: &QueryCtx) -> Result<Vec<Option<String>>> {
        let session = ctx.auth.session_id();
        let impersonated_by = match session {
            Some(s) => ctx.db.get(s).await?.and_then(|s| s.value.impersonated_by).map(|a| a.to_string()),
            None => None,
        };
        Ok(vec![ctx.auth.user_id().map(|u| u.to_string()), session.map(|s| s.to_string()), impersonated_by])
    }

    #[mutation]
    pub async fn audit_add(ctx: &MutationCtx, title: String) -> Result<Id<AuditedItem>> {
        if title.is_empty() {
            return Err(UserError::new("empty", "empty title").into());
        }
        ctx.db.insert(AuditedItem { title, rank: 0 }).await
    }
}

async fn setup(config: AuthConfig) -> (TestApp, Http, String, String, Value) {
    let t = TestApp::with_auth(config.rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let http = Http::new(t.serve().await.unwrap());
    let admin_id = app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap().to_string();
    let admin = token(&http.admin_sign_in("admin@example.com", "admin password").await, "accessToken");
    let user = http.sign_up("user@example.com", "user password").await;
    (t, http, admin, admin_id, user)
}

async fn list(http: &Http, admin: &str, filter: Value) -> Vec<Value> {
    let mut args = filter;
    args["paginationOpts"] = json!({ "numItems": 100, "cursor": null });
    http.ok("query", "_system/audit:list", args, admin).await["page"].as_array().unwrap().clone()
}

#[tokio::test]
async fn dashboard_writes_are_audited_in_their_transaction() {
    let (t, http, admin, admin_id, user) = setup(AuthConfig::password()).await;
    let user_id = token(&user, "userId");
    let call = |name: &'static str, args: Value| {
        let (http, admin) = (&http, admin.clone());
        async move { http.call("mutation", &format!("_system/dashboard:{name}"), args, Some(&admin)).await }
    };

    let (_, b) = call("insert_document", json!({ "table": "audited_items", "document": { "title": "a", "rank": 1 } })).await;
    let id = b["value"].as_str().unwrap().to_string();
    call("patch_document", json!({ "id": id, "patch": { "rank": 2 } })).await;
    call("replace_document", json!({ "id": id, "document": { "title": "b", "rank": 2 } })).await;
    // A failing write leaves no entry: it rolled back together with its audit row.
    let (s, b) = call("patch_document", json!({ "id": id, "patch": { "rank": "not a number" } })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_document"));
    call("patch_document", json!({ "id": user_id, "patch": { "name": "Ada" } })).await;
    call("delete_documents", json!({ "ids": [id] })).await;

    let entries = list(&http, &admin, json!({ "target": id })).await;
    let actions: Vec<&str> = entries.iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["delete", "replace", "patch", "insert"], "newest first");
    let by = |a: &str| entries.iter().find(|e| e["action"] == a).unwrap().clone();
    assert_eq!((&by("insert")["before"], &by("insert")["after"]["title"]), (&Value::Null, &json!("a")));
    assert_eq!((&by("patch")["before"], &by("patch")["after"]), (&json!({ "rank": 1 }), &json!({ "rank": 2 })));
    assert_eq!((&by("replace")["before"], &by("replace")["after"]), (&json!({ "title": "a" }), &json!({ "title": "b" })));
    assert_eq!((&by("delete")["before"]["title"], &by("delete")["after"]), (&json!("b"), &Value::Null));
    for e in &entries {
        assert_eq!((&e["admin"], &e["adminEmail"], &e["targetTable"]), (&json!(admin_id), &json!("admin@example.com"), &json!("audited_items")));
        assert!(e["at"].as_f64().unwrap() > 0.0);
    }
    let users = list(&http, &admin, json!({ "table": "_users" })).await;
    assert_eq!((&users[0]["action"], &users[0]["after"]), (&json!("patch"), &json!({ "name": "Ada" })));

    // Sessions and users: secrets in the before-image are masked.
    let sessions = http.ok("query", "_system/dashboard:user_sessions", json!({ "user": user_id }), &admin).await;
    let sid = sessions[0]["_id"].as_str().unwrap().to_string();
    http.ok("mutation", "_system/dashboard:revoke_session", json!({ "session": sid }), &admin).await;
    http.ok("mutation", "_system/dashboard:delete_user", json!({ "user": user_id }), &admin).await;
    let revoked = list(&http, &admin, json!({ "action": "revoke_session" })).await;
    assert_eq!(revoked[0]["targetId"], json!(sid));
    assert_eq!(revoked[0]["before"]["refreshTokenHash"], "••••");
    let deleted = list(&http, &admin, json!({ "action": "delete_user", "admin": admin_id })).await;
    assert_eq!((&deleted[0]["before"]["email"], &deleted[0]["details"]["credentials"]), (&json!("user@example.com"), &json!(1)));

    // Filters combine; pagination works; the log is read-only and admin-only.
    assert_eq!(list(&http, &admin, json!({ "action": "patch", "table": "audited_items" })).await.len(), 1);
    assert!(list(&http, &admin, json!({ "admin": "someone-else" })).await.is_empty());
    let all = list(&http, &admin, json!({})).await;
    let page = http.ok("query", "_system/audit:list", json!({ "paginationOpts": { "numItems": 2, "cursor": null } }), &admin).await;
    assert_eq!((page["page"].as_array().unwrap().len(), page["isDone"].as_bool()), (2, Some(false)));
    assert_eq!(page["page"][0]["_id"], all[0]["_id"]);
    let (_, b) = call("delete_documents", json!({ "ids": [all[0]["_id"]] })).await;
    assert_eq!(code_of(&b), "read_only_table");
    let (s, b) = http.call("query", "_system/audit:list", json!({ "paginationOpts": { "numItems": 1, "cursor": null } }), None).await;
    assert_eq!((s, code_of(&b)), (400, "unauthenticated"));

    // Retention: entries older than 90 days are swept.
    t.app().db().clock().advance(Duration::from_secs(91 * 24 * 3600));
    let swept = ferrix::audit::sweep(t.app()).await.unwrap();
    assert_eq!(swept, all.len());
    assert!(list(&http, &admin, json!({})).await.is_empty());
}

#[tokio::test]
async fn run_function_as_a_user_uses_a_real_audited_session() {
    let (t, http, admin, admin_id, user) = setup(AuthConfig::password()).await;
    let user_id = token(&user, "userId");
    let run = |args: Value| http.ok("action", "_system/dashboard:run_function", args, &admin);

    let r = run(json!({ "path": app::audit_whoami::path(), "asUser": user_id })).await;
    assert_eq!(r["success"], true, "{r}");
    let (uid, sid, by) = (&r["value"][0], &r["value"][1], &r["value"][2]);
    assert_eq!(uid, &json!(user_id));
    assert!(sid.is_string(), "ctx.auth.session_id() works when acting as a user");
    assert_eq!(by, &json!(admin_id), "the session is marked impersonatedBy");
    // The session ends with the run.
    let sessions = http.ok("query", "_system/dashboard:user_sessions", json!({ "user": user_id }), &admin).await;
    assert!(sessions.as_array().unwrap().iter().all(|s| s["_id"] != *sid && s["impersonatedBy"].is_null()));

    // Mutations: the run_function entry commits with the mutation; a failed one is audited separately.
    let r = run(json!({ "path": app::audit_add::path(), "args": { "title": "x" }, "asUser": user_id })).await;
    assert_eq!(r["success"], true);
    let r = run(json!({ "path": app::audit_add::path(), "args": { "title": "" } })).await;
    assert_eq!(r["success"], false);

    let runs = list(&http, &admin, json!({ "action": "run_function" })).await;
    let outcomes: Vec<(&str, &str)> =
        runs.iter().map(|e| (e["details"]["path"].as_str().unwrap(), e["details"]["outcome"].as_str().unwrap())).collect();
    assert_eq!(
        outcomes,
        [(app::audit_add::path().as_str(), "error"), (app::audit_add::path().as_str(), "success"), (app::audit_whoami::path().as_str(), "started")]
    );
    assert_eq!(runs[1]["details"]["asUser"], json!(user_id));
    let impersonations = list(&http, &admin, json!({ "action": "impersonate" })).await;
    assert_eq!(impersonations.len(), 2);
    assert_eq!((&impersonations[0]["targetId"], &impersonations[0]["details"]["purpose"]), (&json!(user_id), &json!("run_function")));
    drop(t);
}

#[tokio::test]
async fn impersonation_tokens_are_short_lived_real_sessions() {
    let (t, http, admin, admin_id, user) = setup(AuthConfig::password().redirect_url("https://app.example.com")).await;
    let user_id = token(&user, "userId");
    let user_token = token(&user, "accessToken");
    async fn imp(http: &Http, body: Value, tok: Option<&str>) -> (u16, Value) {
        http.post("/api/admin/impersonate", tok, body).await
    }

    for (tok, status, code) in [(None, 401, "unauthenticated"), (Some(user_token.as_str()), 403, "forbidden")] {
        let (s, b) = imp(&http, json!({ "user": user_id }), tok).await;
        assert_eq!((s, code_of(&b)), (status, code));
    }
    let (s, b) = imp(&http, json!({ "user": user_id, "ttlSeconds": 7200 }), Some(&admin)).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_request"), "at most an hour");
    let (s, b) = imp(&http, json!({ "user": user_id, "redirectTo": "https://evil.example.com" }), Some(&admin)).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_redirect"));
    let (s, b) = imp(&http, json!({ "user": "00000000000000000000000000" }), Some(&admin)).await;
    assert_eq!((s, code_of(&b)), (404, "not_found"));

    let (s, minted) = imp(&http, json!({ "user": user_id, "ttlSeconds": 120, "redirectTo": "https://app.example.com/home" }), Some(&admin)).await;
    assert_eq!(s, 200, "{minted}");
    assert_eq!((&minted["userId"], &minted["expiresIn"]), (&json!(user_id), &json!(120)), "the access token ends with the session");
    let url = minted["url"].as_str().unwrap();
    assert!(url.starts_with("https://app.example.com/home#ferrix_access_token=") && url.contains("&ferrix_refresh_token="));
    let access = token(&minted, "accessToken");
    let (_, me) = http.call("query", &app::audit_whoami::path(), json!({}), Some(&access)).await;
    assert_eq!(me["value"], json!([user_id, minted["sessionId"], admin_id]));

    // Refreshing never extends it.
    let (s, refreshed) = http.post("/api/auth/refresh", None, json!({ "refreshToken": minted["refreshToken"] })).await;
    assert_eq!(s, 200);
    assert!(refreshed["expiresIn"].as_u64().unwrap() <= 120);
    let sessions = http.ok("query", "_system/dashboard:user_sessions", json!({ "user": user_id }), &admin).await;
    let session = sessions.as_array().unwrap().iter().find(|s| s["_id"] == minted["sessionId"]).unwrap().clone();
    assert_eq!(session["impersonatedBy"], json!(admin_id));
    assert!((session["expiresAt"].as_f64().unwrap() - minted["expiresAt"].as_f64().unwrap()).abs() < 1.0);

    // Audited, and revocable like any session (with live revocation of its sockets).
    let entries = list(&http, &admin, json!({ "action": "impersonate" })).await;
    assert_eq!((&entries[0]["details"]["sessionId"], &entries[0]["details"]["purpose"]), (&minted["sessionId"], &json!("open_app")));
    let mut ws = Ws::connect(t.serve().await.unwrap()).await;
    ws.authenticate(Some(&token(&refreshed, "accessToken"))).await;
    http.ok("mutation", "_system/dashboard:revoke_session", json!({ "session": minted["sessionId"] }), &admin).await;
    let msg = loop {
        let m = ws.recv().await;
        if m["type"] != "Transition" {
            break m;
        }
    };
    assert_eq!(msg["type"], "AuthError");

    // It can be switched off.
    let t2 = TestApp::with_auth(AuthConfig::password().impersonation(false)).await.unwrap();
    let app2 = t2.app().clone();
    let http2 = Http::new(t2.serve().await.unwrap());
    app2.auth().create_admin(&app2, "a@example.com", "admin password".into()).await.unwrap();
    let admin2 = token(&http2.admin_sign_in("a@example.com", "admin password").await, "accessToken");
    let u2 = http2.sign_up("u2@example.com", "user password").await;
    let (s, b) = http2.post("/api/admin/impersonate", Some(&admin2), json!({ "user": u2["userId"] })).await;
    assert_eq!((s, code_of(&b)), (403, "impersonation_disabled"));
}

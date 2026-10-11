//! The dashboard backend over real HTTP and WebSockets: admin gating of the `_system/dashboard:*` functions and the
//! `/api/admin/*` endpoints, untyped CRUD with schema validation, live `documents` pages, secret masking, running
//! functions as a user, the logs history and SSE tail, user and session management, and stats.

use std::net::SocketAddr;
use std::time::Duration;

use ferrix::FunctionRef;
use ferrix::auth::AuthConfig;
use ferrix::testing::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

mod app {
    use ferrix::log;
    use ferrix::prelude::*;

    #[table(name = "dash_items")]
    #[index(by_category = [category, rank])]
    pub struct DashItem {
        pub title: String,
        pub category: String,
        pub rank: i64,
        pub note: Option<String>,
    }

    #[mutation]
    pub async fn add_item(ctx: &MutationCtx, title: String, category: String, rank: i64) -> Result<Id<DashItem>> {
        ctx.db.insert(DashItem { title, category, rank, note: None }).await
    }

    #[query]
    pub async fn all_items(ctx: &QueryCtx) -> Result<Vec<Doc<DashItem>>> {
        ctx.db.query::<DashItem>().collect().await
    }

    /// Internal, so only reachable from the dashboard's runner (or other functions).
    #[internal_mutation]
    pub async fn whoami_logged(ctx: &MutationCtx, greeting: String) -> Result<String> {
        let me = ctx.auth.require_user()?;
        log::info!(user = %me, "{greeting}");
        Ok(me.to_string())
    }
}

const D: &str = "_system/dashboard";

fn path(name: &str) -> String {
    format!("{D}:{name}")
}

struct Http {
    client: reqwest::Client,
    base: String,
}

impl Http {
    fn new(addr: SocketAddr) -> Http {
        Http { client: reqwest::Client::new(), base: format!("http://{addr}") }
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

    /// Call a function that must succeed; returns its value.
    async fn ok(&self, kind: &str, path: &str, args: Value, token: &str) -> Value {
        let (s, b) = self.call(kind, path, args, Some(token)).await;
        assert_eq!(s, 200, "{path}: {b}");
        b["value"].clone()
    }
}

fn code_of(v: &Value) -> &str {
    v["errorData"]["code"].as_str().unwrap_or("")
}

/// An app with password auth, an admin and a user; returns (app, http, admin token, user token, user id).
async fn setup() -> (TestApp, Http, String, String, String) {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let http = Http::new(t.serve().await.unwrap());
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let (s, admin) = http
        .post("/api/auth/admin/signin", None, json!({ "email": "admin@example.com", "password": "admin password" }))
        .await;
    assert_eq!(s, 200, "{admin}");
    let (s, user) =
        http.post("/api/auth/signup", None, json!({ "email": "user@example.com", "password": "user password" })).await;
    assert_eq!(s, 200, "{user}");
    let admin_token = admin["accessToken"].as_str().unwrap().to_string();
    let user_token = user["accessToken"].as_str().unwrap().to_string();
    let user_id = user["userId"].as_str().unwrap().to_string();
    (t, http, admin_token, user_token, user_id)
}

fn page(table: &str) -> Value {
    json!({ "table": table, "paginationOpts": { "numItems": 50, "cursor": null } })
}

// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn system_functions_and_admin_endpoints_are_admin_only() {
    let (_t, http, admin, user, user_id) = setup().await;
    let calls = [
        ("query", "tables", json!({})),
        ("query", "documents", page("dash_items")),
        ("query", "document", json!({ "id": user_id })),
        ("query", "functions", json!({})),
        ("query", "users", json!({ "paginationOpts": { "numItems": 10, "cursor": null } })),
        ("query", "user_sessions", json!({ "user": user_id })),
        ("query", "me", json!({})),
        ("mutation", "insert_document", json!({ "table": "dash_items", "document": {} })),
        ("mutation", "patch_document", json!({ "id": user_id, "patch": {} })),
        ("mutation", "replace_document", json!({ "id": user_id, "document": {} })),
        ("mutation", "delete_documents", json!({ "ids": [user_id] })),
        ("mutation", "revoke_session", json!({ "session": user_id })),
        ("mutation", "delete_user", json!({ "user": user_id })),
        ("action", "run_function", json!({ "path": app::all_items::path() })),
    ];
    for (kind, name, args) in &calls {
        let (s, b) = http.call(kind, &path(name), args.clone(), None).await;
        assert_eq!((s, code_of(&b)), (400, "unauthenticated"), "anonymous {name}: {b}");
        let (s, b) = http.call(kind, &path(name), args.clone(), Some(&user)).await;
        assert_eq!((s, code_of(&b)), (400, "forbidden"), "user {name}: {b}");
    }
    // The user still exists: nothing above ran.
    let me = http.ok("query", &path("me"), json!({}), &admin).await;
    assert_eq!(me["email"], "admin@example.com");
    for name in ["tables", "functions"] {
        http.ok("query", &path(name), json!({}), &admin).await;
    }

    for endpoint in ["/api/admin/logs", "/api/admin/stats", "/api/admin/logs/stream"] {
        let (s, b) = http.get(endpoint, None).await;
        assert_eq!((s, code_of(&b)), (401, "unauthenticated"), "{endpoint}");
        let (s, b) = http.get(endpoint, Some(&user)).await;
        assert_eq!((s, code_of(&b)), (403, "forbidden"), "{endpoint}");
        let (s, b) = http.get(endpoint, Some("not.a.token")).await;
        assert_eq!((s, code_of(&b)), (401, "invalid_token"), "{endpoint}");
    }
    let (s, b) = http.get("/api/admin/stats", Some(&admin)).await;
    assert_eq!(s, 200, "{b}");
    let o = &b["overview"];
    assert!(o["ts"].as_u64().unwrap() > 0 && o["documents"].as_u64().unwrap() >= 3, "{o}");
    assert_eq!(o["files"][0]["name"], "data.db");
    assert!(o["files"][0]["bytes"].as_u64().unwrap() > 0);
    assert!(b["functions"].as_array().unwrap().iter().any(|f| f["path"] == path("tables")));
}

#[tokio::test]
async fn untyped_crud_validates_against_the_schema() {
    let (t, http, admin, _, _) = setup().await;
    let insert = path("insert_document");
    let ins =
        |doc: Value| http.call("mutation", &insert, json!({ "table": "dash_items", "document": doc }), Some(&admin));

    let (s, b) = ins(json!({ "title": "one", "category": "a", "rank": 1 })).await;
    assert_eq!(s, 200, "{b}");
    let id = b["value"].as_str().unwrap().to_string();
    // System fields from an editor are ignored on insert.
    let (s, b) =
        ins(json!({ "_id": "x", "_creationTime": 1, "title": "two", "category": "a", "rank": 2, "note": "n" })).await;
    assert_eq!(s, 200, "{b}");
    let id2 = b["value"].as_str().unwrap().to_string();

    // Schema validation: wrong type, missing field, unknown table, non-object.
    let (s, b) = ins(json!({ "title": 5, "category": "a", "rank": 1 })).await;
    assert_eq!((s, code_of(&b)), (400, "invalid_document"), "{b}");
    assert!(b["errorMessage"].as_str().unwrap().contains("does not match table `dash_items`"), "{b}");
    let (_, b) = ins(json!({ "title": "x", "category": "a" })).await;
    assert!(b["errorMessage"].as_str().unwrap().contains("missing field `rank`"), "{b}");
    let (_, b) =
        http.call("mutation", &path("insert_document"), json!({ "table": "nope", "document": {} }), Some(&admin)).await;
    assert_eq!(code_of(&b), "invalid_document", "{b}");
    let (_, b) = ins(json!([1])).await;
    assert_eq!(code_of(&b), "invalid_document");

    // Read back through the untyped and the typed API.
    let doc = http.ok("query", &path("document"), json!({ "id": id }), &admin).await;
    assert_eq!(doc["table"], "dash_items");
    assert_eq!(doc["document"]["title"], "one");
    assert_eq!(doc["document"]["_id"], id.as_str());

    // Patch: merged, validated.
    http.ok("mutation", &path("patch_document"), json!({ "id": id, "patch": { "rank": 10, "note": "hi" } }), &admin)
        .await;
    let (_, b) = http
        .call("mutation", &path("patch_document"), json!({ "id": id, "patch": { "rank": "ten" } }), Some(&admin))
        .await;
    assert_eq!(code_of(&b), "invalid_document");
    assert!(b["errorMessage"].as_str().unwrap().contains("patched document"), "{b}");
    let (_, b) = http
        .call("mutation", &path("patch_document"), json!({ "id": id, "patch": { "title": null } }), Some(&admin))
        .await;
    assert_eq!(code_of(&b), "invalid_document", "nulling a required field: {b}");

    // Replace: the editor's whole document, system fields included; `_id` must match.
    let mut edited = http.ok("query", &path("document"), json!({ "id": id }), &admin).await["document"].clone();
    edited["title"] = json!("one (edited)");
    edited["category"] = json!("b");
    http.ok("mutation", &path("replace_document"), json!({ "id": id, "document": edited.clone() }), &admin).await;
    edited["_id"] = json!(id2);
    let (_, b) =
        http.call("mutation", &path("replace_document"), json!({ "id": id, "document": edited }), Some(&admin)).await;
    assert_eq!(code_of(&b), "invalid_document");

    let items = http.ok("query", &app::all_items::path(), json!({}), &admin).await;
    let one = items.as_array().unwrap().iter().find(|d| d["_id"] == id.as_str()).unwrap();
    assert_eq!(
        (&one["title"], &one["category"], &one["rank"], &one["note"]),
        (&json!("one (edited)"), &json!("b"), &json!(10), &json!("hi"))
    );

    // Index filters and order in `documents`.
    let res = http
        .ok(
            "query",
            &path("documents"),
            json!({ "table": "dash_items", "index": "by_category", "filters": [{ "field": "category", "op": "eq", "value": "a" }],
                    "order": "desc", "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await;
    let titles: Vec<_> = res["page"].as_array().unwrap().iter().map(|d| d["title"].clone()).collect();
    assert_eq!(titles, vec![json!("two")]);
    let (_, b) = http
        .call("query", &path("documents"), json!({ "table": "dash_items", "index": "by_category",
            "filters": [{ "field": "rank", "op": "eq", "value": 1 }], "paginationOpts": { "numItems": 10, "cursor": null } }), Some(&admin))
        .await;
    assert_eq!(code_of(&b), "invalid_query", "filters must follow the index fields: {b}");
    let (_, b) = http.call("query", &path("documents"), page("nope"), Some(&admin)).await;
    assert_eq!(code_of(&b), "unknown_table");

    // Delete (atomically, several at once).
    let n = http.ok("mutation", &path("delete_documents"), json!({ "ids": [id, id2] }), &admin).await;
    assert_eq!(n, 2);
    assert_eq!(http.ok("query", &path("document"), json!({ "id": id }), &admin).await, Value::Null);
    let (_, b) = http.call("mutation", &path("delete_documents"), json!({ "ids": [id] }), Some(&admin)).await;
    assert_eq!(code_of(&b), "not_found");
    let (_, b) = http.call("query", &path("document"), json!({ "id": "nonsense" }), Some(&admin)).await;
    assert_eq!(code_of(&b), "invalid_id");

    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new(), "untyped writes maintain indexes");
}

struct Ws {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Ws {
    async fn connect(addr: SocketAddr, token: &str) -> Ws {
        let (stream, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
        let mut ws = Ws { stream };
        ws.send(json!({ "type": "Connect", "sessionId": "dash", "connectionCount": 0, "lastCloseReason": null })).await;
        ws.send(json!({ "type": "Authenticate", "baseVersion": 0, "token": token })).await;
        ws.recv_type("Transition").await;
        ws
    }

    async fn send(&mut self, v: Value) {
        self.stream.send(Message::text(v.to_string())).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.stream.next())
                .await
                .expect("timeout")
                .unwrap()
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
        assert_eq!(v["type"], ty, "{v}");
        v
    }

    /// The next value of query 0.
    async fn next_value(&mut self) -> Value {
        loop {
            let t = self.recv_type("Transition").await;
            if let Some(m) = t["modifications"].as_array().unwrap().iter().find(|m| m["queryId"] == 0) {
                assert_eq!(m["type"], "QueryUpdated", "{m}");
                return m["value"].clone();
            }
        }
    }

    /// No update for query 0 within `ms` (other messages are fine).
    async fn quiet(&mut self, ms: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout_at(deadline, self.stream.next()).await {
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            let touched = v["modifications"].as_array().is_some_and(|m| m.iter().any(|m| m["queryId"] == 0));
            assert!(!touched, "unexpected update {v}");
        }
    }
}

#[tokio::test]
async fn documents_subscription_is_live() {
    let (t, http, admin, _, _) = setup().await;
    let addr: SocketAddr = http.base.trim_start_matches("http://").parse().unwrap();
    let mut ws = Ws::connect(addr, &admin).await;
    ws.send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
        { "type": "Add", "queryId": 0, "udfPath": path("documents"), "args": {
            "table": "dash_items", "index": "by_category", "filters": [{ "field": "category", "op": "eq", "value": "live" }],
            "paginationOpts": { "numItems": 10, "cursor": null } } }
    ] }))
    .await;
    let first = ws.next_value().await;
    assert_eq!(first["page"], json!([]));

    // A typed mutation (from an anonymous client) shows up in the admin's untyped page.
    let (s, b) = http
        .call("mutation", &app::add_item::path(), json!({ "title": "x", "category": "live", "rank": 1 }), None)
        .await;
    assert_eq!(s, 200, "{b}");
    let v = ws.next_value().await;
    assert_eq!(v["page"][0]["title"], "x");
    assert_eq!(v["page"][0]["_id"], b["value"]);

    // Writes outside the read range (another category) don't rerun it.
    http.call("mutation", &app::add_item::path(), json!({ "title": "y", "category": "other", "rank": 1 }), None).await;
    ws.quiet(300).await;

    // A dashboard write updates it too.
    let id = b["value"].as_str().unwrap();
    http.ok("mutation", &path("patch_document"), json!({ "id": id, "patch": { "title": "x2" } }), &admin).await;
    assert_eq!(ws.next_value().await["page"][0]["title"], "x2");

    // Subscriptions are counted for the overview.
    let (_, stats) = http.get("/api/admin/stats", Some(&admin)).await;
    assert!(stats["overview"]["sockets"].as_u64().unwrap() >= 1);
    assert!(stats["overview"]["subscriptions"].as_u64().unwrap() >= 1);
    drop(t);
}

#[tokio::test]
async fn secrets_are_masked_and_system_tables_protected() {
    let (_t, http, admin, user, user_id) = setup().await;
    // A refresh creates a `_used_refresh_tokens` row.
    let (s, signin) =
        http.post("/api/auth/signin", None, json!({ "email": "user@example.com", "password": "user password" })).await;
    assert_eq!(s, 200);
    let (s, _) = http.post("/api/auth/refresh", None, json!({ "refreshToken": signin["refreshToken"] })).await;
    assert_eq!(s, 200);

    let tables = http.ok("query", &path("tables"), json!({}), &admin).await;
    let find = |name: &str| tables.as_array().unwrap().iter().find(|t| t["name"] == name).unwrap().clone();
    assert_eq!(find("_credentials")["secretFields"], json!(["passwordHash"]));
    assert_eq!(find("_sessions")["access"], "readOnly");
    assert_eq!(find("_users")["access"], "limited");
    assert_eq!(find("dash_items")["access"], "readWrite");
    assert_eq!(find("_users")["documents"], 1);
    let first_system = tables.as_array().unwrap().iter().position(|t| t["system"] == true).unwrap();
    assert!(tables.as_array().unwrap()[first_system..].iter().all(|t| t["system"] == true), "app tables come first");
    let session_indexes = find("_sessions")["indexes"].clone();
    let by_hash = session_indexes.as_array().unwrap().iter().find(|i| i["name"] == "by_refresh_token_hash").unwrap();
    assert_eq!(by_hash["usable"], false);
    // The analytics copy's columns, from the Rust types; tables with secrets are not in it.
    let columns: Vec<(String, String)> = find("dash_items")["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["name"].as_str().unwrap().into(), c["sqlType"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        &columns[..3],
        &[("_id".into(), "VARCHAR".into()), ("_creationTime".into(), "DOUBLE".into()), ("_ts".into(), "BIGINT".into())]
    );
    assert!(columns.contains(&("rank".into(), "BIGINT".into())), "{columns:?}");
    assert_eq!(
        (find("dash_items")["analytics"].clone(), find("_users")["analytics"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(
        (find("_credentials")["analytics"].clone(), find("_credentials")["columns"].clone()),
        (json!(false), json!([]))
    );

    for (table, fields) in [
        ("_credentials", vec!["passwordHash"]),
        ("_admins", vec!["passwordHash"]),
        ("_sessions", vec!["refreshTokenHash"]),
        ("_used_refresh_tokens", vec!["tokenHash"]),
    ] {
        let (s, raw) = http.call("query", &path("documents"), page(table), Some(&admin)).await;
        assert_eq!(s, 200, "{raw}");
        let docs = raw["value"]["page"].as_array().unwrap();
        assert!(!docs.is_empty(), "{table}");
        for d in docs {
            for f in &fields {
                assert_eq!(d[f], "••••", "{table}.{f}");
            }
        }
        let text = raw.to_string();
        assert!(!text.contains("argon2"), "{table} leaked a password hash");
        // One document by id is masked too.
        let doc = http.ok("query", &path("document"), json!({ "id": docs[0]["_id"] }), &admin).await;
        assert_eq!(doc["document"][fields[0]], "••••");
    }
    // Hash indexes can't be used to probe hashes.
    let (_, b) = http
        .call("query", &path("documents"), json!({ "table": "_sessions", "index": "by_refresh_token_hash",
            "filters": [{ "field": "refreshTokenHash", "op": "gt", "value": "8" }], "paginationOpts": { "numItems": 5, "cursor": null } }), Some(&admin))
        .await;
    assert_eq!(code_of(&b), "index_not_allowed", "{b}");

    // System tables are read-only, except the profile fields of `_users`.
    let cred = http.ok("query", &path("documents"), page("_credentials"), &admin).await["page"][0]["_id"].clone();
    for (kind, name, args) in [
        (
            "insert_document",
            "insert",
            json!({ "table": "_credentials", "document": { "user": user_id, "passwordHash": "x" } }),
        ),
        ("patch_document", "update", json!({ "id": cred, "patch": { "passwordHash": "x" } })),
        ("replace_document", "replace", json!({ "id": cred, "document": { "user": user_id, "passwordHash": "x" } })),
        ("delete_documents", "delete", json!({ "ids": [cred] })),
        ("delete_documents", "delete", json!({ "ids": [user_id] })),
        ("insert_document", "insert", json!({ "table": "_users", "document": { "emailVerified": true } })),
    ] {
        let (_, b) = http.call("mutation", &path(kind), args, Some(&admin)).await;
        assert_eq!(code_of(&b), "read_only_table", "{kind} {name}: {b}");
    }
    http.ok(
        "mutation",
        &path("patch_document"),
        json!({ "id": user_id, "patch": { "name": "Ada", "emailVerified": true } }),
        &admin,
    )
    .await;
    let (_, b) = http
        .call(
            "mutation",
            &path("patch_document"),
            json!({ "id": user_id, "patch": { "email": "evil@example.com" } }),
            Some(&admin),
        )
        .await;
    assert_eq!(code_of(&b), "read_only_field", "{b}");
    // A full-document save from the editor works when only editable fields changed.
    let mut doc = http.ok("query", &path("document"), json!({ "id": user_id }), &admin).await["document"].clone();
    doc["name"] = json!("Ada L.");
    http.ok("mutation", &path("replace_document"), json!({ "id": user_id, "document": doc.clone() }), &admin).await;
    doc["email"] = json!("other@example.com");
    let (_, b) =
        http.call("mutation", &path("replace_document"), json!({ "id": user_id, "document": doc }), Some(&admin)).await;
    assert_eq!(code_of(&b), "read_only_field");
    let (_, b) = http
        .call(
            "mutation",
            &path("patch_document"),
            json!({ "id": user_id, "patch": { "image": "javascript:alert(1)" } }),
            Some(&admin),
        )
        .await;
    assert_eq!(code_of(&b), "invalid_image");
    let me = http.ok("query", "_system/auth:me", json!({}), &user).await;
    assert_eq!(
        (&me["name"], &me["email"], &me["emailVerified"]),
        (&json!("Ada L."), &json!("user@example.com"), &json!(true))
    );
}

#[tokio::test]
async fn run_function_as_admin_or_as_a_user() {
    let (t, http, admin, _, user_id) = setup().await;
    let run_path = path("run_function");
    let run = |args: Value| http.ok("action", &run_path, args, &admin);

    // A public mutation as the admin.
    let r = run(json!({ "path": app::add_item::path(), "args": { "title": "r", "category": "c", "rank": 1 } })).await;
    assert_eq!((&r["success"], &r["kind"]), (&json!(true), &json!("mutation")), "{r}");
    assert!(r["value"].is_string() && r["ts"].as_u64().unwrap() > 0 && r["durationMs"].as_f64().unwrap() >= 0.0);
    assert!(r["identity"].as_str().unwrap().starts_with("admin:"));

    // An internal function as a user: identity, log lines and result.
    let r =
        run(json!({ "path": app::whoami_logged::path(), "args": { "greeting": "hello" }, "asUser": user_id })).await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["value"], user_id.as_str());
    assert_eq!(r["identity"], format!("user:{user_id}"));
    assert!(r["logLines"][0].as_str().unwrap().starts_with("[INFO] hello"), "{r}");
    // A system function as the user.
    let r = run(json!({ "path": "_system/auth:me", "asUser": user_id })).await;
    assert_eq!(r["value"]["email"], "user@example.com");

    // Failures are results, not errors: the same internal function as the admin is unauthenticated.
    let r = run(json!({ "path": app::whoami_logged::path(), "args": { "greeting": "x" } })).await;
    assert_eq!((&r["success"], &r["error"]["code"]), (&json!(false), &json!("unauthenticated")), "{r}");
    let r = run(json!({ "path": app::add_item::path(), "args": { "title": 1 } })).await;
    assert!(r["error"]["message"].as_str().unwrap().contains("Invalid args"), "{r}");

    // Bad requests are errors.
    let (_, b) = http.call("action", &path("run_function"), json!({ "path": "nope:nothing" }), Some(&admin)).await;
    assert_eq!(code_of(&b), "not_found");
    let (_, b) = http
        .call(
            "action",
            &path("run_function"),
            json!({ "path": app::all_items::path(), "asUser": "garbage" }),
            Some(&admin),
        )
        .await;
    assert_eq!(code_of(&b), "invalid_id");
    let (_, b) = http
        .call(
            "action",
            &path("run_function"),
            json!({ "path": app::all_items::path(), "asUser": "00000000000000000000000000" }),
            Some(&admin),
        )
        .await;
    assert_eq!(code_of(&b), "not_found", "{b}");

    // The internal function's execution is in the log, attributed to the user.
    t.app().logs().flush().await.unwrap();
    let (s, logs) = http
        .get(
            &format!("/api/admin/logs?pathPrefix={}&identity=user:{user_id}", app::whoami_logged::path()),
            Some(&admin),
        )
        .await;
    assert_eq!(s, 200, "{logs}");
    assert_eq!(logs["entries"].as_array().unwrap().len(), 1, "{logs}");
}

#[tokio::test]
async fn logs_history_and_live_tail() {
    let (t, http, admin, _, _) = setup().await;
    for i in 0..3 {
        http.call(
            "mutation",
            &app::add_item::path(),
            json!({ "title": format!("l{i}"), "category": "logs", "rank": i }),
            None,
        )
        .await;
    }
    http.call("mutation", &app::add_item::path(), json!({ "title": 1 }), None).await;
    t.app().logs().flush().await.unwrap();

    let (s, b) = http.get(&format!("/api/admin/logs?pathPrefix={}&limit=2", app::add_item::path()), Some(&admin)).await;
    assert_eq!(s, 200, "{b}");
    let entries = b["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries[0]["id"].as_i64() > entries[1]["id"].as_i64(), "newest first");
    let (_, b) = http.get("/api/admin/logs?outcome=userError&kind=mutation", Some(&admin)).await;
    assert!(b["entries"].as_array().unwrap().iter().all(|e| e["outcome"] == "userError" && e["kind"] == "mutation"));
    assert!(!b["entries"].as_array().unwrap().is_empty());
    let (_, b) = http.get("/api/admin/logs?excludeSystem=true&limit=1000", Some(&admin)).await;
    assert!(b["entries"].as_array().unwrap().iter().all(|e| !e["path"].as_str().unwrap().starts_with("_system")));
    let (s, _) = http.get("/api/admin/logs?limit=lots", Some(&admin)).await;
    assert_eq!(s, 400);

    // Live tail over SSE with the bearer token.
    let res = http
        .client
        .get(format!("{}/api/admin/logs/stream?excludeSystem=true&pathPrefix={}", http.base, app::add_item::path()))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let mut body = res;
    let mut buf = String::new();
    let mut next_event = async || -> (String, String) {
        loop {
            if let Some(end) = buf.find("\n\n") {
                let raw: String = buf.drain(..end + 2).collect();
                let mut event = String::new();
                let mut data = String::new();
                for line in raw.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        event = e.to_string();
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push_str(d);
                    }
                }
                if !event.is_empty() {
                    return (event, data);
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), body.chunk())
                .await
                .expect("sse timeout")
                .unwrap()
                .unwrap();
            buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    };
    assert_eq!(next_event().await.0, "ready");
    // Not matching the filter: skipped. Matching: delivered with its id.
    http.call("query", &app::all_items::path(), json!({}), None).await;
    http.call("mutation", &app::add_item::path(), json!({ "title": "tail", "category": "logs", "rank": 9 }), None)
        .await;
    let (event, data) = next_event().await;
    assert_eq!(event, "log");
    let entry: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(entry["path"], app::add_item::path());
    assert!(entry["args"].as_str().unwrap().contains("tail"));
    assert!(entry["id"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn users_sessions_revoke_and_delete() {
    let (t, http, admin, user_token, user_id) = setup().await;
    let (_, second) =
        http.post("/api/auth/signin", None, json!({ "email": "user@example.com", "password": "user password" })).await;

    let users =
        http.ok("query", &path("users"), json!({ "paginationOpts": { "numItems": 10, "cursor": null } }), &admin).await;
    let u = &users["page"][0];
    assert_eq!(
        (&u["_id"], &u["email"], &u["hasPassword"], &u["sessions"]),
        (&json!(user_id), &json!("user@example.com"), &json!(true), &json!(2))
    );
    assert_eq!(u["providers"], json!([]));
    let found = http
        .ok(
            "query",
            &path("users"),
            json!({ "search": "USER@", "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await;
    assert_eq!(found["page"].as_array().unwrap().len(), 1);
    let none = http
        .ok(
            "query",
            &path("users"),
            json!({ "search": "zzz", "paginationOpts": { "numItems": 10, "cursor": null } }),
            &admin,
        )
        .await;
    assert_eq!(none["page"], json!([]));

    let sessions = http.ok("query", &path("user_sessions"), json!({ "user": user_id }), &admin).await;
    let sessions = sessions.as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().all(|s| s.get("refreshTokenHash").is_none() && s["expired"] == false));

    // Revoke the second session: its refresh token stops working; the first still works.
    // The most recently used session is the second sign-in's.
    let sid = sessions[0]["_id"].clone();
    http.ok("mutation", &path("revoke_session"), json!({ "session": sid }), &admin).await;
    let (_, b) = http.call("mutation", &path("revoke_session"), json!({ "session": sid }), Some(&admin)).await;
    assert_eq!(code_of(&b), "not_found");
    let remaining = http.ok("query", &path("user_sessions"), json!({ "user": user_id }), &admin).await;
    assert_eq!(remaining.as_array().unwrap().len(), 1);
    let statuses = [
        http.call("query", "_system/auth:me", json!({}), Some(&user_token)).await.0,
        http.call("query", "_system/auth:me", json!({}), Some(second["accessToken"].as_str().unwrap())).await.0,
    ];
    assert_eq!(
        statuses.iter().filter(|s| **s == 401).count(),
        1,
        "exactly the revoked session is refused: {statuses:?}"
    );

    // Delete the user with everything that belongs to them.
    let deleted = http.ok("mutation", &path("delete_user"), json!({ "user": user_id }), &admin).await;
    assert_eq!((&deleted["sessions"], &deleted["credentials"]), (&json!(1), &json!(1)), "{deleted}");
    let (s, _) =
        http.post("/api/auth/signin", None, json!({ "email": "user@example.com", "password": "user password" })).await;
    assert_eq!(s, 401);
    let users =
        http.ok("query", &path("users"), json!({ "paginationOpts": { "numItems": 10, "cursor": null } }), &admin).await;
    assert_eq!(users["page"], json!([]));
    for table in ["_credentials", "_used_refresh_tokens"] {
        assert_eq!(http.ok("query", &path("documents"), page(table), &admin).await["page"], json!([]), "{table}");
    }
    let sessions = http.ok("query", &path("documents"), page("_sessions"), &admin).await;
    assert!(sessions["page"].as_array().unwrap().iter().all(|s| s["user"].is_null()), "only admin sessions remain");
    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new());
}

#[tokio::test]
async fn stats_timeseries_and_overview_counters() {
    let (t, http, admin, user, _) = setup().await;
    for i in 0..4 {
        http.call(
            "mutation",
            &app::add_item::path(),
            json!({ "title": format!("ts{i}"), "category": "ts", "rank": i }),
            None,
        )
        .await;
    }
    http.call("mutation", &app::add_item::path(), json!({ "title": 1 }), None).await; // a user error
    t.app().logs().flush().await.unwrap();

    let endpoint = format!("/api/admin/stats/timeseries?path={}&bucket=1m", app::add_item::path());
    let (s, b) = http.get(&endpoint, None).await;
    assert_eq!((s, code_of(&b)), (401, "unauthenticated"));
    let (s, b) = http.get(&endpoint, Some(&user)).await;
    assert_eq!((s, code_of(&b)), (403, "forbidden"));

    let (s, b) = http.get(&endpoint, Some(&admin)).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!((b["path"].as_str(), b["bucket"].as_i64()), (Some(app::add_item::path().as_str()), Some(60_000)));
    assert_eq!(b["since"].as_i64().unwrap() % 60_000, 0, "aligned to the bucket");
    let points = b["points"].as_array().unwrap();
    let calls: u64 = points.iter().map(|p| p["calls"].as_u64().unwrap()).sum();
    let errors: u64 = points.iter().map(|p| p["errors"].as_u64().unwrap()).sum();
    assert_eq!((calls, errors), (5, 1), "{b}");
    let p = &points[0];
    assert!(p["t"].as_i64().unwrap() >= b["since"].as_i64().unwrap());
    assert!(p["p95Us"].as_u64().unwrap() >= p["p50Us"].as_u64().unwrap() && p["maxUs"].as_u64() >= p["p95Us"].as_u64());

    // Without a path: every app function; system functions (the admin's own calls) are left out.
    http.ok("query", &path("tables"), json!({}), &admin).await;
    t.app().logs().flush().await.unwrap();
    let (s, b) = http.get("/api/admin/stats/timeseries?bucket=3600000", Some(&admin)).await;
    assert_eq!(s, 200, "{b}");
    let calls: u64 = b["points"].as_array().unwrap().iter().map(|p| p["calls"].as_u64().unwrap()).sum();
    assert_eq!(calls, 5, "{b}");
    // An unknown function has no points.
    let (_, b) = http.get("/api/admin/stats/timeseries?path=nope:nothing", Some(&admin)).await;
    assert_eq!(b["points"], json!([]));

    for bad in ["bucket=0", "bucket=5x", "bucket=1s&since=0", "since=10&until=5", "since=abc"] {
        let (s, b) = http.get(&format!("/api/admin/stats/timeseries?{bad}"), Some(&admin)).await;
        assert_eq!((s, code_of(&b)), (400, "invalid_request"), "{bad}: {b}");
    }

    // The overview's live counters.
    let (s, b) = http.get("/api/admin/stats", Some(&admin)).await;
    assert_eq!(s, 200);
    let o = &b["overview"];
    for field in ["commitsPerSecond", "queueDepth", "runningJobs", "subscriptionRounds", "queryReruns"] {
        assert!(o[field].is_number(), "{field}: {o}");
    }
    assert!(o["commitsPerSecond"].as_f64().unwrap() > 0.0, "{o}");
    assert_eq!(o["metricsEnabled"], false);
}

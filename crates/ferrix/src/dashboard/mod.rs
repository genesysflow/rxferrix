//! The admin dashboard's backend (PLAN.md §6): admin-only system functions, the `/api/admin/*` HTTP endpoints
//! ([`http`]) and the embedded single-page app at `/_/` ([`assets`], cargo feature `dashboard`).
//!
//! The functions below are ordinary system functions (paths `_system/dashboard:<name>`) defined with the normal
//! macros, so the dashboard calls them over the regular sync protocol and every view is live. Each one starts with
//! `ctx.auth.require_admin()`: anonymous callers get the user error `unauthenticated`, signed-in users `forbidden`.
//!
//! | Function | Kind | What it does |
//! |---|---|---|
//! | `tables` | query | Every table (system tables included): document count, indexes, TS type, secret fields. |
//! | `documents` | query | An untyped, paginated, live scan of a table by name, with an index, eq/range filters and order. |
//! | `document` | query | One document by id (any table), with its table name. |
//! | `insert_document`, `patch_document`, `replace_document`, `delete_documents` | mutation | Untyped writes, validated with `TableDef::validate`. |
//! | `functions` | query | The function registry, internal and system functions included. |
//! | `run_function` | action | Run any function by path with JSON args, optionally as a given user. |
//! | `users`, `user_sessions`, `revoke_session`, `delete_user` | query / mutation | The Users page. |
//! | `me` | query | The signed-in admin. |
//!
//! Secrets are never returned and system tables are mostly read-only; see [`secrets`].

#[cfg(feature = "dashboard")]
pub mod assets;
pub mod http;
pub mod secrets;

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::auth::store;
use crate::auth::tables::{Account, Admin, Credential, Session, User, VerificationCode};
use crate::db::catalog::TableInfo;
use crate::error::ClientError;
use crate::prelude::*;
use crate::private::{FunctionKind, is_system_path};
use crate::runtime::Identity;
use crate::schema::{BY_CREATION_TIME, CREATION_TIME_FIELD, ID_FIELD};
use secrets::{EDITABLE_USER_FIELDS, USERS};

fn user_error(code: &str, message: impl Into<String>) -> Error {
    UserError::new(code, message).into()
}

/// Engine errors (unknown table, bad range, validation failure, ...) are the admin's input errors: report them with
/// their full message.
fn input_error(code: &'static str) -> impl FnOnce(Error) -> Error {
    move |e| {
        if e.chain().any(|c| c.is::<UserError>()) {
            return e;
        }
        user_error(code, format!("{e:#}"))
    }
}

fn table_of(db: &crate::QueryDb, id: &str) -> Result<std::sync::Arc<TableInfo>> {
    let doc_id = crate::id::DocId::decode(id).map_err(|_| user_error("invalid_id", format!("`{id}` is not a document id")))?;
    db.catalog()
        .table_by_number(doc_id.table_number())
        .cloned()
        .ok_or_else(|| user_error("invalid_id", format!("id `{id}` belongs to no table")))
}

fn parse_id<T>(id: &str, what: &str) -> Result<Id<T>> {
    id.parse().map_err(|_| user_error("invalid_id", format!("`{id}` is not a valid {what} id")))
}

// ---------------------------------------------------------------------------------------------------------------------
// Tables and documents
// ---------------------------------------------------------------------------------------------------------------------

/// How the dashboard may write a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum TableAccess {
    /// Insert, edit and delete documents.
    ReadWrite,
    /// `_users`: only `name`, `image` and `emailVerified` can be edited.
    Limited,
    /// System tables and tables no longer defined in code.
    ReadOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct IndexSummary {
    pub name: String,
    /// JSON field names (empty for `by_id`).
    pub fields: Vec<String>,
    /// `by_id` and `by_creation_time`.
    pub builtin: bool,
    /// Whether `documents` can scan it (not `by_id`, no secret fields).
    pub usable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct TableSummary {
    pub name: String,
    /// The table number (the first 4 bytes of its ids).
    pub number: u32,
    /// A ferrix system table (`_`-prefixed).
    pub system: bool,
    /// Defined in the code of this binary (otherwise only its stored documents remain).
    pub defined: bool,
    pub documents: u64,
    pub indexes: Vec<IndexSummary>,
    /// The TS type of the table's documents (user fields), when defined in code.
    pub ts: Option<String>,
    /// Fields whose values are masked.
    pub secret_fields: Vec<String>,
    pub access: TableAccess,
}

fn access(table: &TableInfo) -> TableAccess {
    match () {
        _ if table.name == USERS => TableAccess::Limited,
        _ if secrets::is_system_table(&table.name) || table.def.is_none() => TableAccess::ReadOnly,
        _ => TableAccess::ReadWrite,
    }
}

/// Every table, app tables first (by name), then system tables. Live: document counts update with every write.
#[query]
pub async fn tables(ctx: &QueryCtx) -> Result<Vec<TableSummary>> {
    ctx.auth.require_admin()?;
    let mut tables: Vec<_> = ctx.db.catalog().tables().cloned().collect();
    tables.sort_by(|a, b| (a.name.starts_with('_'), &a.name).cmp(&(b.name.starts_with('_'), &b.name)));
    let mut out = Vec::with_capacity(tables.len());
    for t in tables {
        let documents = ctx.db.count(&t.name).await?;
        let mut indexes: Vec<IndexSummary> = t
            .indexes
            .iter()
            .map(|i| IndexSummary {
                name: i.name.clone(),
                fields: i.fields.clone(),
                builtin: i.name == crate::schema::BY_ID || i.name == BY_CREATION_TIME,
                usable: secrets::index_usable(&t.name, i),
            })
            .collect();
        indexes.sort_by(|a, b| (!a.builtin, &a.name).cmp(&(!b.builtin, &b.name)));
        out.push(TableSummary {
            name: t.name.clone(),
            number: t.number,
            system: secrets::is_system_table(&t.name),
            defined: t.def.is_some(),
            documents,
            indexes,
            ts: t.def.as_ref().map(|d| (d.ts)()),
            secret_fields: secrets::secret_fields(&t),
            access: access(&t),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "lowercase")]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "lowercase")]
pub enum FilterOp {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// A constraint on an index field: equality on a prefix of the index's fields, then at most one lower and one upper
/// bound on the next field (the rules of `with_index`).
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct IndexFilter {
    pub field: String,
    pub op: FilterOp,
    pub value: Value,
}

/// A page of `table`'s documents as JSON (secrets masked), scanned through `index` (default `by_creation_time`) with
/// `filters` on its fields, in `order` (default ascending). Records read sets like any query, so a subscribed page
/// updates live. Use it like any paginated query (`usePaginatedQuery`).
#[query]
pub async fn documents(
    ctx: &QueryCtx,
    table: String,
    index: Option<String>,
    filters: Option<Vec<IndexFilter>>,
    order: Option<SortOrder>,
    pagination_opts: PaginationOpts,
) -> Result<PaginationResult<Value>> {
    ctx.auth.require_admin()?;
    let info = ctx.db.catalog().table(&table).map_err(input_error("unknown_table"))?.clone();
    let index = index.unwrap_or_else(|| BY_CREATION_TIME.to_string());
    if let Some(i) = info.index(&index)
        && !secrets::index_usable(&info.name, i)
    {
        return Err(user_error("index_not_allowed", format!("Index `{index}` can't be used to browse `{table}`")));
    }
    let filters = filters.unwrap_or_default();
    let order = match order.unwrap_or_default() {
        SortOrder::Asc => Order::Asc,
        SortOrder::Desc => Order::Desc,
    };
    let page = ctx
        .db
        .query_table(&table)
        .with_index(&index, |range| {
            filters.iter().fold(range, |r, f| {
                let v = f.value.clone();
                match f.op {
                    FilterOp::Eq => r.eq(&f.field, v),
                    FilterOp::Gt => r.gt(&f.field, v),
                    FilterOp::Gte => r.gte(&f.field, v),
                    FilterOp::Lt => r.lt(&f.field, v),
                    FilterOp::Lte => r.lte(&f.field, v),
                }
            })
        })
        .order(order)
        .paginate(pagination_opts)
        .await
        .map_err(input_error("invalid_query"))?;
    Ok(page.map(|doc| secrets::mask(&info.name, doc)))
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct TableDocument {
    pub table: String,
    pub document: Value,
}

/// The document with this id (any table; the table number is part of the id), secrets masked. `null` if it doesn't
/// exist.
#[query]
pub async fn document(ctx: &QueryCtx, id: String) -> Result<Option<TableDocument>> {
    ctx.auth.require_admin()?;
    table_of(&ctx.db, &id)?;
    let found = ctx.db.get_raw(&id).await.map_err(input_error("invalid_id"))?;
    Ok(found.map(|(table, doc)| TableDocument { table: table.name.clone(), document: secrets::mask(&table.name, doc) }))
}

/// The user fields of a document from the editor: system fields are dropped (`_id`, if present, must match).
fn user_fields(document: Value, id: Option<&str>) -> Result<Value> {
    let Value::Object(mut fields) = document else {
        return Err(user_error("invalid_document", "A document must be a JSON object"));
    };
    if let (Some(id), Some(given)) = (id, fields.get(ID_FIELD))
        && given.as_str() != Some(id)
    {
        return Err(user_error("invalid_document", "`_id` can't be changed"));
    }
    fields.remove(ID_FIELD);
    fields.remove(CREATION_TIME_FIELD);
    Ok(Value::Object(fields))
}

/// Insert a document (JSON object, system fields ignored) into an app table. Validated against the table's type.
#[mutation]
pub async fn insert_document(ctx: &MutationCtx, table: String, document: Value) -> Result<String> {
    ctx.auth.require_admin()?;
    secrets::forbid_system_write(&table, "insert")?;
    let fields = user_fields(document, None)?;
    ctx.db.insert_raw(&table, fields).await.map_err(input_error("invalid_document"))
}

/// The `_users` fields changed by `new` relative to `old`; only [`EDITABLE_USER_FIELDS`] may change.
fn user_profile_patch(old: &Value, new: &Map<String, Value>, replace: bool) -> Result<Map<String, Value>> {
    let old = old.as_object().cloned().unwrap_or_default();
    let mut patch = Map::new();
    let keys: std::collections::BTreeSet<&String> =
        if replace { old.keys().chain(new.keys()).collect() } else { new.keys().collect() };
    for key in keys {
        if key == ID_FIELD || key == CREATION_TIME_FIELD {
            continue;
        }
        let after = new.get(key).cloned().unwrap_or(Value::Null);
        if old.get(key).cloned().unwrap_or(Value::Null) == after {
            continue;
        }
        if !EDITABLE_USER_FIELDS.contains(&key.as_str()) {
            return Err(user_error(
                "read_only_field",
                format!("`{key}` of a user can't be edited here; only `name`, `image` and `emailVerified` can"),
            ));
        }
        match (key.as_str(), &after) {
            ("name", Value::String(s)) if s.chars().count() > 200 => {
                return Err(user_error("invalid_name", "Name must be at most 200 characters"));
            }
            ("image", Value::String(s)) if !(s.starts_with("https://") || s.starts_with("http://")) || s.len() > 2048 => {
                return Err(user_error("invalid_image", "Image must be an http(s) URL"));
            }
            _ => {}
        }
        patch.insert(key.clone(), after);
    }
    Ok(patch)
}

/// Shared by `patch_document` and `replace_document`: `_users` gets a profile-only patch, other system tables are
/// refused, app tables are written as asked.
async fn write_existing(ctx: &MutationCtx, id: &str, document: Value, replace: bool) -> Result<()> {
    let table = table_of(&ctx.db, id)?;
    if table.name == USERS {
        let Value::Object(new) = user_fields(document, Some(id))? else { unreachable!("user_fields returns an object") };
        let Some((_, old)) = ctx.db.get_raw(id).await? else {
            return Err(user_error("not_found", format!("Document {id} does not exist")));
        };
        let patch = user_profile_patch(&old, &new, replace)?;
        if patch.is_empty() {
            return Ok(());
        }
        return ctx.db.patch_raw(id, Value::Object(patch)).await.map_err(input_error("invalid_document"));
    }
    secrets::forbid_system_write(&table.name, if replace { "replace" } else { "update" })?;
    let result = if replace {
        ctx.db.replace_raw(id, user_fields(document, Some(id))?).await
    } else {
        if !document.is_object() {
            return Err(user_error("invalid_document", "A patch must be a JSON object"));
        }
        ctx.db.patch_raw(id, document).await
    };
    result.map_err(input_error("invalid_document"))
}

/// Shallow-merge `patch` into a document (`null` sets a field to null). The result must match the table's type.
#[mutation]
pub async fn patch_document(ctx: &MutationCtx, id: String, patch: Value) -> Result<()> {
    ctx.auth.require_admin()?;
    write_existing(ctx, &id, patch, false).await
}

/// Replace a document's user fields (system fields in `document` are ignored; `_id` must match if given).
#[mutation]
pub async fn replace_document(ctx: &MutationCtx, id: String, document: Value) -> Result<()> {
    ctx.auth.require_admin()?;
    write_existing(ctx, &id, document, true).await
}

/// Delete documents of app tables, all in one transaction. Returns how many were deleted.
#[mutation]
pub async fn delete_documents(ctx: &MutationCtx, ids: Vec<String>) -> Result<u32> {
    ctx.auth.require_admin()?;
    for id in &ids {
        secrets::forbid_system_write(&table_of(&ctx.db, id)?.name, "delete")?;
    }
    for id in &ids {
        ctx.db.delete_raw(id).await.map_err(input_error("not_found"))?;
    }
    Ok(ids.len() as u32)
}

// ---------------------------------------------------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct FunctionSummary {
    pub path: String,
    /// `"query"`, `"mutation"` or `"action"`.
    pub kind: String,
    /// `"public"` or `"internal"`.
    pub visibility: String,
    /// A ferrix built-in (`_system/...`).
    pub system: bool,
    pub args_ts: String,
    pub output_ts: String,
}

/// Every registered function, sorted by path.
#[query]
pub async fn functions(ctx: &QueryCtx) -> Result<Vec<FunctionSummary>> {
    ctx.auth.require_admin()?;
    Ok(crate::runtime::registry()?
        .list()
        .into_iter()
        .map(|f| FunctionSummary {
            system: is_system_path(&f.path),
            path: f.path,
            kind: f.kind.as_str().to_string(),
            visibility: f.visibility.as_str().to_string(),
            args_ts: f.args_ts,
            output_ts: f.output_ts,
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct RunError {
    /// The full error message (admins see server errors in full, as in the execution log).
    pub message: String,
    /// The `UserError` code, if it was one.
    pub code: Option<String>,
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct RunResult {
    pub kind: String,
    pub success: bool,
    /// The function's return value (`null` on failure).
    pub value: Value,
    pub error: Option<RunError>,
    pub log_lines: Vec<String>,
    pub duration_ms: f64,
    /// The snapshot ts (query) or commit ts (mutation).
    pub ts: Option<u64>,
    /// Who it ran as (`"user:<id>"`, `"admin:<id>"`).
    pub identity: Option<String>,
}

/// Run any registered function (public, internal or system) by path with JSON `args`. Runs as the calling admin,
/// or with `as_user` as that user (an identity without a session). A failing function still returns normally, with
/// `success: false`, its error, its log lines and its duration.
#[action]
pub async fn run_function(ctx: &ActionCtx, path: String, args: Option<Value>, as_user: Option<String>) -> Result<RunResult> {
    ctx.auth.require_admin()?;
    let app = &ctx.app;
    let Some(def) = app.registry().get(&path) else {
        return Err(user_error("not_found", format!("No function is registered at `{path}`")));
    };
    let identity = match as_user.filter(|u| !u.is_empty()) {
        None => ctx.auth.identity().clone(),
        Some(user) => {
            let id: Id<User> = parse_id(&user, "user")?;
            if store::read(app, |db| async move { db.get(id).await }).await.ok().flatten().is_none() {
                return Err(user_error("not_found", format!("User {user} does not exist")));
            }
            Identity::user(id, None)
        }
    };
    let args = args.filter(|a| !a.is_null()).unwrap_or_else(|| json!({}));
    let kind = def.handler.kind();
    let started = Instant::now();
    let (result, ts, log_lines) = match kind {
        FunctionKind::Query => {
            let run = app.call_query(&path, args, &identity, true).await;
            (run.result, Some(run.ts), run.log_lines)
        }
        FunctionKind::Mutation => {
            let run = app.call_mutation(&path, args, &identity, true, None).await;
            let ts = run.result.as_ref().ok().map(|(_, ts)| *ts);
            (run.result.map(|(v, _)| v), ts, run.log_lines)
        }
        FunctionKind::Action => {
            let run = app.call_action(&path, args, &identity, true).await;
            (run.result, None, run.log_lines)
        }
    };
    let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    let (success, value, error) = match result {
        Ok(v) => (true, v, None),
        Err(e) => {
            let ce = ClientError::from_anyhow(&e, true);
            let code = ce.data.as_ref().and_then(|d| d["code"].as_str()).map(str::to_string);
            let data = ce.data.as_ref().map(|d| d["data"].clone()).filter(|d| !d.is_null());
            (false, Value::Null, Some(RunError { message: ce.message, code, data }))
        }
    };
    Ok(RunResult {
        kind: kind.as_str().to_string(),
        success,
        value,
        error,
        log_lines,
        duration_ms,
        ts,
        identity: identity.log_summary(),
    })
}

// ---------------------------------------------------------------------------------------------------------------------
// Users and sessions
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct UserSummary {
    #[serde(rename = "_id")]
    #[ts(rename = "_id")]
    pub id: String,
    #[serde(rename = "_creationTime")]
    #[ts(rename = "_creationTime")]
    pub creation_time: f64,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    pub image: Option<String>,
    /// Linked OAuth providers, e.g. `["github"]`.
    pub providers: Vec<String>,
    pub has_password: bool,
    /// Unexpired sessions.
    pub sessions: u32,
}

/// Users, newest first; with `search`, the users whose email starts with it (by email). Paginated and live.
#[query]
pub async fn users(
    ctx: &QueryCtx,
    search: Option<String>,
    pagination_opts: PaginationOpts,
) -> Result<PaginationResult<UserSummary>> {
    ctx.auth.require_admin()?;
    let search = search.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let query = match search {
        Some(prefix) => {
            let end = format!("{prefix}{}", char::MAX);
            ctx.db
                .query::<User>()
                .with_index(User::by_email, |q| q.gte(User::email, Some(prefix)).lt(User::email, Some(end)))
        }
        None => ctx.db.query::<User>().order(Order::Desc),
    };
    let page = query.paginate(pagination_opts).await?;
    let now = ctx.now().max(crate::db::wall_clock_ms());
    let mut out = Vec::with_capacity(page.page.len());
    for u in &page.page {
        let accounts =
            ctx.db.query::<Account>().with_index(Account::by_user, |q| q.eq(Account::user, u.id)).collect().await?;
        let has_password =
            ctx.db.query::<Credential>().with_index(Credential::by_user, |q| q.eq(Credential::user, u.id)).first().await?.is_some();
        let sessions = ctx
            .db
            .query::<Session>()
            .with_index(Session::by_user, |q| q.eq(Session::user, Some(u.id)))
            .collect()
            .await?
            .iter()
            .filter(|s| s.expires_at > now)
            .count();
        out.push(UserSummary {
            id: u.id.to_string(),
            creation_time: u.creation_time,
            email: u.email.clone(),
            email_verified: u.email_verified,
            name: u.name.clone(),
            image: u.image.clone(),
            providers: accounts.into_iter().map(|a| a.value.provider).collect(),
            has_password,
            sessions: sessions as u32,
        });
    }
    Ok(PaginationResult { page: out, is_done: page.is_done, continue_cursor: page.continue_cursor })
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct SessionSummary {
    #[serde(rename = "_id")]
    #[ts(rename = "_id")]
    pub id: String,
    #[serde(rename = "_creationTime")]
    #[ts(rename = "_creationTime")]
    pub creation_time: f64,
    pub expires_at: f64,
    pub last_used_at: f64,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
    pub expired: bool,
}

/// A user's sessions, most recently used first (no token hashes).
#[query]
pub async fn user_sessions(ctx: &QueryCtx, user: String) -> Result<Vec<SessionSummary>> {
    ctx.auth.require_admin()?;
    let user: Id<User> = parse_id(&user, "user")?;
    let now = ctx.now().max(crate::db::wall_clock_ms());
    let mut sessions: Vec<SessionSummary> = ctx
        .db
        .query::<Session>()
        .with_index(Session::by_user, |q| q.eq(Session::user, Some(user)))
        .collect()
        .await?
        .into_iter()
        .map(|s| SessionSummary {
            id: s.id.to_string(),
            creation_time: s.creation_time,
            expires_at: s.expires_at,
            last_used_at: s.last_used_at,
            expired: s.expires_at <= now,
            user_agent: s.value.user_agent,
            ip: s.value.ip,
        })
        .collect();
    sessions.sort_by(|a, b| b.last_used_at.total_cmp(&a.last_used_at));
    Ok(sessions)
}

/// End a session (user or admin): its refresh token stops working at once, and new connections with its access
/// tokens are refused. Sockets already authenticated with it keep their identity until the access token expires.
#[mutation]
pub async fn revoke_session(ctx: &MutationCtx, session: String) -> Result<()> {
    ctx.auth.require_admin()?;
    let id: Id<Session> = parse_id(&session, "session")?;
    if ctx.db.get(id).await?.is_none() {
        return Err(user_error("not_found", format!("Session {session} does not exist")));
    }
    store::delete_session_in(&ctx.db, id).await
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct DeletedUser {
    pub sessions: u32,
    pub accounts: u32,
    pub credentials: u32,
    pub verification_codes: u32,
}

/// Delete a user together with their sessions (and rotated refresh tokens), linked OAuth accounts, password and
/// pending email codes. Documents of app tables that reference the user are left alone.
#[mutation]
pub async fn delete_user(ctx: &MutationCtx, user: String) -> Result<DeletedUser> {
    ctx.auth.require_admin()?;
    let id: Id<User> = parse_id(&user, "user")?;
    let Some(doc) = ctx.db.get(id).await? else {
        return Err(user_error("not_found", format!("User {user} does not exist")));
    };
    let db = &ctx.db;
    let mut out = DeletedUser::default();
    for s in db.query::<Session>().with_index(Session::by_user, |q| q.eq(Session::user, Some(id))).collect().await? {
        store::delete_session_in(db, s.id).await?;
        out.sessions += 1;
    }
    for a in db.query::<Account>().with_index(Account::by_user, |q| q.eq(Account::user, id)).collect().await? {
        db.delete(a.id).await?;
        out.accounts += 1;
    }
    for c in db.query::<Credential>().with_index(Credential::by_user, |q| q.eq(Credential::user, id)).collect().await? {
        db.delete(c.id).await?;
        out.credentials += 1;
    }
    if let Some(email) = &doc.email {
        let codes = db
            .query::<VerificationCode>()
            .with_index(VerificationCode::by_email_purpose, |q| q.eq(VerificationCode::email, email.clone()))
            .collect()
            .await?;
        for c in codes {
            db.delete(c.id).await?;
            out.verification_codes += 1;
        }
    }
    db.delete(id).await?;
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct AdminSummary {
    #[serde(rename = "_id")]
    #[ts(rename = "_id")]
    pub id: String,
    pub email: String,
}

/// The signed-in administrator.
#[query]
pub async fn me(ctx: &QueryCtx) -> Result<Option<AdminSummary>> {
    let id: Id<Admin> = ctx.auth.require_admin()?;
    Ok(ctx.db.get(id).await?.map(|a| AdminSummary { id: a.id.to_string(), email: a.value.email }))
}

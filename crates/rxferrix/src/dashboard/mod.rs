//! The admin dashboard's backend (docs/CONTRACT.md §4.1): admin-only system functions, the `/api/admin/*` HTTP endpoints
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
//! | `search_documents` | query | An untyped, paginated, live full-text search of a table through one of its search indexes. |
//! | `document` | query | One document by id (any table), with its table name. |
//! | `insert_document`, `patch_document`, `replace_document`, `delete_documents` | mutation | Untyped writes, validated with `TableDef::validate`. |
//! | `functions` | query | The function registry, internal and system functions included. |
//! | `run_function` | action | Run any function by path with JSON args, optionally as a given user (through a real, audited, short-lived session). |
//! | `users`, `user_sessions`, `revoke_session`, `delete_user`, `reset_mfa` | query / mutation | The Users page. |
//! | `me` | query | The signed-in admin. |
//!
//! Secrets are never returned and system tables are mostly read-only; see [`secrets`]. Every write is recorded in the
//! audit log (`crate::audit`) in its own transaction. Settings are in `crate::settings`, the audit log's query in
//! `crate::audit`, and `POST /api/admin/impersonate` in [`impersonate`].

#[cfg(feature = "dashboard")]
pub mod assets;
pub mod http;
pub mod impersonate;
pub mod secrets;

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::audit;
use crate::auth::store;
use crate::auth::store::Principal;
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
    let doc_id =
        crate::id::DocId::decode(id).map_err(|_| user_error("invalid_id", format!("`{id}` is not a document id")))?;
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
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
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
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct IndexSummary {
    pub name: String,
    /// JSON field names (empty for `by_id`).
    pub fields: Vec<String>,
    /// `by_id` and `by_creation_time`.
    pub builtin: bool,
    /// Whether `documents` can scan it (not `by_id`, no secret fields).
    pub usable: bool,
}

/// A live aggregate of a table (`#[aggregate]`).
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct AggregateSummary {
    pub name: String,
    pub group_fields: Vec<String>,
    pub sum_fields: Vec<String>,
}

/// A column of a table in the analytics copy (`crate::olap`).
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct ColumnSummary {
    pub name: String,
    /// The DuckDB type (`BIGINT`, `VARCHAR[]`, `STRUCT(...)`, `JSON`, ...).
    pub sql_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct TableSummary {
    pub name: String,
    /// The table number (the first 4 bytes of its ids).
    pub number: u32,
    /// A rxferrix system table (`_`-prefixed).
    pub system: bool,
    /// Defined in the code of this binary (otherwise only its stored documents remain).
    pub defined: bool,
    pub documents: u64,
    pub indexes: Vec<IndexSummary>,
    /// Full-text search and vector indexes.
    pub search_indexes: Vec<crate::db::search::SearchIndexSummary>,
    /// The TS type of the table's documents (user fields), when defined in code.
    pub ts: Option<String>,
    /// Fields whose values are masked.
    pub secret_fields: Vec<String>,
    pub access: TableAccess,
    /// Whether the analytics copy (`--olap`) includes it: user tables and the system tables without secrets.
    pub analytics: bool,
    /// Its columns in the analytics copy, from its Rust type's shape (empty when not defined in code or not copied).
    pub columns: Vec<ColumnSummary>,
    /// Its live aggregates.
    pub aggregates: Vec<AggregateSummary>,
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
            search_indexes: crate::db::search::summaries(&t),
            ts: t.def.as_ref().map(|d| (d.ts)()),
            secret_fields: secrets::secret_fields(&t),
            access: access(&t),
            analytics: crate::olap::columns::copied(&t.name),
            columns: match &t.def {
                Some(d) if crate::olap::columns::copied(&t.name) => {
                    crate::olap::columns::TablePlan::new(&t.name, &(d.shape)())
                        .columns
                        .into_iter()
                        .map(|c| ColumnSummary { name: c.name, sql_type: c.sql_type })
                        .collect()
                }
                _ => Vec::new(),
            },
            aggregates: t
                .aggregates
                .iter()
                .map(|a| AggregateSummary {
                    name: a.name.clone(),
                    group_fields: a.spec.group_fields.clone(),
                    sum_fields: a.spec.sum_fields.clone(),
                })
                .collect(),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "lowercase")]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "lowercase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "lowercase")]
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
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
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

/// An equality filter on a filter field of a search index.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct SearchFilterValue {
    pub field: String,
    pub value: Value,
}

/// A page of `table`'s documents (secrets masked) matching `text` in its full-text search index `index`, most
/// relevant first, optionally restricted to documents whose filter fields equal `filters`. Same matching, cursors and
/// read sets as `SearchQuery::paginate` (docs/CONTRACT.md §1.8), so a subscribed page updates live and
/// `usePaginatedQuery` works. Errors: `unknown_table`, `invalid_query` (unknown or vector index, a field that is not a
/// filter field, a bad cursor), `index_not_allowed` (an index over a secret field).
#[query]
pub async fn search_documents(
    ctx: &QueryCtx,
    table: String,
    index: String,
    text: String,
    filters: Option<Vec<SearchFilterValue>>,
    pagination_opts: PaginationOpts,
) -> Result<PaginationResult<Value>> {
    ctx.auth.require_admin()?;
    let info = ctx.db.catalog().table(&table).map_err(input_error("unknown_table"))?.clone();
    let Some(search) = info.search_indexes.iter().find(|i| i.name == index) else {
        return Err(user_error("invalid_query", format!("Table `{table}` has no search index `{index}`")));
    };
    if search.is_vector() {
        return Err(user_error("invalid_query", format!("`{index}` is a vector index, not a full-text search index")));
    }
    let fields = std::iter::once(search.field()).chain(search.filter_fields().iter().map(String::as_str));
    if fields.into_iter().any(|f| secrets::is_secret_field(&info.name, f)) {
        return Err(user_error("index_not_allowed", format!("Index `{index}` can't be used to search `{table}`")));
    }
    let mut query = ctx.db.search_table(&table, &index).search(search.field(), text);
    for f in filters.unwrap_or_default() {
        query = query.eq(&f.field, f.value);
    }
    let page = query.paginate(pagination_opts).await.map_err(input_error("invalid_query"))?;
    Ok(page.map(|doc| secrets::mask(&info.name, doc)))
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
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
    let admin = ctx.auth.require_admin()?;
    secrets::forbid_system_write(&table, "insert")?;
    let fields = user_fields(document, None)?;
    let id = ctx.db.insert_raw(&table, fields).await.map_err(input_error("invalid_document"))?;
    let after = ctx.db.get_raw(&id).await?.map(|(_, d)| d);
    let rec = audit::Record::new("insert").target(&table, Some(id.clone())).documents(&table, None, after);
    audit::record(&ctx.db, Some(admin), rec).await?;
    Ok(id)
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
            ("image", Value::String(s))
                if !(s.starts_with("https://") || s.starts_with("http://")) || s.len() > 2048 =>
            {
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
    let admin = ctx.auth.require_admin()?;
    let table = table_of(&ctx.db, id)?;
    let action = if replace { "replace" } else { "patch" };
    if table.name != USERS {
        secrets::forbid_system_write(&table.name, if replace { "replace" } else { "update" })?;
    }
    let Some((_, old)) = ctx.db.get_raw(id).await? else {
        return Err(user_error("not_found", format!("Document {id} does not exist")));
    };
    if table.name == USERS {
        let Value::Object(new) = user_fields(document, Some(id))? else {
            unreachable!("user_fields returns an object")
        };
        let patch = user_profile_patch(&old, &new, replace)?;
        if patch.is_empty() {
            return Ok(());
        }
        ctx.db.patch_raw(id, Value::Object(patch)).await.map_err(input_error("invalid_document"))?;
    } else {
        let result = if replace {
            ctx.db.replace_raw(id, user_fields(document, Some(id))?).await
        } else {
            if !document.is_object() {
                return Err(user_error("invalid_document", "A patch must be a JSON object"));
            }
            ctx.db.patch_raw(id, document).await
        };
        result.map_err(input_error("invalid_document"))?;
    }
    let new = ctx.db.get_raw(id).await?.map(|(_, d)| d).unwrap_or(Value::Null);
    let rec = audit::Record::new(action).target(&table.name, Some(id.to_string())).diff(&table.name, &old, &new);
    audit::record(&ctx.db, Some(admin), rec).await
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

/// Delete documents of app tables, all in one transaction (one audit entry each). Returns how many were deleted.
#[mutation]
pub async fn delete_documents(ctx: &MutationCtx, ids: Vec<String>) -> Result<u32> {
    let admin = ctx.auth.require_admin()?;
    for id in &ids {
        secrets::forbid_system_write(&table_of(&ctx.db, id)?.name, "delete")?;
    }
    for id in &ids {
        let before = ctx.db.get_raw(id).await?;
        ctx.db.delete_raw(id).await.map_err(input_error("not_found"))?;
        if let Some((table, doc)) = before {
            let rec = audit::Record::new("delete").target(&table.name, Some(id.clone())).documents(
                &table.name,
                Some(doc),
                None,
            );
            audit::record(&ctx.db, Some(admin), rec).await?;
        }
    }
    Ok(ids.len() as u32)
}

// ---------------------------------------------------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct FunctionSummary {
    pub path: String,
    /// `"query"`, `"mutation"` or `"action"`.
    pub kind: String,
    /// `"public"` or `"internal"`.
    pub visibility: String,
    /// A rxferrix built-in (`_system/...`).
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
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct RunError {
    /// The full error message (admins see server errors in full, as in the execution log).
    pub message: String,
    /// The `UserError` code, if it was one.
    pub code: Option<String>,
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
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

/// Run any registered function (public, internal or system) by path with JSON `args`. Runs as the calling admin, or
/// with `as_user` as that user through a real, short-lived session (marked `impersonatedBy`, audited as
/// `impersonate`, deleted when the run ends), so `ctx.auth.session_id()` works and shared queries are keyed exactly
/// as for the user. Every run is audited (`run_function`; for a mutation, in the mutation's own transaction). A
/// failing function still returns normally, with `success: false`, its error, its log lines and its duration.
#[action]
pub async fn run_function(
    ctx: &ActionCtx,
    path: String,
    args: Option<Value>,
    as_user: Option<String>,
) -> Result<RunResult> {
    let admin = ctx.auth.require_admin()?;
    let app = &ctx.app;
    let Some(def) = app.registry().get(&path) else {
        return Err(user_error("not_found", format!("No function is registered at `{path}`")));
    };
    let args = args.filter(|a| !a.is_null()).unwrap_or_else(|| json!({}));
    let kind = def.handler.kind();
    let as_user = as_user.filter(|u| !u.is_empty());
    let (identity, impersonation) = match &as_user {
        None => (ctx.auth.identity().clone(), None),
        Some(user) => {
            let id: Id<User> = parse_id(user, "user")?;
            let purpose = json!({ "purpose": "run_function", "path": path });
            let ttl = std::time::Duration::from_secs(300);
            let Some((_, session, _)) =
                app.auth().impersonate(app, admin, id, ttl, purpose, Default::default()).await?
            else {
                return Err(user_error("not_found", format!("User {user} does not exist")));
            };
            (Identity::user(id, Some(session)), Some(session))
        }
    };
    let details = json!({
        "path": path,
        "kind": kind.as_str(),
        "asUser": as_user,
        "sessionId": impersonation.map(|s| s.to_string()),
        "args": crate::exec_log::truncate_args(&crate::settings::redact_args(&path, &args)).0,
    });
    let audit_entry = |outcome: Value| {
        let mut d = details.clone();
        d["outcome"] = outcome;
        audit::Record::new("run_function").details(d)
    };
    if kind != FunctionKind::Mutation {
        // Recorded before the run: an action's writes happen in their own transactions.
        let rec = audit_entry(json!("started"));
        store::write(app, |db| async move { audit::record(&db, Some(admin), rec).await }).await?;
    }
    let started = Instant::now();
    let (result, ts, log_lines) = match kind {
        FunctionKind::Query => {
            let run = app.call_query(&path, args, &identity, true).await;
            (run.result, Some(run.ts), run.log_lines)
        }
        FunctionKind::Mutation => {
            // Audited inside the mutation's transaction, so the entry commits with its writes; a failed run (rolled
            // back) is audited on its own.
            let hook = AuditHook { admin, record: audit_entry(json!("success")) };
            let run = app.call_mutation_hooked(&path, args, &identity, true, None, Some(&hook)).await;
            if run.result.is_err() {
                let rec = audit_entry(json!("error"));
                store::write(app, |db| async move { audit::record(&db, Some(admin), rec).await }).await?;
            }
            let ts = run.result.as_ref().ok().map(|(_, ts)| *ts);
            (run.result.map(|(v, _)| v), ts, run.log_lines)
        }
        FunctionKind::Action => {
            let run = app.call_action(&path, args, &identity, true).await;
            (run.result, None, run.log_lines)
        }
    };
    if let Some(session) = impersonation
        && let Err(e) = store::delete_session(app, session).await
    {
        tracing::warn!("dashboard: could not end impersonation session {session}: {e:#}");
    }
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

/// Writes the `run_function` audit entry inside the mutation's transaction.
struct AuditHook {
    admin: Id<Admin>,
    record: audit::Record,
}

impl crate::runtime::MutationHook for AuditHook {
    fn before<'a>(&'a self, _db: &'a crate::MutationDb) -> futures::future::BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn after<'a>(&'a self, db: &'a crate::MutationDb, _value: &'a Value) -> futures::future::BoxFuture<'a, Result<()>> {
        Box::pin(audit::record(db, Some(self.admin), self.record.clone()))
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Users and sessions
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
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
    /// Two-factor authentication is enabled (a confirmed factor exists).
    pub mfa_enabled: bool,
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
        let has_password = ctx
            .db
            .query::<Credential>()
            .with_index(Credential::by_user, |q| q.eq(Credential::user, u.id))
            .first()
            .await?
            .is_some();
        let sessions = ctx
            .db
            .query::<Session>()
            .with_index(Session::by_user, |q| q.eq(Session::user, Some(u.id)))
            .collect()
            .await?
            .iter()
            .filter(|s| s.expires_at > now)
            .count();
        let mfa_enabled =
            crate::auth::mfa::factor_in(&ctx.db, Principal::User(u.id)).await?.is_some_and(|f| f.confirmed);
        out.push(UserSummary {
            id: u.id.to_string(),
            creation_time: u.creation_time,
            email: u.email.clone(),
            email_verified: u.email_verified,
            name: u.name.clone(),
            image: u.image.clone(),
            providers: accounts.into_iter().map(|a| a.value.provider).collect(),
            has_password,
            mfa_enabled,
            sessions: sessions as u32,
        });
    }
    Ok(PaginationResult { page: out, is_done: page.is_done, continue_cursor: page.continue_cursor })
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
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
    /// The admin who minted this session to act as the user (impersonation), if any.
    pub impersonated_by: Option<String>,
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
            impersonated_by: s.value.impersonated_by.map(|a| a.to_string()),
            ip: s.value.ip,
        })
        .collect();
    sessions.sort_by(|a, b| b.last_used_at.total_cmp(&a.last_used_at));
    Ok(sessions)
}

/// End a session (user or admin): its refresh token stops working at once, new connections with its access tokens
/// are refused, and sockets already authenticated with it get `AuthError` right away (live revocation).
#[mutation]
pub async fn revoke_session(ctx: &MutationCtx, session: String) -> Result<()> {
    let admin = ctx.auth.require_admin()?;
    let id: Id<Session> = parse_id(&session, "session")?;
    let Some(doc) = ctx.db.get(id).await? else {
        return Err(user_error("not_found", format!("Session {session} does not exist")));
    };
    let before = serde_json::to_value(&doc)?;
    store::delete_session_in(&ctx.db, id).await?;
    let rec = audit::Record::new("revoke_session").target("_sessions", Some(session)).documents(
        "_sessions",
        Some(before),
        None,
    );
    audit::record(&ctx.db, Some(admin), rec).await
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct DeletedUser {
    pub sessions: u32,
    pub accounts: u32,
    pub credentials: u32,
    pub verification_codes: u32,
    /// Second factors (0 or 1).
    #[serde(default)]
    pub mfa_factors: u32,
}

/// Delete a user together with their sessions (and rotated refresh tokens), linked OAuth accounts, password, second
/// factor and pending email codes. Documents of app tables that reference the user are left alone.
#[mutation]
pub async fn delete_user(ctx: &MutationCtx, user: String) -> Result<DeletedUser> {
    let admin = ctx.auth.require_admin()?;
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
    if crate::auth::mfa::remove_factor_in(db, Principal::User(id)).await? {
        out.mfa_factors += 1;
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
    let before = serde_json::to_value(&doc)?;
    let rec = audit::Record::new("delete_user")
        .target(USERS, Some(user))
        .documents(USERS, Some(before), None)
        .details(serde_json::to_value(&out)?);
    audit::record(db, Some(admin), rec).await?;
    Ok(out)
}

/// Remove a user's second factor (they lost their authenticator and recovery codes): their next sign-in needs no
/// code, and they can enroll again. Audited (`reset_mfa`). `not_found` if the user doesn't exist, `mfa_not_enabled`
/// if they have no factor.
#[mutation]
pub async fn reset_mfa(ctx: &MutationCtx, user: String) -> Result<()> {
    let admin = ctx.auth.require_admin()?;
    let id: Id<User> = parse_id(&user, "user")?;
    if ctx.db.get(id).await?.is_none() {
        return Err(user_error("not_found", format!("User {user} does not exist")));
    }
    if !crate::auth::mfa::remove_factor_in(&ctx.db, Principal::User(id)).await? {
        return Err(user_error("mfa_not_enabled", "This user has no second factor"));
    }
    let rec = audit::Record::new("reset_mfa").target(USERS, Some(user));
    audit::record(&ctx.db, Some(admin), rec).await
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct AdminSummary {
    #[serde(rename = "_id")]
    #[ts(rename = "_id")]
    pub id: String,
    pub email: String,
    /// Two-factor authentication is enabled for this administrator.
    #[serde(default)]
    pub mfa_enabled: bool,
}

/// The signed-in administrator (live: reruns when their second factor changes).
#[query]
pub async fn me(ctx: &QueryCtx) -> Result<Option<AdminSummary>> {
    let id: Id<Admin> = ctx.auth.require_admin()?;
    let Some(admin) = ctx.db.get(id).await? else { return Ok(None) };
    let mfa_enabled = crate::auth::mfa::factor_in(&ctx.db, Principal::Admin(id)).await?.is_some_and(|f| f.confirmed);
    Ok(Some(AdminSummary { id: admin.id.to_string(), email: admin.value.email, mfa_enabled }))
}

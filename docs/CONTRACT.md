# RxFerrix Rust contract: user API, runtime internals, and macro expansions

This document is the **contract between `crates/rxferrix` (runtime) and `crates/rxferrix-macros` (proc macros)**,
plus the user-facing API that both serve. Everything the macros emit refers only to paths listed under
"`::rxferrix::__private` items" or under the public API. The runtime must provide exactly those items.

Wire and storage naming: tables, args and system fields use **camelCase JSON keys**. The macros add
`#[serde(rename_all = "camelCase")]` to table structs and to generated args structs. Field and index handles always
carry the *JSON* name (`created_by` → `"createdBy"`).

---

## 1. User-facing API (what app code writes)

```rust
use rxferrix::prelude::*;
// prelude = Id, Doc, Field, Index, Order, QueryCtx, MutationCtx, ActionCtx, Result, Error, UserError, Bytes,
//           PaginationOpts, PaginationResult, SearchIndex, VectorIndex, VectorFilter, VectorSearchOpts, Aggregate, DocChange,
//           table, object, query, mutation, action, internal_query, internal_mutation, internal_action, migration,
//           workflow, WorkflowCtx, json!, RxFerrix
use rxferrix::log;   // logging is a separate, explicit import (§1.4): log::info!(...), log::warn!(...)

#[table(name = "todos")]                      // name is optional; default = snake_case(struct name), e.g. TodoItem → "todo_item"
#[index(by_done = [done, created_by])]        // zero or more #[index(name = [rust_field, ...])] attributes
pub struct Todo {
    pub text: String,
    pub done: bool,
    pub created_by: Option<String>,
}

#[query]
pub async fn list(ctx: &QueryCtx, done: Option<bool>) -> Result<Vec<Doc<Todo>>> {
    match done {
        Some(d) => ctx.db.query::<Todo>().with_index(Todo::by_done, |q| q.eq(Todo::done, d)).collect().await,
        None => ctx.db.query::<Todo>().order(Order::Desc).take(100).await,
    }
}

#[mutation]
pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
    if text.is_empty() { return Err(UserError::new("empty", "text is empty").into()); }
    log::info!(len = text.len(), "adding a todo");   // logLines: "[INFO] adding a todo len=5"
    ctx.db.insert(Todo { text, done: false, created_by: None }).await
}

#[action]
pub async fn add_many(ctx: &ActionCtx, n: u32) -> Result<()> {
    for i in 0..n { ctx.run_mutation::<add>((format!("item {i}"),)).await?; }
    Ok(())
}

fn main() -> rxferrix::Result<()> {
    RxFerrix::new().run()        // tables and functions are discovered automatically (linkme)
}
```

Rules:
- A function's first parameter is the context, `&QueryCtx`, `&MutationCtx` or `&ActionCtx`, matching the macro.
  The remaining parameters become the args. Parameter patterns must be plain identifiers.
- The return type must be `rxferrix::Result<T>` (written `Result<T>` with the prelude), where
  `T: Serialize + DeserializeOwned + ts_rs::TS + Send + 'static`.
- Every arg type must be `Serialize + DeserializeOwned + ts_rs::TS + Send + 'static`. For types that are not tables
  (a function's output "view" struct, an enum argument), `#[rxferrix::object]` derives all three with camelCase JSON
  through rxferrix's re-exports, so apps need no serde / ts-rs dependency (§3.4).
- `#[query(dev_only)]` (also `mutation`, `action` and the `internal_*` variants): a function for development (seeding,
  reading a dev inbox). It is refused unless the app runs in dev mode (`serve --dev`, `TestApp`), whoever calls it,
  with the not-found error `'<path>' is a dev-only function: it only runs when the server is started with --dev`
  (clients always see the message), and codegen puts it in a separate `devApi` export instead of `api`.
- The annotated fn stays a normal, directly callable async fn (useful as a helper). The macro *also* emits a
  **function reference type** with the same name: a braced struct `pub struct list {}`. A braced struct lives only in
  the *type* namespace, so it never clashes with the fn or with local variables named `list`. Call other functions
  through the runtime with turbofish: `ctx.run_query::<list>(args)`, `ctx.run_mutation::<add>(args)`,
  `ctx.run_action::<..>(args)`, `app.query::<list>(args)`, where `args` is the generated args struct or a tuple of the
  params (`(x,)` for one param, `()` for none).

### 1.1 Core types (in `rxferrix`, re-exported by the prelude)

```rust
pub type Result<T, E = Error> = std::result::Result<T, E>;
pub type Error = anyhow::Error;                     // re-export anyhow::Error

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserError { pub code: String, pub message: String, pub data: serde_json::Value }
impl UserError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self;
    pub fn with_data(self, data: serde_json::Value) -> Self;
}
impl std::error::Error for UserError {}   // + Display = message. Detected at the boundary via anyhow downcast_ref.

/// Typed document id. String form: 26-char lowercase Crockford base32 of 16 bytes:
/// [table_number: u32 big-endian][12 random bytes]. Serializes as a JSON string.
pub struct Id<T> { /* raw: [u8; 16], PhantomData<fn() -> T> */ }
// Copy, Clone, Eq, Ord, Hash, Debug, Display (the string form), FromStr (validates format, not table),
// Serialize/Deserialize as string, ts_rs::TS => `Id<"<table name>">` (requires T: Table).
impl<T> Id<T> { pub fn table_number(&self) -> u32; pub fn as_str(&self) -> String; pub fn as_bytes(&self) -> &[u8; 16]; }

/// A stored document: system fields + the user's struct, flattened.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc<T> {
    #[serde(rename = "_id")] pub id: Id<T>,
    #[serde(rename = "_creationTime")] pub creation_time: f64,   // ms since unix epoch
    #[serde(flatten)] pub value: T,
}
impl<T> Deref for Doc<T> { type Target = T; }   // + DerefMut; PartialEq when T: PartialEq
// ts_rs::TS for Doc<T> => `Doc<"<table name>">` (requires T: Table). Codegen emits the Doc/Id TS helpers.

pub trait Table: Serialize + DeserializeOwned + ts_rs::TS + Clone + Send + Sync + 'static {
    const NAME: &'static str;
    fn table_def() -> TableDef;
}
pub struct TableDef {
    pub name: &'static str,
    pub indexes: Vec<IndexDef>,
    pub search_indexes: Vec<SearchIndexDef>,   // #[search_index] (§1.8)
    pub vector_indexes: Vec<VectorIndexDef>,   // #[vector_index] (§1.8)
    pub ts: fn() -> String,   // inline TS object type of the table struct, for codegen (`ts_inline::<Self>()`)
    /// Ok iff a stored document (system fields included) deserializes as `Doc<Self>`; Err = the serde error.
    /// Used by the startup schema check (§1.4). Emitted as `__private::validate_doc::<Self>`.
    pub validate: fn(&serde_json::Value) -> Result<(), String>,
    /// The fields' JSON shape (`rxferrix::shape::Shape`, §3.1 "Shapes"): the analytics copy's columns (§1.11).
    pub shape: fn() -> rxferrix::shape::Shape,
    /// `#[aggregate]`s (§1.12): `AggregateDef { name, group_fields, sum_fields }`, JSON names. Compared by PartialEq.
    pub aggregates: Vec<AggregateDef>,
}   // PartialEq compares name, all index and aggregate definitions and the rendered `ts()`, never the fn pointers
pub struct IndexDef { pub name: &'static str, pub fields: Vec<&'static str> }   // JSON field names
pub struct SearchIndexDef {
    pub name: &'static str, pub search_field: &'static str, pub filter_fields: Vec<&'static str>,
    pub tokenizer: Tokenizer,   // { kind: TokenizerKind, language: Option<&'static str>, remove_diacritics: bool } (§1.8)
}
pub struct VectorIndexDef {
    pub name: &'static str, pub vector_field: &'static str, pub dimensions: usize, pub filter_fields: Vec<&'static str>,
    pub kind: VectorIndexKind, pub hnsw: HnswParams,   // Exact (default) | Hnsw; HnswParams::DEFAULT for exact (§1.8)
}

/// Typed handle for a field (JSON name). V is the Rust value type.
pub struct Field<T, V> { /* name: &'static str, PhantomData<fn() -> (T, V)> */ }
impl<T, V> Field<T, V> { pub const fn new(name: &'static str) -> Self; pub fn name(&self) -> &'static str; }
// Copy + Clone regardless of T, V (implement manually).

/// Typed handle for an index.
pub struct Index<T> { /* name: &'static str, PhantomData */ }
impl<T> Index<T> { pub const fn new(name: &'static str) -> Self; pub fn name(&self) -> &'static str; }
/// Typed handles for search and vector indexes (same shape as Index<T>: new, name, Copy).
pub struct SearchIndex<T> { /* … */ }
pub struct VectorIndex<T> { /* … */ }

pub enum Order { Asc, Desc }

/// Binary data (`rxferrix::Bytes`, in the prelude). JSON on the wire and in storage: `{"$bytes": "<base64>"}`
/// (standard alphabet, padded; decoding is strict, so every value has exactly one form). TS: `{ $bytes: string }`;
/// `@rxferrix/client` exports `type Bytes`, `bytesToJson(Uint8Array | ArrayBuffer | ArrayBufferView)`,
/// `bytesFromJson(b): Uint8Array` and `isBytes(v)`.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub Vec<u8>);   // Deref<Target = Vec<u8>>, AsRef<[u8]>, From<Vec<u8> | &[u8] | [u8; N]>, Into<Vec<u8>>,
                                 // From/Into axum::body::Bytes (so `ctx.storage.store(bytes, ..)` takes it)
impl Bytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self;
    pub fn into_vec(self) -> Vec<u8>;
    pub fn to_json(&self) -> serde_json::Value;               // {"$bytes": "..."}
    pub fn from_json(v: &serde_json::Value) -> Option<Bytes>; // only an object whose single key is "$bytes"
}
```
**Bytes in indexes.** The key encoding ([Design](DESIGN.md#storage), `rxferrix::keys`) recognizes the tagged form and gives it its own
type tag between strings and arrays, `Undefined < Null < Int64 < Float64 < Bool < String < Bytes < Array < Object`,
with an order-preserving body (bytes compare byte-wise; a prefix sorts first), so `Bytes` fields work in `#[index]`,
`with_index` ranges, search filter fields and the dashboard's index filters. Any other object, including one whose
`$bytes` is not canonical base64, stays an Object. Adding the tag renumbered Array and Object, so the key encoding
has a version in `_meta` (`key_encoding`, now 3, see §1.2; absent = 1): opening a database written with v1
rebuilds every index (ordinary, search and vector) before anything else. Pagination cursors from before such an
upgrade then fail with `paginate: cursor does not belong to this query` (the version is part of their fingerprint).

### 1.2 Contexts and database API

```rust
pub struct QueryCtx    { pub db: QueryDb,    pub auth: Auth, pub storage: QueryStorage, pub env: Env,    /* private */ }
pub struct MutationCtx { pub db: MutationDb, pub auth: Auth, pub storage: MutationStorage, pub scheduler: Scheduler, pub workflows: Workflows, pub env: Env, pub users: Users, /* private */ }
pub struct ActionCtx   { pub auth: Auth, pub storage: ActionStorage, pub scheduler: Scheduler, pub workflows: Workflows, pub olap: Olap, pub env: Env, /* private: runtime handle */ }
// ctx.storage: file storage, see §1.6. ctx.scheduler: scheduling, see §1.7. ctx.env: environment variables, §1.13.
// ctx.users: deleting users and ending sessions in the mutation's transaction, see §1.5.
// All three contexts are Send + Sync. QueryCtx/MutationCtx::now() -> f64 is the ms timestamp of the snapshot or commit
// (in a query: the snapshot's commit time, NOT a reactive clock; see §1.10).
// ctx.is_dev() -> bool on all three: the app runs in dev mode (`serve --dev`; `TestApp` too).

// ctx.auth: the caller, from a validated access token (WS `Authenticate` or HTTP `Authorization: Bearer`).
// Identity flows unchanged into every `ctx.run_*` call an action makes.
pub struct Auth { /* identity */ }                      // rxferrix::Auth = rxferrix::auth::Auth
impl Auth {
    pub fn user_id(&self) -> Option<Id<User>>;          // None when anonymous or for admin sessions
    pub fn require_user(&self) -> Result<Id<User>>;     // else UserError { code: "unauthenticated" }
    pub async fn user(&self, db: &QueryDb) -> Result<Option<Doc<User>>>; // `ctx.auth.user(&ctx.db)`: a tracked read,
                                                        // so a query reruns when the profile changes
    pub fn is_admin(&self) -> bool;
    pub fn admin_id(&self) -> Option<Id<Admin>>;
    pub fn require_admin(&self) -> Result<Id<Admin>>;   // "unauthenticated" when anonymous, "forbidden" for users
    pub fn session_id(&self) -> Option<Id<Session>>;      // None for external JWTs (they have no session row). For an
                                                        // admin acting as the user, an impersonation session whose
                                                        // `Session::impersonated_by` is set (check it to refuse
                                                        // sensitive operations).
    pub fn identity(&self) -> &Identity;
}
// What a query reads of `ctx.auth` decides who shares its subscription (docs/PROTOCOL.md "Shared subscriptions"):
// nothing → every socket; user/admin id (user_id, require_user, user, is_admin, admin_id, require_admin) → every
// socket of that principal (all anonymous sockets share one); session_id or identity → that session only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Identity {                                   // rxferrix::Identity = rxferrix::auth::Identity
    pub user_id: Option<String>, pub admin_id: Option<String>, pub session_id: Option<String>,  // string ids, no tokens
}
impl Identity {
    pub fn anonymous() -> Self; pub fn user(Id<User>, Option<Id<Session>>) -> Self; pub fn admin(Id<Admin>, Option<Id<Session>>) -> Self;
    pub fn log_summary(&self) -> Option<String>;        // "user:<id>" | "admin:<id>" (execution log), None = anonymous
    pub fn principal_key(&self) -> String;              // log_summary or "anonymous" (scopes mutation dedup keys)
}
// `rxferrix::auth::User` is the `_users` table type: { email: Option<String>, email_verified: bool, name: Option<String>,
// image: Option<String> } (creation time = `_creationTime`). Reference it from app tables as `Id<User>`; codegen
// emits it as `Id<"_users">` (declared in `SystemDataModel`). It never contains secrets.

impl QueryDb {
    pub async fn get<T: Table>(&self, id: Id<T>) -> Result<Option<Doc<T>>>;
    pub fn query<T: Table>(&self) -> Query<'_, T>;
}
impl MutationDb {   // MutationDb: Deref<Target = QueryDb>, so reads work identically and see the mutation's own writes
    pub async fn insert<T: Table>(&self, value: T) -> Result<Id<T>>;
    pub async fn replace<T: Table>(&self, id: Id<T>, value: T) -> Result<()>;
    pub async fn patch<T: Table>(&self, id: Id<T>, patch: serde_json::Value) -> Result<()>; // shallow merge of an object; null value = set to null
    pub async fn delete<T: Table>(&self, id: Id<T>) -> Result<()>;
}
// Every write is validated before anything is written: the complete new document (system fields included) must
// deserialize as Doc<T>. For patch this catches wrong types and nulled required fields; for insert/replace it
// catches a Serialize impl that doesn't round-trip through Deserialize. Failure: an error
// "<patched|inserted|replaced> document <id> does not match table `<name>`; nothing was written: <serde error>".
// A patch (and patch_raw) must also name only fields of the table, by JSON name ("completedAt", not "completed_at"):
// serde would ignore any other key, so the write would succeed without changing the field meant. Failure: "`<key>`
// is not a field of table `<name>` (its fields: ...); nothing was written". Tables whose shape is open (a
// `#[serde(flatten)]` map) accept any key; keys starting with `_` get the system-field error.
impl<'a, T: Table> Query<'a, T> {
    pub fn with_index(self, index: Index<T>, range: impl FnOnce(IndexRange<T>) -> IndexRange<T>) -> Self; // default: by_creation_time, full range
    pub fn order(self, order: Order) -> Self;                                   // default Asc
    pub fn filter(self, pred: impl Fn(&Doc<T>) -> bool + Send + Sync + 'a) -> Self;
    pub async fn collect(self) -> Result<Vec<Doc<T>>>;
    pub async fn take(self, n: usize) -> Result<Vec<Doc<T>>>;
    pub async fn first(self) -> Result<Option<Doc<T>>>;
    pub async fn unique(self) -> Result<Option<Doc<T>>>;                       // Err if > 1 match
    pub async fn count(self) -> Result<u64>;   // without a filter: an index range count (SQL COUNT over index_entries,
                                               // no documents read); with one: a scan counting matches. Read set: the
                                               // whole range either way (also RawQuery::count)
    pub async fn paginate(self, opts: PaginationOpts) -> Result<PaginationResult<Doc<T>>>;   // see §1.2.1
    pub fn with_search_index(self, index: SearchIndex<T>, f: impl FnOnce(SearchFilter<T>) -> SearchFilter<T>)
        -> SearchQuery<'a, T>;                                                  // full-text search, see §1.8
}
impl<T> IndexRange<T> {   // fields must follow the index field order: eq* then at most one lower and one upper bound on the next field
    pub fn eq<V: Serialize>(self, f: Field<T, V>, v: V) -> Self;
    pub fn gt<V: Serialize>(self, f: Field<T, V>, v: V) -> Self;   // also gte, lt, lte
}
// Untyped API (table names and JSON; used by the dashboard). Same engine, read sets and pagination as the typed
// API: both build one ScanPlan (table, index, key range, order) run by a shared scan core.
impl QueryDb {
    pub fn query_table(&self, table: &str) -> RawQuery<'_>;                 // unknown tables/indexes fail when it runs
    pub async fn get_raw(&self, id: &str) -> Result<Option<(Arc<TableInfo>, Value)>>;  // any table (from the id)
    pub async fn count(&self, table: &str) -> Result<u64>;                  // read set: the whole table
    pub fn catalog(&self) -> &Arc<Catalog>;
}
impl RawQuery<'_> {   // with_index(name, |r| r.eq("field", json).gte(..)), order, filter(Fn(&Value) -> bool),
                      // collect, take, first, paginate(PaginationOpts) -> PaginationResult<Value>
}
impl MutationDb {     // validated with TableDef::validate (same rule and error message as the typed writes)
    pub async fn insert_raw(&self, table: &str, fields: Value) -> Result<String>;   // returns the new id
    pub async fn replace_raw(&self, id: &str, fields: Value) -> Result<()>;
    pub async fn patch_raw(&self, id: &str, patch: Value) -> Result<()>;
    pub async fn delete_raw(&self, id: &str) -> Result<()>;
}
// The catalog keeps each table's TableDef (`TableInfo::def`); a table that exists only in data.db (no longer
// defined in code) has `def: None`: readable, but untyped writes are refused.
impl ActionCtx {
    pub async fn run_query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output>;
    pub async fn run_mutation<F: MutationFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output>;
    pub async fn run_action<F: ActionFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output>;
    pub fn mailer(&self) -> Option<Arc<dyn Mailer>>;          // the auth mailer in effect (§1.5); None: not configured
    pub async fn send_email(&self, email: Email) -> Result<()>;   // through it; UserError `email_not_configured`
    pub async fn vector_search<T: Table>(&self, index: VectorIndex<T>, vector: impl AsRef<[f32]>,
                                         opts: VectorSearchOpts<T>) -> Result<Vec<(Id<T>, f32)>>;   // §1.8
}
// Built-in indexes on every table: "by_id" (point lookups) and "by_creation_time" (fields ["_creationTime"]).
// Every index key implicitly ends with `_creationTime, _id`: enc(f1) ++ … ++ enc(fn) ++ enc(_creationTime)
// ++ id (by_creation_time is just enc(_creationTime) ++ id). So documents with equal declared fields are in creation
// order (`with_index(by_channel, |q| q.eq(channel, c)).order(Desc)` is newest first), and keys are unique.
// IndexRange works over these key fields: after an eq on every declared field, `_creationTime` can take an eq or
// range bounds: `q.eq(Message::channel, c).gt(Message::_creation_time, since)`. Listing a system field in
// `#[index(..)]` (`[a, _creation_time]`, `[a, _id]`) is a compile error (it is always there already).
// The layout is versioned (`keys::KEY_ENCODING_VERSION` = 3, in `_meta.key_encoding`): opening a database written
// with v2 (keys ended in the id alone) rebuilds its ordinary indexes in the startup transaction (v1, before the Bytes
// tag, rebuilds search and vector indexes too). Pagination cursors hash the version into their fingerprint, so a
// cursor from before the upgrade fails with `paginate: cursor does not belong to this query`.
// Each table also gets the field handles `_id: Field<Self, Id<Self>>` and `_creation_time: Field<Self, f64>`
// (generated by #[table]), plus `by_creation_time: Index<Self>`.
```

### 1.2.1 Pagination

```rust
#[derive(Serialize, Deserialize, ts_rs::TS)]            // camelCase JSON and TS
pub struct PaginationOpts {
    pub num_items: usize,                // >= 1; ignored when end_cursor is set
    pub cursor: Option<String>,          // None = start of the range
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_cursor: Option<String>,      // TS: endCursor?: string | null
}
impl PaginationOpts {
    pub fn first(num_items: usize) -> Self;
    pub fn after(cursor: impl Into<String>, num_items: usize) -> Self;
    pub fn with_end_cursor(self, end_cursor: impl Into<String>) -> Self;
}
#[derive(Serialize, Deserialize, ts_rs::TS)]
pub struct PaginationResult<T> { pub page: Vec<T>, pub is_done: bool, pub continue_cursor: String }
impl<T> PaginationResult<T> {
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> PaginationResult<U>;
    // Async variants keep the cursors too. They take closures returning futures (`|m| async move { .. }`, or a call
    // of an async fn), not `AsyncFnMut`: the futures of async closures that borrow their captures are not provably
    // Send today, and handlers must be. Share a cache behind a std Mutex (never held across an await).
    pub async fn map_async<U, Fut: Future<Output = U>>(self, f: impl FnMut(T) -> Fut) -> PaginationResult<U>;  // in order
    pub async fn try_map<U, E, Fut: Future<Output = Result<U, E>>>(self, f: impl FnMut(T) -> Fut)
        -> Result<PaginationResult<U>, E>;                                          // in order; stops at the first error
    pub async fn try_map_concurrent<U, E, Fut: Future<Output = Result<U, E>>>(self, concurrency: usize,
        f: impl Fn(T) -> Fut) -> Result<PaginationResult<U>, E>;                     // up to `concurrency` at a time (>= 1),
                                                                                 // results in page order
}
// e.g. `page.try_map(|m| view(ctx, &names, m)).await` turns a PaginationResult<Doc<Message>> into PaginationResult<MessageView>.
pub const rxferrix::db::pagination::MAX_PAGE_SCAN: usize = 10_000;

#[query]
pub async fn list(ctx: &QueryCtx, channel: String, pagination_opts: PaginationOpts)
    -> Result<PaginationResult<Doc<Message>>> {
    ctx.db.query::<Message>()
        .with_index(Message::by_channel, |q| q.eq(Message::channel, channel))
        .order(Order::Desc)
        .paginate(pagination_opts)
        .await
}
```

- **Cursors** are opaque strings naming a position in the query's index range, in its scan direction: just past
  an index key, or the end of the range. Format: URL-safe base64 (no padding) of
  `[1][fingerprint: 8 bytes][0 + key bytes | 1 (end)]`, where the fingerprint is a blake3 hash of the index id, the
  order, the range bounds and the key encoding version. A cursor that is not valid base64 or not in this format fails with
  `paginate: malformed cursor` (or `endCursor`); one returned for another index, range (e.g. different `eq` args) or
  order fails with `paginate: cursor does not belong to this query`. The filter is not part of the fingerprint.
- **Without `end_cursor`**: the page holds the first `num_items` matching documents after `cursor`.
  `continue_cursor` points just past the last *scanned* document (a document rejected by `filter` can be the last
  one scanned). When the range is exhausted, `continue_cursor` is the end-of-range cursor and `is_done = true`; a
  page that stops at exactly `num_items` is not known to be the last (`is_done = false`, the next page is empty and
  done). A page also stops after scanning `MAX_PAGE_SCAN` documents, possibly with fewer than `num_items` (even 0)
  documents and `is_done = false`.
- **With `end_cursor`**: the page holds *every* matching document in `[cursor, end_cursor)` (positions: after the
  cursor's key, up to and including the end cursor's key), whatever `num_items` says, with no scan cap.
  `continue_cursor = end_cursor` and `is_done` is true only if `end_cursor` is the end of the range.
  `end_cursor` before `cursor` is an error; equal cursors give an empty page.
- **Read set**: exactly the scanned span: `[cursor, last scanned key]` when the page stopped early,
  `[cursor, end of range)` when the range was exhausted, `[cursor, end_cursor)` when pinned. So a write in a later
  page never invalidates an earlier page.
- **Codegen**: `PaginationOpts` and `PaginationResult<T>` are declared in `dataModel.ts`; the function above becomes
  `makeFunctionReference<"query", { channel: string, paginationOpts: PaginationOpts, }, PaginationResult<Doc<"messages">>>`.
  In general a function's output refers to named types by name, also inside wrappers (`Array<MessageView>`,
  `ChannelInfo | null`, `PaginationResult<MessageView>`), and declares them in `dataModel.ts`
  (`__private::ts_output`); only table types and the generated args structs are written out as object types
  (`__private::ts_inline`). Field doc comments become JSDoc on their own lines: an object type with a documented field
  is laid out one field per line, indented (`codegen::format_ts`); a one-line type doc becomes `/** … */`.
- **Clients** also have `usePaginatedQueryWithStatus` (`@rxferrix/react`): the same, but a failed page comes back as
  `error` (with the pages that did load still in `results`) instead of being thrown. (`@rxferrix/svelte`'s
  `usePaginatedQuery` always returns `error`.)
- **Clients** (`client.paginatedQuery`, `usePaginatedQuery` in `@rxferrix/react` / `@rxferrix/svelte`) subscribe to
  page 0 as `{ ...args, paginationOpts: { numItems, cursor: null } }`, each further page from the previous page's
  `continueCursor`, and re-subscribe a page with `endCursor` pinned to its `continueCursor` as soon as a next page
  follows it (showing the unpinned result until the pinned one arrives). Every page is an ordinary query, so
  `OptimisticLocalStore.getAllQueries(ref)` lists each page; `insertAtTop({ paginatedQuery, argsToMatch,
  localQueryStore, item })` prepends to first pages (`cursor: null`).

### 1.3 Function reference traits

```rust
pub trait FunctionRef: Send + Sync + 'static {   // implemented on a type-namespace-only braced struct; never instantiated
    type Args: Serialize + DeserializeOwned + Send + 'static;
    type Output: Serialize + DeserializeOwned + Send + 'static;
    /// Function path as in docs/PROTOCOL.md, e.g. "todos:list".
    fn path() -> String;
}
pub trait QueryFunction: FunctionRef {}
pub trait MutationFunction: FunctionRef {}
pub trait ActionFunction: FunctionRef {}
/// What `ctx.scheduler` and crons accept (§1.7). The macros implement it for every mutation and action (internal ones
/// included), never for queries, so scheduling a query is a compile error.
pub trait SchedulableFunction: FunctionRef {}
```

### 1.4 Logging, the execution log, and runtime options

**Function log lines.** `rxferrix::log` re-exports the `tracing` macros `trace!`, `debug!`, `info!`, `warn!`,
`error!`. Write `use rxferrix::log;` and then `log::info!(...)` (plain `tracing::info!` is the same thing). Events
emitted while a function runs, at DEBUG and above (WARN and above for a few chatty crates: hyper, h2, reqwest, rustls,
tokio, tower, ...), are captured per execution as `"[LEVEL] message key=value ..."` (at most 256 lines of 4KB each). A nested `ctx.run_*` call's lines are also appended to its caller's. Events from
tasks the function spawns are not captured. Lines are always stored in the execution log and sent to clients as
`logLines` only in `--dev`. The events still reach the normal subscriber: `rxferrix serve` prints them to stdout,
filtered by `RUST_LOG`.

*Why not in the prelude:* glob-importing a `log` module or `info!`-style macros is ambiguous (E0659) wherever the app
also uses the `log` crate or `#[macro_use] extern crate log`, so the prelude leaves them out and the explicit
`use rxferrix::log;` (which shadows the `log` crate cleanly in that file) is the one import to add.

Capture uses a tracing layer, `rxferrix::log::layer()`, plus a task-local buffer. `rxferrix serve` installs stdout output (`--log-format json|pretty|compact`, §4.4)
and the layer, each with its own filter. If no global subscriber is set when an `App` opens, rxferrix installs one
with just the layer. An app that installs its own subscriber should add `rxferrix::log::layer()` to it, and do so
before opening the `App`.

**Runtime options.**
```rust
pub struct AppOptions {
    pub dev: bool,                    // real error messages and logLines to clients
    pub readers: usize,               // default 1.5 per core (`--readers`): reads that can run at once; a query holds one for its run
    pub mutation_timeout: Duration,   // default 5s (`--mutation-timeout 5s`); see below
    pub query_timeout: Duration,      // default 30s (`--query-timeout 30s`): fails with rxferrix::QueryTimeout
    pub skip_schema_check: bool,      // `--skip-schema-check`; see below
    pub log_retention: LogRetention,  // { max_age: 7 days, max_rows: 200_000, trim_interval: 1 min }
    pub scheduler: SchedulerOptions,  // concurrency, job retention, retry backoff, declared crons and workpools (§1.7)
    pub auth: rxferrix::auth::AuthConfig, // sign-in methods, token lifetimes, email, OAuth (see §1.5)
    pub storage: rxferrix::storage::StorageConfig, // file storage backend and limits (see §1.6)
    pub ops: rxferrix::ops::OpsOptions, // `/metrics` (enabled, token) and the backup target (see §4.4)
    pub query_cache: rxferrix::QueryCacheOptions, // { idle_ttl: 30 min, max_bytes: 64 MiB } (see §1.10, Query cache)
    pub fsync_commits: bool,          // default false (synchronous = NORMAL); true (`--fsync-commits`): commits wait for the disk (FULL)
    pub writer_cache_size: u64,       // default 32 MiB (`--writer-cache-size`): the writer connection's page cache, taken as used
    pub determinism_check: Option<Duration>, // None; Some(10s) in `--dev` and TestApp: see below
    pub olap: rxferrix::olap::OlapOptions, // the analytics copy (§1.11): enabled = false
    pub guard: Option<rxferrix::Guard>, // None; `RxFerrix::guard`: see below
}
impl Default for AppOptions { /* the defaults above, dev = false */ }
```
- **Guard** (`rxferrix::guard`). `Guard::new(|ctx: GuardCtx, function: GuardedFunction| async { .. -> Result<()> })`,
  or `RxFerrix::guard(..)` (which `serve` puts here), runs before every call a client (WebSocket, HTTP API) makes to
  a public, non-system function, every rerun of a subscribed query included; an error refuses the call and reaches the
  client like the function's own (a `UserError` keeps its code; the execution log records a failed call).
  `GuardCtx { db: QueryDb, auth: Auth, env: Env }` is the query's snapshot and `Auth` (its reads are the query's: it
  reruns with them, and its `ctx.auth` reads decide who shares the result), the mutation's transaction, or for actions
  a fresh snapshot; `GuardedFunction::{path(), kind(), is::<F>()}` names the call, so exemptions are checked by the
  compiler. It runs inside the function's timeout and panic handling. Not guarded: internal and `_system/*`
  functions, and server code (`ctx.run_*`, `app.query/mutation/action`, scheduled jobs and crons, migrations, the
  dashboard's function runner); `TestApp::{query_as, mutation_as, action_as, as_user}` are guarded like a client for
  public functions. A query run by server code that skipped the guard is not offered to the query cache (a client's
  call would otherwise be answered without it).
- **Query timeout.** A query still running after `query_timeout` fails with `rxferrix::QueryTimeout { path, timeout }`
  (a server error; checked at `.await` points only). A stuck query would otherwise keep a reader connection, and the
  older results its subscribers' other queries are kept at, for good.
- **Determinism check.** With `determinism_check: Some(every)`, a query is run a second time in the same snapshot,
  at most once per `every` per path, and a result (or error message) that differs is reported: a `WARN` in the server
  log and a ``[WARN] query `<path>` gave a different result when run again at the same ts …`` line appended to the
  run's log lines (so in the execution log and, in `--dev`, the client's `logLines`), with where the two results
  first differ. Such a query reads something besides its arguments and data (`SystemTime::now()`, random numbers,
  `HashMap` iteration order): subscribers get updates for nothing and cached results are wrong. Runs that read the
  time (`ctx.now()`, signed URLs) and timeouts are not compared. The rerun is not an execution (no log entry, no
  metrics, its log lines dropped; stdout still prints them) and isn't counted in the run's duration.
- **Slow mutations.** A mutation that holds the database writer for 100 ms or more (`rxferrix::db::WARN_WRITE`) is
  logged as a warning, at most once a minute per path: every other write waited for it.
- **Mutation timeout.** A mutation still running after `mutation_timeout` is dropped and rolled back, and fails with
  `rxferrix::MutationTimeout { path, timeout }`, a server error (its message reaches clients only in `--dev`). It can
  only fire at an `.await`: synchronous code in a mutation (a busy loop, blocking I/O) is never preempted and keeps
  holding the writer lock.
- **Idempotency.** `App::execute_mutation(path, args, &identity, Some(&IdempotencyKey { session_id, request_id }))`
  runs a mutation at most once per key and replays the recorded response for repeats. The response is recorded in
  the mutation's own write transaction, in the raw table `_mutation_dedup` of `data.db`, and kept about 10 minutes.
  The sync worker uses this for every WS mutation; see PROTOCOL.md guarantee 7. The runtime scopes the stored key
  to the caller (`IdempotencyKey::scoped_to(identity)`, prefixing `Identity::principal_key()`), so a response is
  only ever replayed to the identity that ran the mutation.
- **Startup schema check.** `App::open` fingerprints each table: a hash of its rendered TS type, the TS declarations
  of the named types it uses, and its index definitions. It re-validates the documents of a table only when the
  fingerprint differs from the one stored after the last successful check (raw table `_schema_fingerprints`), using
  `TableDef::validate`. If any document fails, `App::open` returns an error listing, per table, how many documents
  fail and up to 20 ids with their serde errors, plus how to fix it. Fingerprints are then not updated, so the
  check runs again next time. With `skip_schema_check` nothing is validated or recorded. The check misses changes
  that don't show in the TS type (e.g. `u32` → `u8`, or `deny_unknown_fields`); reads of such documents still fail
  with a clear error. A table with failing documents **opens anyway while `#[migration]`s for it are pending** (not
  `done` in `_migrations`): it is then *migrating*, with lenient writes, until its last migration has run (§1.9).
  The error message suggests a migration (with a snippet) and names migrations of the table that are already done.

**Running functions with their log lines** (what the WS worker and the HTTP API use):
```rust
impl App {
    pub async fn run_query(&self, path: &str, args: Value, identity: &Identity) -> QueryRun;  // .result .read_set .ts .log_lines
    pub async fn execute_mutation(&self, path: &str, args: Value, identity: &Identity,
                                  idempotency_key: Option<&IdempotencyKey>) -> MutationRun;   // .result: Result<(Value, ts)> .log_lines
    pub async fn execute_action(&self, path: &str, args: Value, identity: &Identity) -> ActionRun; // .result .log_lines
    // run_mutation / run_action: same, without log lines or idempotency (unchanged signatures)
}
```

**Execution log** (`<dir>/aux.db`, table `_logs`; `aux.db` also holds the scheduler's job history, §1.7). Every execution is recorded: queries (including subscription
reruns: one entry per shared rerun, however many sockets it served, with the identity it ran as), mutations,
actions, and nested `ctx.run_*` calls. Duplicate mutations answered from the dedup table and
calls to unknown functions are not recorded. Writes go through a bounded channel (8192 entries) to a background
thread that inserts in batches. When the channel is full, entries are dropped and counted rather than slowing the
function down.

```rust
impl App {
    pub fn logs(&self) -> &Logs;
    pub fn dir(&self) -> &Path;               // the data directory
    pub fn metrics(&self) -> &Metrics;        // .sockets(), .subscriptions(), .shared_queries(): open WebSockets, live
                                              // (socket, queryId) subscriptions, distinct shared queries
    pub fn subscription_stats(&self) -> SubscriptionStats; // rxferrix::sync::subscriptions: { connections, shared_queries,
                                              // subscriptions, index_ranges, rounds, reruns, last_round_reruns }
    pub fn subscriptions(&self) -> &SubscriptionManager;   // the shared subscription manager (started on first use)
    pub fn started_at(&self) -> f64;          // ms since the epoch
}

#[derive(Clone)] pub struct Logs { /* … */ }
impl Logs {
    pub async fn query(&self, filter: LogFilter) -> Result<Vec<LogEntry>>;   // newest first
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LogEntry>;   // live tail, after each entry is written
    pub async fn stats(&self, since_ms: i64, exclude_background: bool) -> Result<Vec<FunctionStats>>;  // per path, sorted
    pub async fn timeseries(&self, spec: TimeseriesSpec) -> Result<Vec<TimePoint>>;  // per time bucket, oldest first
    pub fn dropped(&self) -> u64;                                             // entries dropped since startup
    pub async fn flush(&self) -> Result<()>;                                  // wait until queued entries are written
    pub async fn trim_now(&self) -> Result<()>;                               // apply retention now
}

#[derive(Default, Serialize, Deserialize)] #[serde(rename_all = "camelCase", default)]
pub struct LogFilter {
    pub path_prefix: Option<String>,      // e.g. "todos:" or "todos:list"
    pub path: Option<String>,             // exact path
    pub kind: Option<FunctionKind>,       // JSON: "query" | "mutation" | "action"
    pub outcome: Option<LogOutcome>,
    pub since: Option<i64>,               // ms since the epoch, inclusive
    pub limit: Option<usize>,             // default 100, max 1000
    pub before_id: Option<i64>,           // page backwards: pass the last id of the previous page
    pub identity: Option<String>,         // "user:<id>" | "admin:<id>" | "anonymous"
    pub exclude_system: bool,             // leave out `_system/…` functions (e.g. the dashboard's own queries)
    pub exclude_background: bool,         // leave out background work: identity "scheduler" (scheduled jobs, crons,
                                          // workflow steps, and the functions they call); what is left clients ran
}
impl LogFilter { pub fn matches(&self, entry: &LogEntry) -> bool; }   // same conditions, for the live tail

#[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: i64,                      // increasing
    pub timestamp: i64,               // start, ms since the epoch
    pub path: String,
    pub kind: FunctionKind,           // JSON: "query" | "mutation" | "action"
    pub identity: Option<String>,     // Identity::log_summary(): "user:<id>" | "admin:<id>"; None = anonymous
    pub duration_us: u64,
    pub outcome: LogOutcome,          // JSON: "success" | "userError" | "serverError"
    pub error: Option<String>,        // full message with context (never redacted: server-side data)
    pub log_lines: Vec<String>,
    pub args: String,                 // JSON, cut to at most 2048 bytes (then possibly not valid JSON)
    pub args_truncated: bool,
}
pub enum LogOutcome { Success, UserError /* UserError, invalid args, not found */, ServerError /* bugs, panics, timeouts */ }

#[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct FunctionStats {
    pub path: String, pub kind: FunctionKind,
    pub calls: u64, pub errors: u64, pub user_errors: u64, pub server_errors: u64,
    pub p50_us: u64, pub p95_us: u64,     // nearest-rank percentiles of duration_us
}

pub struct TimeseriesSpec {                // executions of `path` (None: every non-system function)
    pub path: Option<String>, pub since: i64 /* rounded down to the bucket */, pub until: i64, pub bucket_ms: i64,
    pub exclude_background: bool,          // as LogFilter::exclude_background
}                                          // at most MAX_TIMESERIES_BUCKETS (1500) buckets, else an error
#[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct TimePoint {                     // only buckets with executions
    pub t: i64, pub calls: u64, pub errors: u64, pub user_errors: u64, pub server_errors: u64,
    pub p50_us: u64, pub p95_us: u64, pub max_us: u64,
}
```

### 1.5 Auth configuration and services

```rust
use rxferrix::auth::{Apple, AuthConfig, Discord, GitHub, Google, Oidc, SmtpConfig};
fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new()
        .auth(AuthConfig::password()                // email + password (verification and reset included)
            .magic_link()                           // magic link + 6-digit code by email
            .oauth(GitHub::from_env())              // RXFERRIX_AUTH_GITHUB_CLIENT_ID / _SECRET (skipped with a warning if unset)
            .oauth(Google::from_env())              // RXFERRIX_AUTH_GOOGLE_CLIENT_ID / _SECRET
            .oauth(Discord::from_env())             // RXFERRIX_AUTH_DISCORD_CLIENT_ID / _SECRET
            .oauth(Apple::from_env())               // RXFERRIX_AUTH_APPLE_{CLIENT_ID,TEAM_ID,KEY_ID,PRIVATE_KEY[_FILE]}
            .oauth(Oidc::new("corp", "https://idp.example.com", "client-id", "secret"))   // discovery
            .redirect_url("https://app.example.com")) // allowlist for OAuth redirects and email links
        .run()
}
```
OAuth presets (`rxferrix::auth`): `GitHub`, `Google`, `Discord::new(client_id, secret)` (`identify email`; the
`/users/@me` profile; the email is kept only when Discord marks it `verified`), `Apple::new(services_id, team_id,
key_id, p8_pem)` and `Oidc::new(id, issuer, client_id, secret)`; each is an `OAuthProvider` (`.id(..)`,
`.scopes(..)`, and `.base_url(..)` to point Discord or Apple at a mock or proxy). Sign in with Apple: the client
secret is an ES256 JWT (`iss` team id, `sub` services id, `aud` `https://appleid.apple.com`, `kid` key id) signed
with the `.p8` key, cached and replaced 10 minutes before its 1-hour expiry; the authorization request adds
`response_mode=form_post` and a `nonce`; the callback is a `POST` (the state cookie is then `SameSite=None; Secure`
when the public URL is https); the profile is the token endpoint's ID token, verified with the external-JWT code
(Apple's JWKS at `/auth/keys`, RS256 only, `iss`, `aud` = services id, `exp`, `nonce`); the name comes from the
`user` form field of the first sign-in (later sign-ins keep it; its email is ignored). A provider whose Apple key
doesn't parse is logged at startup and refuses sign-ins. Whatever the provider, an email it didn't verify is neither
stored on a new user nor allowed to claim an existing one (`email_in_use`); a verified one that matches a user whose
address was never verified takes that account over, removing its password, sessions and provider links
(`store::strip_unverified_account`), as does confirming the address through the verification link. The callback
checks the state cookie before consuming the pending flow.

`AuthConfig::default()` enables no user sign-in method (tokens are still validated; admin sign-in works). Other
builder methods: `passwordless()`, `signups(bool)`, `require_email_verification(bool)`, `access_token_ttl` (15 min),
`session_ttl` (30 days, sliding), `refresh_reuse_grace` (10 s), `public_url`, `smtp(SmtpConfig)`,
`mailer(Arc<dyn Mailer>)` (e.g. `MemoryMailer` in tests), `dev_mailer(Arc<dyn Mailer>)` (replaces the built-in
`DevLogMailer`, which prints every email to the log, in `--dev` when no mailer or SMTP is configured in code, on the
command line or in `_settings`; never used outside `--dev`; it can forward to `DevLogMailer.send(email)` to keep the
log lines; `Email { to, subject, body, html: Option<String> }`, built with `Email::new(to, subject, text).html(..)`,
an HTML version is sent as `multipart/alternative`; apps send their own email through the same mailer with
`ActionCtx::send_email`), `rate_limits(Option<RateLimits>)` (`credential_*` per client IP and, for password
sign-ins, per account; `email_*` per IP and per address; `refresh_*` per IP, shared by sign-out; `None` disables
them), `trust_proxy(bool)`, `trusted_proxies([..])` (the proxy's IPs or CIDR ranges: proxy headers count only on
requests from them; an invalid entry fails the start),
`jwt(JwtProvider)` (external tokens, below), `audit_retention(Duration)` (`_audit_log`, 90 days),
`impersonation(bool)` (`POST /api/admin/impersonate`, default true), `on_sign_in(|app: App, sign_in: SignIn| async
{ .. })` (after each successful user sign-in that starts a session, awaited before the response; `SignIn { user,
session, method, client }`, `SignInMethod::{Password, SignUp, PasswordReset, MagicLink, OAuth(provider)}`; not for
`sign_in_as`, impersonation, admins, refreshes or external JWTs; with two-factor authentication it runs once the code
was accepted), `totp_issuer(name)` (what authenticator apps show for this server's second factors; default: the
public URL's host, else `RxFerrix`), `before_sign_up(|app: App, attempt: SignUpAttempt| async { .. -> Result<()> })`
and `allowed_emails([..])` / `allowed_domains([..])` (the sign-up check, below).
CLI flags / env vars add to it: `--public-url` (`RXFERRIX_PUBLIC_URL`), `--auth-redirect-url` (repeatable;
`RXFERRIX_AUTH_REDIRECT_URLS`, comma-separated), `--smtp-url` / `--smtp-from` (`RXFERRIX_SMTP_URL`, `RXFERRIX_SMTP_FROM`),
`--trust-proxy`, `--trust-proxy-from` (repeatable; `RXFERRIX_TRUST_PROXY_FROM`, comma-separated). Commands: `admin create <email> [--password …]`, `admin reset-password <email> [--password …]`
(the password is read from stdin when omitted), `admin disable-2fa <email>` (removes an administrator's second factor).

**Sign-up check.** Before a sign-up creates an account (password sign-up; the first sign-in by magic link or code, or
by OAuth) and only when `signups` allows sign-ups: first the allowlists, then `before_sign_up`. The allowlists
(`allowed_emails`, `allowed_domains`: exact, case-insensitive; a domain list doesn't include subdomains) refuse with
403 `signup_not_allowed` an address that isn't listed, or isn't proven (an OAuth address the provider didn't verify,
or none); while one is set, password accounts must verify their address before signing in (`verification_required`:
`require_email_verification` or a list) and sign-up answers `verificationRequired`, and a magic link is only sent to
an address without an account if it is listed. The hook gets `SignUpAttempt { email: Option<String>, email_verified,
method: SignUpMethod::{Password, MagicLink, OAuth(provider)}, redirect_to: Option<String>, name, client }`: `email` is
the address the account gets (normalized; for OAuth only a verified one), `redirect_to` the sign-up's app URL (the
client's `redirectTo` to `signUp` / `requestMagicLink` or the OAuth start's `redirect_to`, which passed the redirect
allowlist, else the default target). A `UserError` from it refuses the sign-up with status 403 and its code and
message (for OAuth: `rxferrix_error` / `rxferrix_error_description` in the redirect's fragment); another error is a
500 `server_error`. Nothing is created when it refuses. It runs: for a password sign-up after the address was found
free (a taken one answers `email_in_use` without it); for a magic link when the link or code is used (the code row
keeps `redirectTo`), not when it is requested; for OAuth when the callback finds neither a linked account nor a user
with the address (`store::oauth_would_create`). If an account appears or disappears between that look and the write,
the write neither creates an account nor runs the check again (`signups_disabled`). Sign-ins to existing accounts,
`sign_in_as`, `TestApp::create_user` and external JWT users never run it. `AuthService::sign_up(app, email, password,
name, redirect_to, client)`; `POST /api/auth/signup` takes `redirectTo` (the verification email's link target too).

**Users from app code** (`rxferrix::auth::users`). `ctx.users: Users` in mutations: `delete(user) ->
Result<Option<DeletedUser>>` (the user with their sessions and rotated refresh tokens, accounts, password, second
factor and email codes; `DeletedUser { sessions, accounts, credentials, verification_codes, mfa_factors }`; `None` if
there is no such user; app documents referencing the user are kept), `revoke_sessions(user) -> Result<u32>` and
`revoke_session(session) -> Result<bool>` (`false` if already gone). They write in the mutation's transaction (undone
if it fails), take effect like sign-out (live revocation, below), and check nothing: the calling function decides who
may. Not audited (the dashboard's `delete_user` / `revoke_session`, which use the same code, are). From outside a
mutation: `app.auth().delete_user(app, user)` and `app.auth().revoke_sessions(app, user)`, each in its own transaction.

`App::auth() -> &AuthService` exposes the operations behind the endpoints (`authenticate(app, token)`,
`authenticate_watched(app, token)` (also returns a `SessionWatch`, below), `sign_in_as(app, user)`, `create_admin`,
`reset_admin_password`, `disable_admin_mfa`, `create_setup_token`, `oauth_provider_ids()`, `signups_allowed()`,
`revocations()`, ...). The sign-in operations (`sign_in`, `admin_sign_in`, `verify_magic_link`,
`confirm_password_reset`, `oauth_sign_in`) return a `SignInOutcome`: `Tokens(AuthTokens)`, or `MfaRequired { mfa_token,
.. }` for an account with two-factor authentication, which `mfa_verify(app, mfa_token, code, client)` turns into a
session (`SignInOutcome::tokens()` unwraps the former).

**Two-factor authentication** (`rxferrix::auth::mfa`, `rxferrix::auth::totp`; docs/PROTOCOL.md "Auth"). Users and
administrators enroll an authenticator app (TOTP, RFC 6238) through `/api/auth/mfa/*` with their access token
(`mfa_setup` → `mfa_confirm` with the first code, which returns 8 recovery codes; `mfa_status`, `mfa_disable`,
`mfa_regenerate_recovery_codes`; accounts with a password confirm it to start a setup). Once a factor is confirmed,
every sign-in method answers `MfaRequired` until a TOTP or recovery code is presented. Codes are checked in constant
time, a TOTP code works once, 5 wrong codes lock the factor for 5 minutes (`mfa_locked`, 429). The secret is sealed
with the settings key (`secretbox`) and the owner's id as associated data; recovery codes are keyed hashes. An
impersonation session can't change a factor. The dashboard's Account page manages the administrator's own factor
(audited `mfa_enable` / `mfa_disable`), its Users page shows `mfaEnabled` and resets a user's factor
(`_system/dashboard:reset_mfa`, audited `reset_mfa`); `admin disable-2fa <email>` rescues an administrator.
`TestApp::with_auth(cfg)` and `TestApp::{query_as, mutation_as, action_as}(&identity, args)` help in tests, as do
`TestApp::create_user(email, password: Option<&str>) -> Result<Id<User>>` (a verified `_users` row, plus a password
credential when given; no auth method needs to be enabled and no email is sent; `email_in_use` if taken) and
`TestApp::as_user(id) -> TestUser` (`query` / `mutation` / `action` as `Identity::user(id, None)`, and `identity()`).
`TestApp::query_run::<F>(args) -> Result<QueryRun>` runs a query (internal ones too) and returns the result with its read
set, so a test can check which commits invalidate it: `run.read_set.overlaps(&commit)`, with the commit from
`app.db().write_log().entries_between(ts - 1, ts)` and `ts` from `TestApp::mutation_ts`.

**External JWTs** (`rxferrix::auth::jwt`). Tokens of another identity provider are accepted on `Authenticate` and as
HTTP Bearer tokens, alongside rxferrix's own:

```rust
use rxferrix::auth::{AuthConfig, JwtProvider, JwtAlgorithm};
AuthConfig::default()
    .jwt(JwtProvider::new("https://clerk.example.com").any_audience())                // JWKS by OIDC discovery
    .jwt(JwtProvider::new("https://tenant.auth0.com/").audience("https://api.example.com"))
    .jwt(JwtProvider::new("https://<ref>.supabase.co/auth/v1").audience("authenticated")
        .jwks_url("https://<ref>.supabase.co/auth/v1/.well-known/jwks.json"))
// JwtProvider: jwks_url(url), audience(aud) (repeatable), any_audience(), user_id_claim("sub"), leeway(60s),
// algorithms([JwtAlgorithm::Rs256, Es256, EdDsa]), jwks_refetch_interval(30s), jwks_cache_ttl(1h).
```

- Routing: a header `alg` of `HS256` means a rxferrix token; anything else is looked up by its `iss` (exact match). An
  unknown issuer is refused without network traffic. HMAC and `none` are never accepted for external tokens, and the
  `alg` must match the key's type and curve (and the JWK's own `alg`, if any).
- Keys: the issuer's JWKS, cached for an hour; an unknown `kid` refetches it (key rotation), at most once per
  `jwks_refetch_interval` per issuer; 5s timeouts, no redirects, 1 MiB cap; on fetch failure the cached keys stay.
- Claims: `exp` required and `nbf` optional, both with the leeway; `aud` (string or array) must contain a configured
  audience, unless `any_audience()` (startup fails if neither is set); the user id claim must be a non-empty string
  (or number) of at most 255 characters. Rejections are `AuthError` / 401 `invalid_token` with the messages listed in
  docs/PROTOCOL.md.
- Users: the subject maps to an `_accounts` row `{ provider: <issuer>, providerAccountId: <subject> }` and its
  `_users` row, both created on first sight, so `ctx.auth.user_id()` is an ordinary `Id<User>` for every sign-in
  method. External subjects are never linked to an existing user by email (the issuer vouches for its subjects, not
  for addresses another sign-in method may own); the new user copies `email` only if `email_verified` is true and no
  user has it yet, plus `name` and an https `picture`. The profile isn't updated on later sign-ins. The identity has
  no session id; the connection's identity ends at the token's `exp` (plus leeway), or as soon as the user is
  deleted. Deleting a user doesn't block the subject: its next token provisions a new user (block it at the issuer).

**Live revocation** (`rxferrix::auth::revocation`). Connections that outlive a request (sync sockets, the admin SSE
log stream) authenticate with `authenticate_watched`, which registers the token's session id (or, for external
tokens, the user id) in the app's `Revocations` registry *before* checking that it exists, and returns a
`SessionWatch` whose `ended()` future resolves when the document is deleted. One watcher task per app follows the
write log; for commits that wrote a watched document's `by_id` key it checks the document in a fresh snapshot and
fires the watches of deleted ones. So every way a session ends is covered (sign-out, refresh-token reuse,
`revoke_session`, `delete_user`, password reset, ...) and refreshes (which rewrite the session) are not mistaken for
deletions. The sync worker then sends `AuthError("session has ended")` and closes the socket (docs/PROTOCOL.md,
guarantee 10).

**Runtime settings and precedence** (`rxferrix::settings`, §4.1.4). Administrators can configure OAuth providers, SMTP,
the redirect allowlist, sign-ups, token lifetimes and CORS origins at runtime; they are stored in `_settings` (secrets
encrypted) and apply without a restart. What `AuthConfig` sets (in code, on the command line or in the environment)
wins, field by field: an explicit `signups(..)`, `access_token_ttl(..)` or `session_ttl(..)` call; a non-empty
redirect list (`redirect_url`, `--auth-redirect-url`); any `smtp(..)` / `--smtp-url` or custom `mailer`; and, per
provider id, an OAuth provider with credentials (settings may add providers with other ids). Everything else is taken
from `_settings` when set there, else the defaults apply. CORS origins exist only as a setting (default: any origin).

**System tables** (all defined with `#[table]` in `rxferrix::auth::tables`): `_users` (`User`), `_credentials` (argon2id
password hash per user; an imported bcrypt hash, `$2a$`/`$2b$`/`$2y$`, is accepted too and replaced with an argon2id
hash at the user's next successful password sign-in, without ending their sessions), `_accounts` (provider and provider
account id; for external JWTs the provider is the issuer URL), `_sessions` (user or admin, keyed hash of the current
refresh token, sliding `expires_at`, user agent, ip, `last_used_at`, and `impersonated_by: Option<Id<Admin>>` on
impersonation sessions, whose expiry is fixed), `_used_refresh_tokens` (rotated refresh token hashes, for reuse
detection; deleted with their session), `_verification_codes` (keyed hashes of a 6-digit code and a link token, purpose,
expiry, attempts (per code, and per email and purpose over the last hour), and for sign-in codes the `redirectTo` app URL, for the sign-up check), `_admins` (email, argon2id hash), `_mfa_factors` (`MfaFactor`: `user` or `admin`, `kind: "totp"`,
`secretEncrypted`, `confirmed`, `recoveryCodeHashes`, `lastUsedStep`, `failedAttempts`, `lockedUntil`; at most one
per owner, deleted with the user). The auth code reads and writes them directly through `QueryDb` /
`MutationDb` (not through registered functions), so hashes never appear in function args or the execution log.

**System functions**: `_system/auth:me` (query: the signed-in `Doc<User>` or null) and `_system/auth:update_profile`
(mutation: `{ name?, image? }`; omitted = unchanged, `""` = clear). `@rxferrix/client` exports hand-written references
as `systemApi.auth.me` / `systemApi.auth.updateProfile`. The admin-only dashboard functions are listed in §4.1;
`RxFerrixAuth.setupAdmin({ token, email, password })` completes the printed setup link.

### 1.6 File storage

```rust
use rxferrix::storage::{StorageConfig, StoredFile};

#[mutation]
pub async fn generate_upload_url(ctx: &MutationCtx) -> Result<String> { ctx.storage.generate_upload_url().await }

#[mutation]
pub async fn attach(ctx: &MutationCtx, doc: Id<Note>, file: Id<StoredFile>) -> Result<()> {
    ctx.db.patch(doc, json!({ "file": file })).await
}

#[query]
pub async fn file_url(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<String>> { ctx.storage.get_url(file).await }

#[query]   // private files: an expiring, signed URL (see "Signed URLs" for how queries stay deterministic)
pub async fn private_url(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<String>> {
    ctx.storage.get_signed_url(file, Duration::from_secs(3600)).await
}

fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new().storage(StorageConfig::s3("my-bucket", "uploads").max_upload_size(20 << 20).private(true)).run()
}
```

**The `_storage` table** (`rxferrix::storage::StoredFile`, defined with `#[table]`; TS `Id<"_storage">`, declared in
`SystemDataModel` so app tables may reference it):
```rust
#[table(name = "_storage")]
#[index(by_sha256 = [sha256])]
pub struct StoredFile {
    pub sha256: String,               // hex SHA-256 of the content (also the HTTP ETag)
    pub size: u64,                    // bytes (TS: number)
    pub content_type: Option<String>, // as uploaded (`Content-Type`), validated; None = none was sent
    pub created_by: Option<String>,   // Identity::log_summary() of whoever generated the upload URL / ran the action
    pub filename: Option<String>,     // the original name (`?filename=` on upload), sanitized; None if not given
}
pub type StorageId = Id<StoredFile>;
// created_at is the document's `_creationTime`.
```
The table is read-only through the dashboard's generic writes; files are removed with `ctx.storage.delete` (or the
Files page).

**`ctx.storage`.**
```rust
impl QueryStorage {      // deterministic, tracked reads: a query reruns when the file is deleted
    pub async fn get_url(&self, id: Id<StoredFile>) -> Result<Option<String>>;           // None if no such file
    pub async fn get_signed_url(&self, id: Id<StoredFile>, ttl: Duration) -> Result<Option<String>>; // expiry bucketed
    pub async fn get_download_url(&self, id: Id<StoredFile>, ttl: Duration) -> Result<Option<String>>; // presigned S3
    pub async fn get_download_url_with(&self, id: Id<StoredFile>, ttl: Duration, options: DownloadOptions)
        -> Result<Option<String>>;                    // DownloadOptions::default().attachment()
    pub async fn get_metadata(&self, id: Id<StoredFile>) -> Result<Option<Doc<StoredFile>>>;
}
impl MutationStorage {   // the same reads (signed and download URLs: exactly ttl from now), in the mutation's transaction, plus:
    pub async fn generate_upload_url(&self) -> Result<String>;   // single-use, signed, upload_url_ttl (1h)
    pub async fn delete(&self, id: Id<StoredFile>) -> Result<()>; // transactional; UserError "not_found" if missing
}
impl ActionStorage {     // generate_upload_url, get_url, get_signed_url / get_download_url(_with) (exact), get_metadata (latest snapshot),
                         // delete (own transaction), plus:
    pub async fn store(&self, bytes: impl Into<Bytes>, content_type: Option<&str>) -> Result<Id<StoredFile>>;
    pub async fn get(&self, id: Id<StoredFile>) -> Result<Option<Bytes>>;   // None if no such file
}
```
- **URLs.** `get_url` returns `<public url>/api/storage/<id>`. The base is `StorageConfig::public_url`, else the auth
  public URL (`--public-url`, which `serve` defaults to `http://<--http address>`), else nothing (a root-relative
  `/api/storage/<id>`, e.g. in `TestApp`). **In `--dev`**, unless a public URL is set explicitly (`--public-url` /
  `RXFERRIX_PUBLIC_URL`, `AuthConfig::public_url`, a TLS domain, or `StorageConfig::public_url`), file, signed and
  upload URLs are root-relative, so an app served by a dev server that proxies `/api` (Vite) loads and uploads files
  through it, same-origin. (`StorageConfig::public_url("")` asks for root-relative URLs explicitly.) The id carries 96 random bits, so the URL is an **unguessable capability
  URL**: whoever has it can read the file, nobody can enumerate files. It never changes, which keeps
  `get_url` deterministic inside queries. In private mode `get_url` fails with the user error `storage_private`.
  Upload URLs are `<base>/api/storage/upload?token=<token>`, token = `base64url(json {n: nonce, e: exp_s, b?:
  created_by}) "." base64url(HMAC-SHA256)`, keyed by a key derived from `auth_secret.key` (BLAKE3 KDF, context
  `"rxferrix storage v1 upload url HMAC key"`), so restarting keeps URLs valid and deleting the secret voids them.
- **Signed URLs** (`get_signed_url(id, ttl)`): `<base>/api/storage/<id>?exp=<unix seconds>&sig=<base64url
  HMAC-SHA256(id "\n" exp)>`, keyed by `blake3::derive_key("rxferrix storage v1 signed file url HMAC key", storage
  key)`. The server checks them in constant time against its clock (`Database::clock`); `thumb`, `fit` and
  `download` may be appended (the signature covers the file and the expiry, not the variant). `ttl` is 1 s to
  `max_signed_url_ttl` (7 days), else the user error `invalid_ttl`; `None` if the file doesn't exist.
  **Expiry:** in mutations and actions it is exactly `now + ttl`. In a **query** it is `now + ttl` **rounded up to a
  multiple of `ttl`** (now by the app's clock, not the snapshot's time, which is the last commit's and can be hours
  old on a quiet database): every run within one `ttl` window (aligned to the epoch) returns the same URL, so
  browsers can cache the file; the URL is valid for between `ttl` and `2·ttl`. The result depends on the time, so the
  query is never answered from the query cache. A subscription reruns only when its data changes, not when a URL
  expires: pick a `ttl` longer than the result will be shown, or fetch fresh URLs from a mutation or action.
- **Direct download URLs** (`get_download_url(id, ttl)`, `get_download_url_with(.., DownloadOptions)`): with the S3
  backend (or a custom store with `StorageConfig::signer(Arc<dyn object_store::signer::Signer>)`), a **presigned
  SigV4 URL to the bucket** (`<endpoint>/<bucket>/<prefix>/<id>?X-Amz-…`), so the bytes don't go through rxferrix and
  the bucket stays private. It carries signed `response-content-type` (the file's), `response-content-disposition`
  (`inline` for the types rxferrix serves inline, else `attachment`; always `attachment` with `.attachment()`; with
  the file name) and `response-cache-control` (`private, max-age=<lifetime>`). S3 can't add `nosniff` or the CSP
  rxferrix sends; unsafe types are attachments, on the bucket's origin, not the app's. `ttl` is 1 s to
  `max_signed_url_ttl`, and at most 7 days (S3's limit, `MAX_DIRECT_URL_TTL`), else `invalid_ttl`; lifetime exactly `ttl` in mutations and actions, `ttl` to
  `2·ttl` (at most 7 days) in queries, where the URL also changes with every run (it embeds the signing time).
  `?thumb=` can't be appended (the signature covers the whole URL): thumbnails use signed URLs of rxferrix. Once the
  file is deleted its blob goes after the commit and the URL fails (404), except with `keep_deleted`, where the blob
  (and so an earlier URL) lasts until the sweep or the URL's expiry. Temporary credentials (STS, instance roles) end
  a URL early when they expire. **Without a presigning backend** (local storage) `get_download_url` is
  `get_signed_url` (plus `&download=1` with `.attachment()`), so the same code works in development.
  `StorageService::presigns_downloads()` tells which.
- **Private mode** (`StorageConfig::private(true)`, `--storage-private` / `RXFERRIX_STORAGE_PRIVATE`): `GET
  /api/storage/<id>` without `exp`/`sig` is 403 `signature_required` (for unknown ids too), so only signed URLs
  work; `get_url` fails with `storage_private`. The dashboard's file list returns signed URLs (an hour, bucketed).
- **Single use.** A token's nonce is recorded (raw table `_storage_used_upload_tokens` in `data.db`, kept until the
  token expires) in the same transaction that inserts the file's document. A failed upload (413, a dropped
  connection) does not use the URL up; a concurrent second upload with the same URL is refused (409).
- **Blob layout and consistency.** A blob is stored at `<prefix>/<storage id>` (local default `<data dir>/storage/`,
  no prefix). The id is chosen before the upload; the blob becomes visible only when complete (one `put` up to 8 MiB,
  otherwise a multipart upload completed at the end: a staged file renamed into place locally, a completed multipart
  upload on S3; aborted on failure), and the document is inserted after that. So a document always has a complete
  blob. `MutationStorage::delete` deletes the document in the transaction and registers the blob deletion as an
  after-commit hook (`WriteTxn::on_commit`), which never runs if the mutation rolls back; it deletes the file's cached
  thumbnails too. **Orphans** (a crash between blob and document, a failed delete) are removed by
  `StorageService::sweep`: blobs named like a `_storage` id and thumbnails under `_thumbs/<id>/`, without a document,
  older than `orphan_grace` (1 h, by the app clock), and not being uploaded. Other keys under the prefix are left
  alone. The sweep runs as the **system cron `_storage_sweep`** (§1.7): an interval of `sweep_interval` (6 h; the
  first run 6 h after the first start, then on schedule across restarts) running the internal action
  `_system/storage:sweep_orphans`, whose job result is the `SweepStats` (`{ scanned, deleted, thumbnails, kept }`). With
  `sweep_interval: None` there is no such cron (`StorageService::sweep(&app)` still works).
- **Keeping deleted files.** Every deletion of a `_storage` document records its time in the raw table
  `_storage_deleted` (in the write path, so a replayed journal records it too; recreating the document removes it).
  With `keep_deleted: Some(d)` (default: `--replicate-keep` when replicating, else `None`; `--storage-keep-deleted`),
  `ctx.storage.delete` leaves the blob, and the sweep keeps an orphan until `d` after its deletion (or after it was
  written, when no deletion is recorded: a file from after the point a restore went back to), then deletes it and
  forgets the deletion. `SweepStats` adds `kept`. Without it, blobs are deleted right after the commit, as before.
- **Thumbnails** (`GET /api/storage/<id>?thumb=WxH|N[&fit=contain|cover]`, docs/PROTOCOL.md): JPEG, PNG, WebP and GIF
  (first frame) sources up to `thumbnail_max_source_bytes`, decoded with the `image` crate only after the header's
  dimensions passed `thumbnail_max_source_pixels` (decompression-bomb guard; the decoder also runs with matching
  `image::Limits`); EXIF orientation applied; `contain` never upscales, `cover` fills exactly; PNG output when the
  image has alpha, else JPEG (quality 85). Each variant is cached at `<prefix>/_thumbs/<id>/<W>x<H>-<fit>` and has
  the ETag `"<sha256>.t1.<W>x<H>.<fit>"`. At most min(cores, 4) thumbnails are generated at once, on blocking
  threads.
- **Configuration.**
  ```rust
  pub struct StorageConfig {
      pub backend: StorageBackend,              // Local(Option<PathBuf>) | S3 { bucket, prefix } | Custom { store: Arc<dyn ObjectStore>, prefix }
      pub max_upload_size: u64,                 // 100 MiB; `--max-upload-size 100MB` (RXFERRIX_MAX_UPLOAD_SIZE)
      pub upload_url_ttl: Duration,             // 1 h
      pub cache_max_age: Duration,              // 30 days (Cache-Control of served files)
      pub public_url: Option<String>,           // see URLs
      pub upload_rate_limit: Option<(u32, u32)>, // (burst, per minute) per client IP; Some((60, 120))
      pub sweep_interval: Option<Duration>,     // Some(6 h): the `_storage_sweep` system cron; None = none
      pub orphan_grace: Duration,               // 1 h
      pub keep_deleted: Option<Duration>,       // None, or --replicate-keep when replicating
      pub signer: Option<Arc<dyn Signer>>,      // presigns get_download_url for custom stores; S3 signs by itself
      pub private: bool,                        // false; `--storage-private`
      pub max_signed_url_ttl: Duration,         // 7 days
      pub thumbnail_max_dimension: u32,         // 1024 (W and H in 1..=this) unless:
      pub thumbnail_sizes: Option<Vec<(u32, u32)>>, // None; Some(allowlist) = only these sizes (bounds the cache)
      pub thumbnail_max_source_pixels: u64,     // 40_000_000
      pub thumbnail_max_source_bytes: u64,      // 32 MiB
      pub thumbnail_rate_limit: Option<(u32, u32)>, // (burst, per minute) per client IP, for variants not yet cached; Some((30, 60))
  }
  // StorageConfig::local(dir), ::s3(bucket, prefix), ::object_store(store, prefix), ::parse(location), plus builder
  // setters (`.private(true)`, `.thumbnail_sizes(&[(64, 64), (256, 256)])`, ...).
  ```
  `--storage <location>` (`RXFERRIX_STORAGE`): a directory, `file:///dir`, or `s3://bucket[/prefix]`. S3 (feature `s3`,
  default) reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` /
  `AWS_DEFAULT_REGION`, `AWS_ENDPOINT_URL` (R2, MinIO), `AWS_ALLOW_HTTP`, ... (`object_store`'s `from_env`).
  `rxferrix::storage::object_store` re-exports the crate, e.g. `object_store::memory::InMemory` for tests. Configure an
  abort-incomplete-multipart-uploads lifecycle rule on S3 buckets.
- `App::storage() -> &Arc<StorageService>`: `object_store()`, `blob_path(id)`, `thumbnail_path(id, variant)`,
  `file_url(id)`, `signed_url(id, expires_s)`, `config()`, `sweep(&app)`.
### 1.7 Scheduler and crons

```rust
use std::time::Duration;
use rxferrix::scheduler::ScheduledJob;   // the `_scheduled_jobs` table type

#[mutation]
pub async fn send(ctx: &MutationCtx, body: String) -> Result<Id<ScheduledJob>> {
    let id = ctx.db.insert(Message { body }).await?;
    ctx.scheduler.run_after::<moderate>(Duration::ZERO, (id,)).await          // any mutation or action, internal too
}

#[internal_action]
pub async fn import(ctx: &ActionCtx, pages: u32) -> Result<u32> {
    for page in 0..pages {
        tokio::select! {
            _ = ctx.canceled() => return Ok(page),                           // canceled while running: stop cleanly
            r = fetch_page(page) => { ctx.run_mutation::<save_page>((r?,)).await?; }
        }
    }
    Ok(pages)
}

fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new()
        .cron::<cleanup>("cleanup", "0 0 3 * * *", ())                       // 6 fields with seconds, UTC
        .cron_tz::<report>("report", "0 0 9 * * Mon-Fri", "Europe/Bucharest", ())   // local time, DST-aware
        .interval::<sync_feeds>("sync-feeds", Duration::from_secs(300), ())
        .run()
}
```

```rust
pub struct Scheduler { /* private */ }   // ctx.scheduler in MutationCtx and ActionCtx; Send + Sync
impl Scheduler {
    pub async fn run_after<F: SchedulableFunction>(&self, delay: Duration, args: impl Into<F::Args>) -> Result<Id<ScheduledJob>>;
    pub async fn run_at<F: SchedulableFunction>(&self, at_ms: f64, args: impl Into<F::Args>) -> Result<Id<ScheduledJob>>;
    pub async fn cancel(&self, id: Id<ScheduledJob>) -> Result<()>;      // reason "Canceled by <caller path>"
    pub async fn cancel_with_reason(&self, id: Id<ScheduledJob>, reason: &str) -> Result<()>;
    pub fn with_retries(&self, retries: u32) -> ScheduleWith<'_>;   // .run_after::<F>(..) / .run_at::<F>(..); actions only
}
impl ActionCtx {
    pub fn is_canceled(&self) -> bool;     // this scheduled action's job was canceled (false outside scheduled jobs)
    pub async fn canceled(&self);          // resolves when it is (never outside scheduled jobs); for `select!`
}
```
- **In a mutation** the job row is written in the mutation's transaction: it exists only if the mutation commits.
  `run_after` counts from `ctx.now()`. `cancel` is just as atomic.
- **In an action** scheduling and canceling take effect immediately: they run the internal system mutations
  `_system/scheduler:schedule { path, args, runAt, maxAttempts, origin, pool?, onComplete?, backoff? }`,
  `_system/scheduler:cancel { id, reason? }` and `_system/scheduler:cancel_pool { pool, reason }`.
- Args are serialized when scheduling (at most 1 MiB of JSON). `run_at` in the past means "as soon as possible".
- `cancel`: a pending job, or an action in progress, becomes `canceled` with `completedAt` and the reason in `error`
  (`"Canceled by <path>"`, the `cancel_with_reason` text, or `"Canceled from the dashboard by admin:<id>"`; `" (while
  running)"` is appended for a running action); a finished job is left alone (no error either way). An unknown id is
  the user error `not_found`.
- Scheduled functions run with **no identity** (`ctx.auth` is anonymous). The execution log names their caller
  `scheduler` (filter `identity=scheduler`), including the functions they call.

**Canceling a running action** (`crate::scheduler::cancellation`). The cancel commits the job as `canceled`; only
after that commit is the running action signaled (`ctx.is_canceled()` becomes true, `ctx.canceled()` resolves). If it
hasn't returned `SchedulerOptions::cancel_grace` (5 s) after the cancel, its future is **dropped at its next
`.await`** (a hard abort; CPU-bound code that never awaits can't be interrupted), and the execution log records the
error `ActionAborted` ("canceled: the action was aborted 5s after its job was canceled"). Consequences:
- **Nested calls:** mutations the action already ran (`ctx.run_mutation`, `ctx.scheduler`, `ctx.storage` writes)
  **stay committed**: an action is not a transaction. A nested mutation still in flight at the abort is rolled back
  (its transaction is dropped uncommitted). Nested `ctx.run_action` calls see the same cancellation (it lives in a
  task-local of the job's task) and are aborted with it.
- **At most once:** a canceled job is finished; it is never retried (not even `with_retries`), a restart leaves it
  alone, and the action's late outcome doesn't change its state: the runner only appends how it ended to `error`
  (`"; it returned before being aborted"` (its value kept in `result`), `"; it stopped with an error: …"`, or
  `"; aborted after the 5s grace period"`).
- Only scheduled actions can be canceled; an action called by a client (or directly) never sees a cancellation.
  A job canceled between its claim and its start never starts.
- The cron no-pile-up rule sees a canceled job as finished, so a cron's next run may start while a canceled action
  is still winding down (for at most `cancel_grace`). The action keeps its concurrency slot until it really stops.

**Delivery guarantees** (`crate::scheduler::runner`).
- **Mutations: exactly once.** The runner calls the mutation with a hook inside its write transaction: first it
  re-reads the job and skips it unless still `pending`; after the handler succeeds it sets `success` (attempts,
  times, result) right before the commit, so the job's writes and its completion commit together. A failing mutation
  is rolled back and a separate transaction marks the job `failed` (no automatic retry). A crash before the commit
  leaves the job `pending` with none of its writes, so it runs once after the restart.
- **Actions: at most once.** The job is committed as `inProgress` (attempts + 1) before the action starts; the outcome
  is recorded afterwards. At startup, jobs still `inProgress` become `failed` with the error `server restarted while
  running`. With `with_retries(n)`, an action that *returns an error* goes back to `pending` up to `n` times, after
  `retry_backoff` (1s) doubled per retry (at most 1h); an interrupted one is never retried.
- **Ordering:** due jobs start in `(runAt, scheduledAt)` order (`scheduledAt` is the job's strictly increasing
  `_creationTime`) within mutations and within each pool, so jobs due at the same time run in scheduling order. Each
  dispatch round runs the due mutation jobs first, one at a time, in that order (they serialize on the single writer
  anyway), then starts each pool's due actions; actions run concurrently.
- **Concurrency:** at most `SchedulerOptions::concurrency` (10) actions of the default pool run at once; workpools
  have their own slots and mutations need none (see Workpools below).
- **Waking:** the background loop sleeps until the next job or cron is due (at most 60s) and wakes as soon as a commit
  in the write log touches `_scheduled_jobs`.

**Crons.** `RxFerrix::cron::<F>(name, expr, args)` / `RxFerrix::cron_tz::<F>(name, expr, tz, args)` /
`RxFerrix::interval::<F>(name, every, args)` (or the same methods on `SchedulerOptions`; `CronSpec::cron` /
`CronSpec::cron_tz` / `CronSpec::interval`, and `CronSpec::timezone(tz)`). Expressions have 6 fields with seconds
(`sec min hour day-of-month month day-of-week`, an optional 7th is the year; `cron` crate syntax), evaluated in UTC or
in the given IANA zone (`chrono-tz`); an interval runs first `every` after it is first declared (or changed), then
on schedule across restarts (an unchanged `_crons` row keeps its next run). `App::open` fails on an invalid
expression (or one with 5 fields), an unknown zone, a zone on an interval, a duplicate or empty name, a name starting
with `_` (reserved for system crons), a zero interval, or args that don't serialize.
- **Time zones and DST.** Matching local times are mapped to instants: one in a **spring-forward gap** runs later by
  the gap's length (`0 30 2 * * *` in `America/New_York` runs at 03:30 EDT that day), so daily jobs never skip a day;
  one in a **fall-back fold** runs once, at its first occurrence, except for expressions whose hour field is `*`,
  which also run in the repeated hour (they follow elapsed time). `_crons.timezone` holds the zone (`null` for UTC;
  `"UTC"` is normalized to `null`).
- **System crons** are declared by rxferrix itself, named with a leading `_`: today `_storage_sweep` (§1.6). They are
  ordinary `_crons` rows and jobs (origin `cron:_storage_sweep`); the dashboard's `crons` lists them last with
  `system: true`. Their row is inserted by the startup reconcile, so a fresh app has already committed once
  before any function runs; don't assume a new `TestApp` starts at ts 0 (`app.db().write_log().latest()` is the
  starting ts).
- At startup the declared crons are **reconciled** into `_crons`: new ones are inserted with their next run, changed
  ones (schedule, zone, function or args) get a fresh next run, and rows of crons no longer declared are deleted (jobs
  they already enqueued stay).
- A due cron **enqueues an ordinary job** (origin `cron:<name>`, due now): one execution path for everything.
- **Missed runs** (downtime, or a test clock jump) run **once**: the next run is computed from now, not from the missed
  time.
- **No pile-up:** while the job of the cron's previous run is still `pending` or `inProgress`, a due run is skipped
  (logged as a warning); its next run still advances.
- **Run now:** `_system/scheduler:run_cron_now { name, force? }` (admin, §4.1.2) enqueues a job due immediately and
  makes it the cron's `lastRun` / `lastJob` (`nextRun` unchanged); the no-pile-up rule applies unless `force`.

**History and retention.** Finished jobs (`success`, `failed`, `canceled`) stay in `_scheduled_jobs` for
`history_after` (1h), so subscriptions and `ctx.db.get(job)` see how they ended. Then they **move to the job history**
in `aux.db` (table `_job_history`: the whole document as JSON, by id), outside `data.db`: backups and replication
snapshots don't grow with the number of jobs run. Each batch is committed to the history before it is deleted from
`_scheduled_jobs` (a crash in between writes the same row again next time). The last job of every cron stays in
`_scheduled_jobs` (the Scheduler page shows its state). Jobs, in either place, are deleted `retention` (7 days;
`None` keeps them) after they finished, as are finished workflows (with their steps). `history_after: None` keeps
finished jobs in `_scheduled_jobs` until `retention`. A built-in cleanup does all this at startup and every
`cleanup_interval` (1h); `App::cleanup_scheduled_jobs()` runs it now. The history is read with
`App::job_history(HistoryFilter { state, path, limit, before_completed_at, before_id })` (most recently finished
first; pass the `completedAt` and id of a page's last job for the next one) and `App::job_history_get(id)`, and over
HTTP (§4.2). It is not reactive, and it is in a backup only with `--with-aux`.

```rust
#[derive(Debug, Clone)]
pub struct SchedulerOptions {          // AppOptions::scheduler; RxFerrix::scheduler(opts) keeps declared crons and workpools
    pub concurrency: usize,            // 10
    pub history_after: Option<Duration>, // Some(1 hour): finished jobs then move to the job history (aux.db)
    pub retention: Option<Duration>,   // Some(7 days)
    pub cleanup_interval: Duration,    // 1 hour
    pub retry_backoff: Duration,       // 1 second
    pub cancel_grace: Duration,        // 5 seconds: a canceled action is aborted this long after the cancel
    pub crons: Vec<CronSpec>,
    pub pools: Vec<Workpool>,          // RxFerrix::workpool(..) / SchedulerOptions::workpool(..)
}
impl App {
    pub async fn start_scheduler(&self) -> Result<()>;      // `rxferrix serve` calls it; idempotent
    pub async fn run_due_jobs(&self) -> Result<usize>;      // run what is due now, until nothing is (tests)
    pub async fn cleanup_scheduled_jobs(&self) -> Result<usize>;   // jobs moved or deleted from _scheduled_jobs
    pub async fn job_history(&self, filter: HistoryFilter) -> Result<Vec<Doc<ScheduledJob>>>;
    pub async fn job_history_get(&self, id: Id<ScheduledJob>) -> Result<Option<Doc<ScheduledJob>>>;
}
// Database::clock(): Clock { now_ms(), advance(Duration), offset() }, the wall clock plus an offset that only grows.
// Mutations' `now()` and the scheduler read it.
```
**Tests.** A `TestApp` runs the scheduler's startup work (recovery, cron reconcile) but no background loop, so tests
are deterministic: `t.run_due_jobs()` runs what is due; `t.advance_time(d)` moves the clock forward and then runs
what became due; `t.restart()` / `t.restart_with(options)` reopen the data dir (keeping the clock offset) like a
restart; `t.start_scheduler()` runs the background loop (e.g. for tests over a socket).

**Workpools and `on_complete`** (built into the scheduler). A workpool is a named limit on
how many of its actions run at once, with defaults for their retries:

```rust
rxferrix::RxFerrix::new()
    .workpool(Workpool::new("emails").max_parallelism(5).retries(3).backoff(Duration::from_millis(250), 2.0))

#[mutation]
pub async fn invite(ctx: &MutationCtx, to: String) -> Result<Id<ScheduledJob>> {
    ctx.scheduler.pool("emails").on_complete::<sent>(to.clone()).enqueue::<send_email>((to,)).await
}

#[internal_mutation]   // `{ workId, context, result }`, result = { kind: "success", returnValue } | { kind: "failed", error } | { kind: "canceled" }
pub async fn sent(ctx: &MutationCtx, work_id: Id<ScheduledJob>, context: String, result: WorkResult<String>) -> Result<()> { … }
```
```rust
pub struct Workpool { pub name: String, pub max_parallelism: usize /* 10 */, pub retries: u32 /* 0 */, pub backoff: Option<Backoff> }
impl Scheduler {   // each returns a ScheduleWith; the options combine, in any order
    pub fn pool(&self, name: &str) -> ScheduleWith<'_>;
    pub fn on_complete<F: MutationFunction>(&self, context: impl Serialize) -> ScheduleWith<'_>;
    pub async fn cancel_pool(&self, name: &str) -> Result<u64>;   // every pending job and running action of the pool
}
impl ScheduleWith<'_> {
    pub fn pool(self, name: &str) -> Self;  pub fn with_retries(self, n: u32) -> Self;
    pub fn backoff(self, initial: Duration, base: f64) -> Self;
    pub fn on_complete<F: MutationFunction>(self, context: impl Serialize) -> Self;
    pub async fn run_after<F>(self, delay, args) -> Result<Id<ScheduledJob>>;  run_at<F>(..);  enqueue<F>(args) /* = run_after(0) */
}
#[serde(tag = "kind")] pub enum WorkResult<T = Value> { Success { return_value: T /* "returnValue" */ }, Failed { error: String }, Canceled }
```
- **Slots.** Every running action holds a slot of its pool: the default pool's (`SchedulerOptions::concurrency`) when
  scheduled without `pool`, else its workpool's `max_parallelism`. Pools don't share slots: a full pool doesn't hold
  up the default pool or another pool. Within a pool, actions start in the order they are due. **Mutations need no
  slot** (they run one at a time in the dispatcher anyway, first in each round), whatever their pool.
- **Defaults.** An action in a pool gets the pool's `retries` and `backoff` unless the call sets its own. Backoff:
  wait `initial`, then `base` times longer before each further retry, at most an hour (default: `retry_backoff`,
  doubling). Pools are declared at startup (unknown name: an error when scheduling; duplicate names, empty names,
  names over 64 characters, `max_parallelism: 0`, a backoff `base` below 1 or a negative `initial` fail `App::open`). Pending actions naming a pool the app no longer declares still run, in a
  pool of the default size (with a warning at startup).
- **`on_complete`.** When the job reaches its final state (success; failed once no retries are left, including
  "server restarted while running"; canceled, also while running), the transaction that records it also schedules
  the mutation `F` (due now, origin `onComplete:<job id>`) with args `{ workId, context, result }`. So it runs exactly
  once, after the outcome is committed, in its own transaction (if it fails, that's its own failed job; the work's
  outcome stands). A return value too large for job args (1 MiB) is passed as `failed`. `on_complete::<F>` only takes
  a mutation (`MutationFunction`: an action doesn't compile).

### 1.7.1 Workflows

Durable functions made of steps. A `#[workflow]` body awaits **steps**; each step's
outcome is journaled, so a workflow survives restarts, can sleep for days, and wait for events:

```rust
#[workflow]
pub async fn onboard(step: &WorkflowCtx, user: Id<User>) -> Result<String> {
    let email = step.run_query::<email_of>((user,)).await?;
    step.run_action::<send_welcome>((email,)).with_retries(3).pool("emails").await?;
    step.sleep(Duration::from_secs(3 * 86_400)).await?;
    let (a, b) = futures::try_join!(step.run_action::<x>(()).into_future(), step.run_action::<y>(()).into_future())?;
    let answer: String = step.wait_for_event("survey").await?;
    step.run_mutation::<save_answer>((user, answer.clone())).await?;
    Ok(answer)
}

#[mutation]
pub async fn sign_up(ctx: &MutationCtx, email: String) -> Result<Id<Workflow>> {
    let user = ctx.db.insert(User { email }).await?;
    ctx.workflows.on_complete::<onboarded>(user).start::<onboard>((user,)).await   // only if this commits
}
```
```rust
impl WorkflowCtx {
    pub fn workflow_id(&self) -> Id<Workflow>;
    pub fn run_query<F: QueryFunction>(&self, args) -> Step<F::Output>;        // inside the advancing transaction
    pub fn run_mutation<F: MutationFunction>(&self, args) -> Step<F::Output>;  // a scheduled job: exactly once
    pub fn run_action<F: ActionFunction>(&self, args) -> Step<F::Output>;      // a scheduled job: at most once per attempt
    pub fn sleep(&self, d: Duration) -> Step<()>;
    pub fn wait_for_event<T: DeserializeOwned>(&self, name: &str) -> Step<T>;
}
impl<T> Step<T> { pub fn with_retries(self, n: u32) -> Self; pub fn pool(self, name: &str) -> Self; pub fn backoff(self, initial: Duration, base: f64) -> Self; }
impl<T: DeserializeOwned> IntoFuture for Step<T> { type Output = Result<T>; }
pub struct Workflows;   // ctx.workflows in MutationCtx (transactional) and ActionCtx (immediate)
impl Workflows {
    pub async fn start<W: WorkflowFunction>(&self, args: impl Into<W::Args>) -> Result<Id<Workflow>>;
    pub fn on_complete<F: MutationFunction>(&self, context: impl Serialize) -> StartWith<'_>;   // .start::<W>(args)
    pub async fn cancel(&self, id: Id<Workflow>) -> Result<()>;
    pub async fn send_event(&self, id: Id<Workflow>, name: &str, value: impl Serialize) -> Result<()>;
}
```
- **How it runs** (`crate::workflow::engine`). `start` inserts the `_workflows` row (state `running`) and schedules
  `_system/workflow:advance`. Advancing runs the body **from the start** inside that mutation: every step it starts
  gets the next index (in the order the step futures are created, i.e. awaited or passed to `try_join!`); a step in
  the journal (`_workflow_steps`) resolves at once with its recorded result or error, a new one is recorded and stays
  pending. When the body can't go further, the new steps start: **queries** run right there in the transaction (and
  the body runs again with their result, up to 100 times per transaction, then a new `advance` job continues);
  **mutations and actions** become scheduled jobs (pool, retries and backoff from the step's options, origin
  `workflow:<id>`) whose `on_complete` is `_system/workflow:step_done`, which records the outcome and advances, in one
  transaction; a **sleep** schedules `_system/workflow:wake` for when it ends; an **event** waits for `send_event`.
  Steps started together (`try_join!`) run in parallel.
- **The body must be deterministic**: replayed with the same step outcomes it must start the same steps in the same
  order. It reads and writes only through steps, sleeps with `step.sleep`, and keeps the clock and randomness in steps.
  Each run checks that step `i` is the same kind and function (or event name) as journaled; if not (e.g. code changed
  while it ran) the workflow **fails** ("not deterministic"). Racing steps (`select!`) can replay differently: avoid.
- **Errors.** A failed step is an `Err` the body can handle (compensate, try something else); `?` lets it end the
  workflow as `failed` with that error. A step that can't be scheduled (e.g. an unknown pool) fails the same way. A
  body that returns `Err`, panics, or waits on anything that isn't a step fails the workflow. At most 10 000 steps.
- **Events.** `send_event(id, name, value)` completes the oldest step waiting for `name`, or is kept on the workflow
  (`events`) for the next `wait_for_event(name)`; each event is taken once. `not_found` for an unknown workflow,
  `finished` for a finished one.
- **Cancel.** `cancel(id)` finishes a running workflow as `canceled` (`error` = `"Canceled by <path>"`): its running
  steps are canceled (their jobs too, a running action is signaled as in §1.7), what its steps already did stays done.
  A finished workflow is left alone.
- **`on_complete`** works like the scheduler's: `{ workflowId, context, result: WorkResult }`, scheduled in the
  transaction that finishes the workflow (origin `workflow:<id>`).
- **Status** is the workflow's document: `ctx.db.get(id)` (live in a query); `Id<Workflow>` can be returned to
  clients (`Id<"_workflows">`, see §4). Finished workflows and their steps are deleted with the scheduler's
  `retention`.
- In an action, `ctx.workflows` calls the internal mutations `_system/workflow:start`, `:cancel` and `:send_event`.

### 1.8 Search: full-text (FTS5) and vector (sqlite-vec)

```rust
#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel])]            // body: String or Option<String>
#[search_index(search_stems, search = body, tokenizer = "porter", language = "english")]   // "run" finds "running"
#[search_index(search_sub, search = body, tokenizer = "trigram", diacritics = "keep")]     // substrings
#[search_index(search_ro, search = body, tokenizer = "snowball", language = "romanian")]   // "orașelor" finds "orașul"
#[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel], kind = "hnsw")]  // Vec<f32> / Option<Vec<f32>>
// HNSW parameters (optional, kind = "hnsw" only): #[vector_index(.., kind = "hnsw", m = 32, ef_construction = 256, ef_search = 64)]
pub struct Message { pub channel: Id<Channel>, pub body: String, pub embedding: Option<Vec<f32>> }

#[query]
pub async fn search(ctx: &QueryCtx, channel: Id<Channel>, text: String) -> Result<Vec<Doc<Message>>> {
    ctx.db.query::<Message>()
        .with_search_index(Message::search_body, |q| q.search(Message::body, text).eq(Message::channel, channel))
        .take(10)
        .await
}

#[query]   // works with usePaginatedQuery like index pagination (§1.2.1)
pub async fn search_page(ctx: &QueryCtx, channel: Id<Channel>, text: String, pagination_opts: PaginationOpts)
    -> Result<PaginationResult<Doc<Message>>> {
    ctx.db.query::<Message>()
        .with_search_index(Message::search_body, |q| q.search(Message::body, text).eq(Message::channel, channel))
        .paginate(pagination_opts)
        .await
}

#[action]
pub async fn similar(ctx: &ActionCtx, embedding: Vec<f32>, channel: Id<Channel>) -> Result<Vec<Id<Message>>> {
    let opts = VectorSearchOpts::new(10).filter(VectorFilter::eq(Message::channel, channel));
    let hits = ctx.vector_search(Message::by_embedding, &embedding, opts).await?;   // [(id, cosine similarity)]
    Ok(hits.into_iter().map(|(id, _score)| id).collect())
}

#[query]   // exact, deterministic, reactive: reruns when a write changes the nearest messages of the channel
pub async fn more_like(ctx: &QueryCtx, id: Id<Message>) -> Result<Vec<Doc<Message>>> {
    let Some(msg) = ctx.db.get(id).await? else { return Ok(vec![]) };
    let Some(embedding) = msg.embedding.clone() else { return Ok(vec![]) };
    let opts = VectorSearchOpts::new(6).filter(VectorFilter::eq(Message::channel, msg.channel));
    let mut out = Vec::new();
    for (other, _score) in ctx.db.vector_search(Message::by_embedding, &embedding, opts).await? {
        if other != id && let Some(doc) = ctx.db.get(other).await? { out.push(doc); }
    }
    Ok(out)
}
```

```rust
impl<T> SearchFilter<T> {
    pub fn search<V>(self, field: Field<T, V>, text: impl Into<String>) -> Self;   // exactly once, on the search field
    pub fn eq<V: Serialize>(self, field: Field<T, V>, value: V) -> Self;          // any filter field, any number
}
impl<'a, T: Table> SearchQuery<'a, T> {      // no order: results are ordered by relevance
    pub fn filter(self, pred: impl Fn(&Doc<T>) -> bool + Send + Sync + 'a) -> Self;   // applied after ranking
    pub async fn take(self, n: usize) -> Result<Vec<Doc<T>>>;     // n clamped to MAX_SEARCH_RESULTS (1024)
    pub async fn first(self) -> Result<Option<Doc<T>>>;
    pub async fn collect(self) -> Result<Vec<Doc<T>>>;            // up to MAX_SEARCH_RESULTS
    pub async fn paginate(self, opts: PaginationOpts) -> Result<PaginationResult<Doc<T>>>;   // see "Pagination"
}
impl QueryDb { pub fn search_table(&self, table: &str, index: &str) -> RawSearchQuery<'_>; }   // untyped (dashboard)
impl RawSearchQuery<'_> {   // search(field, text), eq(field, json), filter(Fn(&Value) -> bool), take, collect, paginate
}
pub struct VectorSearchOpts<T> {
    pub limit: usize,                       // 1..=256, default 10
    pub filter: Option<VectorFilter<T>>,
    pub exact: bool,                        // actions on HNSW indexes: search exactly anyway (default false)
    pub ef: Option<usize>,                  // actions on HNSW indexes: search width (default: the index's ef_search)
}
impl<T> VectorSearchOpts<T> {
    pub fn new(limit: usize) -> Self; pub fn filter(self, f: VectorFilter<T>) -> Self; pub fn exact(self) -> Self;
    pub fn ef(self, ef: usize) -> Self;     // 1..=4096 (HnswParams::EF_SEARCH_RANGE), checked when the search runs
}
impl QueryDb {   // queries and mutations: always exact (vec0) at the snapshot / in the transaction; `exact`, `ef` ignored (`ef` still range-checked)
    pub async fn vector_search<T: Table>(&self, index: VectorIndex<T>, vector: impl AsRef<[f32]>,
                                         opts: VectorSearchOpts<T>) -> Result<Vec<(Id<T>, f32)>>;
}
impl ActionCtx {  // the latest committed state; the HNSW graph unless `exact` (or while it isn't ready)
    pub async fn vector_search<T: Table>(&self, index: VectorIndex<T>, vector: impl AsRef<[f32]>,
                                         opts: VectorSearchOpts<T>) -> Result<Vec<(Id<T>, f32)>>;
}
// TableDef additions: SearchIndexDef { .., tokenizer: Tokenizer },
//                     VectorIndexDef { .., kind: VectorIndexKind, hnsw: HnswParams }.
pub struct Tokenizer { pub kind: TokenizerKind /* Unicode61 | Porter | Trigram | Snowball */,
                       pub language: Option<&'static str>, pub remove_diacritics: bool }
impl Tokenizer {   // const fns, used by the #[table] expansion
    pub const LANGUAGES: &[&str];            // porter: ["english"]
    pub const SNOWBALL_LANGUAGES: &[&str];   // arabic danish dutch english finnish french german greek hungarian italian
                                             // norwegian portuguese romanian russian spanish swedish tamil turkish
    pub const fn unicode61() -> Self; pub const fn porter(language: &'static str) -> Self; pub const fn trigram() -> Self;
    pub const fn snowball(language: &'static str) -> Self;
    pub const fn keep_diacritics(self) -> Self;
}
pub enum VectorIndexKind { Exact /* default */, Hnsw }
pub struct HnswParams { pub m: usize, pub ef_construction: usize, pub ef_search: usize }   // camelCase JSON, TS
impl HnswParams {
    pub const DEFAULT: HnswParams;           // m 16, ef_construction 128, ef_search 128 (required for exact indexes)
    pub const M_RANGE: RangeInclusive<usize> = 4..=64;
    pub const EF_CONSTRUCTION_RANGE: RangeInclusive<usize> = 16..=2048;
    pub const EF_SEARCH_RANGE: RangeInclusive<usize> = 1..=4096;
    pub fn check(&self) -> Result<(), String>;
}
impl<T> VectorFilter<T> {   // OR of ANDs of equalities on filter fields
    pub fn eq<V: Serialize>(field: Field<T, V>, value: V) -> Self;
    pub fn any_of<V: Serialize>(field: Field<T, V>, values: impl IntoIterator<Item = V>) -> Self;
    pub fn or(self, other: Self) -> Self;
    pub fn and(self, other: Self) -> Self;
}
```

**Text search semantics.**
- The text is split into terms at every non-alphanumeric character; each term becomes an FTS5 string literal, so
  user input can never inject FTS5 syntax (quotes, `*`, `NEAR`, `AND`/`OR`/`NOT`, `col:` filters and parentheses
  are separators or plain words). A document matches when its search field contains **every** term; the **last
  term is a prefix** (search-as-you-type: `"hello wor"` matches "hello world"). At most 32 terms are used.
- **Empty text** (or text with no alphanumeric characters) matches nothing: the query returns `[]` and reads nothing.
- **Tokenizers** (`#[search_index(.., tokenizer = .., language = .., diacritics = ..)]`, all optional):
  - `"unicode61"` (default): FTS5 `unicode61 remove_diacritics 2`: words, case-insensitive, `café` = `cafe`. Scripts
    without spaces (e.g. Chinese) are indexed as whole runs.
  - `"porter"`: FTS5 `porter unicode61 …`: words reduced to their English stem, so "run", "runs" and "running" match
    each other (the last term is a prefix of the stem). `language` is only accepted with `porter` (only
    `"english"`, the default; FTS5's stemmer is English-only) and `snowball`.
  - `"snowball"` with `language = "…"` (required; one of `Tokenizer::SNOWBALL_LANGUAGES`): the custom FTS5 tokenizer
    `rxferrix_snowball <language> remove_diacritics <0|2>` (`db::search::stem`), wrapping `unicode61`: words are split
    and case-folded exactly as by `unicode61`, then reduced to their stem by the language's Snowball stemmer
    (`rust-stemmers`), so inflected forms match each other (Romanian "orașul" / "orașelor", French "chanter" /
    "chantait", German "Haus" / "Häusern"); the last term is a prefix of the stem, as with `porter`. Stemming sees
    the word with its accents (the stemmers' suffix rules need them; Romanian `ș`/`ț` with a comma below are read as
    the cedilla forms the stemmer knows). With `diacritics = "remove"` (default) the token is the stem folded like
    `unicode61 remove_diacritics 2`, and, when the stemmer cuts the unaccented spelling differently, the folded
    word's stem is added as a colocated synonym (in documents and in queries), so a word typed without accents still
    finds the accented forms. Synonyms don't change relevance: they add no occurrences and no length. With `"keep"`
    accents distinguish terms. The tokenizer is registered on every SQLite connection (by the same auto extension as
    `rxferrix_score`): the writer, the read-only readers, and those opened by the HNSW thread, backups and exports.
  - `"trigram"`: FTS5 `trigram`: substring search; each term matches anywhere inside a word ("ell" finds "hello"),
    the last term is not a prefix (substrings cover it), and terms shorter than 3 characters are ignored (FTS5
    can't match them), so text whose terms are all shorter matches nothing. Case-insensitive.
  - `diacritics = "remove"` (default) folds diacritics; `"keep"` makes `café` and `cafe` different terms
    (`remove_diacritics 0`).
  Changing an index's tokenizer (kind, language or diacritics) rebuilds it at startup (same index id, backfilled).
- **Relevance**: BM25's per-document part (`db::search::score`): for each query phrase with `tf` occurrences in a
  document of `dl` tokens, `Σ tf·(k1+1) / (tf + k1·(1 − b + b·dl/32))`, `k1 = 1.2`, `b = 0.75`. There is no IDF and
  the reference length is fixed (not the corpus average), so a document's score depends only on its own text and the
  query: more occurrences rank higher (saturating), shorter documents rank higher for equal occurrences. Every term
  must match anyway (AND), so IDF would only re-weigh terms every result contains; in exchange, results, page
  boundaries and per-filter-value invalidation are exact (see below). Ties: first-indexed first (FTS rowid).
- At most `MAX_SEARCH_RESULTS` (1024) documents are examined per query (per page with `paginate`); `take(n)` with a
  larger `n` is clamped. `eq` filters are applied inside FTS5 (they don't use up the cap); a `.filter(...)` predicate
  is applied to the ranked candidates.
- Documents whose search field is missing, null or not a string are not indexed (never returned).
- `with_search_index` after `with_index` or `order(Desc)` fails when run. Using a field other than the index's search
  field in `search`, a field that is not a filter field in `eq`, or `search` other than exactly once is an error.
- Equality on filter fields compares the memcomparable encoding ([Design](DESIGN.md#storage)), i.e. the same equality as index
  keys: a missing field is not equal to `null`.

**Pagination** (`SearchQuery::paginate`, `RawSearchQuery::paginate`; same `PaginationOpts` / `PaginationResult` and
client behaviour as §1.2.1, so `usePaginatedQuery` works unchanged).
- **Order and positions.** Matches are ordered by `(score desc, FTS rowid asc)`. Because a score depends only on its
  document (see Relevance), this is a *stable ordering key*: a document's position changes only when that document
  is written. A cursor is a position "just past `(score, rowid)`" or "the end": URL-safe base64 (no padding) of
  `[2][fingerprint: 8 bytes][0][score: f64 bits BE][rowid: i64 BE]` or `[2][fingerprint][1]`, the fingerprint being a
  blake3 hash of the index id, the FTS5 match expression (the normalized terms: whitespace and punctuation don't
  matter) and the `eq` filters. A cursor of another search (text, filters, index) or of index pagination fails with
  `paginate: cursor does not belong to this query`; garbage with `paginate: malformed cursor`.
- **Without `end_cursor`**: the first `numItems` matches after `cursor` (clamped to 1024) passing `.filter(..)`,
  examining at most 1024 matches; `continueCursor` is just past the last examined match. `isDone` is exact (one more
  match is looked up), so a page ending at the last match is done (`continueCursor` = the end) and no trailing empty
  page is needed.
- **With `end_cursor`**: every match in `(cursor, endCursor]`, whatever `numItems` says; `continueCursor =
  endCursor`, `isDone` iff `endCursor` is the end. `endCursor` before `cursor` is an error.
- **Within one snapshot** a chain of pinned pages partitions the matches: no document is duplicated or skipped (the
  client applies all its pages at one ts, so this holds on screen). **Across changes**, only written documents move:
  a new or newly matching document appears in the page whose interval contains its position (pinned pages grow, the
  last page grows up to `numItems`), a deleted or no longer matching one disappears, an edited one may move to
  another page. Nothing else shifts.
- **Read set**: the same synthetic key range as `take` (below) plus a `by_id` point per examined document. So every
  page of a search reruns after a write in its filter partition, and none after writes elsewhere.

**Vector search semantics.** Two entry points with the same options and results: `(id, score)` with `score` the
cosine similarity (higher = more similar), best first.
- **`ctx.db.vector_search` (`QueryDb`: queries, and mutations through `MutationDb`)**: always the **exact** `vec0`
  scan, also on HNSW indexes (`exact` and `ef` have no effect, though `ef` is still range-checked), at the query's snapshot (a mutation sees its own
  writes). Deterministic: the same snapshot gives the same results, ties broken by document id. Reactive: see
  "Reactivity" below. Use it when the query vector is already known to the query (stored on a document, sent by the
  client); it costs a full scan of the matching entries per run and per rerun (about 18 ms per k=10 search over
  50k × 384).
- **`ctx.vector_search` (`ActionCtx`)**: for embeddings from external APIs (only actions may call them) and large
  indexes: the latest committed snapshot, the HNSW graph when the index has one (approximate), not reactive (actions
  have no read set). The documents may change before the action loads them (`ctx.run_query`).
- Why queries never use the graph: an HNSW search is deterministic for a fixed graph, but the graph is not a function
  of the snapshot (it is updated after commits, rebuilt in the background, loaded from a file at startup), so two
  runs at one snapshot could return different neighbours, and a rerun could change its result without any write it
  depends on.
Errors: a query vector whose length isn't the index's `dims` (or containing NaN/inf), `limit` outside `1..=256`, `ef`
outside `1..=4096`, a filter on a field that isn't a filter field, more than 64 `or` branches. Writing a document whose vector field has the
wrong length (or isn't an array of numbers) fails before anything is written, unless the write leaves that field
unchanged (a vector made unindexable by a change of `dims` stays unindexed, and the document stays editable).
Documents whose vector field is missing or null are not indexed. Results are `(id, score)` tuples, which can be
returned as is (TS `Array<[Id<"x">, number]>`), though a named `#[object]` struct reads better in the client.
- **`kind = "exact"`** (default): sqlite-vec's `vec0` brute-force scan; exact (checked against brute force). About
  18 ms per k=10 query at 50k × 384, 75–120 ms at 200k × 384.
- **`kind = "hnsw"`** (`db::search::hnsw`, `db::search::ann`): an in-memory HNSW graph (cosine; by default M = 16,
  ef_construction = 128, search width `ef = max(ef_search = 128, k')`) on top of the same `vec0` table, which stays
  the source of truth.
  Approximate: recall@10 ≈ 0.95 at 200k × 384 for embedding-like data (low intrinsic dimension), ~0.4 ms per query;
  uniformly random high-dimensional vectors (no structure) are the known worst case. `opts.exact()` forces the exact
  scan. While the graph isn't ready (building at startup, or failed), searches are exact.
  - *Consistency*: every write changing an HNSW entry adds `(index id, doc id)` to its commit entry; the graph is
    updated after commit from the write log (a background thread follows commits, and every search first catches
    the graph up to its own snapshot), so a search sees every commit up to its snapshot (or later). If the log no
    longer covers the gap (it keeps 60 s / 100k commits / 32 MiB), the whole `vec0` table is diffed instead. Rolled-back writes never
    reach the log.
  - *Deletes*: a deleted or changed document's node becomes a tombstone (still routing, never returned); when
    tombstones exceed 25% of the nodes (and 256) the graph is rebuilt from the live nodes on all cores in the
    background, with changes made meanwhile journaled and replayed before the swap.
  - *Filters*: post-filtering with oversampling. Each node keeps its filter values and each `(field, value)` has a
    posting set; if at most 4096 live documents match the filter, their distances are computed exactly; otherwise
    the graph is asked for `k' = k · live / matching · 1.25` candidates, filtered, and retried with `4k'`, then `16k'` (three graph
    searches at most, then the exact computation) if fewer than `k` remain.
  - *Parameters*: `#[vector_index(.., kind = "hnsw", m = N, ef_construction = N, ef_search = N)]` (`HnswParams`:
    `m` in 4..=64, links per node, layer 0 allows 2m; `ef_construction` in 16..=2048; `ef_search` in 1..=4096, the
    default search width), checked by the macro and at startup (parameters on an exact index are an error). Per
    search: `VectorSearchOpts::ef(n)` (higher: better recall, slower; e.g. recall@10 0.77 at `ef = 10` and 1.0 at
    `ef = 400` on 10k random 24-dim vectors). The stored definition holds the parameters: changing `m` or
    `ef_construction` keeps the `vec0` table but builds a new graph (a saved file whose graph was built with other
    parameters is discarded and replaced); changing `ef_search` keeps the graph.
  - *Concurrent inserts*: a node writes its own link lists on every layer before any neighbour links back to it, so
    it is reachable only once fully linked (otherwise a preempted insert could leave nodes that even a search for
    their own vector misses).
  - *Persistence*: indexes with ≥ 10 000 live vectors are saved to `<data dir>/vector_index_<id>.hnsw` (graph,
    vectors, per-node doc id, filter values and vector hash; blake3 checksum; written to a temporary file and renamed)
    after the build, after rebuilds and every `max(1000, live/10)` changes. At startup a file with a valid checksum,
    format and definition is loaded and **validated against the table**: the whole `vec0` table is scanned at one
    snapshot and every difference (changed vector or filter values, missing or extra documents) is applied. A missing,
    stale or corrupt file only costs a rebuild; smaller indexes are rebuilt at every startup. Files of indexes that no
    longer exist (or became exact) are deleted.
  - The background thread stops, and closes its reader connection, before a dropped `Database` finishes dropping.
  - Benchmarks (`search::perf_hnsw`, release, 16 threads): building 200k × 384 takes 9–25 s; saving the 330 MB
    file ~0.2 s, loading it ~0.2 s.

**Storage and consistency (search).** Each search index is an FTS5 table (`_search_<n>`: the text plus the filter fields as
`UNINDEXED` columns, each the hex of its memcomparable encoding, and `_search_<n>_docs` mapping FTS rowids to doc
ids); each vector index is a `vec0` table (`_vector_<n>`, cosine distance, filter fields as metadata columns). They
are listed in `_search_indexes` and updated inside the mutation's write transaction by both the typed and untyped
write paths, so search results are always consistent with document reads and roll back with the mutation.
`App::open` reconciles them like ordinary indexes: creates and backfills new ones, rebuilds (drop, create, backfill)
indexes whose definition changed (fields, dimensions, tokenizer), and drops removed ones (a backfill skips documents
whose vector has the wrong length, with a warning). Switching a vector index between `exact` and `hnsw`, or changing
its HNSW parameters, only updates its stored definition (the `vec0` table is kept). Stored definitions from before tokenizers existed read as the
default tokenizer, so they are not rebuilt. `Database::verify_indexes` also recomputes every search and vector index
and compares each ready HNSW graph's live nodes with its `vec0` table.

**Reactivity (per filter value).** Search and vector indexes have ids in the upper half of the index id space
(`db::search::SPECIAL_INDEX_BASE = 1 << 31`). A search index's synthetic keys are laid out as
`[filter column i: 1 byte][memcomparable encoding of the document's value of filter field i][doc id: 16 bytes]`:
every write that changes a document's entry in a search index (its text or a filter value; inserts and deletes of
indexed documents) adds, for the old entry and for the new one, one such key per filter field to the commit entry (an
index without filter fields adds `[doc id]`). A search query records:
- with `eq` filters: the prefix range `[i][enc(v)]` of its `eq` on the lowest filter column (a document matching
  every filter has a key under each prefix, so one is enough), i.e. the documents whose value of that field is, or was
  before the write, `v`;
- without filters: the full range `[b"", ∞)`;
- plus a `by_id` point for each document it examined.
So a search with `.eq(channel, c)` is invalidated only by writes to documents whose channel is or was `c` (and by
changes to documents it returned), never by writes in other channels.

Vector indexes use the same layout (their filter fields are the `vec0` metadata columns): every write that changes a
document's vector entry (its vector or a filter value; inserting or deleting a document with a vector) writes the
same keys for the old and the new entry. A query's `ctx.db.vector_search` records, for each `or` branch of its filter,
the prefix range of the branch's equality on its lowest filter column, or the full range for an unfiltered search;
branches that can't match (`f == a AND f == b`) and an empty `any_of` record nothing. No `by_id` points: the results
are `(id, score)` pairs, which depend only on the entries (loading the documents with `ctx.db.get` records them).
This is sound because the results are a function of the entries matching the filter: an entry that matches a branch
before or after a write has a key under that branch's prefix. So a subscribed "nearest in channel `c`" query reruns
when a nearer vector is written in `c`, and never for other channels. This is exact because the relevance of a
document depends only on that document (see Relevance): documents outside the filter can neither enter the result nor
reorder it. Encoded values are self-delimiting, so one value's prefix never contains another's keys, and the
subscription manager's interval index stabs each written key like any index key. No change to `ReadSet`/`CommitEntry`
was needed. HNSW indexes additionally write the bare `(index id, doc id)` (16 bytes; filter keys are longer) for the
graph's catch-up; it can only make a query rerun more often.

### 1.9 Migrations

```rust
use rxferrix::prelude::*;          // migration, DocChange
use serde_json::Value;

/// Old todos have no priority. Untyped: the document is the stored JSON (system fields included), so it works
/// however far the Rust type has moved on.
#[migration(table = Todo, batch = 100)]          // table = a #[table] type or "name"; batch 1..=10000 (default 100)
async fn add_priority(ctx: &MutationCtx, doc: Value) -> Result<Option<Value>> {
    Ok(doc.get("priority").is_none().then(|| json!({ "priority": 0 })))   // Some(patch) or None (keep)
}

/// On read: a plain fn of the stored document (no context, no database), so it can also be applied in memory: while
/// it is pending, typed reads see old documents in the new shape (see "On read" below).
#[migration(table = Todo, name = "0003_tags", on_read)]
fn tags_from_tag(doc: &Value) -> Result<Option<Value>> {
    Ok(doc.get("tag").filter(|_| doc.get("tags").is_none()).map(|tag| json!({ "tags": [tag] })))
}

/// Typed, when the old shape still deserializes; a stable name set explicitly.
#[migration(table = "todos", name = "0002_split_owner", after = ["todos:add_priority"])]   // runs after that one
async fn split_owner(ctx: &MutationCtx, doc: Doc<OldTodo>) -> Result<DocChange> {
    if doc.owner.is_empty() { return Ok(DocChange::Delete); }
    let user = ctx.db.query::<User>().first().await?;                   // ctx is a normal MutationCtx
    Ok(DocChange::Patch(json!({ "ownerId": user.map(|u| u.id) })))
}
```

```rust
pub enum DocChange { Keep, Patch(Value) /* shallow merge */, Replace(Value) /* system fields optional, unchanged */, Delete }
pub trait MigrationOutput { fn into_change(self) -> DocChange; }   // DocChange, Option<Value> (None = Keep), () (Keep)
pub trait MigrationRef: 'static { fn name() -> String; fn table() -> &'static str; }   // emitted by the macro

#[derive(Default)]
pub struct RunOptions {
    pub dry_run: bool,              // run over every document, report, write nothing (every batch rolled back)
    pub restart: bool,              // start over from the first document, even if done
    pub max_batches: Option<u64>,   // stop after n batches, still `running` (as if the process died); for tests
    pub force: bool,                // run even if some migration it runs after isn't done
}
impl App {
    pub async fn run_migration<M: MigrationRef>(&self, opts: RunOptions) -> Result<MigrationReport>;
    pub async fn run_migration_named(&self, name: &str, opts: RunOptions) -> Result<MigrationReport>; // dependencies_not_done
    pub async fn run_pending_migrations(&self, opts: RunOptions) -> Result<Vec<MigrationReport>>; // run order; stops at
                                    // the first failed, canceled, stopped or blocked one (dry runs go on and report each)
    pub async fn migration_summaries(&self) -> Result<Vec<MigrationSummary>>;                    // what `list` returns
}
pub struct MigrationReport {   // camelCase JSON
    pub name: String, pub table: String,
    pub outcome: RunOutcome,   // completed | blocked | failed | canceled | alreadyDone | alreadyRunning | stopped | dryRun
    pub batches: u64, pub processed: u64, pub changed: u64,   // this run only
    pub error: Option<String>, pub failed_document: Option<String>,
    pub schema_check: Option<String>,   // "passed" or "failed: <report>" when it was the last pending migration of a
                                        // migrating table (on "failed" the table stays migrating)
    pub examples: Vec<ChangeExample>,   // dry runs: up to 5 { id, before, after (null = deleted) }
}
// rxferrix::migrations::registry(): every #[migration] by name (validated once: unique names of 1..=128 characters out
// of `A-Za-z0-9_-.:/`, batch sizes, `after` names that exist, no cycle, no duplicate); App::open also fails if a
// migration's table is not defined in code. Migrations::iter() is in run order; Migrations::dependencies(name).
```

- **Names** are stable ids: `name = "..."` or, by default, the function path (`todos:add_priority`, like functions).
  Moving or renaming the function changes a default name, which makes it a new migration: set `name` for
  migrations that may move.
- **Order and dependencies.** `after = ["name", ...]` lists migrations that must be done before this one (by name:
  `name = "..."` or the default function path). The dependencies form a DAG, checked at startup: an unknown name, a
  cycle (a migration after itself included) or a name listed twice is an error naming the migrations involved.
  Migrations run in a **topological order with name order as the tie-break** (Kahn's algorithm, taking the smallest
  name among the migrations whose dependencies are ordered; without `after` it is plain name order): `rxferrix migrate`,
  `serve --migrate`, `run_pending_migrations` and `run` without a name, stopping at the first that fails, is
  canceled, or is `blocked` (a dependency not done, e.g. running in another process). Running **one** migration
  whose dependencies aren't all `done` is refused before anything is written, with the user error
  `dependencies_not_done` ("Migration `x` runs after `a` (pending), …"), unless forced (`RunOptions::force`,
  `rxferrix migrate --name x --force`, `force` in `_system/migrations:run`); the check is in the `start` transaction.
  Dry runs are never refused (they write nothing, and see the data without the changes of dependencies that haven't
  run).
- **Batches.** A run first marks the migration `running` (creating its `_migrations` row). Each batch is one mutation
  transaction, the internal system mutation `_system/migrations:run_batch` (so it gets the mutation timeout, panic
  handling and an execution log entry): it reads the cursor from the row, pages through the table's
  `by_creation_time` index (`query_table(..).paginate`, `batch` documents after the cursor), calls the migration on
  each document, applies the change with `patch_raw` (without its field-name check: a migration may set fields the
  current type doesn't have) / `replace_raw` / `delete_raw` (the normal write path: indexes,
  search, write log, so **subscriptions update**), and stores the new cursor and counts in the same transaction.
  Each document is therefore processed **exactly once per run**, also across crashes and with concurrent runners.
  Documents inserted during a run (by the app, or by the migration into its own table) come after the cursor and are
  processed too, so a migration must leave documents already in the new shape alone (return `None`).
- **Failure.** An error or panic from the migration, a rejected write (validation, a patch that isn't an object, a
  replacement that changes `_id`), or the batch timing out rolls the whole batch back; then the row becomes
  `failed` with `error` and `failedDocument` (when one document caused it). Running again resumes at the failed
  batch. `cancel` stops a run after its current batch (`canceled`); running again resumes. A restart turns rows left
  `running` into `pending` (cursor kept; `App::open` does this). `done` migrations don't run again unless
  `restart`ed.
- **Dry runs** go through every document from the start in batches that are rolled back (writes the migration makes
  through `ctx.db` included) and never touch `_migrations`; they report counts and examples, or the first failure.
- **Schema check interplay (lenient mode).** At startup a changed table whose documents don't match its type opens
  only if it has pending migrations; it is then *migrating* (`Catalog::is_migrating`; `tableMigrating` in `list`),
  and its fingerprint is not recorded. In a migrating table, a patch or replace may leave a document that doesn't
  match the type **only if it didn't match before the write either** (so migrations can rewrite documents step by
  step, and app writes to not-yet-migrated documents still work); inserts, and writes to documents that match, are
  validated strictly. So the number of invalid documents can only go down. Typed reads of not-yet-migrated documents
  fail (`document does not match table`) unless the table has `on_read` migrations, so run the migrations promptly
  (`serve --migrate` runs them right after startup) or keep the type backward compatible. A run that starts (at its
  first batch) as the table's **last** pending migration (`validated` in `list`) checks every document it processes
  against the type (else the run fails on that document, `failedDocument`) and finishes without scanning the table:
  documents written meanwhile were validated when written (inserts always; updates of migrated documents, which
  matched). Otherwise, in the transaction where the last migration finishes, every document of the
  table is validated. Either way, if all match, the fingerprint is recorded and the table leaves the migrating state
  (`schemaCheck: "passed"`); otherwise the report is stored in `schemaCheck` and logged as an error, the table stays
  migrating until the process stops, and the next start refuses to open (unless a new migration for the table is
  pending). Invalid documents never survive a restart unnoticed. With `--skip-schema-check` nothing is migrating, and
  migration writes are validated strictly.
- **On read.** `#[migration(on_read)]` takes `fn(doc: &serde_json::Value) -> Result<O>` (not async, no context). The
  background run calls it like any migration. In addition, a typed read (`get`, queries, pagination, search, a
  filtered `count`) of a document that doesn't deserialize as `Doc<T>` applies the table's `on_read` migrations to it
  in memory, in run order (`Patch` merges, `Replace` keeps the system fields, `Keep` passes it on; `Delete` fails the
  read), and deserializes the result. Documents that already match are never touched, so an `on_read` migration that
  stays in code after it ran costs nothing. Untyped reads (`query_table`, the dashboard) see the stored document.
  Index ranges, search filters and aggregates use the stored values until the migration rewrote a document; a query
  that reads the transformed result depends on the document, so it reruns (with the same result) when the migration
  rewrites it.
- **Running.** `rxferrix migrate [--name NAME] [--dry-run] [--restart --name NAME] [--force --name NAME] [--dir DIR]`
  runs every migration that isn't done (or one) and prints a report per migration (exit code 1 if one failed or was
  blocked); `rxferrix migrate status` lists them in run order with their state, progress and dependencies. Both refuse
  while a server holds the data directory (its lock; opening the app writes, and a running server would not see the
  writes in its subscriptions): stop the server first. For a running server use the dashboard functions
  (§4.1.3) or start it with `serve --migrate` (`RXFERRIX_MIGRATE`), which runs pending migrations in the background
  after startup. Without `--migrate`, `serve` logs a warning listing the migrations that aren't done.

---

### 1.10 Semantics to keep in mind

- **Optimistic updates are replayed.** The client re-runs an optimistic update on top of every new server result until
  the query results include its mutation's writes (or the mutation fails). The update must be a pure function of the local store and the
  args: create ids, timestamps and random values **outside** the closure (once per call), not inside it, or each
  replay inserts a different item:
  ```ts
  const optimisticId = `optimistic:${crypto.randomUUID()}` as Id<"messages">;   // once
  const now = Date.now();                                                      // once
  send.withOptimisticUpdate((store, args) => insertAtTop({ …, item: { _id: optimisticId, _creationTime: now, … } }));
  ```
- **Scheduled functions run without an identity.** A job from `ctx.scheduler.run_after/run_at` (and a cron) runs as
  the anonymous identity (the execution log shows `scheduler`), whoever scheduled it: `ctx.auth.require_user()`
  fails there. Pass the user (and whatever else the job needs) as arguments and use an internal function, so clients
  can't call it with someone else's id: `ctx.scheduler.run_after::<deliver_reminder>(delay, (channel, user, text))`.
- **`ctx.now()` in a query is the snapshot time, not a clock.** A subscribed query reruns when something it read
  changes, never because time passes, so a result computed from `now()` ("expired", "5 minutes ago") goes stale.
  Store the state and change it at the right time with a scheduled mutation (`run_at` an `expire` job, as the chat
  example does for typing indicators), and format relative times in the client.
- **Mutations commit in groups.** Mutations that queue for the writer share one SQLite transaction, each in its own
  savepoint: one that fails rolls back alone, and every mutation still gets its own ts (consecutive across those that
  write). A caller gets its result only after the shared `COMMIT`; if that commit fails, every mutation in it fails
  and nothing of them is kept. Two consequences: a mutation whose caller goes away after it finished but before the
  commit is still committed (its idempotency record too, so a retry replays its response), and a duplicate request
  waits for the original's commit before replaying. A mutation that recently took over 0.5 ms (tracked per path)
  commits the open group before it starts, so fast mutations are not held back by a slow one queued behind them;
  the slow one still blocks every write while it runs (`rxferrix::db` module docs, `MAX_BATCH`, `MAX_BATCH_AGE`,
  `SLOW_WRITE`).
- **Query cache.** A query's result is reused, not recomputed, while nothing it read has changed: by every subscriber
  of the same query (always), by the next subscriber after the last one left, and by HTTP queries and
  `ctx.run_query`. After its last subscriber leaves, a result stays cached until a commit writes into what it read
  (then it is dropped, never rerun), for at most `QueryCacheOptions::idle_ttl` (30 minutes), and within
  `QueryCacheOptions::max_bytes` (64 MiB, least recently used dropped first; `--query-cache-size`,
  `--query-cache-ttl`). An HTTP query or `ctx.run_query` that misses twice in a row of the cache's doorkeeper (a
  query asked again soon after) is kept too; one-off queries are not. A cached answer is valid at the latest commit,
  so a caller always sees its own writes; it is never shared across identities a query's auth reads tell apart; and a
  query that reads the time (`ctx.now()`, or a signed storage URL) is never cached (its result depends on more than
  the data). Cached answers are not executions: they don't appear in the execution log. Args are compared as
  canonical JSON: typed calls leave `None` args out (§3.2), so they match clients that omit them, but a client sending
  `{"done": null}` and one sending `{}` get separate entries.
- **Invalidation is by index range, so writes are coarse.** Any write to a document (even one that leaves it
  unchanged) invalidates every query that read it through any index, and an insert invalidates every query whose
  range covers its keys. Tips: skip no-op writes (check first; `mark_read` only patches when something is new, since
  each membership write reruns the channel header for all its viewers); keep frequently changing fields (presence,
  typing, counters, `lastSeen`) in their own small table rather than on a widely read document; read narrow ranges
  (an `eq` prefix plus a `_creationTime` bound, `take(n)`: a limited scan records only what it scanned); prefer
  `Query::count()` (no documents read) to collecting a range just to count it.

### 1.11 Analytics (OLAP)

With `AppOptions::olap.enabled` (`serve --olap`) and the cargo feature `olap` (DuckDB, built from source; ~32 MB more
in a static binary), the app keeps a **columnar copy** of its data in `<data dir>/olap.duckdb` and actions query it
with read-only SQL. Without the feature the API below still compiles and `--olap` fails at startup.

```rust
#[object]
pub struct DayStats { pub day: String, pub added: i64, pub completed: i64 }

#[action]
pub async fn stats_by_day(ctx: &ActionCtx, since: f64) -> Result<Vec<DayStats>> {
    ctx.olap.query_as::<DayStats>(
        "SELECT strftime(to_timestamp(_creationTime / 1000), '%Y-%m-%d') AS day,
                count(*) AS added, count(*) FILTER (WHERE done) AS completed
         FROM todos WHERE _creationTime >= ? GROUP BY day ORDER BY day",
        (since,),
    ).await
}

impl ActionCtx { pub olap: rxferrix::olap::Olap }
impl Olap {
    pub async fn query(&self, sql: &str, params: impl OlapParams) -> Result<OlapResult>;
    pub async fn query_as<T: DeserializeOwned>(&self, sql: &str, params: impl OlapParams) -> Result<Vec<T>>;
    pub fn at_least(&self, ts: u64) -> Olap;   // wait (up to the timeout) until the copy has applied commit `ts`
    pub fn latest(&self) -> Olap;              // at_least(the newest commit)
}
pub struct OlapResult { pub columns: Vec<OlapColumn>, pub rows: Vec<Map<String, Value>>, pub as_of_ts: u64 }
pub struct OlapColumn { pub name: String, pub type_name: String }
// OlapParams: (), tuples of up to 8 `Into<OlapValue>` (bool, i8..i64, u8..u32, f32, f64, strings, Id<T>, Bytes,
// Vec<u8>, Option<_>; no u64/usize),
// Vec<OlapValue>. Positional `?` placeholders.
```
- **Actions only**, not queries or mutations: the copy lags the data (a few hundred milliseconds) and is not
  reactive. `as_of_ts`: the copy reflects every commit up to it and none after (a query sees whole commits). After
  `ctx.run_mutation`, `ctx.olap.latest()` (or `at_least(ts)` with a known commit ts) reads its writes.
- **Tables** are named like rxferrix's: every user table, and the system tables `_users`, `_accounts`, `_storage`,
  `_scheduled_jobs`, `_crons`, `_workflows`, `_workflow_steps`, `_migrations`. The others hold secrets (password and
  token hashes, settings) or admin-only history (`_audit_log`) and are never copied. Columns: `_id` VARCHAR,
  `_creationTime` DOUBLE (ms), `_ts` BIGINT (the document's commit ts), then the fields in declaration order, typed
  from the table's shape (§3.1 "Shapes"): Bool → BOOLEAN, Int → BIGINT, UInt → UBIGINT, Float → DOUBLE, Text →
  VARCHAR, Bytes → BLOB (JSON when nested), List → `T[]`, a closed non-empty Struct → `STRUCT(...)` (fields in
  order), Json, open or empty structs → JSON; Optional is the inner type, nullable. Fields starting with `_` and
  repeated names are skipped. A table whose shape is open also gets `_extra` JSON with the keys that have no column.
  Values are converted by DuckDB (`TRY_CAST`, `json_transform`): one that doesn't fit its column reads as NULL.
  Unit-only enums (`#[object]` ones, and `state` / `kind` of system tables) are VARCHAR; other enums are JSON.
- **Results**: one object per row keyed by column name; lists and structs as arrays and objects; top-level JSON
  columns parsed (JSON nested in a list or struct comes back as JSON text); DATE / TIMESTAMP / TIME as ISO 8601
  strings; BLOB as `{"$bytes": "<base64>"}`; HUGEINT as an integer (a float beyond 64 bits), DECIMAL as a float;
  NaN and infinities as null.
- **The sandbox**: exactly one SELECT per call (`WITH ... SELECT` included; checked with DuckDB's own parser,
  `json_serialize_sql`), run in a transaction that is rolled back, on a database with `enable_external_access =
  false` (no files, no network: `read_csv`, `glob`, `COPY`, `ATTACH` fail), extension installing and autoloading off,
  and the configuration locked. At most `max_result_rows` rows (100k; more is an error asking for a LIMIT), at most
  `max_concurrent_queries` at once (4), interrupted after `query_timeout` (30s).
- **Errors** are user errors: `olap_unavailable` (off, not built with the feature, or the copy is being built),
  `olap_query_failed` (not one SELECT, DuckDB's message, more than `max_result_rows` rows, or a row `query_as` can't
  deserialize), `olap_timeout` (interrupted, or `at_least` waited too
  long).
- **How the copy follows the data.** It reads the change journal (§4.4, consumer `olap`) every `apply_every`
  (250ms) and applies each batch (the final state of every document written) in one DuckDB transaction. A journal
  row is deleted once every active consumer (replication, the copy, a takeover in progress) has applied or shipped it;
  journaling is on while any is. A table is **rebuilt** from one snapshot when there is no copy yet, after a restore (`journal::scrub`
  removes the copy's id), when the journal can't continue the copy (commits made without `--olap` or replication),
  when more than `max_lag_bytes` (512 MiB) of journal wait to be applied, or alone when its shape changed (a
  fingerprint per table). Queries during a rebuild fail with `olap_unavailable`. The copy is derived data: backups
  and replication skip it; deleting the file rebuilds it.
- **Options** (`AppOptions::olap: OlapOptions`; flags): `enabled` (`--olap`, `RXFERRIX_OLAP`), `memory_limit`
  (`--olap-memory 2GB`; default DuckDB's, 80% of RAM), `threads` (`--olap-threads`), `query_timeout`
  (`--olap-timeout 30s`), `max_lag_bytes` (`--olap-max-lag 512MB`), `max_concurrent_queries`, `max_result_rows`,
  `max_export_rows` (10M), `export_timeout` (5 min), `apply_every`, `background` (off: tests drive it with
  `TestApp::olap_sync()`).
- **Exports**: `OlapService::export(sql, params, OlapExportFormat::Csv | Parquet, path) -> OlapExport { rows,
  columns, as_of_ts }` writes a whole result (the dashboard's downloads, `POST /api/admin/olap/query`).
- **Operations**: `GET /api/admin/olap` (`{ enabled: false }` or the status: `state` building / ready / failed,
  `detail`, `appliedTs`, `appliedTime`, `latestTs`, `lagMs`, tables with row counts and columns, `sizeBytes`, `rowsApplied`,
  `rebuilds`), `POST /api/admin/olap/rebuild` (202; rebuilds every table in the background; audited),
  `POST /api/admin/olap/query` (SQL as an administrator, JSON or a CSV / Parquet download; audited), the
  `rxferrix_olap_*` metrics (§4.2, `GET /metrics`), `olap` in `/api/admin/stats`, the dashboard Overview and its Analytics page, and
  each table's analytics columns in `_system/dashboard:tables` (`analytics`, `columns: [{ name, sqlType }]`).
- **Tests**: `TestApp::with_options(AppOptions { olap: OlapOptions { enabled: true, background: false, .. }, .. })`
  and `t.olap_sync().await?` before querying.

### 1.12 Live aggregates

Counts and sums per group, maintained in every write's own transaction and read in queries without scanning documents.
Reactive: a write reruns only the queries that read a group it changed.

```rust
#[table(name = "orders")]
#[aggregate(by_customer, group = [customer], sum = [amount, quantity])]
#[aggregate(by_customer_day, group = [customer, day], sum = [amount])]
#[aggregate(total, sum = [amount])]          // no group fields: one group for the whole table
#[aggregate(count_only)]                       // no sums: just the number of documents
pub struct Sale { pub customer: String, pub day: String, pub amount: f64, pub quantity: Option<i64> }

#[query]
pub async fn customer_total(ctx: &QueryCtx, customer: String) -> Result<f64> {
    let a = ctx.db.aggregate(Sale::by_customer).get(|g| g.eq(Sale::customer, customer)).await?;
    Ok(a.sum(Sale::amount))
}

impl QueryDb { pub fn aggregate<T: Table>(&self, a: Aggregate<T>) -> AggregateQuery<'_, T>; }   // MutationDb too
impl AggregateQuery<'_, T> {
    // One group: `eq` on every group field, in order (|g| g for no group fields). Zero when it has no documents.
    pub async fn get(self, group: impl FnOnce(IndexRange<T>) -> IndexRange<T>) -> Result<AggregateValue>;
    // The groups in a range (eq on a prefix of the group fields, then at most a lower and an upper bound on the
    // next one, like `with_index`), combined; `total()` = every group.
    pub async fn sum_over(self, range: impl FnOnce(IndexRange<T>) -> IndexRange<T>) -> Result<AggregateValue>;
    pub async fn total(self) -> Result<AggregateValue>;
    // The groups in a range, in group-key order, at most `limit`: `AggregateGroup { group: Vec<Value>, value }`.
    pub async fn groups(self, range: impl FnOnce(IndexRange<T>) -> IndexRange<T>, order: Order, limit: usize)
        -> Result<Vec<AggregateGroup>>;
}
pub struct AggregateValue { pub count: u64, pub sums: Vec<AggregateSum>, .. }
impl AggregateValue {
    pub fn sum<T, V>(&self, field: Field<T, V>) -> f64;              // 0.0 for a field that isn't summed
    pub fn sum_i64<T, V>(&self, field: Field<T, V>) -> Option<i64>;   // exact while every contribution was an integer
    pub fn average<T, V>(&self, field: Field<T, V>) -> Option<f64>;   // sum / count (count includes documents
                                                                      // without the field); None without documents
}
pub enum AggregateSum { Int(i64), Float(f64) }
```
- **Group and sum fields**: any fields for the group (values compare like index keys; a missing field is its own
  group value, before `null`, and reads as `null` in `AggregateGroup::group`); numeric fields or `Option`s of them for the sums (checked at compile time). A null,
  missing or non-numeric value adds nothing to a sum; `count` counts documents.
- **Maintenance**: inside the write (insert, replace, patch, delete; migrations, imports and point-in-time replays
  included, since they all write through the same path), in its transaction: a failed mutation leaves its aggregates
  untouched, and a mutation reads its own changes. A write that changes no group key and no summed value (another
  field) changes nothing and invalidates nothing. Integer sums are exact while every contribution is an integer
  (past `i64` they become floats); a float makes the sum an `f64`, which accumulates rounding like any running
  total.
- **Reactivity**: `get` reads one group key, `sum_over` and `groups` the range they cover (for `groups`, the range is
  recorded whole). A changed group is a write of its key, so only queries reading that group, or a range holding
  it, rerun. Read cost: one row for `get`, one per group for `sum_over` and `groups`; group by fewer fields, or
  keep ranges narrow, when there are many groups.
- **Definitions** are reconciled at startup: a new aggregate, or one whose fields changed, is built from the
  documents (like an index backfill); a removed one is deleted. `Database::verify_indexes` (backups, restores)
  recomputes them and reports differences. Storage: `_aggregates` (definitions; ids above `0xC000_0000`, above index
  and search index ids) and `aggregate_values (agg_id, key, count, sums)`, one row per non-empty group.
- The dashboard's schema view lists them (`aggregates: [{ name, groupFields, sumFields }]` in
  `_system/dashboard:tables`).

---

### 1.13 Environment variables

Named string values set at run time by an administrator (the dashboard's Environment page, `_system/env:set`) or
offline from the command line, read by functions through `ctx.env` (`rxferrix::env::Env`):

```rust
impl Env {
    pub async fn get(&self, name: &str) -> Result<Option<String>>;   // None when not set
    pub async fn require(&self, name: &str) -> Result<String>;       // a server error naming the variable when unset
}
```

- **Reactive.** In a query, `get` is an indexed read of the variable's `_env` row (index `by_name`, `eq(name)`), so it
  joins the query's read set: setting, changing or removing that variable reruns the query on every subscription; other
  variables don't. In a mutation it reads in the transaction (a concurrent change is serialized like any read). In an
  action it reads the latest commit.
- **Deterministic**: the value is a function of the snapshot, so the query cache and the determinism check treat it
  like any document read.
- **Names**: 1–64 ASCII letters, digits and `_`, not starting with a digit (`invalid_name`). **Values**: UTF-8, at most
  8 KiB (`invalid_value`); at most 200 variables (`too_many_variables`).
- **Command line** (offline: refuses while a server holds the data directory): `<binary> env list [--values] [--dir]`,
  `env set NAME [VALUE] [--dir]` (one line from stdin when VALUE is omitted; prints `Set NAME` or `NAME already had that
  value`), `env unset NAME [--dir]` (`Removed NAME` / `NAME is not set`). They run the internal mutation
  `_system/env:put`, so the write is audited (with no administrator) like a dashboard edit.
- `std::env::var` is unaffected: process environment variables are fixed at startup and not tracked.

## 2. `::rxferrix::__private` items (used ONLY by macro output)

```rust
pub use ::linkme;           // macro output uses #[linkme(crate = ::rxferrix::__private::linkme)]
pub use ::serde;            // #[serde(crate = "::rxferrix::__private::serde")]
pub use ::serde_json;
pub use ::ts_rs;            // #[ts(crate = "::rxferrix::__private::ts_rs")]
pub use ::futures::future::BoxFuture;

#[linkme::distributed_slice] pub static TABLES: [fn() -> TableDef];
#[linkme::distributed_slice] pub static FUNCTIONS: [FunctionDef];

pub enum FunctionKind { Query, Mutation, Action }
pub enum Visibility { Public, Internal }

pub type QueryHandler    = fn(QueryCtx,    serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value>>;
pub type MutationHandler = fn(MutationCtx, serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value>>;
pub type ActionHandler   = fn(ActionCtx,   serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value>>;
pub enum Handler { Query(QueryHandler), Mutation(MutationHandler), Action(ActionHandler) }

pub struct FunctionDef {
    pub module_path: &'static str,   // module_path!() at the definition site
    pub name: &'static str,          // the Rust fn name
    pub visibility: Visibility,
    pub dev_only: bool,              // #[query(dev_only)]: refused outside dev mode, generated into `devApi`
    pub handler: Handler,
    /// TS signature for codegen: (args TS type, output TS type), built with ts_rs.
    pub ts: fn() -> (String, String),
}

/// "todo_server::todos" + "list" → "todos:list"; "todo_server" + "list" → "list"; "a::b::c" + "f" → "b/c:f".
pub fn function_path(module_path: &str, name: &str) -> String;
/// True for paths of rxferrix's own functions (first segment `_system`, §4).
pub fn is_system_path(path: &str) -> bool;

/// Deserialize args; on failure returns an error whose message starts with "Invalid args for <name>: ".
pub fn decode_args<A: DeserializeOwned>(name: &str, args: serde_json::Value) -> Result<A>;
pub fn encode_output<O: Serialize>(out: O) -> Result<serde_json::Value>;
pub fn ts_inline<T: ts_rs::TS + ?Sized + 'static>() -> String;   // T::inline(cfg) with the runtime's one ts-rs config
                                                                 // ('static: codegen also walks T's dependencies);
                                                                 // table types and generated args structs
pub fn ts_output<T: ts_rs::TS + ?Sized + 'static>() -> String;   // T::name(cfg): a function's output, named types by
                                                                 // name (codegen declares them and their dependencies)
/// `TableDef::validate` of a table: Ok iff `doc` deserializes as `Doc<T>`; Err = the serde error message.
pub fn validate_doc<T: Table>(doc: &serde_json::Value) -> Result<(), String>;
#[linkme::distributed_slice] pub static WORKFLOWS: [WorkflowDef];
pub type WorkflowHandler = fn(WorkflowCtx, serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value>>;
pub struct WorkflowDef { pub module_path: &'static str, pub name: &'static str, pub handler: WorkflowHandler }
#[linkme::distributed_slice] pub static MIGRATIONS: [MigrationDef];
pub type MigrationHandler = for<'a> fn(&'a MutationCtx, serde_json::Value) -> BoxFuture<'a, Result<DocChange>>;
pub struct MigrationDef {
    pub module_path: &'static str, pub fn_name: &'static str,
    pub name: Option<&'static str>,   // #[migration(name = "...")]
    pub table: &'static str,          // <T as Table>::NAME or the string given
    pub batch_size: usize,
    pub after: &'static [&'static str],   // #[migration(after = [...])]: names of migrations to run first
    pub handler: MigrationHandler,
    pub on_read: Option<OnReadFn>,    // #[migration(on_read)] (§3.3)
}
pub type OnReadFn = fn(&serde_json::Value) -> Result<DocChange>;
impl MigrationDef { pub fn name(&self) -> String; }   // `name`, else function_path(module_path, fn_name)
/// Deserialize the document for the migration's parameter type (identity for `Value`).
pub fn decode_migration_doc<T: DeserializeOwned>(doc: serde_json::Value) -> Result<T>;
pub fn migration_output<O: MigrationOutput>(out: O) -> Result<DocChange>;
/// Field type checks for #[search_index] / #[vector_index] (`#[diagnostic::on_unimplemented]` explains failures).
pub trait SearchFieldType {}   // String, Option<String>
pub trait VectorFieldType {}   // Vec<f32>, Option<Vec<f32>>
pub const fn assert_search_field<T: SearchFieldType + ?Sized>();
pub const fn assert_vector_field<T: VectorFieldType + ?Sized>();
pub trait SumFieldType {}      // integers, f32, f64 and Options of them (#[aggregate(sum = [...])])
pub const fn assert_sum_field<T: SumFieldType + ?Sized>();
```

---

## 3. Macro expansions (normative)

### 3.1 `#[table]`

Input:
```rust
#[table(name = "todos")]
#[index(by_done = [done, created_by])]
pub struct Todo { pub text: String, pub done: bool, pub created_by: Option<String> }
```
Output (the `#[index]` attributes are consumed; any other user attributes and doc comments are kept):
```rust
#[derive(::rxferrix::__private::serde::Serialize, ::rxferrix::__private::serde::Deserialize,
         ::rxferrix::__private::ts_rs::TS, ::core::clone::Clone, ::core::fmt::Debug)]
#[serde(crate = "::rxferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct Todo { pub text: String, pub done: bool, pub created_by: Option<String> }

impl ::rxferrix::Table for Todo {
    const NAME: &'static str = "todos";
    fn table_def() -> ::rxferrix::TableDef {
        ::rxferrix::TableDef { name: "todos", indexes: vec![
            ::rxferrix::IndexDef { name: "by_done", fields: vec!["done", "createdBy"] },
        ], ts: || ::rxferrix::__private::ts_inline::<Todo>(),
        validate: ::rxferrix::__private::validate_doc::<Todo>,
        shape: || {
            let mut s = ::rxferrix::shape::StructShape::default();
            s.field("text", SHAPE_OF!(String));          // see "Shapes" below
            s.field("done", SHAPE_OF!(bool));
            s.field("createdBy", SHAPE_OF!(Option<String>));
            ::rxferrix::shape::Shape::Struct(s)
        } }
    }
}

#[allow(non_upper_case_globals)]
impl Todo {
    pub const text: ::rxferrix::Field<Todo, String> = ::rxferrix::Field::new("text");
    pub const done: ::rxferrix::Field<Todo, bool> = ::rxferrix::Field::new("done");
    pub const created_by: ::rxferrix::Field<Todo, Option<String>> = ::rxferrix::Field::new("createdBy");
    pub const _id: ::rxferrix::Field<Todo, ::rxferrix::Id<Todo>> = ::rxferrix::Field::new("_id");
    pub const _creation_time: ::rxferrix::Field<Todo, f64> = ::rxferrix::Field::new("_creationTime");
    pub const by_done: ::rxferrix::Index<Todo> = ::rxferrix::Index::new("by_done");
    pub const by_creation_time: ::rxferrix::Index<Todo> = ::rxferrix::Index::new("by_creation_time");
}

#[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::TABLES)]
#[linkme(crate = ::rxferrix::__private::linkme)]
static __RXFERRIX_TABLE_Todo: fn() -> ::rxferrix::TableDef = <Todo as ::rxferrix::Table>::table_def;
```
Compile errors (with spans) for: tuple or unit structs, enums, generics, an index naming an unknown field, a duplicate
index name, an index named `by_id` or `by_creation_time`, and an existing `#[serde(rename_all)]` other than camelCase.
Per-field `#[serde(rename = "x")]` is honored: the handle and IndexDef use "x".

**Shapes.** `SHAPE_OF!(T)` above stands for the block
`{ use ::rxferrix::shape::{ProbeFallback as _, ProbeHasShape as _}; #[allow(clippy::needless_borrow)] let shape =
(&&::rxferrix::shape::ShapeProbe::<T>::new()).probe_shape(); shape }`: `T`'s `HasShape::shape()` when `T` implements
`rxferrix::shape::HasShape`, else `Shape::Json` (autoref specialization), so a field of a type without one still
compiles. Field names follow serde: the struct's `rename_all` (camelCase for tables), `rename = "x"`; `skip` /
`skip_serializing` fields are left out, `skip_serializing_if` ones become `optional_field` (they may be missing),
`flatten` fields `s.flatten(SHAPE_OF!(T))` (a struct's fields join, anything else makes the shape open: `s.open()`),
`with` / `serialize_with` fields are `Shape::Json`, and a field whose name can't be read makes the shape open.
`HasShape` is implemented for `bool` (Bool), `i8`..`i64`, `u8`..`u32`, `isize` (Int), `u64`, `usize` (UInt), `f32`,
`f64` (Float), `String`, `str`, `char`, `Id<T>` (Text), `Bytes` (Bytes), `Option<T>` (Optional), `Vec`, `VecDeque`,
`BTreeSet`, `HashSet`, slices and arrays (List), `Box`, `Arc`, `&T` (as `T`), `Doc<T>` (system fields, then the
table's), and `serde_json::Value`, maps, `i128`, `u128`, `()` (Json).

**Aggregates.** `#[aggregate(name, group = [f, ...], sum = [f, ...])]` (consumed; `group` and `sum` optional, any
order) adds `::rxferrix::AggregateDef { name: "name", group_fields: vec!["f"], sum_fields: vec!["f"] }` (JSON names) to
`aggregates` in the `TableDef` literal (`vec![]` when unused), `pub const name: ::rxferrix::Aggregate<T> =
::rxferrix::Aggregate::new("name");` next to the index handles, and `const _: () =
::rxferrix::__private::assert_sum_field::<Ty>();` per sum field (numbers and `Option`s of them). Compile errors: a name
taken by an index, a search or vector index, another aggregate, a field handle or a built-in; unknown, duplicate or
system (`_id`, `_creation_time`) fields; keys other than `group` and `sum`.

**Search and vector indexes.** `#[search_index(name, search = field, filter = [f, ...], tokenizer = "...", language =
"...", diacritics = "...")]` and `#[vector_index(name, field = f, dims = N, filter = [f, ...], kind = "...", m = N,
ef_construction = N, ef_search = N)]` (both consumed; `filter`, `tokenizer`, `language`, `diacritics`, `kind` and the
HNSW parameters optional) add, for
```rust
#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel])]
#[search_index(search_stems, search = body, tokenizer = "porter", language = "english", diacritics = "keep")]
#[vector_index(by_embedding, field = embedding, dims = 1536, kind = "hnsw", m = 32)]
pub struct Message { pub channel: String, pub body: String, pub embedding: Option<Vec<f32>> }
```
these items to the expansion above (the `TableDef` literal always has both fields, `vec![]` when unused):
```rust
::rxferrix::TableDef { name: "messages", indexes: vec![],
    search_indexes: vec![
        ::rxferrix::SearchIndexDef { name: "search_body", search_field: "body", filter_fields: vec!["channel"],
                                   tokenizer: ::rxferrix::Tokenizer::unicode61() },
        ::rxferrix::SearchIndexDef { name: "search_stems", search_field: "body", filter_fields: vec![],
                                   tokenizer: ::rxferrix::Tokenizer::porter("english").keep_diacritics() }],
    vector_indexes: vec![::rxferrix::VectorIndexDef { name: "by_embedding", vector_field: "embedding", dimensions: 1536usize,
                                                     filter_fields: vec![], kind: ::rxferrix::VectorIndexKind::Hnsw,
                                                     hnsw: ::rxferrix::HnswParams { m: 32usize, ..::rxferrix::HnswParams::DEFAULT } }],
    ts: …, validate: … }
// tokenizer: unicode61() (default or "unicode61"), porter("<language>") (language defaults to "english"), trigram(),
// snowball("<language>") (language required); `.keep_diacritics()` is appended for diacritics = "keep".
// kind: Exact (default or "exact") or Hnsw. hnsw: ::rxferrix::HnswParams::DEFAULT without parameters, the struct with
// the given ones and `..::rxferrix::HnswParams::DEFAULT` for the rest (no `..` when all three are given).
// in `impl Message { … }`, after by_creation_time:
pub const search_body: ::rxferrix::SearchIndex<Message> = ::rxferrix::SearchIndex::new("search_body");
pub const by_embedding: ::rxferrix::VectorIndex<Message> = ::rxferrix::VectorIndex::new("by_embedding");
// one check per index, spanned at the field's type (with the struct's #[cfg]s):
const _: () = ::rxferrix::__private::assert_search_field::<String>();
const _: () = ::rxferrix::__private::assert_vector_field::<Option<Vec<f32>>>();
```
Compile errors (with spans) for: a missing or unknown `search`/`field`, unknown or duplicate filter fields (or the
searched field itself), system fields, a missing `dims` or one outside `1..=8192`, more than 16 filter fields,
unknown or duplicate arguments, a name equal to another index (of any kind), to a field handle, or to a reserved name
(`by_id`, `by_creation_time`, `_id`, `_creation_time`), and a field type that is visibly wrong (`u32` as a search
field, `Vec<f64>` as a vector field). Types behind aliases are caught by the `assert_*_field` check instead. Also
(spanned at the string literal): an unknown `tokenizer` (not `"unicode61"`, `"porter"`, `"trigram"`, `"snowball"`),
`diacritics` (not `"remove"`, `"keep"`) or `kind` (not `"exact"`, `"hnsw"`), a `language` that no Snowball stemmer
supports, a `language` other than `"english"` with `porter`, a `language` without `tokenizer = "porter"` or
`"snowball"`, `tokenizer = "snowball"` without a `language`, a non-string value, and `tokenizer`/`language`/
`diacritics` on a vector index or `kind` on a search index (unknown arguments). Also: `m`, `ef_construction` or
`ef_search` that aren't integer literals in their range (4..=64, 16..=2048, 1..=4096), given twice, or given without
`kind = "hnsw"`. `TableDef`s built by hand are checked at startup the same way.

### 3.2 `#[query]` / `#[mutation]` / `#[action]` (and the `internal_*` variants)

Input:
```rust
/// Lists todos.
#[query]
pub async fn list(ctx: &QueryCtx, done: Option<bool>, limit: u32) -> Result<Vec<Doc<Todo>>> { BODY }
```
Output:
```rust
/// Lists todos.                                                  (the user's fn is emitted unchanged)
pub async fn list(ctx: &QueryCtx, done: Option<bool>, limit: u32) -> Result<Vec<Doc<Todo>>> { BODY }

#[allow(non_camel_case_types)]
#[derive(::rxferrix::__private::serde::Serialize, ::rxferrix::__private::serde::Deserialize, ::rxferrix::__private::ts_rs::TS)]
#[serde(crate = "::rxferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct __rxferrix_args_list {
    #[serde(skip_serializing_if = "::core::option::Option::is_none")]
    pub done: Option<bool>,
    pub limit: u32,
}

impl ::core::convert::From<(Option<bool>, u32,)> for __rxferrix_args_list {
    fn from((done, limit,): (Option<bool>, u32,)) -> Self { Self { done, limit } }
}

/// Function reference for [`list`](fn@list).                    (type namespace only; same visibility as the fn)
#[allow(non_camel_case_types, dead_code)]
pub struct list {}

impl ::rxferrix::FunctionRef for list {
    type Args = __rxferrix_args_list;
    type Output = Vec<Doc<Todo>>;
    fn path() -> String { ::rxferrix::__private::function_path(module_path!(), "list") }
}
impl ::rxferrix::QueryFunction for list {}
// (a mutation or action also gets `impl ::rxferrix::SchedulableFunction for <name> {}` here)

#[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::FUNCTIONS)]
#[linkme(crate = ::rxferrix::__private::linkme)]
static __RXFERRIX_FN_list: ::rxferrix::__private::FunctionDef = ::rxferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "list",
    visibility: ::rxferrix::__private::Visibility::Public,     // Internal for internal_*
    dev_only: false,                                         // true for #[query(dev_only)]
    handler: ::rxferrix::__private::Handler::Query(|ctx, args| ::std::boxed::Box::pin(async move {
        let a: __rxferrix_args_list = ::rxferrix::__private::decode_args("list", args)?;
        let out = list(&ctx, a.done, a.limit).await?;            // value namespace => the user's fn
        ::rxferrix::__private::encode_output(out)
    })),
    ts: || (::rxferrix::__private::ts_inline::<__rxferrix_args_list>(),
            ::rxferrix::__private::ts_output::<Vec<Doc<Todo>>>()),
};
```
- The only attribute argument is `dev_only` (`#[query(dev_only)]`); anything else, a value (`dev_only = true`) or a
  repeat is a compile error.
- `Output` is the `T` in `Result<T>`. The macro must handle `Result<T>`, `rxferrix::Result<T>` and
  `anyhow::Result<T>`: it takes the first generic argument of the last path segment named `Result`. Anything else is
  a compile error.
- `optional_fields = nullable` makes `Option<T>` args optional in TS (`done?: boolean | null`); serde already treats a missing `Option` field as `None`.
- `skip_serializing_if` on every field written as `Option<T>` (unless the param sets its own `skip_serializing*`): a
  typed call with `None` (`ctx.run_query`, the scheduler) sends the args without the field, as clients that omit it
  do, so both share one subscription and query cache entry (args are compared as canonical JSON).
- Zero params: `pub struct __rxferrix_args_list {}` and `impl From<()>`.
- Compile errors (with spans) for: a non-async fn, a missing ctx param, a ctx type that doesn't match the macro kind
  (match on the last path segment: `QueryCtx` / `MutationCtx` / `ActionCtx`), `self` params, non-identifier
  patterns, generics, and a missing return type.
- Mutations use `Handler::Mutation` + `impl MutationFunction`; actions use `Handler::Action` + `impl ActionFunction`.
  Both also get `impl ::rxferrix::SchedulableFunction for <name> {}` (same `#[cfg]`s), right after that impl; queries
  don't.
- **`#[workflow]`** expands the same way (args struct, `From` tuple, reference struct, `FunctionRef`) with ctx type
  `WorkflowCtx`, `impl ::rxferrix::workflow::WorkflowFunction` instead of a function trait, no `SchedulableFunction`,
  and registers a `WorkflowDef { module_path, name, handler }` in `__private::WORKFLOWS` (not `FUNCTIONS`: workflows
  are started by functions, never called by clients, and aren't in codegen). It takes no arguments (`dev_only` is an
  error).

### 3.3 `#[migration]`

Input:
```rust
/// Adds a default priority.
#[migration(table = Todo, batch = 100)]
pub async fn add_priority(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> { BODY }
```
Output (the fn is emitted unchanged; `#[cfg]`s are copied to every generated item):
```rust
/// Adds a default priority.
pub async fn add_priority(ctx: &MutationCtx, doc: serde_json::Value) -> Result<Option<serde_json::Value>> { BODY }

/// Migration reference for [`add_priority`](fn@add_priority).
#[allow(non_camel_case_types, dead_code)]
pub struct add_priority {}
impl ::rxferrix::migrations::MigrationRef for add_priority {
    fn name() -> ::std::string::String { ::rxferrix::__private::MigrationDef::name(&__RXFERRIX_MIGRATION_add_priority) }
    fn table() -> &'static str { __RXFERRIX_MIGRATION_add_priority.table }
}

#[::rxferrix::__private::linkme::distributed_slice(::rxferrix::__private::MIGRATIONS)]
#[linkme(crate = ::rxferrix::__private::linkme)]
#[allow(non_upper_case_globals)]
static __RXFERRIX_MIGRATION_add_priority: ::rxferrix::__private::MigrationDef = ::rxferrix::__private::MigrationDef {
    module_path: ::core::module_path!(),
    fn_name: "add_priority",
    name: ::core::option::Option::None,                      // Some("...") with `name = "..."`
    table: <Todo as ::rxferrix::Table>::NAME,                  // `table = "todos"` → "todos"
    batch_size: 100usize,
    after: &[],                                              // `after = ["a", "b"]` → &["a", "b"]
    handler: {
        fn __rxferrix_migrate<'a>(__rxferrix_ctx: &'a ::rxferrix::MutationCtx, __rxferrix_doc: ::rxferrix::__private::serde_json::Value)
            -> ::rxferrix::__private::BoxFuture<'a, ::rxferrix::Result<::rxferrix::migrations::DocChange>> {
            ::std::boxed::Box::pin(async move {
                let __rxferrix_doc: serde_json::Value = ::rxferrix::__private::decode_migration_doc::<serde_json::Value>(__rxferrix_doc)?;
                let __rxferrix_out: Option<serde_json::Value> = add_priority(__rxferrix_ctx, __rxferrix_doc).await?;
                ::rxferrix::__private::migration_output(__rxferrix_out)
            })
        }
        __rxferrix_migrate
    },
    on_read: ::core::option::Option::None,                  // `on_read`: Some(fn) calling the (sync) fn, see below
};
```
- Arguments: `table` (required; a type path, checked to implement `Table`, or a string literal), `batch` (an integer
  literal in `1..=10000`, default 100), `name` (a string literal of 1..=128 characters out of `A-Za-z0-9_-.:/`),
  `after` (an array of migration names, each a valid name; compile errors for a non-array, a non-string element, an
  invalid or repeated name, and the migration's own `name`; unknown names and cycles are startup errors).
- The fn takes exactly `(ctx: &MutationCtx, doc: T)` with `T: DeserializeOwned` (`serde_json::Value`, `Doc<Old>`,
  ...) and returns `Result<O>` (any `...::Result`) with `O: MigrationOutput`; an unsupported `O` is reported by
  `#[diagnostic::on_unimplemented]`.
- With the flag `on_read`, the fn is a plain (not async) `fn(doc: &serde_json::Value) -> Result<O>`: the handler
  ignores the context and calls it with `&doc`, and `on_read` is `Some(__rxferrix_on_read)`, a
  `fn(&Value) -> Result<DocChange>` doing the same. Compile errors for an async fn, a context parameter, or a document
  taken by value.
- Compile errors (with spans) for: a missing `table`, unknown or duplicate arguments, values of the wrong kind, `batch`
  out of range, an invalid `name`, a non-async fn, `const`/`unsafe`/ABI/generics/variadics, a wrong context
  (`&QueryCtx`, by value, ...), a parameter count other than two, non-identifier patterns, parameter attributes, a
  borrowed document type, and a missing or non-`Result` return type.

### 3.4 `#[object]`

For types used in function signatures that are not tables: output "view" structs, enums, nested argument types.

Input:
```rust
/// A message as the app shows it.
#[object]
#[derive(Clone, Debug)]
pub struct MessageView {
    #[serde(flatten)]
    pub message: Doc<Message>,
    pub author_name: String,
}
```
Output:
```rust
#[derive(::rxferrix::__private::serde::Serialize, ::rxferrix::__private::serde::Deserialize, ::rxferrix::__private::ts_rs::TS)]
#[serde(crate = "::rxferrix::__private::serde")]
#[ts(crate = "::rxferrix::__private::ts_rs")]
#[serde(rename_all = "camelCase")]          // these two only when the item has no `rename_all` of its own
#[ts(rename_all = "camelCase")]
/// A message as the app shows it.
#[derive(Clone, Debug)]
pub struct MessageView { #[serde(flatten)] pub message: Doc<Message>, pub author_name: String }
```
and, for a type without generics, its shape (§3.1 "Shapes"):
```rust
impl ::rxferrix::shape::HasShape for MessageView {
    fn shape() -> ::rxferrix::shape::Shape {
        ::rxferrix::shape::guard(::core::any::type_name::<Self>(), || {
            let mut s = ::rxferrix::shape::StructShape::default();
            s.flatten(SHAPE_OF!(Doc<Message>));
            s.field("authorName", SHAPE_OF!(String));
            ::rxferrix::shape::Shape::Struct(s)
        })
    }
}
```
(an enum whose variants are all units, without `tag` / `content` / `untagged`: `Shape::Text`; another enum, a tuple
struct of several fields or a unit struct: `Shape::Json`; a newtype or `transparent` struct: its field's shape;
`guard` returns `Shape::Json` for a type already being computed, so recursive types terminate).
- Structs (named, tuple, unit) and enums, generic or not; for an enum, `rename_all` renames the variants
  (`InProgress` → `"inProgress"`). A `#[serde(rename_all = "...")]` (or `#[ts(...)]`) on the item replaces the camelCase
  default; other `#[serde]` / `#[ts]` attributes are kept (ts-rs follows serde's, e.g. `rename`, `skip`, `flatten`).
- Compile errors: arguments (`#[object(x)]`), an item that is not a struct or enum, deriving `Serialize`,
  `Deserialize` or `TS` again, and `crate = ...` in `#[serde]` / `#[ts]`.
- **Embedding documents.** `Id<T>` fields work as is (TS `Id<"table">`). `#[serde(flatten)] doc: Doc<T>` puts the
  document's fields (`_id`, `_creationTime` and the table's) at the top level of the object: `Doc<T>` implements
  ts-rs' `inline_flattened`, so the TS type is `{ authorName: string, … } & Doc<"messages">`. Field names must not
  clash with the table's (rename the view's field, e.g. `file` next to the document's `attachment`).
- Codegen declares the type under its name in `dataModel.ts` (§1.2.1, "Codegen").

---

## 4. Built-in (system) tables and functions

rxferrix ships some tables and functions of its own (auth, dashboard, bookkeeping). They are defined **inside the
`rxferrix` crate with the same macros** (`extern crate self as rxferrix;` makes `::rxferrix::` paths resolve there).

- **System tables** have names starting with `_` (e.g. `_users`, `_sessions`). `#[table]` rejects `_`-prefixed names
  in any crate other than `rxferrix`, checked via `CARGO_CRATE_NAME` at expansion time. Codegen leaves system tables out
  of `DataModel`. The few that app tables may reference (`codegen::REFERENCEABLE_SYSTEM_TABLES`: `_users`, `_storage`, `_workflows`
  and `_scheduled_jobs`, whose TS types inline their enums so no extra named types are declared)
  are declared in a separate `SystemDataModel`, and the generated `Id<T>` / `Doc<T>` accept
  `TableNames | keyof SystemDataModel`.
- **System functions** get paths under `_system/` (`function_path` maps crate `rxferrix` to `_system`), e.g.
  `rxferrix::auth::api::sign_in` → `"_system/auth/api:sign_in"`. Use `__private::is_system_path(path)` to test for
  them. Codegen leaves them out of the app's `api`. Who may call a public system function from a client (anyone, a
  signed-in user, or only an admin) is decided by the function itself (e.g. `ctx.auth.require_admin()`), not by the
  path.

### 4.1 Dashboard system functions

`crates/rxferrix/src/dashboard` defines the admin dashboard's backend with the same macros. Every function starts with
`ctx.auth.require_admin()`: anonymous callers get the user error `unauthenticated`, signed-in users `forbidden`. They
are called over the normal sync protocol (or the HTTP function API), so every dashboard view is a live query.
`@rxferrix/dashboard` holds hand-written references (`packages/dashboard/src/lib/api.ts`).

| Path `_system/dashboard:…` | Kind | Args → result |
|---|---|---|
| `tables` | query | `{}` → `TableSummary[]`: `{ name, number, system, defined, documents, indexes: [{ name, fields, builtin, usable }], searchIndexes: [{ name, kind: "search" \| "vector", field, filterFields, dimensions, tokenizer: "unicode61" \| "porter" \| "trigram" \| "snowball" \| null, language, removeDiacritics, method: "exact" \| "hnsw" \| null, hnsw: { m, efConstruction, efSearch } \| null }], ts, secretFields, access: "readWrite" \| "limited" \| "readOnly", analytics, columns: [{ name, sqlType }], aggregates: [{ name, groupFields, sumFields }] }`, app tables first. Reruns on any write (it counts every table). |
| `documents` | query | `{ table, index?, filters?: [{ field, op: "eq"\|"gt"\|"gte"\|"lt"\|"lte", value }], order?: "asc"\|"desc", paginationOpts }` → `PaginationResult<object>`. Untyped `query_table(..).with_index(..).paginate(..)`: same cursors and read sets as typed queries, so pages update live (`usePaginatedQuery` works). Filters follow the `with_index` rules. Errors: `unknown_table`, `invalid_query`, `index_not_allowed`. |
| `search_documents` | query | `{ table, index, text, filters?: [{ field, value }], paginationOpts }` → `PaginationResult<object>`, most relevant first: untyped `search_table(table, index).search(<its search field>, text).eq(field, value)….paginate(..)` (§1.8: same matching, cursors and per-filter-value read sets, so pages update live and `usePaginatedQuery` works). For the Data page's search box; `tables` lists each table's `searchIndexes` (`kind: "search"`, `field`, `filterFields`, `tokenizer`, `removeDiacritics`). Errors: `unknown_table`, `invalid_query` (no such search index, a vector index, a field that is not a filter field, a bad cursor), `index_not_allowed` (an index over a secret field). |
| `document` | query | `{ id }` → `{ table, document } \| null` (any table; the table number is in the id). `invalid_id`. |
| `insert_document` | mutation | `{ table, document }` → new id. System fields in `document` are ignored. |
| `patch_document` | mutation | `{ id, patch }` → `null`. Shallow merge, validated. |
| `replace_document` | mutation | `{ id, document }` → `null`. System fields are ignored (`_id`, if given, must match). |
| `delete_documents` | mutation | `{ ids }` → count, in one transaction. |
| `functions` | query | `{}` → `[{ path, kind, visibility, system, argsTs, outputTs }]`, internal and system functions included. |
| `run_function` | action | `{ path, args?, asUser? }` → `{ kind, success, value, error: { message, code, data } \| null, logLines, durationMs, ts, identity }`. Runs any function (internal and system included) as the calling admin, or as `asUser` through a real session of that user (5 minutes, `impersonatedBy` = the admin, audited as `impersonate`, deleted when the run ends), so `ctx.auth.session_id()` works and queries are scoped exactly as for the user. The function and the user must exist (`not_found`). Every run is audited as `run_function` (`details`: path, kind, asUser, sessionId, args with secrets redacted, outcome): a mutation's entry is written in the mutation's own transaction (`outcome: "success"`), a failed mutation gets its own entry (`"error"`), queries and actions are recorded before they run (`"started"`). A failing function is a result with `success: false`, not an error. |
| `users` | query | `{ search?, paginationOpts }` → `PaginationResult<{ _id, _creationTime, email, emailVerified, name, image, providers, hasPassword, mfaEnabled, sessions }>`; newest first, or by email prefix with `search`. |
| `user_sessions` | query | `{ user }` → `[{ _id, _creationTime, expiresAt, lastUsedAt, userAgent, ip, expired, impersonatedBy }]` (no token hashes; `impersonatedBy` is the admin id of an impersonation session, else null). |
| `revoke_session` | mutation | `{ session }` → `null`. Deletes the session and its rotated refresh tokens; its sockets get `AuthError` at once (live revocation). |
| `delete_user` | mutation | `{ user }` → `{ sessions, accounts, credentials, verificationCodes, mfaFactors }`: the user and everything auth keeps for them, in one transaction. App documents referencing the user are left alone. |
| `reset_mfa` | mutation | `{ user }` → `null`. Removes the user's second factor (lost authenticator and recovery codes): their next sign-in needs no code. `not_found`, `mfa_not_enabled`. |
| `me` | query | `{}` → `{ _id, email, mfaEnabled } \| null` of the signed-in admin (live). |

Write errors (schema validation, unknown table, missing document) are user errors with codes `invalid_document` /
`not_found`, carrying the engine's message (e.g. ``inserted document … does not match table `todos`; nothing was
written: …``).

**Audited.** `insert_document`, `patch_document`, `replace_document`, `delete_documents` (one entry per document),
`revoke_session`, `delete_user` and `reset_mfa` append an `_audit_log` entry in their own transaction (§4.1.5), so an
entry exists exactly for the writes that committed.

**Secrets.** In every `_`-prefixed table, a field whose name ends in `hash`, `hashes` or `encrypted` (case-insensitive:
`passwordHash`, `refreshTokenHash`, `tokenHash`, `codeHash`, `_settings.encrypted`, `_mfa_factors.secretEncrypted` and
`recoveryCodeHashes`) is replaced by `"••••"` in
everything the dashboard returns (audit log diffs included), and indexes
over such a field are `usable: false` and refused by `documents` (a range filter on them would let an admin
binary-search a hash).

**System tables are read-only** through the generic writes (`read_only_table`), with one exception: in `_users`,
`name`, `image` and `emailVerified` can be changed through `patch_document` / `replace_document` (any other changed
field: `read_only_field`; `image` must be an http(s) URL). Users and sessions are removed with `delete_user` and
`revoke_session`, which keep the auth invariants (no orphaned credentials, accounts, or rotated-token rows).

### 4.1.1 Files page functions

`crates/rxferrix/src/storage/mod.rs` defines the Files page backend, admin-only like §4.1 (`unauthenticated` /
`forbidden`). References: `storage` in `packages/dashboard/src/lib/api.ts`.

| Path `_system/storage:…` | Kind | Args → result |
|---|---|---|
| `files` | query | `{ paginationOpts }` → `PaginationResult<FileSummary>`, newest first, live. `FileSummary` = `{ _id, _creationTime, sha256, size, contentType, createdBy, filename, url, downloadUrl }`. In private mode `url` is a signed URL (valid one to two hours, stable within the hour) that already has a query string: append variants with `&` (`&thumb=96x96&fit=cover` for a preview), or use `downloadUrl` (`download=1` appended correctly). |
| `file` | query | `{ id }` → `FileSummary \| null`. `invalid_id` for a malformed id. |
| `delete_file` | mutation | `{ id }` → `null` (`ctx.storage.delete`). `not_found` if missing. |
| `generate_upload_url` | mutation | `{}` → an upload URL (the dashboard's upload button; `createdBy` = the admin). |
| `sweep_orphans` | internal action | `{}` → `{ scanned, deleted, thumbnails, kept }`: the orphan sweep, run by the system cron `_storage_sweep` (run it now with `_system/scheduler:run_cron_now { name: "_storage_sweep" }`). |

Thumbnails for previews: `<url>?thumb=WxH&fit=cover` (or `&thumb=…` on a signed URL), see docs/PROTOCOL.md.
### 4.1.2 Scheduler tables and system functions

Defined in `crates/rxferrix/src/scheduler` with the normal macros (§1.7).

**`_scheduled_jobs`** (`ScheduledJob`): `{ path, kind: "mutation" | "action", args, runAt, state: "pending" |
"inProgress" | "success" | "failed" | "canceled", attempts, maxAttempts, result: string | null (the return value as
JSON text), error: string | null (both cut to 4 KiB), scheduledAt, startedAt, completedAt, origin: string | null (the
scheduling function's path, "cron:<name>" or "dashboard") }`, with indexes `by_state_run_at = [state, runAt,
scheduledAt]`, `by_completed_at = [completedAt]`, `by_kind_state_run_at = [kind, state, runAt, scheduledAt]` (the
dispatcher's mutations) and `by_kind_pool_state_run_at = [kind, pool, state, runAt, scheduledAt]` (each pool's
actions). Mutation jobs never show `inProgress`. Workpools (§1.7) add `pool: string | null`, `onComplete: { path,
context } | null` and `backoff: { initialMs, base } | null`; `origin` can also be `"onComplete:<job id>"` or
`"workflow:<id>"`.

**`_crons`** (`Cron`): `{ name, schedule (the expression, or "every 5m"), timezone (IANA name, null for UTC), path,
args, lastRun, nextRun, lastJob }`, indexes `by_name` and `by_next_run`. Names starting with `_` are system crons.

| Path `_system/scheduler:…` | Kind | Args → result |
|---|---|---|
| `schedule` | internal mutation | `{ path, args, runAt, maxAttempts, origin?, pool?, onComplete?, backoff? }` → job id. `ctx.scheduler` in actions. |
| `cancel_pool` | internal mutation | `{ pool, reason }` → how many were canceled. `ctx.scheduler.cancel_pool` in actions. |
| `cancel` | internal mutation | `{ id, reason? }` → `"canceled" \| "alreadyFinished"`. `ctx.scheduler.cancel` in actions. |
| `jobs` | query (admin) | `{ state?, paginationOpts }` → `PaginationResult<Doc<"_scheduled_jobs">>`. With `state`: that state through `by_state_run_at`, pending/in-progress soonest first, finished ones most recent first; without: newest first. Live. Finished jobs are listed until they move to the job history (§1.7; `GET /api/admin/job-history`, §4.2). |
| `cancel_job` | mutation (admin) | `{ id }` → `"canceled" \| "alreadyFinished"`. Pending jobs and **actions in progress** (signaled, then aborted after `cancel_grace`; §1.7); `error` = `"Canceled from the dashboard by admin:<id>"` (+ `" (while running)"`, then how the action ended). |
| `run_job_now` | mutation (admin) | `{ id }` → the id of the job that will run: a pending job becomes due now (same id); a finished one is run again as a new job (same path and args, origin `dashboard`). `running` for an action in progress. |
| `crons` | query (admin) | `{}` → `[{ _id, name, schedule, timezone, system, path, args, lastRun, nextRun, lastJob, lastJobState }]`: the app's crons by name, then the system crons (`system: true`, e.g. `_storage_sweep`). Live. |
| `run_cron_now` | mutation (admin) | `{ name, force? }` → the id of the job enqueued now (origin `cron:<name>`), which becomes the cron's `lastRun` / `lastJob` (`nextRun` unchanged). `running` while the cron's previous job is pending or in progress, unless `force: true`; `not_found` for an unknown name. Works for system crons. |

The admin functions start with `ctx.auth.require_admin()` like §4.1; `invalid_id` / `not_found` for bad ids. The
dashboard's references are in `packages/dashboard/src/lib/api.ts` (`scheduler`), its Scheduler page at `/_/scheduler`.

### 4.1.2.1 Workflow tables and system functions

Defined in `crates/rxferrix/src/workflow` (§1.7.1).

**`_workflows`** (`Workflow`): `{ name (the workflow's path), args, state: "running" | "success" | "failed" |
"canceled", result (the return value, in full), error, steps (started so far), startedAt, completedAt, origin,
onComplete: { path, context } | null, events: [{ name, value }] (sent before they were waited for) }`, indexes
`by_state = [state, startedAt]` and `by_completed_at = [completedAt]`. Referenceable by app code (§4).

**`_workflow_steps`** (`WorkflowStep`): `{ workflow, index, kind: "query" | "mutation" | "action" | "sleep" | "event",
name (function path, event name, or "sleep"), args (`{ ms }` for a sleep), state: "running" | "success" | "failed" |
"canceled", result (in full: replays read it), error, job (the job that runs or ends it), startedAt, completedAt }`,
index `by_workflow = [workflow, index]`.

| Path `_system/workflow:…` | Kind | Args → result |
|---|---|---|
| `advance` | internal mutation | `{ workflow }`: run the body, start new steps (the engine's job). |
| `step_done` | internal mutation | `{ workId, context: step id, result }`: the `on_complete` of mutation and action steps. |
| `wake` | internal mutation | `{ step }`: a sleep has ended. |
| `start` | internal mutation | `{ name, args, onComplete?, origin? }` → workflow id. `ctx.workflows.start` in actions. |
| `cancel` | internal mutation | `{ id, reason }` → whether it was running. `ctx.workflows.cancel` in actions. |
| `send_event` | internal mutation | `{ id, name, value }`. `ctx.workflows.send_event` in actions. |
| `list` | query (admin) | `{ state?, paginationOpts }` → `PaginationResult<Doc<"_workflows">>`, newest first. Live. |
| `steps` | query (admin) | `{ id }` → the workflow's steps in order. Live. |
| `cancel_workflow` | mutation (admin) | `{ id }` → `"canceled" \| "alreadyFinished"`; `error` = `"Canceled from the dashboard by admin:<id>"`. |

The dashboard's Workflows page (`/_/workflows[?state=…&id=…]`) lists them and shows an open workflow's steps.

### 4.1.3 Migration table and system functions

Defined in `crates/rxferrix/src/migrations` with the normal macros (§1.9).

**`_migrations`** (`MigrationRecord`), one row per migration that has run at least once (a declared migration without
a row is pending): `{ name, table, state: "pending" | "running" | "done" | "failed" | "canceled", cursor: string |
null (a pagination cursor over the table's by_creation_time index), processed, changed, startedAt, finishedAt, error
(cut to 4 KiB), failedDocument, schemaCheck: string | null ("passed" or "failed: <report>"), validated: boolean |
null (the run checks every document it processes, so finishing needs no scan; null before its first batch) }`, index
`by_name`.
Read-only through the dashboard's generic writes; codegen leaves it out.

| Path `_system/migrations:…` | Kind | Args → result |
|---|---|---|
| `list` | query (admin) | `{}` → `MigrationSummary[]`: every declared migration in run order (dependencies first, then name order), then rows of migrations no longer declared. `MigrationSummary` = `{ name, table, declared, batchSize: number \| null, after: string[] (its dependencies; [] when not declared), state, processed, changed, documents (the table's current count, for a progress bar), startedAt, finishedAt, error, failedDocument, schemaCheck, tableMigrating, onRead (an `on_read` migration), validated }`. Live (reruns with each batch and each write to the migrated tables). |
| `status` | query (admin) | `{ name }` → `MigrationSummary \| null`. |
| `run` | action (admin) | `{ name?, dryRun?, restart?, force? }` → `{ started: string[], reports: MigrationReport[] }`. Without `name`: every migration that isn't done, in run order (a `blocked` one stops the background run). With `name`: `dependencies_not_done` if a migration it runs after isn't done, unless `force`. Real runs continue in the background (`started`; follow them with `list`); `reports` then holds the ones that didn't start (`alreadyDone`, `alreadyRunning`). Dry runs run to the end and return a report each (`outcome: "dryRun"` or `"failed"`, counts, up to 5 `examples` `{ id, before, after }`). `not_found` for an unknown name. |
| `cancel` | mutation (admin) | `{ name }` → `"canceled"` (it stops after its current batch; the next run resumes) or `"notRunning"`. |
| `start` | internal mutation | `{ name, restart, force? }` → `"started" \| "alreadyRunning" \| "alreadyDone" \| "blocked"` (`blocked`: a dependency isn't done and not `force`; nothing changed). |
| `run_batch` | internal mutation | `{ name, dryRun, cursor? }` → `{ ran, state, processed, changed, done, cursor, schemaCheck, examples }`: one batch (§1.9). |
| `fail` | internal mutation | `{ name, error, document? }` → `null`: marks the migration failed. |

The admin functions start with `ctx.auth.require_admin()` like §4.1.

### 4.1.4 Settings (`_settings`, `_system/settings:*`)

Defined in `crates/rxferrix/src/settings`. Admin-only (`unauthenticated` / `forbidden`). They are **actions** (they
need the server's configuration and keys), so the dashboard calls them with `useAction` / `client.action`
(references: `settings` in `packages/dashboard/src/lib/api.ts`; the form logic is `lib/settings.ts`).

**`_settings`** (`Setting`): `{ key, value, encrypted: string | null, updatedAt, updatedBy }`, index `by_key`. One row
per key: `oauth`, `smtp`, `redirectUrls`, `signups`, `accessTokenTtlSecs`, `sessionTtlSecs`, `corsOrigins`, `backups`
(§4.4), `trustedProxy`, `rateLimits`, `emailTemplates`. `value` holds the non-secret JSON; `encrypted` the secret part (the OAuth client secrets, and Apple private keys, as a JSON map by provider id, the
SMTP password, or the backups' S3 secret access key), sealed as `v1.<base64url(nonce ‖ ciphertext ‖ tag)>` with XChaCha20-Poly1305 under a key derived
from `auth_secret.key` (BLAKE3 KDF, context `rxferrix settings v1 …`), a random 192-bit nonce per write, and the row's
key as associated data. The data browser shows `encrypted` as `"••••"`; generic writes are refused (`read_only_table`).
A row that can't be decrypted (another `auth_secret.key`) is ignored with an error in the log: its providers are off
until the secret is entered again.

| Path `_system/settings:…` | Kind | Args → result |
|---|---|---|
| `get` | action | `{}` → `SettingsView` |
| `update` | action | `{ oauth?, smtp?, redirectUrls?, signups?, accessTokenTtlSecs?, sessionTtlSecs?, corsOrigins?, backups?, trustedProxy?, rateLimits?, emailTemplates?, clear? }` → `SettingsView`. Omitted fields are unchanged; `clear: ["smtp", …]` resets keys to their defaults; `oauth` replaces the list of providers kept in settings, `emailTemplates` all the templates. Validated as a whole before anything is written (`invalid_settings` with a message): provider ids `[a-z0-9_-]{1,40}`, unique; `kind` `github` / `google` / `discord` / `apple` / `oidc` (`oidc` needs an https `issuer`, http only for loopback or in `--dev`; `apple` needs `teamId` and `keyId` (letters and digits) and its secret is the `.p8` private key, which must parse as a P-256 PKCS#8 key, up to 4 KiB); a client id; a secret to be enabled; scopes without spaces; SMTP `smtp://` / `smtps://` URL with a host and a valid `from` mailbox (lettre must accept the transport); absolute http(s) redirect URLs; `accessTokenTtlSecs` 30 s–24 h, `sessionTtlSecs` 60 s–400 days; CORS entries that are bare origins (`https://app.example.com`) or `*`; `trustedProxy` header names that are valid HTTP header names (trimmed, deduplicated case-insensitively, at most 10; none left clears the key) and `proxies` that are IP addresses or CIDR ranges (at most 64); `rateLimits` rules with a valid label (below; trimmed), `maxRequests` 1–1,000,000, `intervalSecs` 1–86,400, no label listed twice for the same `audience`, at most 50 (off with no rules clears the key); `emailTemplates` (trimmed; nothing left clears the key): an `appName` of at most 100 characters on one line, and per template a subject of 1–200 characters on one line and an HTML body of up to 64 KiB that contains `{code}` or `{link}`, with no `{name}` (lowercase letters and `_`) other than the placeholders; at most 20 providers and 50 list entries. Writes the rows and an `_audit_log` entry (`settings_update`, masked before/after of the changed keys, `details.keys`) in one transaction, then swaps in the new configuration before returning. |
| `test_backup_destination` | action | `{ backups?: BackupsInput }` → the destination's location (`/var/backups/app`, `s3://bucket/prefix`). Writes a small `.rxferrix-probe-*` object there, reads it back (`HEAD`) and deletes it; without `backups`, the destination in effect. `invalid_settings` with the store's error. |
| `send_test_email` | action | `{ to, smtp? }` → `null`. Sends a test email through the mailer in effect, or through the given (unsaved) `SmtpInput` (a missing password means the stored one). `email_not_configured`, or `email_failed` with the transport's error (the password is masked out of it). 30 s timeout. |

```ts
type OAuthKind = "github" | "google" | "discord" | "apple" | "oidc";
type SettingsView = {
  oauth: { id: string; kind: OAuthKind; issuer: string | null; clientId: string; clientSecret: "••••" | null;
           enabled: boolean; scopes: string[] | null; teamId: string | null; keyId: string | null; /* Apple */
           locked: boolean }[];    // locked: a provider with this id is configured in code
  smtp: { url: string /* no password */; from: string; password: "••••" | null } | null;
  redirectUrls: string[] | null; signups: boolean | null; accessTokenTtlSecs: number | null;
  sessionTtlSecs: number | null; corsOrigins: string[] | null;               // null: not set in _settings
  backups: object | null;  // the stored backups setting (§4.4), secretAccessKey masked
  trustedProxy: TrustedProxy | null;
  rateLimits: RateLimitSettings | null;
  emailTemplates: EmailTemplates | null;
  emailTemplateDefaults: EmailTemplates; // the built-in emails as templates (each one set, appName null)
  locked: string[];      // fields the configuration fixes: "signups" | "accessTokenTtlSecs" | "sessionTtlSecs" | "redirectUrls" | "smtp" | "oauth.<id>"
                         // | "trustedProxy" (with AuthConfig::trust_proxy / --trust-proxy)
                         // | "trustedProxy.proxies" (with AuthConfig::trusted_proxies / --trust-proxy-from)
                         // | "backups.schedule" | "backups.destination"
  effective: { signups: boolean; accessTokenTtlSecs: number; sessionTtlSecs: number; redirectUrls: string[];
               oauthProviders: string[]; emailConfigured: boolean; corsOrigins: string[] | null /* null: any */;
               backups: object /* §4.4 */; trustedProxy: TrustedProxy /* lowercase names; none: the peer address */;
               appName: string /* {app_name}: the setting, else the display name of the SMTP sender in use, else "" */ };
};
// The verification, password reset and sign-in emails. A template not set: the built-in plain-text email. A template
// set: its subject, its HTML, and a plain-text part derived from the HTML (tags dropped, links as `text (url)`).
// Placeholders, replaced in one pass (HTML-escaped in the HTML; other braces are left alone): {link} (the app URL with
// the token in its fragment, §2; empty when no redirect URL is set), {code}, {expires} ("24 hours", "30 minutes",
// "15 minutes"), {app_name}, {email} (the recipient).
type EmailTemplates = { appName: string | null; verifyEmail: EmailTemplate | null;
                        resetPassword: EmailTemplate | null; signIn: EmailTemplate | null };
type EmailTemplate = { subject: string; html: string };
// The headers a reverse proxy puts the client IP in, trusted in order: the first one that holds an IP address wins
// (else the peer address). In a list (`client, proxy1, proxy2`, over every line of the header) the rightmost entry is
// taken, or the leftmost with useLeftmostIp. `AuthConfig::trust_proxy` / `--trust-proxy` means
// `{ headers: ["x-forwarded-for"], useLeftmostIp: false }` and wins over the setting. With `proxies` (IPs or CIDR
// ranges, normalized to `10.0.0.0/8` form, at most 64), the headers count only on requests whose peer is one of them;
// `AuthConfig::trusted_proxies` / `--trust-proxy-from` replace them. Absent: trusted from any peer.
type TrustedProxy = { headers: string[]; useLeftmostIp: boolean; proxies?: string[] };
// Request rate limits per client IP (as `trustedProxy` resolves it; IPv6 clients by their /64). Off: kept, not applied.
// Each rule is a token bucket per client: bursts of up to maxRequests, refilled evenly over intervalSecs. A label is
// - a path prefix (`/api/`, `/api/storage/upload`, an app route, static files): HTTP requests, counted by a middleware
//   before the handler (CORS preflights aren't); the longest matching prefix decides, alone; WebSocket connections
//   (`/api/sync`) and session refreshes (`/api/auth/refresh`) only count against a rule naming them (`/api/sync`), not
//   against `/api/` or `/`, so the dashboard can't be locked out;
// - `module:function`, either side `*`, or `*`: app functions called by clients, over HTTP (`/api/query|mutation|
//   action`) and the WebSocket (mutations and actions; query subscriptions aren't counted); the most specific match
//   decides (`m:f`, then `m:*`, then `*:f`, then `*`).
// A `guest` (no valid access token) or `signedIn` rule goes before an `all` rule with the same label. Over the limit:
// 429 `rate_limited` with `Retry-After` and `errorData.data.retryAfterSecs` in the function API's error shape; over
// the WebSocket, that mutation or action fails with the same error (the connection stays). Never limited: `/_/`,
// `/api/admin/*`, `/api/auth/admin/*`, `/api/health`, `/metrics`, `_system/*` functions and administrators. Saving keeps the counts of
// unchanged rules.
type RateLimitSettings = { enabled: boolean; rules: { label: string; maxRequests: number; intervalSecs: number;
                                                      audience: "all" | "guest" | "signedIn" }[] };
type OAuthProviderInput = { id: string; kind: OAuthKind; issuer?: string | null; clientId: string;
                            clientSecret?: string | null /* omitted, null or "••••": keep; else replace */;
                            enabled: boolean; scopes?: string[] | null;
                            teamId?: string | null; keyId?: string | null /* Apple only, required there */ };
type SmtpInput = { url: string /* a password in the URL is moved out of it */; from: string;
                   password?: string | null /* omitted, null or "••••": keep; "": none */ };
```

Precedence: §1.5 ("Runtime settings and precedence"); `locked` and `effective` show its result. Secrets are only ever
accepted, never returned, and never logged: the execution log stores system functions' arguments with every
field whose name contains `password`, `secret` or `token` (case-insensitive) and every URL password replaced by
`"••••"` (`settings::redact_args`).

### 4.1.5 Audit log (`_audit_log`, `_system/audit:list`)

Defined in `crates/rxferrix/src/audit.rs`. **`_audit_log`** (`AuditEntry`): `{ admin: string | null, adminEmail,
action, targetTable, targetId, before, after, details, at }` with indexes `by_at = [at]`, `by_admin = [admin, at]`,
`by_action = [action, at]`, `by_table = [targetTable, at]`, `by_target = [targetId, at]`. Actions: `insert`, `patch`,
`replace`, `delete` (target: the document; `before` / `after`: the changed fields only for updates, the whole document
for inserts and deletes), `revoke_session` (`before`: the session, hashes masked), `delete_user` (`before`: the user,
`details`: the counts), `reset_mfa` (target: the user), `mfa_enable` / `mfa_disable` (target: the administrator who
turned their own second factor on or off), `settings_update`, `impersonate` (target: the user; `details`: `purpose` (`run_function` or
`open_app`), `sessionId`, `expiresAt`), `run_function` (`details` as in §4.1), `restore_stage` / `restore_cancel` /
`restore_restart` (staged restores, §4.2; `details`: the backup's `source`, its `ts`, `secretIgnored`, `pointInTime`;
the canceled `source`; or the `exitCode`), `replication_snapshot` (`details`: the snapshot's name and ts),
`olap_rebuild`, `olap_query`, `olap_export` (§4.2; `details`: the SQL and row count), `env_create` / `env_update` /
`env_delete` (§4.1.6). Secrets are masked by the §4.1 rule. The operations entries (`restore_*`, `replication_snapshot`,
`olap_*`) are written right after the action, best effort.

Each entry is written in the same transaction as the write it describes. The table is a system table, so the
dashboard's generic writes refuse it, and no function edits or deletes entries; the only deletion is retention:
entries older than `AuthConfig::audit_retention` (90 days) are swept at startup and hourly (`rxferrix::audit::sweep`).

| Path | Kind | Args → result |
|---|---|---|
| `_system/audit:list` | query (admin) | `{ admin?, action?, table?, target?, since?, until?, paginationOpts }` → `PaginationResult<Doc<"_audit_log">>`, newest first, live. The most selective filter picks the index (`target` > `admin` > `action` > `table` > time); the others are applied while scanning; `since` / `until` bound `at` (ms, inclusive). |

### 4.1.6 Environment variables (`_env`, `_system/env:*`)

Defined in `crates/rxferrix/src/env.rs` (API: §1.13). **`_env`** (`EnvVar`): `{ name, valueEncrypted, updatedAt,
updatedBy: string | null }`, index `by_name`. `valueEncrypted` is sealed like `_settings` secrets (§4.1.4: XChaCha20-
Poly1305 under the settings key derived from `auth_secret.key`), with `env:<name>` as associated data. The data
browser shows it as `"••••"`; generic writes are refused (`read_only_table`). The execution log masks the `value`
argument of `_system/env:*` (also inside `run_function`'s `args`). Audit actions `env_create`, `env_update`,
`env_delete`: target `_env` (with the row's id for updates and deletes; none for creates), `details: { name }`,
never the value. Setting a variable to the value it already
has writes nothing.

| Path | Kind | Args → result |
|---|---|---|
| `_system/env:list` | query (admin) | `{}` → `{ name, value, updatedAt, updatedBy }[]` by name, values in clear, live. |
| `_system/env:set` | mutation (admin) | `{ name, value }` → `null`. Creates or replaces. |
| `_system/env:remove` | mutation (admin) | `{ name }` → `null`. A no-op when not set. |
| `_system/env:put` | internal mutation | `{ name, value: string \| null }` → `boolean` (changed). What `env set` / `env unset` run. |

### 4.2 Admin HTTP endpoints

The execution log and server stats live outside the reactive database, so they are plain HTTP. Every endpoint
requires `Authorization: Bearer <access token>` with the `adm` claim: 401 `unauthenticated` without a token, 401
`invalid_token` for an invalid, expired or revoked one, 403 `forbidden` for a user's token. Errors have the auth
endpoints' shape; responses carry `Cache-Control: no-store`. The backup, export and import endpoints are defined in
`crates/rxferrix/src/ops/http.rs` (see §4.4 for what the archives and files contain).

| Endpoint | Response |
|---|---|
| `GET /api/admin/backups` | `{ target, backups: BackupInfo[], pendingRestore: PendingRestore \| null, restart: { available, exitCode }, replication: ReplicationStatus \| null }`, newest first (`replication` as for `GET /api/admin/replication`, null without `--replicate-to`) (`pendingRestore` and `restart` as for `GET /api/admin/restore`). `target` is where backups are kept (`--backup-to`, default `<data dir>/backups`). `BackupInfo` = `{ name, bytes, createdAt (ms), scheduled (a `rxferrix-auto-*` archive from `--backup-every`; only those are rotated), ts (commit ts of the snapshot, or null when unknown) }`. |
| `POST /api/admin/backups` | Body (optional) `{ includeAux?, includeFiles? }` → 201 `BackupInfo` once the archive is stored. Runs while the server serves (writes are not blocked); one backup at a time per process (a second request waits). `includeSecret: true` → 400 `invalid_request` (`auth_secret.key` is only ever included by the CLI). 500 `server_error` with the reason (e.g. S3 credentials) on failure. |
| `GET /api/admin/backups/{name}` | The archive, streamed: `Content-Type: application/zstd`, `Content-Length`, `Content-Disposition: attachment; filename="<name>"`. 404 `not_found`; 400 `invalid_name` for a name that isn't `rxferrix-….tar.zst`. |
| `DELETE /api/admin/backups/{name}` | `{ deleted: true }`; 404 `not_found`; 400 `invalid_name`. |
| `POST /api/admin/backups/{name}/restore` | Stage that backup of the target for a restore at the next start (§4.4 "Staged restores") → `{ pendingRestore, restart }` once it passed every check. 400 `invalid_backup` with the reason (nothing is staged); 409 `restore_pending` if one is staged already; 404 `not_found`; 400 `invalid_name`. Audited (`restore_stage`). |
| `POST /api/admin/restore?name=<file name>` | The same with the archive as the body (any content type; ≤ 64 GiB, else 413 `too_large`; spooled to the data directory). `name` (sanitized to `[A-Za-z0-9._-]`) is only a label. 400 `invalid_request` for an empty body. |
| `GET /api/admin/restore` | `{ pendingRestore: PendingRestore \| null, restart: { available: boolean, exitCode: number \| null } }`. `PendingRestore` = `{ source, stagedAt, stagedBy (admin id), backupCreatedAt, ts, rxferrixVersion, includesAux, blobs, bytes, secretIgnored, error, pointInTime }`; `pointInTime` (null for an archive) = `PointInTimeInfo` `{ generation, snapshot, snapshotTs, requestedTs, requestedTime, ts, time, replayed, segments, reached }` (`reached: false`: the point asked for is beyond the newest replicated commit, which was restored instead); `error` is set when the last start found the staged files changed and didn't apply them. `restart.available`: the server runs with `--restart-exit-code`. |
| `POST /api/admin/restore/point-in-time` | Body `{ toTs?, toTime? (RFC 3339 string or ms), generation? }` (all optional: the newest point) → `{ pendingRestore, restart }`: ships what is unshipped (best effort), rebuilds the point from the replication target (§4.4 "Continuous replication"), checks it like an archive and stages it. 404 `replication_off` without `--replicate-to`; 400 `invalid_point_in_time` (before the oldest snapshot, an unknown generation, both `toTs` and `toTime`), 400 `invalid_backup` when the rebuilt point fails the checks, 400 `invalid_request` for a malformed body; 409 `restore_pending`. Audited (`restore_stage`, with `pointInTime`). |
| `GET /api/admin/olap` | `{ enabled: false }` without `--olap`, else `{ enabled: true, ...OlapStatus }` = `{ state ("building" \| "ready" \| "failed"), detail, appliedTs, latestTs, appliedTime, lagMs, tables: [{ name, rows, columns: [{ name, typeName }] }], sizeBytes, rowsApplied, rebuilds }` (§1.11). |
| `POST /api/admin/olap/rebuild` | 202 `{ rebuilding: true }`: every table of the analytics copy is rebuilt from a snapshot in the background (audited `olap_rebuild`); 404 `olap_off` without `--olap`. |
| `POST /api/admin/olap/query` | Body `{ sql, params?: Json[], format?: "json" \| "csv" \| "parquet" }`: one SELECT over the analytics copy in the sandbox of `ctx.olap` (§1.11). `json` (default): an `OlapResult` `{ columns: [{ name, typeName }], rows, asOfTs }` (at most `max_result_rows`; audited `olap_query`). `csv` / `parquet`: the whole result as a download (`query.csv`, RFC 4180 with a header, nested values as JSON; `query.parquet`, zstd, DuckDB's types nested), headers `x-rxferrix-rows` and `x-rxferrix-as-of-ts`, at most `max_export_rows` (10M) rows within `export_timeout` (5 min); written to a temporary file in the data directory by rxferrix (DuckDB itself has no file access), streamed, then deleted (audited `olap_export`). 400 `olap_query_failed` / `olap_timeout` / `olap_unavailable`, 404 `olap_off`. |
| `GET /api/admin/replication` | `{ enabled: false }` without `--replicate-to`, else `{ enabled: true, ...ReplicationStatus }` = `{ target, everyMs, jobsEveryMs, snapshotEveryMs, keepMs, generation, currentTs, currentTime, shippedTs, shippedTime, lagCommits, lagMs (age of the oldest unshipped commit; 0 when caught up), pendingBytes, maxPendingBytes, journaling, lastSegment: { name, from, to, entries, bytes, shippedAt } \| null, lastSnapshot: SnapshotInfo \| null, nextSnapshotAt, segmentsShipped, snapshotsTaken, lastSuccessAt, lastError, lastErrorAt, failures, generations: [{ id, createdAt, current, snapshots: SnapshotInfo[], segments, fromTs, toTs, fromTime, toTime }] }`, `SnapshotInfo` = `{ name, ts, createdAt, bytes }`. Built from memory and one reader snapshot (no call to the target). |
| `POST /api/admin/replication/snapshot` | 201 `SnapshotInfo`: a snapshot of the current generation now (after shipping), then retention. 404 `replication_off`. Audited (`replication_snapshot`). |
| `DELETE /api/admin/restore` | Cancel the staged restore → `{ canceled: true }`; 404 `not_found` if none. Audited (`restore_cancel`). |
| `POST /api/admin/restore/restart` | 202 `{ restarting: true, exitCode }`, then a graceful shutdown and exit with `--restart-exit-code`, for the supervisor (systemd, Docker) to start the server again, which applies the restore. 400 `restart_unavailable` without `--restart-exit-code`; 409 `no_pending_restore`; 400 `invalid_backup` when the staged restore has an `error`. Audited (`restore_restart`). |
| `GET /api/admin/export/{table}?format=jsonl\|csv` | The table, streamed from one read transaction (a consistent snapshot; never blocks writes), in id order. JSONL (`application/x-ndjson`): one stored document per line. CSV (`text/csv`): header `_id,_creationTime,<other top-level fields, sorted>`, RFC 4180 quoting, CRLF line ends; strings as is, `null`/missing empty, other values as JSON. `Content-Disposition: attachment; filename="<table>.<format>"`. In system tables, secret fields are masked as in §4.1. 404 `not_found` for an unknown table, 400 `invalid_request` for another format. |
| `POST /api/admin/import/{table}?mode=insert\|upsert\|replace&format=jsonl\|csv` | Body: JSONL, or CSV with `format=csv` (as exported: a header row, RFC 4180; cells typed with the table's shape, §4.4) (≤ 256 MiB, else 413 `too_large`) → `ImportReport` = `{ tables: [{ table, documents, replaced, deleted, droppedFields }], files, missingFiles, remappedIds }`. App tables only (400 `read_only_table` for `_`-prefixed ones; 404 `not_found` for unknown tables). Every line is parsed and validated before anything is written (400 `import_failed` with up to 20 `<line>: <error>` messages); then batches of 500 documents per transaction through the normal write path, so the dashboard's live views update. `insert` (default) refuses ids that exist; `upsert` replaces them; `replace` deletes every document of the table first. Ids and `_creationTime` are kept; lines without `_id` get a new one. Fields the table doesn't define (also inside nested structs, not in open ones or JSON fields) are left out and listed in `droppedFields` by path (`notes`, `address.zip`, `items[].extra`). |
| `GET /metrics` | Not under `/api/admin`: Prometheus text (`text/plain; version=0.0.4`), only when the server runs with `--metrics` (404 otherwise); with `--metrics-token`, `Authorization: Bearer <token>` is required (401). Metrics are listed in `crates/rxferrix/src/ops/metrics.rs`. |
| `GET /api/admin/logs?<LogFilter>` | `{ entries: LogEntry[] }`, newest first. Query parameters: `pathPrefix`, `path` (exactly this function), `kind`, `outcome`, `since`, `limit` (≤ 1000), `beforeId`, `identity`, `excludeSystem`, `excludeBackground` (leave out scheduled jobs, crons and workflow steps, and the functions they call). 400 `invalid_request` for malformed ones. |
| `GET /api/admin/job-history?<HistoryFilter>` | `{ jobs: Doc<"_scheduled_jobs">[] }`: the job history (§1.7), most recently finished first. Query parameters: `state` (`success`, `failed`, `canceled`), `path` (exactly this function), `limit` (50, ≤ 500), and `beforeCompletedAt` with `beforeId` (the last job of the previous page; 400 `invalid_request` with only one). |
| `POST /api/admin/job-history/retry` | Body `{ id }` → `{ id }`: run a job of the history again as a new job due now (same path, args, pool, attempts, `onComplete` and backoff; origin `dashboard`), like `run_job_now` for a finished job. 404 `not_found` when the history has no such job. |
| `GET /api/admin/logs/stream?<LogFilter>` | `text/event-stream`: `event: ready` once, then `event: log` (`data`: a `LogEntry` as JSON) for each new matching entry, `event: lagged` (`data`: how many entries were skipped because the client fell behind), keep-alive comments every 15s, `event: expired` right before the stream ends when the access token expires (reconnect with a fresh token), and `event: revoked` (`data`: `session has ended`) when it ends because the admin's session ended (sign-out, revocation). |
| `POST /api/admin/impersonate` | Body `{ user, ttlSeconds?, redirectTo? }` → `{ accessToken, refreshToken, expiresIn, userId, sessionId, expiresAt, url }`. Mints a session of `user` (default 15 minutes, 1–3600 s, 400 `invalid_request` otherwise) marked `impersonatedBy` the admin, with a fixed expiry (refreshing never extends it; access tokens never outlive it), audited as `impersonate` in the same transaction. With `redirectTo` (checked against the redirect allowlist, 400 `invalid_redirect`), `url` is that app URL with `#rxferrix_access_token=…&rxferrix_refresh_token=…&rxferrix_expires_in=…&rxferrix_user_id=…`, which `handleRedirectCallback()` consumes: open it in a private window. 404 `not_found` for an unknown user (400 `invalid_id` for a malformed one), 403 `impersonation_disabled` with `AuthConfig::impersonation(false)`. |
| `GET /api/admin/stats?since=<ms>&excludeBackground=<bool>` | `{ since, functions: FunctionStats[], olap: OlapStatus \| null, overview: { now, startedAt, version, dev, ts, commitTime, tables, documents, registeredFunctions, sockets, subscriptions, sharedQueries, droppedLogEntries, files: [{ name, bytes }], commitsPerSecond, queueDepth, runningJobs, subscriptionRounds, queryReruns, cachedQueries, cacheBytes, cacheHits, cacheMisses, metricsEnabled } }`. With `excludeBackground`, `functions` leaves out background work, as for the logs. `subscriptions` counts (socket, queryId) pairs, `sharedQueries` the distinct queries the server actually reruns; `cachedQueries` / `cacheBytes` / `cacheHits` / `cacheMisses` describe the cache of unwatched query results. `olap` is as for `GET /api/admin/olap` (null without `--olap`). `since` defaults to 24 hours ago; `files` are `data.db`, `aux.db` and their WAL files. `commitsPerSecond` is over the last 10 seconds; `queueDepth` / `runningJobs` count pending scheduled jobs and actions in progress; `subscriptionRounds` / `queryReruns` are totals since startup (rate them between polls); all are available without `--metrics` (`metricsEnabled` says whether `/metrics` is served). |
| `GET /api/admin/stats/timeseries?path=&bucket=&since=&until=&excludeBackground=` | `{ path, bucket, since, until, points: TimePoint[] }`: the executions of `path` (every app function, system ones excluded, without it; background work excluded with `excludeBackground`, as for the logs) that started in `[since, until)`, in buckets of `bucket` (ms, or `30s` / `1m` / `15m` / `1h` / `1d`; default `1m`), oldest first, only buckets with executions. `TimePoint` = `{ t (bucket start), calls, errors, userErrors, serverErrors, p50Us, p95Us, maxUs }` (nearest-rank percentiles). `since` is rounded down to a multiple of `bucket` (the returned `since`); defaults: `until` now, `since` an hour before. At most 1500 buckets, else 400 `invalid_request` (also for a malformed bucket or `until <= since`). From the execution log (`Logs::timeseries`), so only as far back as its retention. For the Functions page's charts. |
| `GET /api/admin/client-ip` | `{ ip, header, peer, proxyHeaders: { name, value }[] }`: the caller's IP as RxFerrix resolves it with the trusted proxy headers in effect (`trustedProxy`), to check them from the dashboard. `header`: the trusted header it came from (null: the peer address); `proxyHeaders`: the well-known client IP headers (`cf-connecting-ip`, `true-client-ip`, `fly-client-ip`, `fastly-client-ip`, `x-real-ip`, `x-client-ip`, `x-cluster-client-ip`, `x-envoy-external-address`, `x-forwarded-for`, `forwarded`) and trusted ones present on the request, as received (lines joined with `, `). |

**Why bearer auth for SSE too**: `EventSource` can't send headers, but a token in the URL would end up in proxy and
server logs and browser history. The dashboard reads the stream with `fetch()` and a streamed body instead, so the
short-lived access token travels only in the `Authorization` header. The stream ends when that token expires (at most
15 minutes), and immediately (`event: revoked`) when its session ends.

**Impersonation, security.** Impersonation lets an admin act with all of a user's rights, so: it is admin-only and
can be disabled; every impersonation session is audited at creation (same transaction), carries
`impersonatedBy` (apps can refuse sensitive operations such as changing credentials or payments when
`ctx.db.get(ctx.auth.session_id()?)` has it set), shows in the user's session list, is revocable like any session
(with live revocation), dies with `delete_user`, and has a hard expiry of at most an hour that refreshing can't
extend. `run_function`'s sessions last 5 minutes and are deleted as soon as the run ends. The minted tokens are never
logged; they travel only in the response body or a URL fragment (never sent to servers). Opening the app with them
stores them in that browser's app storage, replacing whoever was signed in there: use a private window.

### 4.3 The embedded dashboard

With the cargo feature `dashboard` (default), `GET /_/` and `/_/{*path}` serve the SPA from
`crates/rxferrix/dashboard-dist` (embedded with `rust-embed`): existing files as is (`assets/*` cached immutably; an unknown
`assets/*` file is a 404), everything else as `index.html` (client-side routes such as `/_/data/todos?doc=<id>` and `/_/setup#token=…`), with a
restrictive Content-Security-Policy and `X-Frame-Options: DENY`. Without the feature, only a minimal `/_/setup` page
is served; the system functions and `/api/admin/*` exist either way.

### 4.3.1 Takeover (`serve --takeover`, `crate::ops::takeover`)

Every `serve` listens on `<dir>/rxferrix.control` (Unix socket, 0600). `serve --takeover` connects and sends
`{"op":"prepare"}`; the running server enables its change journal for a `Takeover` consumer (`_meta.takeover_ts`; rows
after it are kept) and answers `{ok, ts, pid, roles}`. The new process copies `data.db` with `VACUUM INTO` to
`<dir>/takeover/data.db`, opens it with its tables (`Database::open_with_migrations`: catalog reconcile with every
backfill, migration coverage, schema check), and replays `_journal` rows after the copy's ts with
`Database::replay_checked` (a document that doesn't match its table's new type, unless the table is migrating, fails).
Any failure before the handover: it closes the socket, the running server disables the consumer (and the journal, if
nothing else needs it, trimming the rows), and the new process exits non-zero. `{"op":"handover"}` returns the
listening sockets with `SCM_RIGHTS` (roles `main`, `redirect`) and, under systemd, the new process sends
`MAINPID=<pid>`. The running server stops accepting (`RunningServer::pauser`), sets `Connection: close` on responses
for 100 ms (`App::begin_drain`), closes WebSockets with 1012 and ends admin log streams (`App::begin_close`), stops
gracefully (up to 10 s for open requests), flushes the execution log, seals its database (`Database::seal`: no write
transaction can begin) and exits. The new process waits for the socket to close, takes `rxferrix.lock`, replays the last
rows, checks that the copy's ts equals the database's, copies `_mutation_dedup` rows, the replication and analytics
`_meta` positions and `_journal` (trimmed to what those consumers still need), raises `last_creation_time` if needed,
then swaps: `<dir>/takeover/swapping` marks it, `data.db*` moves to `takeover/previous/`, the copy moves in, the
marker goes; a start that finds the marker finishes the move (`takeover::recover`). If anything in this last step
fails, the new process logs it, completes a swap that had begun (or drops the copy) and opens the database as the
old server left it, with the usual schema check (`Incoming::migrating` is `None`): once the old server is gone, the
new one serves regardless. Otherwise the app opens with
`AppOptions::prechecked_schema` (the copy's migrating tables; the schema check isn't repeated) and serves on the
inherited sockets (`tls::start_with`; a socket is reused only if the configured address matches). `check` performs the
copy and open in `<dir>/check-<pid>/` and deletes it. Unix only.

`{"op":"ping"}` answers `{ok, pid}` without starting anything (`takeover::server_running`). The `prepare` reply also
carries the server's network namespace (`netns`: `readlink /proc/self/ns/net`, Linux) and its listening addresses (`addrs`:
`[[role, address]]`). The new process asks for the sockets (`{"op":"handover","sockets":true}`, the default) only if
it runs in the same namespace and the old server listens on the address it is configured with (`takeover::serves`);
otherwise it sends `"sockets":false`, gets none, and binds its own address after the swap. The old server, when its
sockets weren't taken (and on a normal stop), accepts what has queued and closes its plain HTTP listeners as its drain
begins (`Pauser::close`): new connections are refused, which a proxy can retry (a connection reset after the request
was sent can't be retried safely); open connections are served through the drain with `Connection: close`. `serve --takeover` takes `require`
(the default: no server running is an error) or `auto` (start normally when the ping fails), also as
`RXFERRIX_TAKEOVER`. `serve --after-handover wait` (`RXFERRIX_AFTER_HANDOVER`; default `exit`): after a handover the
process releases everything (database, lock, listeners, control socket; it exits its runtime) and waits for SIGTERM
or SIGINT, then exits 0. The Docker image sets `RXFERRIX_TAKEOVER=auto` and `RXFERRIX_AFTER_HANDOVER=wait`. `rxferrix
health [url]` (default `http://127.0.0.1:8090/api/health`) exits 0 if the URL answers 2xx within 3 s, 1 otherwise.

### 4.4 Operations: backups, export and import (`crate::ops`)

**Backup archives** (`rxferrix-<UTC yyyymmddThhmmss.mmmZ>-ts<commit ts>.tar.zst`; scheduled ones `rxferrix-auto-…`; the
snapshot ts in the name is what `GET /api/admin/backups` reports as `ts`, `null` for older names without it): a zstd-compressed
tar of `data.db` (a `VACUUM INTO` snapshot: one read transaction, so a consistent copy at one commit ts that never
blocks the writer; the copy's replication journal and state are removed, below), optionally `aux.db` (same),
`storage/<storage id>` for the blob of every `_storage` document of the snapshot (with `includeFiles` /
`--with-files`; local storage only),
`auth_secret.key` (CLI `--include-secret` only), and last `manifest.json`: `{ format: "rxferrix-backup", version: 1,
createdAt, rxferrixVersion, ts, commitTime (ms of that commit; 0 in older archives), storage: "included" | "excluded" |
"remote", missingBlobs, files: [{ path, bytes, sha256 }] }`.

**Backups from the dashboard** (`_settings` key `backups`, `crates/rxferrix/src/settings/backups.rs`): `update { backups:
{ everySecs: number | null (60 s – 30 days; null: no schedule), keep? (1–1000, default 7), includeAux?, includeFiles?,
destination, secretAccessKey? } }` where `destination` is `{ kind: "default" }` (`<data dir>/backups`), `{ kind:
"local", path }` (absolute) or `{ kind: "s3", bucket, prefix?, region?, endpoint?, accessKeyId? }` (without
`accessKeyId`, the server's `AWS_*` environment; with it, `secretAccessKey` is required the first time and kept when
omitted or `"••••"`; stored as the row's encrypted part, never in the cron's arguments). `clear: ["backups"]` removes
it. Applied at startup and after every change: the `rxferrix-backup` cron is declared, rescheduled or withdrawn at
runtime (`_crons` updated at once; it then runs with `{ schedule: { to: null, keep, includeAux, includeFiles } }`
and reads the destination from the app), and the destination is used for scheduled and manual backups, the listing,
downloads and restores. The command line wins part by part: `--backup-every` fixes the schedule, `--backup-to` the
destination; `SettingsView.locked` then lists `backups.schedule` / `backups.destination`, `backups` shows what is
stored, and `effective.backups` (`{ target, everySecs, keep, includeAux, includeFiles, s3Available }`) what runs.
Replication (`--replicate-to`) stays a command-line setting.

**Scheduled backups**: `serve --backup-every 6h --backup-keep 7 [--backup-to LOCATION] [--backup-with-aux]
[--backup-with-files]` declares the interval cron `rxferrix-backup` running the internal system action
`_system/ops/backup:run_scheduled` with args `{ schedule: { to, keep, includeAux, includeFiles } }` (it shows on the
Scheduler page like any cron; "run now" works). Each run stores a `rxferrix-auto-*` archive and deletes the oldest
scheduled ones beyond `keep`; manual archives are never rotated.

**Restore** (`rxferrix restore <archive | s3://bucket/key>`, offline) takes `<dir>/rxferrix.lock` (held by `serve` and
`import`, so it refuses while a server runs), extracts next to the data, checks entries against the manifest
(unexpected, missing or altered entries are refused), runs `PRAGMA integrity_check`, opens the database with the
binary's tables (schema check, as `serve` does it: a table whose documents predate a changed type passes while a
migration for it is declared) and runs `verify_indexes` (`--skip-verify` skips the last two, for an archive and for a
point in time with `--from` alike). Only then are
`data.db`(`-wal`, `-shm`), `aux.db` (when the archive has one), persisted HNSW indexes (`vector_index_<id>.hnsw`) and, if restored,
`auth_secret.key` moved to `<dir>/pre-restore-<time>/` and the restored files put in place; blobs are added to the
storage directory. A database with a trigger or a view (rxferrix creates neither) is refused. While a restore is staged
(below), `restore` refuses (`restore --cancel` cancels the staged one, offline).

**Staged restores** (`crate::ops::restore`; the endpoints in §4.2, the dashboard's Backups page). Restoring under a
running server would replace a database it has open, so a restore from the dashboard is split in two. *Staging*, while
the server runs: the archive is extracted into a temporary directory in the data directory and checked exactly like
`rxferrix restore` (manifest, `integrity_check`, no triggers or views, opened with the binary's tables, `verify_indexes`;
never `--skip-verify`); a failing archive is refused with the reason and leaves nothing behind. A passing one is
renamed to `<dir>/restore-staged/`, the SHA-256 of its databases recorded, and `<dir>/pending-restore.json` (the
`PendingRestore` of §4.2 plus those checksums) written last, atomically. `auth_secret.key` is never staged, even when
the archive has it. One restore can be staged at a time. *Applying*, when `serve` starts next, after it takes
`rxferrix.lock` and before it opens the database: the staged databases are checked against the recorded checksums
(a file already moved by an interrupted run is checked where it is); then the current `data.db` (with `-wal`, `-shm`),
`aux.db` (when the backup has one) and persisted HNSW indexes go to `<dir>/pre-restore-<staged time>/`, the staged
files take their place and blobs are added to local storage; the marker is removed, then the staging directory.
Each step only moves what is still to be moved, so a crash halfway is completed by the next start. Staged files that
don't match their checksums are not applied: the server starts on its current data and the marker keeps the `error`
until the restore is canceled. *Restart*: `serve --restart-exit-code N` (`RXFERRIX_RESTART_EXIT_CODE`, 1–255) lets an
admin request `POST /api/admin/restore/restart`: `serve` shuts down gracefully (as on SIGTERM) and exits with `N`
for the supervisor to start it again (`deploy/rxferrix.service` sets 75 with `RestartForceExitStatus=75`; with Docker,
`--restart unless-stopped`). Without the flag nothing restarts by itself: the restore waits for the next start.

**Continuous replication** (`crate::ops::replicate`, `crate::db::journal`; `serve --replicate-to <dir | file:///dir |
s3://bucket/prefix> [--replicate-every 1s] [--replicate-jobs-every 60s] [--snapshot-every 24h] [--replicate-keep 7d] [--replicate-max-pending 1GB]`,
env `RXFERRIX_REPLICATE_TO`; `AppOptions::ops.replication`). *Journal*: with replication configured the database opens
with journaling on, and every write transaction that commits documents inserts one row into the raw SQLite table
`_journal (ts INTEGER PRIMARY KEY, time REAL, changes TEXT)` when it finishes, in the same SQLite transaction as its
writes and the `_meta.ts` bump (a group-commit batch, §1.10): `changes` is `[[table, id, document | null], …]`, the final state of every document
written (user and system tables; a document created and deleted by the same transaction is left out, and such a
commit journals `[]`). Index entries, full-text and vector tables and HNSW files are not journaled. So the journal
holds every committed ts exactly once (consecutive), and nothing rolled back; whether a transaction journals is decided
at `BEGIN` under the writer lock, and the switch only flips under that lock. *Layout* in the target:
`generations/<UTC time>-<8 hex>/snapshots/rxferrix-auto-<time>-ts<N>.tar.zst` (backup archives without `aux.db` and
blobs) and `generations/<g>/journal/<from ts>-<to ts>.zst` (ts zero-padded to 20 digits): zstd of a JSON header line
`{ format: "rxferrix-journal", version: 1, generation, from, to, fromTime, toTime, entries, bytes, sha256 }` (the size
and SHA-256 of the body) and one `<ts>\t<time>\t<changes>` line per commit (at most 10 000 commits or ~16 MiB per
segment). *Shipping*: every `--replicate-every` the rows after the shipped ts are read on a reader snapshot, uploaded
as a segment, and only then released, with `_meta.replication_shipped_ts` (and `_generation`, `_shipped_time`)
updated in the same transaction. While every unshipped commit is scheduler bookkeeping (`JournalEntry::
is_scheduler_bookkeeping`: each change is a `_crons` row, or a `_scheduled_jobs` row that was deleted, is past
`pending`, or is pending with origin `cron:…` or `onComplete:…`), shipping waits until the oldest of them is
`--replicate-jobs-every` old (`ReplicationOptions::jobs_every`, 60s; `0s` disables the wait): a busy scheduler on an
otherwise idle app ships about once a minute instead of every second. Any other commit (app writes, a job scheduled by
app code) ships within `--replicate-every` with everything before it; a full segment, a due snapshot, a graceful
shutdown and `POST /api/admin/replication/snapshot` ship everything. A point-in-time restore after losing the server
may then miss up to that long of job progress: those jobs are pending again (mutations that wrote nothing run again;
actions may run again) or in progress (marked failed at start). *Consumers*: the analytics copy (§1.11) reads the same journal; each consumer
records its position (`replication_shipped_ts`, `olap_applied_ts`) and a row is deleted once every active one has
released it (`journal::release`). With the copy on, journaling stays on when the replicator gives up on an unreachable
target (only rows the copy no longer needs are dropped). An upload whose answer is lost is retried and may overlap the earlier segment;
restores pick the segments that reach furthest and verify the overlap is identical. *Generations*: at start the
replicator resumes the generation in `_meta` only if the target has it, holds nothing beyond this database's ts, and
the journal holds every commit after the shipped ts (adopting a segment uploaded right before a crash); otherwise it
starts a new generation with a snapshot (journaling is on first, so every commit after the snapshot's ts is
journaled). Backups and restored databases carry no journal or replication state, so a restored database always starts
a new generation. *Snapshots* every `--snapshot-every` (also `POST /api/admin/replication/snapshot`). *Retention*
after each snapshot: every snapshot taken within `--replicate-keep`, plus the newest one before that window (across
generations), and per generation the segments whose last ts is after its oldest kept snapshot; everything else,
whole generations included, is deleted. *Failure*: errors are retried with exponential backoff (up to a minute),
reported in the status and logged (once, then every minute); the journal keeps growing locally; past
`--replicate-max-pending` it is dropped, journaling pauses and the next successful contact starts a new generation.
`serve` ships what is left at a graceful shutdown (up to 15 s). *Overhead*: one extra row insert per commit, with the
document JSON the write already serialized; measured with `bench_journal_overhead` (`tests/it/replication.rs`, release
build, 5000 sequential insert/patch mutations of a small indexed document, median of 3 rounds): 7065 commits/s
without replication, 6747/s journaling (-4.5%), 6708/s journaling and shipping every second (-5.1%).

**Point-in-time restore** (`rxferrix restore --from <target> [--to-ts N | --to-time <RFC 3339>] [--generation G]
[--storage-dir]`, offline under `rxferrix.lock`; `--list` prints the generations and their restorable ranges; or staged
with `POST /api/admin/restore/point-in-time`). The generation is `--generation`, else with `--to-time` the newest one
created at or before that time, else the newest; the base is its newest snapshot at or before the ts (taken at or
before the time). The snapshot is extracted and checked like an archive (`backup::prepare`), tables created after it
are added with the journal's table numbers, then every journal entry after it up to the point is replayed as its own
commit with its original ts and time (`Database::replay`: unvalidated documents through the write path, so ordinary,
full-text and vector indexes are maintained and the write log is appended). The result goes through the archive
checks (schema check with the binary's tables, `verify_indexes`) and its ts must be the point's: `--to-ts N` restores
exactly N, `--to-time` the last commit at or before it, and a point beyond the newest replicated commit restores that
one (`reached: false`). Before the oldest snapshot is an error. File blobs are not journaled.

A backup holds the whole database, system tables included (`_users`, `_settings`, `_audit_log`, ...); secrets in
`_settings` are encrypted with a key derived from `auth_secret.key`, so they are readable after a restore only with
the same key file. Persisted HNSW vector indexes (`vector_index_<id>.hnsw`) are caches and are not backed up:
they are rebuilt from the restored database at startup.

**Export directories** (`rxferrix export`): `<table>.jsonl` or `<table>.csv` per table (all app tables by default;
`--tables` may name system tables), `files/<storage id>` for exported `_storage` documents, and `manifest.json`:
`{ format: "rxferrix-export", version: 1, documentFormat, exportedAt, ts, rxferrixVersion, tables: [{ name, number,
documents, file }], catalog: { <table>: <number> }, files, missingFiles }`. **Import** (`rxferrix import`, offline;
or the HTTP endpoint above) reads JSONL or CSV: an export directory (its `documentFormat`; ids of tables whose number
differs in the target are rewritten everywhere in the imported documents using `catalog`, and `files/` are stored), a
directory of `<table>.jsonl` / `<table>.csv`, or one file (`--table`); `--format jsonl|csv` (default: from the
extension, `.csv` is CSV). The CLI may write system tables; validation, batching and modes as above.

**CSV import** (`crate::ops::csv`) reads what the CSV export writes: RFC 4180 (quoted fields with commas, doubled
quotes and line breaks; CRLF or LF; a BOM and blank lines are skipped), a header row of unique column names with
`_id` / `_creationTime` optional (missing: a new id, the import time). The export writes strings as is, `null` and
missing fields as empty cells and other values as JSON, so a cell is typed with the table's shape, read from the TS
type of its Rust type (`TableDef::ts`) per top-level field: a field that can only be a string (`string`, `Id<…>`,
string literals) takes the cell as is (empty: `null` if nullable, else `""`); a field that can't be a string parses
the cell as JSON (empty: `null`); for anything else (unions, named `#[object]` types, `any`) a cell that is valid JSON
of a kind the field allows is parsed, otherwise it is a string. If the document then fails the table's validation,
the other reading of each ambiguous cell is tried, field by field. Lossy by construction: a nullable string's `""`
comes back as `null`. Errors name the CSV line a record starts on.

**TLS certificate files** (`--tls-cert`/`--tls-key`) are re-read every `--tls-reload-every` (default `30s`, `0`: never);
a changed pair that loads replaces the certificate for new handshakes (the rustls config is swapped atomically; open
connections keep theirs), a pair that doesn't (a half-written renewal, a mismatched key) is ignored with one error in
the log per change. The HTTP→HTTPS redirect (`--http-redirect`) keeps the requested host when it is one of the
`--https` domains or of the certificate's DNS names (a `*.` name matches one label), else uses the first of them that
isn't a wildcard (400 if none); with no names at all, the requested host. Every HTTPS response gets
`Strict-Transport-Security: max-age=31536000` unless it sets its own (`--no-hsts` turns it off).

**Logs**: `serve --log-format json|pretty|compact` (`RXFERRIX_LOG_FORMAT`; default `pretty` on a terminal, `json`
otherwise) formats the server's `tracing` output on stdout. `json` writes one object per line: `timestamp` (RFC 3339),
`level`, `target`, `fields` (`message` and the event's key-values), `span` and `spans` (the current span and every
enclosing one, with their fields). `RUST_LOG` filters every format; function log lines are captured the same way
(`rxferrix::log::layer`, §1.4).

```rust
// crate::ops (selected)
pub struct OpsOptions { pub metrics: MetricsOptions /* enabled, token */, pub backup_target: Option<String>,
                        pub restart: restore::RestartHandle /* --restart-exit-code */,
                        pub replication: Option<replicate::ReplicationOptions> /* --replicate-to */ }   // AppOptions::ops
backup::create(&app, &BackupSpec { include_aux, include_files, include_secret }, &BackupTarget, scheduled) -> Result<BackupInfo>;
backup::restore(archive: &Path, dir: &Path, &RestoreOptions { storage_dir, skip_verify }) -> Result<RestoreReport>;
backup::prepare(archive, staging, skip_verify) -> Result<Manifest>;   // extract + check, no data dir touched
backup::apply_staged(staging, dir, manifest, storage_dir, previous) -> Result<RestoreReport>;   // offline, repeatable
restore::{stage(dir, archive, label, staged_by) -> Result<PendingRestore>, pending(dir), cancel(dir) -> Result<bool>,
          stage_point_in_time(dir, &BackupTarget, &PointInTime, staged_by) -> Result<PendingRestore>,
          apply_pending(dir, storage_dir) -> Result<Option<RestoreReport>>};   // serve calls apply_pending at startup
replicate::ReplicationOptions { target, store: Option<BackupTarget>, every, snapshot_every, keep, max_pending_bytes,
                                segment_max_entries, segment_max_bytes,
                                background /* default true; false = only sync_now/snapshot_now drive it */ };
                                // AppOptions::ops.replication. The replicator starts at the end of App::open, after the
                                // startup commits, so the first snapshot includes them.
app.replication() -> Option<&Arc<Replicator>>;   // started by App::open when configured
Replicator::{sync_now(&app), flush(&app), snapshot_now(&app) -> Result<SnapshotInfo>, status(&app) -> ReplicationStatus, target()};
replicate::{list_generations(&target) -> Result<Vec<Generation>>, read_segment(&target, generation, from, to),
            plan_restore(&gens, &PointInTime { to_ts, to_time, generation }) -> Result<RestorePlan>,
            prepare_point_in_time(&target, scratch, staging, &point) -> Result<(Manifest, PointInTimeInfo)>,
            restore_point_in_time(&target, dir, &point, &RestoreOptions) -> Result<(RestoreReport, PointInTimeInfo)>,
            plan_retention(&gens, now_ms, keep) -> RetentionPlan, encode_segment, decode_segment};
db.replay(&JournalEntry) -> Result<u64>;   // one journal entry as a commit with its ts and time
RestartHandle::{new(exit_code), exit_code(), request() -> bool, requested().await, is_requested()};
BackupTarget::{parse(location, data_dir), for_app(&app), list(), open(name), download(name, dest), delete(name), rotate(keep)};
export::export_to_dir(data_dir, Option<BlobStore>, out, &ExportOptions { tables, format, include_files }) -> Result<ExportManifest>;
export::import_path(&app, path, table: Option<&str>, &ImportOptions { mode, batch_size, allow_system_tables, format }) -> Result<ImportReport>;
DirLock::acquire(dir) -> Result<DirLock>;     // <dir>/rxferrix.lock
web::router(app, &WebOptions { static_files, routes }) -> axum::Router;   // what `serve` serves
tls::start(router, &Listen::{Http(addr) | Acme { https, redirect, acme } | CertFiles { https, redirect, cert, key, reload_every }}) -> Result<RunningServer>;
```

**Builder**: `RxFerrix::static_dir(dir)`, `RxFerrix::embed_static::<A: rust_embed::RustEmbed>()` (derive with
`#[derive(rxferrix::rust_embed::Embed)] #[crate_path = "rxferrix::rust_embed"]`), `RxFerrix::routes(axum::Router)` and
`RxFerrix::app_routes(axum::Router<App>)` (handlers take `State<App>`: typed `app.query/mutation/action::<F>` calls,
internal functions included, and `app.auth()`; `WebOptions::routes` is a `Router<App>`, a stateless router becomes one
with `.with_state(())`). In app routes, the extractor `rxferrix::auth::Caller { ip, user_agent }` gives the client
address as RxFerrix records it (from the trusted proxy headers: the last `X-Forwarded-For` entry with `--trust-proxy`,
else the `trustedProxy` setting (§4.1.4); otherwise the peer). `RxFerrix::guard(|ctx, function| async { .. })` sets
`AppOptions::guard` (§1.4). `RxFerrix::on_start(|app: App| async { .. -> Result<()> })` (repeatable, in order) runs in
`serve` once the app is open, before the scheduler starts and before the server listens; an error stops `serve`
(`an on_start hook failed: ..`, exit code 1). Other commands and `TestApp` don't run it. Services started from it run
as `App::spawn(future)`: a tracked task that should end when `App::closed()` resolves; on shutdown `serve` waits up to
10 s for these tasks (after `begin_close`, before the replication flush and the seal, so they may still run functions),
then drops the rest, so what they hold (a port) is released before the lock is. With `--takeover` the new process
runs the hooks after the old one has exited.
Routing and caching rules are in `crates/rxferrix/src/ops/web.rs`: RxFerrix's routes and the app's routes first, static
files as the fallback (never under `/api` or `/_`), SPA fallback to `index.html`, `no-cache` HTML, immutable hashed
assets, precompressed `.br`/`.gz`, ETags.


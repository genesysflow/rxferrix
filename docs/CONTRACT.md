# Ferrix Rust contract: user API, runtime internals, and macro expansions

This document is the **contract between `crates/ferrix` (runtime) and `crates/ferrix-macros` (proc macros)**,
plus the user-facing API that both serve. Everything the macros emit refers only to paths listed under
"`::ferrix::__private` items" or under the public API. The runtime must provide exactly those items.

Wire and storage naming: tables, args and system fields use **camelCase JSON keys**. The macros add
`#[serde(rename_all = "camelCase")]` to table structs and to generated args structs. Field and index handles always
carry the *JSON* name (`created_by` → `"createdBy"`).

---

## 1. User-facing API (what app code writes)

```rust
use ferrix::prelude::*;
// prelude = Id, Doc, Field, Index, Order, QueryCtx, MutationCtx, ActionCtx, Result, Error, UserError, Bytes,
//           PaginationOpts, PaginationResult, SearchIndex, VectorIndex, VectorFilter, VectorSearchOpts, DocChange,
//           table, object, query, mutation, action, internal_query, internal_mutation, internal_action, migration,
//           json!, Ferrix
use ferrix::log;   // logging is a separate, explicit import (§1.4): log::info!(...), log::warn!(...)

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

fn main() -> ferrix::Result<()> {
    Ferrix::new().run()        // tables and functions are discovered automatically (linkme)
}
```

Rules:
- A function's first parameter is the context, `&QueryCtx`, `&MutationCtx` or `&ActionCtx`, matching the macro.
  The remaining parameters become the args. Parameter patterns must be plain identifiers.
- The return type must be `ferrix::Result<T>` (written `Result<T>` with the prelude), where
  `T: Serialize + DeserializeOwned + ts_rs::TS + Send + 'static`.
- Every arg type must be `Serialize + DeserializeOwned + ts_rs::TS + Send + 'static`. For types that are not tables
  (a function's output "view" struct, an enum argument), `#[ferrix::object]` derives all three with camelCase JSON
  through ferrix's re-exports, so apps need no serde / ts-rs dependency (§3.4).
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

### 1.1 Core types (in `ferrix`, re-exported by the prelude)

```rust
pub type Result<T, E = Error> = std::result::Result<T, E>;
pub type Error = anyhow::Error;                     // re-export anyhow::Error

#[derive(Debug, Clone, Serialize, Deserialize)]
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
impl<T> Id<T> { pub fn table_number(&self) -> u32; pub fn as_str(&self) -> String; }

/// A stored document: system fields + the user's struct, flattened.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc<T> {
    #[serde(rename = "_id")] pub id: Id<T>,
    #[serde(rename = "_creationTime")] pub creation_time: f64,   // ms since unix epoch
    #[serde(flatten)] pub value: T,
}
impl<T> Deref for Doc<T> { type Target = T; }
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
}   // PartialEq compares name, all index definitions and the rendered `ts()`, never the fn pointers
pub struct IndexDef { pub name: &'static str, pub fields: Vec<&'static str> }   // JSON field names
pub struct SearchIndexDef { pub name: &'static str, pub search_field: &'static str, pub filter_fields: Vec<&'static str> }
pub struct VectorIndexDef {
    pub name: &'static str, pub vector_field: &'static str, pub dimensions: usize, pub filter_fields: Vec<&'static str>,
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

/// Binary data (`ferrix::Bytes`, in the prelude). JSON on the wire and in storage: `{"$bytes": "<base64>"}`
/// (standard alphabet, padded; decoding is strict, so every value has exactly one form). TS: `{ $bytes: string }`;
/// `@ferrix/client` exports `type Bytes`, `bytesToJson(Uint8Array | ArrayBuffer | ArrayBufferView)`,
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
**Bytes in indexes.** The key encoding (PLAN.md §4.3, `ferrix::keys`) recognizes the tagged form and gives it its own
type tag between strings and arrays, `Undefined < Null < Int64 < Float64 < Bool < String < Bytes < Array < Object`,
with an order-preserving body (bytes compare byte-wise; a prefix sorts first), so `Bytes` fields work in `#[index]`,
`with_index` ranges, search filter fields and the dashboard's index filters. Any other object, including one whose
`$bytes` is not canonical base64, stays an Object. Adding the tag renumbered Array and Object, so the key encoding
has a version in `_meta` (`key_encoding`, now 2; absent = 1): opening a database written with an older version
rebuilds every index (ordinary, search and vector) before anything else. Pagination cursors from before such an
upgrade may then fail with `paginate: cursor does not belong to this query` if their range bounds held arrays or
objects.

### 1.2 Contexts and database API

```rust
pub struct QueryCtx    { pub db: QueryDb,    pub auth: Auth, pub storage: QueryStorage,    /* private */ }
pub struct MutationCtx { pub db: MutationDb, pub auth: Auth, pub storage: MutationStorage, pub scheduler: Scheduler, /* private */ }
pub struct ActionCtx   { pub auth: Auth, pub storage: ActionStorage, pub scheduler: Scheduler, /* private: runtime handle */ }
// ctx.storage: file storage, see §1.6. ctx.scheduler: scheduling, see §1.7.
// All three contexts are Send + Sync. QueryCtx/MutationCtx::now() -> f64 is the ms timestamp of the snapshot or commit
// (in a query: the snapshot's commit time, NOT a reactive clock; see §1.10).
// ctx.is_dev() -> bool on all three: the app runs in dev mode (`serve --dev`; `TestApp` too).

// ctx.auth: the caller, from a validated access token (WS `Authenticate` or HTTP `Authorization: Bearer`).
// Identity flows unchanged into every `ctx.run_*` call an action makes.
pub struct Auth { /* identity */ }                      // ferrix::Auth = ferrix::auth::Auth
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
pub struct Identity {                                   // ferrix::Identity = ferrix::auth::Identity
    pub user_id: Option<String>, pub admin_id: Option<String>, pub session_id: Option<String>,  // string ids, no tokens
}
impl Identity {
    pub fn anonymous() -> Self; pub fn user(Id<User>, Option<Id<Session>>) -> Self; pub fn admin(Id<Admin>, Option<Id<Session>>) -> Self;
    pub fn log_summary(&self) -> Option<String>;        // "user:<id>" | "admin:<id>" (execution log), None = anonymous
    pub fn principal_key(&self) -> String;              // log_summary or "anonymous" (scopes mutation dedup keys)
}
// `ferrix::auth::User` is the `_users` table type: { email: Option<String>, email_verified: bool, name: Option<String>,
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
pub const ferrix::db::pagination::MAX_PAGE_SCAN: usize = 10_000;

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
- **Clients** also have `usePaginatedQueryWithStatus` (`@ferrix/react`): the same, but a failed page comes back as
  `error` (with the pages that did load still in `results`) instead of being thrown. (`@ferrix/svelte`'s
  `usePaginatedQuery` always returns `error`.)
- **Clients** (`client.paginatedQuery`, `usePaginatedQuery` in `@ferrix/react` / `@ferrix/svelte`) subscribe to
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

**Function log lines.** `ferrix::log` re-exports the `tracing` macros `trace!`, `debug!`, `info!`, `warn!`,
`error!`. Write `use ferrix::log;` and then `log::info!(...)` (plain `tracing::info!` is the same thing). Events
emitted while a function runs, at DEBUG and above, are captured per execution as `"[LEVEL] message key=value ..."`
(at most 256 lines of 4KB each). A nested `ctx.run_*` call's lines are also appended to its caller's. Events from
tasks the function spawns are not captured. Lines are always stored in the execution log and sent to clients as
`logLines` only in `--dev`. The events still reach the normal subscriber: `ferrix serve` prints them to stdout,
filtered by `RUST_LOG`.

*Why not in the prelude:* glob-importing a `log` module or `info!`-style macros is ambiguous (E0659) wherever the app
also uses the `log` crate or `#[macro_use] extern crate log`, so the prelude leaves them out and the explicit
`use ferrix::log;` (which shadows the `log` crate cleanly in that file) is the one import to add.

Capture uses a tracing layer, `ferrix::log::layer()`, plus a task-local buffer. `ferrix serve` installs stdout output
and the layer, each with its own filter. If no global subscriber is set when an `App` opens, ferrix installs one
with just the layer. An app that installs its own subscriber should add `ferrix::log::layer()` to it, and do so
before opening the `App`.

**Runtime options.**
```rust
pub struct AppOptions {
    pub dev: bool,                    // real error messages and logLines to clients
    pub readers: usize,
    pub mutation_timeout: Duration,   // default 5s (`--mutation-timeout 5s`); see below
    pub skip_schema_check: bool,      // `--skip-schema-check`; see below
    pub log_retention: LogRetention,  // { max_age: 7 days, max_rows: 200_000, trim_interval: 1 min }
    pub scheduler: SchedulerOptions,  // concurrency, job retention, retry backoff, declared crons (§1.7)
    pub auth: ferrix::auth::AuthConfig, // sign-in methods, token lifetimes, email, OAuth (see §1.5)
    pub storage: ferrix::storage::StorageConfig, // file storage backend and limits (see §1.6)
    pub ops: ferrix::ops::OpsOptions, // `/metrics` (enabled, token) and the backup target (see §4.4)
}
impl Default for AppOptions { /* the defaults above, dev = false */ }
```
- **Mutation timeout.** A mutation still running after `mutation_timeout` is dropped and rolled back, and fails with
  `ferrix::MutationTimeout { path, timeout }`, a server error (its message reaches clients only in `--dev`). It can
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

**Execution log** (`<dir>/aux.db`, table `_logs`). Every execution is recorded: queries (including subscription
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
    pub fn subscription_stats(&self) -> SubscriptionStats; // ferrix::sync::subscriptions: { connections, shared_queries,
                                              // subscriptions, index_ranges, rounds, reruns, last_round_reruns }
    pub fn subscriptions(&self) -> &SubscriptionManager;   // the shared subscription manager (started on first use)
    pub fn started_at(&self) -> f64;          // ms since the epoch
}

#[derive(Clone)] pub struct Logs { /* … */ }
impl Logs {
    pub async fn query(&self, filter: LogFilter) -> Result<Vec<LogEntry>>;   // newest first
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LogEntry>;   // live tail, after each entry is written
    pub async fn stats(&self, since_ms: i64) -> Result<Vec<FunctionStats>>;  // per path, sorted by path
    pub fn dropped(&self) -> u64;                                             // entries dropped since startup
    pub async fn flush(&self) -> Result<()>;                                  // wait until queued entries are written
    pub async fn trim_now(&self) -> Result<()>;                               // apply retention now
}

#[derive(Default, Serialize, Deserialize)] #[serde(rename_all = "camelCase", default)]
pub struct LogFilter {
    pub path_prefix: Option<String>,      // e.g. "todos:" or "todos:list"
    pub kind: Option<FunctionKind>,       // JSON: "query" | "mutation" | "action"
    pub outcome: Option<LogOutcome>,
    pub since: Option<i64>,               // ms since the epoch, inclusive
    pub limit: Option<usize>,             // default 100, max 1000
    pub before_id: Option<i64>,           // page backwards: pass the last id of the previous page
    pub identity: Option<String>,         // "user:<id>" | "admin:<id>" | "anonymous"
    pub exclude_system: bool,             // leave out `_system/…` functions (e.g. the dashboard's own queries)
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
pub enum LogOutcome { Success, UserError /* UserError, invalid args */, ServerError /* bugs, panics, timeouts */ }

#[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub struct FunctionStats {
    pub path: String, pub kind: FunctionKind,
    pub calls: u64, pub errors: u64, pub user_errors: u64, pub server_errors: u64,
    pub p50_us: u64, pub p95_us: u64,     // nearest-rank percentiles of duration_us
}
```

### 1.5 Auth configuration and services

```rust
use ferrix::auth::{AuthConfig, GitHub, Google, Oidc, SmtpConfig};
fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new()
        .auth(AuthConfig::password()                // email + password (verification and reset included)
            .magic_link()                           // magic link + 6-digit code by email
            .oauth(GitHub::from_env())              // FERRIX_AUTH_GITHUB_CLIENT_ID / _SECRET (skipped with a warning if unset)
            .oauth(Google::from_env())              // FERRIX_AUTH_GOOGLE_CLIENT_ID / _SECRET
            .oauth(Oidc::new("corp", "https://idp.example.com", "client-id", "secret"))   // discovery
            .redirect_url("https://app.example.com")) // allowlist for OAuth redirects and email links
        .run()
}
```
`AuthConfig::default()` enables no user sign-in method (tokens are still validated; admin sign-in works). Other
builder methods: `passwordless()`, `signups(bool)`, `require_email_verification(bool)`, `access_token_ttl` (15 min),
`session_ttl` (30 days, sliding), `refresh_reuse_grace` (10 s), `public_url`, `smtp(SmtpConfig)`,
`mailer(Arc<dyn Mailer>)` (e.g. `MemoryMailer` in tests), `dev_mailer(Arc<dyn Mailer>)` (replaces the built-in
`DevLogMailer`, which prints every email to the log, in `--dev` when no mailer or SMTP is configured in code, on the
command line or in `_settings`; never used outside `--dev`; it can forward to `DevLogMailer.send(email)` to keep the
log lines), `rate_limits(Option<RateLimits>)`, `trust_proxy(bool)`,
`jwt(JwtProvider)` (external tokens, below), `audit_retention(Duration)` (`_audit_log`, 90 days),
`impersonation(bool)` (`POST /api/admin/impersonate`, default true).
CLI flags / env vars add to it: `--public-url` (`FERRIX_PUBLIC_URL`), `--auth-redirect-url` (repeatable;
`FERRIX_AUTH_REDIRECT_URLS`, comma-separated), `--smtp-url` / `--smtp-from` (`FERRIX_SMTP_URL`, `FERRIX_SMTP_FROM`),
`--trust-proxy`. Commands: `admin create <email> [--password …]`, `admin reset-password <email> [--password …]`
(the password is read from stdin when omitted).

`App::auth() -> &AuthService` exposes the operations behind the endpoints (`authenticate(app, token)`,
`authenticate_watched(app, token)` (also returns a `SessionWatch`, below), `sign_in_as(app, user)`, `create_admin`,
`reset_admin_password`, `create_setup_token`, `oauth_provider_ids()`, `signups_allowed()`, `revocations()`, ...).
`TestApp::with_auth(cfg)` and `TestApp::{query_as, mutation_as, action_as}(&identity, args)` help in tests, as do
`TestApp::create_user(email, password: Option<&str>) -> Result<Id<User>>` (a verified `_users` row, plus a password
credential when given; no auth method needs to be enabled and no email is sent; `email_in_use` if taken) and
`TestApp::as_user(id) -> TestUser` (`query` / `mutation` / `action` as `Identity::user(id, None)`, and `identity()`).

**External JWTs** (`ferrix::auth::jwt`). Tokens of another identity provider are accepted on `Authenticate` and as
HTTP Bearer tokens, alongside ferrix's own:

```rust
use ferrix::auth::{AuthConfig, JwtProvider, JwtAlgorithm};
AuthConfig::default()
    .jwt(JwtProvider::new("https://clerk.example.com").any_audience())                // JWKS by OIDC discovery
    .jwt(JwtProvider::new("https://tenant.auth0.com/").audience("https://api.example.com"))
    .jwt(JwtProvider::new("https://<ref>.supabase.co/auth/v1").audience("authenticated")
        .jwks_url("https://<ref>.supabase.co/auth/v1/.well-known/jwks.json"))
// JwtProvider: jwks_url(url), audience(aud) (repeatable), any_audience(), user_id_claim("sub"), leeway(60s),
// algorithms([JwtAlgorithm::Rs256, Es256, EdDsa]), jwks_refetch_interval(30s), jwks_cache_ttl(1h).
```

- Routing: a header `alg` of `HS256` means a ferrix token; anything else is looked up by its `iss` (exact match). An
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

**Live revocation** (`ferrix::auth::revocation`). Connections that outlive a request (sync sockets, the admin SSE
log stream) authenticate with `authenticate_watched`, which registers the token's session id (or, for external
tokens, the user id) in the app's `Revocations` registry *before* checking that it exists, and returns a
`SessionWatch` whose `ended()` future resolves when the document is deleted. One watcher task per app follows the
write log; for commits that wrote a watched document's `by_id` key it checks the document in a fresh snapshot and
fires the watches of deleted ones. So every way a session ends is covered (sign-out, refresh-token reuse,
`revoke_session`, `delete_user`, password reset, ...) and refreshes (which rewrite the session) are not mistaken for
deletions. The sync worker then sends `AuthError("session has ended")` and closes the socket (docs/PROTOCOL.md,
guarantee 10).

**Runtime settings and precedence** (`ferrix::settings`, §4.1.4). Administrators can configure OAuth providers, SMTP,
the redirect allowlist, sign-ups, token lifetimes and CORS origins at runtime; they are stored in `_settings` (secrets
encrypted) and apply without a restart. What `AuthConfig` sets (in code, on the command line or in the environment)
wins, field by field: an explicit `signups(..)`, `access_token_ttl(..)` or `session_ttl(..)` call; a non-empty
redirect list (`redirect_url`, `--auth-redirect-url`); any `smtp(..)` / `--smtp-url` or custom `mailer`; and, per
provider id, an OAuth provider with credentials (settings may add providers with other ids). Everything else is taken
from `_settings` when set there, else the defaults apply. CORS origins exist only as a setting (default: any origin).

**System tables** (all defined with `#[table]` in `ferrix::auth::tables`): `_users` (`User`), `_credentials`
(argon2id password hash per user), `_accounts` (provider + provider account id; for external JWTs the provider is
the issuer URL), `_sessions` (user or admin, keyed hash of the current refresh token, sliding `expires_at`, user
agent, ip, `last_used_at`, and `impersonated_by: Option<Id<Admin>>` on impersonation sessions, whose expiry is
fixed), `_used_refresh_tokens`
(rotated refresh token hashes, for reuse detection; deleted with their session), `_verification_codes` (keyed hashes
of a 6-digit code and a link token, purpose, expiry, attempts), `_admins` (email, argon2id hash). The auth code
reads and writes them directly through `QueryDb` / `MutationDb` (not through registered functions), so hashes never
appear in function args or the execution log.

**System functions**: `_system/auth:me` (query: the signed-in `Doc<User>` or null) and `_system/auth:update_profile`
(mutation: `{ name?, image? }`; omitted = unchanged, `""` = clear). `@ferrix/client` exports hand-written references
as `systemApi.auth.me` / `systemApi.auth.updateProfile`. The admin-only dashboard functions are listed in §4.1;
`FerrixAuth.setupAdmin({ token, email, password })` completes the printed setup link.

### 1.6 File storage

```rust
use ferrix::storage::{StorageConfig, StoredFile};

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

fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new().storage(StorageConfig::s3("my-bucket", "uploads").max_upload_size(20 << 20).private(true)).run()
}
```

**The `_storage` table** (`ferrix::storage::StoredFile`, defined with `#[table]`; TS `Id<"_storage">`, declared in
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
    pub async fn get_metadata(&self, id: Id<StoredFile>) -> Result<Option<Doc<StoredFile>>>;
}
impl MutationStorage {   // the same reads (get_signed_url: exactly ttl from `now()`), in the mutation's transaction, plus:
    pub async fn generate_upload_url(&self) -> Result<String>;   // single-use, signed, upload_url_ttl (1h)
    pub async fn delete(&self, id: Id<StoredFile>) -> Result<()>; // transactional; UserError "not_found" if missing
}
impl ActionStorage {     // generate_upload_url, get_url, get_signed_url (exact), get_metadata (latest snapshot),
                         // delete (own transaction), plus:
    pub async fn store(&self, bytes: impl Into<Bytes>, content_type: Option<&str>) -> Result<Id<StoredFile>>;
    pub async fn get(&self, id: Id<StoredFile>) -> Result<Option<Bytes>>;   // None if no such file
}
```
- **URLs.** `get_url` returns `<public url>/api/storage/<id>`. The base is `StorageConfig::public_url`, else the auth
  public URL (`--public-url`, which `serve` defaults to `http://<--http address>`), else nothing (a root-relative
  `/api/storage/<id>`, e.g. in `TestApp`). **In `--dev`**, unless a public URL is set explicitly (`--public-url` /
  `FERRIX_PUBLIC_URL`, `AuthConfig::public_url`, a TLS domain, or `StorageConfig::public_url`), file, signed and
  upload URLs are root-relative, so an app served by a dev server that proxies `/api` (Vite) loads and uploads files
  through it, same-origin. (`StorageConfig::public_url("")` asks for root-relative URLs explicitly.) The id carries 96 random bits, so the URL is an **unguessable capability
  URL**: whoever has it can read the file, nobody can enumerate files. It never changes, which keeps
  `get_url` deterministic inside queries. In private mode `get_url` fails with the user error `storage_private`.
  Upload URLs are `<base>/api/storage/upload?token=<token>`, token = `base64url(json {n: nonce, e: exp_s, b?:
  created_by}) "." base64url(HMAC-SHA256)`, keyed by a key derived from `auth_secret.key` (BLAKE3 KDF, context
  `"ferrix storage v1 upload url HMAC key"`), so restarting keeps URLs valid and deleting the secret voids them.
- **Signed URLs** (`get_signed_url(id, ttl)`): `<base>/api/storage/<id>?exp=<unix seconds>&sig=<base64url
  HMAC-SHA256(id "\n" exp)>`, keyed by `blake3::derive_key("ferrix storage v1 signed file url HMAC key", storage
  key)`. The server checks them in constant time against its clock (`Database::clock`); `thumb`, `fit` and
  `download` may be appended (the signature covers the file and the expiry, not the variant). `ttl` is 1 s to
  `max_signed_url_ttl` (7 days), else the user error `invalid_ttl`; `None` if the file doesn't exist.
  **Determinism:** in mutations and actions the expiry is exactly `now + ttl`. In a **query** it is the snapshot
  time plus `ttl` **rounded up to a multiple of `ttl`**: every run within one `ttl` window (aligned to the epoch)
  returns the same URL, so results stay deterministic per ts and browsers can cache the file; the URL is valid for
  between `ttl` and `2·ttl`. A subscription reruns only when its data changes, not when a URL expires: pick a `ttl`
  longer than the result will be shown, or fetch fresh URLs from a mutation or action.
- **Private mode** (`StorageConfig::private(true)`, `--storage-private` / `FERRIX_STORAGE_PRIVATE`): `GET
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
  `_system/storage:sweep_orphans`, whose job result is the `SweepStats` (`{ scanned, deleted, thumbnails }`). With
  `sweep_interval: None` there is no such cron (`StorageService::sweep(&app)` still works).
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
      pub max_upload_size: u64,                 // 100 MiB; `--max-upload-size 100MB` (FERRIX_MAX_UPLOAD_SIZE)
      pub upload_url_ttl: Duration,             // 1 h
      pub cache_max_age: Duration,              // 30 days (Cache-Control of served files)
      pub public_url: Option<String>,           // see URLs
      pub upload_rate_limit: Option<(u32, u32)>, // (burst, per minute) per client IP; Some((60, 120))
      pub sweep_interval: Option<Duration>,     // Some(6 h): the `_storage_sweep` system cron; None = none
      pub orphan_grace: Duration,               // 1 h
      pub private: bool,                        // false; `--storage-private`
      pub max_signed_url_ttl: Duration,         // 7 days
      pub thumbnail_max_dimension: u32,         // 1024 (W and H in 1..=this) unless:
      pub thumbnail_sizes: Option<Vec<(u32, u32)>>, // None; Some(allowlist) = only these sizes (bounds the cache)
      pub thumbnail_max_source_pixels: u64,     // 40_000_000
      pub thumbnail_max_source_bytes: u64,      // 32 MiB
  }
  // StorageConfig::local(dir), ::s3(bucket, prefix), ::object_store(store, prefix), ::parse(location), plus builder
  // setters (`.private(true)`, `.thumbnail_sizes(&[(64, 64), (256, 256)])`, ...).
  ```
  `--storage <location>` (`FERRIX_STORAGE`): a directory, `file:///dir`, or `s3://bucket[/prefix]`. S3 (feature `s3`,
  default) reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` /
  `AWS_DEFAULT_REGION`, `AWS_ENDPOINT_URL` (R2, MinIO), `AWS_ALLOW_HTTP`, ... (`object_store`'s `from_env`).
  `ferrix::storage::object_store` re-exports the crate, e.g. `object_store::memory::InMemory` for tests. Configure an
  abort-incomplete-multipart-uploads lifecycle rule on S3 buckets.
- `App::storage() -> &Arc<StorageService>`: `object_store()`, `blob_path(id)`, `thumbnail_path(id, variant)`,
  `file_url(id)`, `signed_url(id, expires_s)`, `config()`, `sweep(&app)`.
### 1.7 Scheduler and crons

```rust
use std::time::Duration;
use ferrix::scheduler::ScheduledJob;   // the `_scheduled_jobs` table type

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

fn main() -> ferrix::Result<()> {
    ferrix::Ferrix::new()
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
  `_system/scheduler:schedule { path, args, runAt, maxAttempts, origin }` and `_system/scheduler:cancel { id,
  reason? }`.
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
  `_creationTime`), so jobs due at the same time run in scheduling order. Mutation jobs run one at a time, in that
  order (they serialize on the single writer anyway); actions run concurrently.
- **Concurrency:** at most `SchedulerOptions::concurrency` (10) jobs run at once.
- **Waking:** the background loop sleeps until the next job or cron is due (at most 60s) and wakes as soon as a commit
  in the write log touches `_scheduled_jobs`.

**Crons.** `Ferrix::cron::<F>(name, expr, args)` / `Ferrix::cron_tz::<F>(name, expr, tz, args)` /
`Ferrix::interval::<F>(name, every, args)` (or the same methods on `SchedulerOptions`; `CronSpec::cron` /
`CronSpec::cron_tz` / `CronSpec::interval`, and `CronSpec::timezone(tz)`). Expressions have 6 fields with seconds
(`sec min hour day-of-month month day-of-week`, an optional 7th is the year; `cron` crate syntax), evaluated in UTC or
in the given IANA zone (`chrono-tz`); an interval runs first `every` after startup. `App::open` fails on an invalid
expression (or one with 5 fields), an unknown zone, a zone on an interval, a duplicate or empty name, a name starting
with `_` (reserved for system crons), a zero interval, or args that don't serialize.
- **Time zones and DST.** Matching local times are mapped to instants: one in a **spring-forward gap** runs later by
  the gap's length (`0 30 2 * * *` in `America/New_York` runs at 03:30 EDT that day), so daily jobs never skip a day;
  one in a **fall-back fold** runs once, at its first occurrence, except for expressions whose hour field is `*`,
  which also run in the repeated hour (they follow elapsed time). `_crons.timezone` holds the zone (`null` for UTC;
  `"UTC"` is normalized to `null`).
- **System crons** are declared by ferrix itself, named with a leading `_`: today `_storage_sweep` (§1.6). They are
  ordinary `_crons` rows and jobs (origin `cron:_storage_sweep`); the dashboard's `crons` lists them last with
  `system: true`. Their row is inserted by the startup reconcile, so a fresh app has already committed once (ts 1)
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

**Retention.** Finished jobs (`success`, `failed`, `canceled`) are deleted `retention` (7 days; `None` keeps them)
after they finished, by a built-in cleanup that runs at startup and every `cleanup_interval` (1h).
`App::cleanup_scheduled_jobs()` runs it now.

```rust
#[derive(Debug, Clone)]
pub struct SchedulerOptions {          // AppOptions::scheduler; Ferrix::scheduler(opts) keeps declared crons
    pub concurrency: usize,            // 10
    pub retention: Option<Duration>,   // Some(7 days)
    pub cleanup_interval: Duration,    // 1 hour
    pub retry_backoff: Duration,       // 1 second
    pub cancel_grace: Duration,        // 5 seconds: a canceled action is aborted this long after the cancel
    pub crons: Vec<CronSpec>,
}
impl App {
    pub async fn start_scheduler(&self) -> Result<()>;      // `ferrix serve` calls it; idempotent
    pub async fn run_due_jobs(&self) -> Result<usize>;      // run what is due now, until nothing is (tests)
    pub async fn cleanup_scheduled_jobs(&self) -> Result<usize>;
}
// Database::clock(): Clock { now_ms(), advance(Duration), offset() }, the wall clock plus an offset that only grows.
// Mutations' `now()` and the scheduler read it.
```
**Tests.** A `TestApp` runs the scheduler's startup work (recovery, cron reconcile) but no background loop, so tests
are deterministic: `t.run_due_jobs()` runs what is due; `t.advance_time(d)` moves the clock forward and then runs
what became due; `t.restart()` / `t.restart_with(options)` reopen the data dir (keeping the clock offset) like a
restart; `t.start_scheduler()` runs the background loop (e.g. for tests over a socket).

### 1.8 Search: full-text (FTS5) and vector (sqlite-vec)

```rust
#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel])]            // body: String or Option<String>
#[search_index(search_stems, search = body, tokenizer = "porter", language = "english")]   // "run" finds "running"
#[search_index(search_sub, search = body, tokenizer = "trigram", diacritics = "keep")]     // substrings
#[vector_index(by_embedding, field = embedding, dims = 1536, filter = [channel], kind = "hnsw")]  // Vec<f32> / Option<Vec<f32>>
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
    pub exact: bool,                        // HNSW indexes: search exactly anyway (default false)
}
impl<T> VectorSearchOpts<T> {
    pub fn new(limit: usize) -> Self; pub fn filter(self, f: VectorFilter<T>) -> Self; pub fn exact(self) -> Self;
}
// TableDef additions: SearchIndexDef { .., tokenizer: Tokenizer }, VectorIndexDef { .., kind: VectorIndexKind }.
pub struct Tokenizer { pub kind: TokenizerKind /* Unicode61 | Porter | Trigram */, pub language: Option<&'static str>,
                       pub remove_diacritics: bool }
impl Tokenizer {   // const fns, used by the #[table] expansion
    pub const fn unicode61() -> Self; pub const fn porter(language: &'static str) -> Self; pub const fn trigram() -> Self;
    pub const fn keep_diacritics(self) -> Self;
}
pub enum VectorIndexKind { Exact /* default */, Hnsw }
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
    each other (the last term is a prefix of the stem). `language` is only accepted with `porter`, and only
    `"english"` (the default; FTS5's stemmer is English-only).
  - `"trigram"`: FTS5 `trigram`: substring search; each term matches anywhere inside a word ("ell" finds "hello"),
    the last term is not a prefix (substrings cover it), and terms shorter than 3 characters are ignored (FTS5
    can't match them), so text whose terms are all shorter matches nothing. Case-insensitive.
  - `diacritics = "remove"` (default) folds diacritics; `"keep"` makes `café` and `cafe` different terms
    (`remove_diacritics 0`).
  Changing an index's tokenizer rebuilds it at startup (same index id, backfilled).
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
- Equality on filter fields compares the memcomparable encoding (§4.3 of PLAN.md), i.e. the same equality as index
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

**Vector search semantics.** Only `ActionCtx` has `vector_search` (a compile error elsewhere). It reads the latest
committed snapshot; results are `(id, score)` with `score` the cosine similarity (higher = more similar), best first.
Errors: a query vector whose length isn't the index's `dims` (or containing NaN/inf), `limit` outside `1..=256`, a
filter on a field that isn't a filter field, more than 64 `or` branches. Writing a document whose vector field has the
wrong length (or isn't an array of numbers) fails before anything is written, unless the write leaves that field
unchanged (a vector made unindexable by a change of `dims` stays unindexed, and the document stays editable).
Documents whose vector field is missing or null are not indexed. Results are `(id, score)` tuples, which can be
returned as is (TS `Array<[Id<"x">, number]>`), though a named `#[object]` struct reads better in the client.
- **`kind = "exact"`** (default): sqlite-vec's `vec0` brute-force scan; exact (checked against brute force). About
  18 ms per k=10 query at 50k × 384, 75–120 ms at 200k × 384.
- **`kind = "hnsw"`** (`db::search::hnsw`, `db::search::ann`): an in-memory HNSW graph (M = 16, ef_construction =
  128, search width `ef = max(128, k')`, cosine) on top of the same `vec0` table, which stays the source of truth.
  Approximate: recall@10 ≈ 0.95 at 200k × 384 for embedding-like data (low intrinsic dimension), ~0.4 ms per query;
  uniformly random high-dimensional vectors (no structure) are the known worst case. `opts.exact()` forces the exact
  scan. While the graph isn't ready (building at startup, or failed), searches are exact.
  - *Consistency*: every write changing an HNSW entry adds `(index id, doc id)` to its commit entry; the graph is
    updated after commit from the write log (a background thread follows commits, and every search first catches
    the graph up to its own snapshot), so a search sees every commit up to its snapshot (or later). If the log no
    longer covers the gap (5 min / 100k commits), the whole `vec0` table is diffed instead. Rolled-back writes never
    reach the log.
  - *Deletes*: a deleted or changed document's node becomes a tombstone (still routing, never returned); when
    tombstones exceed 25% of the nodes (and 256) the graph is rebuilt from the live nodes on all cores in the
    background, with changes made meanwhile journaled and replayed before the swap.
  - *Filters*: post-filtering with oversampling. Each node keeps its filter values and each `(field, value)` has a
    posting set; if at most 4096 live documents match the filter, their distances are computed exactly; otherwise
    the graph is asked for `k' = k · live / matching · 1.25` candidates, filtered, and retried with `4k'` (up to
    three times, then the exact computation) if fewer than `k` remain.
  - *Persistence*: indexes with ≥ 10 000 live vectors are saved to `<data dir>/vector_index_<id>.hnsw` (graph,
    vectors, per-node doc id, filter values and vector hash; blake3 checksum; written to a temporary file and renamed)
    after the build, after rebuilds and every `max(1000, live/10)` changes. At startup a file with a valid checksum,
    format and definition is loaded and **validated against the table**: the whole `vec0` table is scanned at one
    snapshot and every difference (changed vector or filter values, missing or extra documents) is applied. A missing,
    stale or corrupt file only costs a rebuild; smaller indexes are rebuilt at every startup. Files of indexes that no
    longer exist (or became exact) are deleted.
  - Benchmarks (`search::perf_hnsw`, release, 16 threads): building 200k × 384 takes 9–25 s; saving the 330 MB
    file ~0.2 s, loading it ~0.2 s.

**Storage and consistency (search).** Each search index is an FTS5 table (`_search_<n>`: the text plus the filter fields as
`UNINDEXED` columns, each the hex of its memcomparable encoding, and `_search_<n>_docs` mapping FTS rowids to doc
ids); each vector index is a `vec0` table (`_vector_<n>`, cosine distance, filter fields as metadata columns). They
are listed in `_search_indexes` and updated inside the mutation's write transaction by both the typed and untyped
write paths, so search results are always consistent with document reads and roll back with the mutation.
`App::open` reconciles them like ordinary indexes: creates and backfills new ones, rebuilds (drop, create, backfill)
indexes whose definition changed (fields, dimensions, tokenizer), and drops removed ones (a backfill skips documents
whose vector has the wrong length, with a warning). Switching a vector index between `exact` and `hnsw` only updates
its stored definition (the `vec0` table is kept). Stored definitions from before tokenizers existed read as the
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
changes to documents it returned), never by writes in other channels. This is exact because the relevance of a
document depends only on that document (see Relevance): documents outside the filter can neither enter the result nor
reorder it. Encoded values are self-delimiting, so one value's prefix never contains another's keys, and the
subscription manager's interval index stabs each written key like any index key. No change to `ReadSet`/`CommitEntry`
was needed. Vector indexes are not reactive (actions only); HNSW indexes write `(index id, doc id)` for the graph's
catch-up.

### 1.9 Migrations

```rust
use ferrix::prelude::*;          // migration, DocChange
use serde_json::Value;

/// Old todos have no priority. Untyped: the document is the stored JSON (system fields included), so it works
/// however far the Rust type has moved on.
#[migration(table = Todo, batch = 100)]          // table = a #[table] type or "name"; batch 1..=10000 (default 100)
async fn add_priority(ctx: &MutationCtx, doc: Value) -> Result<Option<Value>> {
    Ok(doc.get("priority").is_none().then(|| json!({ "priority": 0 })))   // Some(patch) or None (keep)
}

/// Typed, when the old shape still deserializes; a stable name set explicitly.
#[migration(table = "todos", name = "0002_split_owner")]
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
}
impl App {
    pub async fn run_migration<M: MigrationRef>(&self, opts: RunOptions) -> Result<MigrationReport>;
    pub async fn run_migration_named(&self, name: &str, opts: RunOptions) -> Result<MigrationReport>;
    pub async fn run_pending_migrations(&self, opts: RunOptions) -> Result<Vec<MigrationReport>>; // name order; stops at a failure
    pub async fn migration_summaries(&self) -> Result<Vec<MigrationSummary>>;                    // what `list` returns
}
pub struct MigrationReport {   // camelCase JSON
    pub name: String, pub table: String,
    pub outcome: RunOutcome,   // completed | failed | canceled | alreadyDone | alreadyRunning | stopped | dryRun
    pub batches: u64, pub processed: u64, pub changed: u64,   // this run only
    pub error: Option<String>, pub failed_document: Option<String>,
    pub schema_check: Option<String>,   // "passed" or "failed: <report>" when it ended the table's migrating state
    pub examples: Vec<ChangeExample>,   // dry runs: up to 5 { id, before, after (null = deleted) }
}
// ferrix::migrations::registry(): every #[migration] by name (validated once: unique names of 1..=128 characters out
// of `A-Za-z0-9_-.:/`, batch sizes); App::open also fails if a migration's table is not defined in code.
```

- **Names** are stable ids: `name = "..."` or, by default, the function path (`todos:add_priority`, like functions).
  Moving or renaming the function changes a default name, which makes it a new migration: set `name` for
  migrations that may move. Migrations run in **name order** (`ferrix migrate`, `serve --migrate`, `run` without a
  name), stopping at the first that fails or is canceled.
- **Batches.** A run first marks the migration `running` (creating its `_migrations` row). Each batch is one mutation
  transaction, the internal system mutation `_system/migrations:run_batch` (so it gets the mutation timeout, panic
  handling and an execution log entry): it reads the cursor from the row, pages through the table's
  `by_creation_time` index (`query_table(..).paginate`, `batch` documents after the cursor), calls the migration on
  each document, applies the change with `patch_raw` / `replace_raw` / `delete_raw` (the normal write path: indexes,
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
  still fail (`document does not match table`), so run the migrations promptly (`serve --migrate` runs them right
  after startup) or keep the type backward compatible. In the transaction where the table's **last** pending
  migration finishes, every document of the table is validated: if all match, the fingerprint is recorded and the
  table leaves the migrating state (`schemaCheck: "passed"`); otherwise the report is stored in `schemaCheck` and
  logged as an error, the table stays migrating until the process stops, and the next start refuses to open (unless
  a new migration for the table is pending). Invalid documents never survive a restart unnoticed. With
  `--skip-schema-check` nothing is migrating, and migration writes are validated strictly.
- **Running.** `ferrix migrate [--name NAME] [--dry-run] [--restart --name NAME] [--dir DIR]` runs every migration
  that isn't done (or one) and prints a report per migration (exit code 1 if one failed); `ferrix migrate status`
  lists them with their state and progress. Run the CLI while the server is stopped (it is a separate process: a
  running server would not see its writes in its subscriptions); for a running server use the dashboard functions
  (§4.1.3) or start it with `serve --migrate` (`FERRIX_MIGRATE`), which runs pending migrations in the background
  after startup. Without `--migrate`, `serve` logs a warning listing the migrations that aren't done.

---

### 1.10 Semantics to keep in mind

- **Optimistic updates are replayed.** The client re-runs an optimistic update on top of every new server result until
  its mutation's result has arrived. The update must be a pure function of the local store and the
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
- **Invalidation is by index range, so writes are coarse.** Any write to a document (even one that leaves it
  unchanged) invalidates every query that read it through any index, and an insert invalidates every query whose
  range covers its keys. Tips: skip no-op writes (check first; `mark_read` only patches when something is new, since
  each membership write reruns the channel header for all its viewers); keep frequently changing fields (presence,
  typing, counters, `lastSeen`) in their own small table rather than on a widely read document; read narrow ranges
  (an `eq` prefix plus a `_creationTime` bound, `take(n)`: a limited scan records only what it scanned); prefer
  `Query::count()` (no documents read) to collecting a range just to count it.

---

## 2. `::ferrix::__private` items (used ONLY by macro output)

```rust
pub use ::linkme;           // macro output uses #[linkme(crate = ::ferrix::__private::linkme)]
pub use ::serde;            // #[serde(crate = "::ferrix::__private::serde")]
pub use ::serde_json;
pub use ::ts_rs;            // #[ts(crate = "::ferrix::__private::ts_rs")]
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
#[linkme::distributed_slice] pub static MIGRATIONS: [MigrationDef];
pub type MigrationHandler = for<'a> fn(&'a MutationCtx, serde_json::Value) -> BoxFuture<'a, Result<DocChange>>;
pub struct MigrationDef {
    pub module_path: &'static str, pub fn_name: &'static str,
    pub name: Option<&'static str>,   // #[migration(name = "...")]
    pub table: &'static str,          // <T as Table>::NAME or the string given
    pub batch_size: usize,
    pub handler: MigrationHandler,
}
impl MigrationDef { pub fn name(&self) -> String; }   // `name`, else function_path(module_path, fn_name)
/// Deserialize the document for the migration's parameter type (identity for `Value`).
pub fn decode_migration_doc<T: DeserializeOwned>(doc: serde_json::Value) -> Result<T>;
pub fn migration_output<O: MigrationOutput>(out: O) -> Result<DocChange>;
/// Field type checks for #[search_index] / #[vector_index] (`#[diagnostic::on_unimplemented]` explains failures).
pub trait SearchFieldType {}   // String, Option<String>
pub trait VectorFieldType {}   // Vec<f32>, Option<Vec<f32>>
pub const fn assert_search_field<T: SearchFieldType + ?Sized>();
pub const fn assert_vector_field<T: VectorFieldType + ?Sized>();
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
#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize,
         ::ferrix::__private::ts_rs::TS, ::core::clone::Clone, ::core::fmt::Debug)]
#[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct Todo { pub text: String, pub done: bool, pub created_by: Option<String> }

impl ::ferrix::Table for Todo {
    const NAME: &'static str = "todos";
    fn table_def() -> ::ferrix::TableDef {
        ::ferrix::TableDef { name: "todos", indexes: vec![
            ::ferrix::IndexDef { name: "by_done", fields: vec!["done", "createdBy"] },
        ], ts: || ::ferrix::__private::ts_inline::<Todo>(),
        validate: ::ferrix::__private::validate_doc::<Todo> }
    }
}

#[allow(non_upper_case_globals)]
impl Todo {
    pub const text: ::ferrix::Field<Todo, String> = ::ferrix::Field::new("text");
    pub const done: ::ferrix::Field<Todo, bool> = ::ferrix::Field::new("done");
    pub const created_by: ::ferrix::Field<Todo, Option<String>> = ::ferrix::Field::new("createdBy");
    pub const _id: ::ferrix::Field<Todo, ::ferrix::Id<Todo>> = ::ferrix::Field::new("_id");
    pub const _creation_time: ::ferrix::Field<Todo, f64> = ::ferrix::Field::new("_creationTime");
    pub const by_done: ::ferrix::Index<Todo> = ::ferrix::Index::new("by_done");
    pub const by_creation_time: ::ferrix::Index<Todo> = ::ferrix::Index::new("by_creation_time");
}

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::TABLES)]
#[linkme(crate = ::ferrix::__private::linkme)]
static __FERRIX_TABLE_Todo: fn() -> ::ferrix::TableDef = <Todo as ::ferrix::Table>::table_def;
```
Compile errors (with spans) for: tuple or unit structs, enums, generics, an index naming an unknown field, a duplicate
index name, an index named `by_id` or `by_creation_time`, and an existing `#[serde(rename_all)]` other than camelCase.
Per-field `#[serde(rename = "x")]` is honored: the handle and IndexDef use "x".

**Search and vector indexes.** `#[search_index(name, search = field, filter = [f, ...], tokenizer = "...", language =
"...", diacritics = "...")]` and `#[vector_index(name, field = f, dims = N, filter = [f, ...], kind = "...")]` (both
consumed; `filter`, `tokenizer`, `language`, `diacritics` and `kind` optional) add, for
```rust
#[table(name = "messages")]
#[search_index(search_body, search = body, filter = [channel])]
#[search_index(search_stems, search = body, tokenizer = "porter", language = "english", diacritics = "keep")]
#[vector_index(by_embedding, field = embedding, dims = 1536, kind = "hnsw")]
pub struct Message { pub channel: String, pub body: String, pub embedding: Option<Vec<f32>> }
```
these items to the expansion above (the `TableDef` literal always has both fields, `vec![]` when unused):
```rust
::ferrix::TableDef { name: "messages", indexes: vec![],
    search_indexes: vec![
        ::ferrix::SearchIndexDef { name: "search_body", search_field: "body", filter_fields: vec!["channel"],
                                   tokenizer: ::ferrix::Tokenizer::unicode61() },
        ::ferrix::SearchIndexDef { name: "search_stems", search_field: "body", filter_fields: vec![],
                                   tokenizer: ::ferrix::Tokenizer::porter("english").keep_diacritics() }],
    vector_indexes: vec![::ferrix::VectorIndexDef { name: "by_embedding", vector_field: "embedding", dimensions: 1536usize,
                                                     filter_fields: vec![], kind: ::ferrix::VectorIndexKind::Hnsw }],
    ts: …, validate: … }
// tokenizer: unicode61() (default or "unicode61"), porter("<language>") (language defaults to "english"), trigram();
// `.keep_diacritics()` is appended for diacritics = "keep". kind: Exact (default or "exact") or Hnsw.
// in `impl Message { … }`, after by_creation_time:
pub const search_body: ::ferrix::SearchIndex<Message> = ::ferrix::SearchIndex::new("search_body");
pub const by_embedding: ::ferrix::VectorIndex<Message> = ::ferrix::VectorIndex::new("by_embedding");
// one check per index, spanned at the field's type (with the struct's #[cfg]s):
const _: () = ::ferrix::__private::assert_search_field::<String>();
const _: () = ::ferrix::__private::assert_vector_field::<Option<Vec<f32>>>();
```
Compile errors (with spans) for: a missing or unknown `search`/`field`, unknown or duplicate filter fields (or the
searched field itself), system fields, a missing `dims` or one outside `1..=8192`, more than 16 filter fields,
unknown or duplicate arguments, a name equal to another index (of any kind), to a field handle, or to a reserved name
(`by_id`, `by_creation_time`, `_id`, `_creation_time`), and a field type that is visibly wrong (`u32` as a search
field, `Vec<f64>` as a vector field). Types behind aliases are caught by the `assert_*_field` check instead. Also
(spanned at the string literal): an unknown `tokenizer` (not `"unicode61"`, `"porter"`, `"trigram"`), `diacritics`
(not `"remove"`, `"keep"`) or `kind` (not `"exact"`, `"hnsw"`), a `language` other than `"english"`, a `language`
without `tokenizer = "porter"`, a non-string value, and `tokenizer`/`language`/`diacritics` on a vector index or
`kind` on a search index (unknown arguments). `TableDef`s built by hand are checked at startup the same way.

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
#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize, ::ferrix::__private::ts_rs::TS)]
#[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", optional_fields = nullable)]
pub struct __ferrix_args_list { pub done: Option<bool>, pub limit: u32 }

impl ::core::convert::From<(Option<bool>, u32,)> for __ferrix_args_list {
    fn from((done, limit,): (Option<bool>, u32,)) -> Self { Self { done, limit } }
}

/// Function reference for [`list`](fn@list).                    (type namespace only; same visibility as the fn)
#[allow(non_camel_case_types, dead_code)]
pub struct list {}

impl ::ferrix::FunctionRef for list {
    type Args = __ferrix_args_list;
    type Output = Vec<Doc<Todo>>;
    fn path() -> String { ::ferrix::__private::function_path(module_path!(), "list") }
}
impl ::ferrix::QueryFunction for list {}
// (a mutation or action also gets `impl ::ferrix::SchedulableFunction for <name> {}` here)

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::FUNCTIONS)]
#[linkme(crate = ::ferrix::__private::linkme)]
static __FERRIX_FN_list: ::ferrix::__private::FunctionDef = ::ferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "list",
    visibility: ::ferrix::__private::Visibility::Public,     // Internal for internal_*
    dev_only: false,                                         // true for #[query(dev_only)]
    handler: ::ferrix::__private::Handler::Query(|ctx, args| ::std::boxed::Box::pin(async move {
        let a: __ferrix_args_list = ::ferrix::__private::decode_args("list", args)?;
        let out = list(&ctx, a.done, a.limit).await?;            // value namespace => the user's fn
        ::ferrix::__private::encode_output(out)
    })),
    ts: || (::ferrix::__private::ts_inline::<__ferrix_args_list>(),
            ::ferrix::__private::ts_output::<Vec<Doc<Todo>>>()),
};
```
- The only attribute argument is `dev_only` (`#[query(dev_only)]`); anything else, a value (`dev_only = true`) or a
  repeat is a compile error.
- `Output` is the `T` in `Result<T>`. The macro must handle `Result<T>`, `ferrix::Result<T>` and
  `anyhow::Result<T>`: it takes the first generic argument of the last path segment named `Result`. Anything else is
  a compile error.
- `optional_fields = nullable` makes `Option<T>` args optional in TS (`done?: boolean | null`); serde already treats a missing `Option` field as `None`.
- Zero params: `pub struct __ferrix_args_list {}` and `impl From<()>`.
- Compile errors (with spans) for: a non-async fn, a missing ctx param, a ctx type that doesn't match the macro kind
  (match on the last path segment: `QueryCtx` / `MutationCtx` / `ActionCtx`), `self` params, non-identifier
  patterns, generics, and a missing return type.
- Mutations use `Handler::Mutation` + `impl MutationFunction`; actions use `Handler::Action` + `impl ActionFunction`.
  Both also get `impl ::ferrix::SchedulableFunction for <name> {}` (same `#[cfg]`s), right after that impl; queries
  don't.

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
impl ::ferrix::migrations::MigrationRef for add_priority {
    fn name() -> ::std::string::String { ::ferrix::__private::MigrationDef::name(&__FERRIX_MIGRATION_add_priority) }
    fn table() -> &'static str { __FERRIX_MIGRATION_add_priority.table }
}

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::MIGRATIONS)]
#[linkme(crate = ::ferrix::__private::linkme)]
#[allow(non_upper_case_globals)]
static __FERRIX_MIGRATION_add_priority: ::ferrix::__private::MigrationDef = ::ferrix::__private::MigrationDef {
    module_path: ::core::module_path!(),
    fn_name: "add_priority",
    name: ::core::option::Option::None,                      // Some("...") with `name = "..."`
    table: <Todo as ::ferrix::Table>::NAME,                  // `table = "todos"` → "todos"
    batch_size: 100usize,
    handler: {
        fn __ferrix_migrate<'a>(__ferrix_ctx: &'a ::ferrix::MutationCtx, __ferrix_doc: ::ferrix::__private::serde_json::Value)
            -> ::ferrix::__private::BoxFuture<'a, ::ferrix::Result<::ferrix::migrations::DocChange>> {
            ::std::boxed::Box::pin(async move {
                let __ferrix_doc: serde_json::Value = ::ferrix::__private::decode_migration_doc::<serde_json::Value>(__ferrix_doc)?;
                let __ferrix_out: Option<serde_json::Value> = add_priority(__ferrix_ctx, __ferrix_doc).await?;
                ::ferrix::__private::migration_output(__ferrix_out)
            })
        }
        __ferrix_migrate
    },
};
```
- Arguments: `table` (required; a type path, checked to implement `Table`, or a string literal), `batch` (an integer
  literal in `1..=10000`, default 100), `name` (a string literal of 1..=128 characters out of `A-Za-z0-9_-.:/`).
- The fn takes exactly `(ctx: &MutationCtx, doc: T)` with `T: DeserializeOwned` (`serde_json::Value`, `Doc<Old>`,
  ...) and returns `Result<O>` (any `...::Result`) with `O: MigrationOutput`; an unsupported `O` is reported by
  `#[diagnostic::on_unimplemented]`.
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
#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize, ::ferrix::__private::ts_rs::TS)]
#[serde(crate = "::ferrix::__private::serde")]
#[ts(crate = "::ferrix::__private::ts_rs")]
#[serde(rename_all = "camelCase")]          // these two only when the item has no `rename_all` of its own
#[ts(rename_all = "camelCase")]
/// A message as the app shows it.
#[derive(Clone, Debug)]
pub struct MessageView { #[serde(flatten)] pub message: Doc<Message>, pub author_name: String }
```
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

ferrix ships some tables and functions of its own (auth, dashboard, bookkeeping). They are defined **inside the
`ferrix` crate with the same macros** (`extern crate self as ferrix;` makes `::ferrix::` paths resolve there).

- **System tables** have names starting with `_` (e.g. `_users`, `_sessions`). `#[table]` rejects `_`-prefixed names
  in any crate other than `ferrix`, checked via `CARGO_CRATE_NAME` at expansion time. Codegen leaves system tables out
  of `DataModel`. The few that app tables may reference (`codegen::REFERENCEABLE_SYSTEM_TABLES`: `_users`, `_storage`
  and `_scheduled_jobs`, whose TS types inline their enums so no extra named types are declared)
  are declared in a separate `SystemDataModel`, and the generated `Id<T>` / `Doc<T>` accept
  `TableNames | keyof SystemDataModel`.
- **System functions** get paths under `_system/` (`function_path` maps crate `ferrix` to `_system`), e.g.
  `ferrix::auth::api::sign_in` → `"_system/auth/api:sign_in"`. Use `__private::is_system_path(path)` to test for
  them. Codegen leaves them out of the app's `api`. Who may call a public system function from a client (anyone, a
  signed-in user, or only an admin) is decided by the function itself (e.g. `ctx.auth.require_admin()`), not by the
  path.

### 4.1 Dashboard system functions

`crates/ferrix/src/dashboard` defines the admin dashboard's backend with the same macros. Every function starts with
`ctx.auth.require_admin()`: anonymous callers get the user error `unauthenticated`, signed-in users `forbidden`. They
are called over the normal sync protocol (or the HTTP function API), so every dashboard view is a live query.
`@ferrix/dashboard` holds hand-written references (`packages/dashboard/src/lib/api.ts`).

| Path `_system/dashboard:…` | Kind | Args → result |
|---|---|---|
| `tables` | query | `{}` → `TableSummary[]`: `{ name, number, system, defined, documents, indexes: [{ name, fields, builtin, usable }], searchIndexes: [{ name, kind: "search" \| "vector", field, filterFields, dimensions, tokenizer: "unicode61" \| "porter" \| "trigram" \| null, removeDiacritics, method: "exact" \| "hnsw" \| null }], ts, secretFields, access: "readWrite" \| "limited" \| "readOnly" }`, app tables first. Reruns on any write (it counts every table). |
| `documents` | query | `{ table, index?, filters?: [{ field, op: "eq"\|"gt"\|"gte"\|"lt"\|"lte", value }], order?: "asc"\|"desc", paginationOpts }` → `PaginationResult<object>`. Untyped `query_table(..).with_index(..).paginate(..)`: same cursors and read sets as typed queries, so pages update live (`usePaginatedQuery` works). Filters follow the `with_index` rules. Errors: `unknown_table`, `invalid_query`, `index_not_allowed`. |
| `search_documents` | query | `{ table, index, text, filters?: [{ field, value }], paginationOpts }` → `PaginationResult<object>`, most relevant first: untyped `search_table(table, index).search(<its search field>, text).eq(field, value)….paginate(..)` (§1.8: same matching, cursors and per-filter-value read sets, so pages update live and `usePaginatedQuery` works). For the Data page's search box; `tables` lists each table's `searchIndexes` (`kind: "search"`, `field`, `filterFields`, `tokenizer`, `removeDiacritics`). Errors: `unknown_table`, `invalid_query` (no such search index, a vector index, a field that is not a filter field, a bad cursor), `index_not_allowed` (an index over a secret field). |
| `document` | query | `{ id }` → `{ table, document } \| null` (any table; the table number is in the id). `invalid_id`. |
| `insert_document` | mutation | `{ table, document }` → new id. System fields in `document` are ignored. |
| `patch_document` | mutation | `{ id, patch }` → `null`. Shallow merge, validated. |
| `replace_document` | mutation | `{ id, document }` → `null`. System fields are ignored (`_id`, if given, must match). |
| `delete_documents` | mutation | `{ ids }` → count, in one transaction. |
| `functions` | query | `{}` → `[{ path, kind, visibility, system, argsTs, outputTs }]`, internal and system functions included. |
| `run_function` | action | `{ path, args?, asUser? }` → `{ kind, success, value, error: { message, code, data } \| null, logLines, durationMs, ts, identity }`. Runs any function (internal and system included) as the calling admin, or as `asUser` through a real session of that user (5 minutes, `impersonatedBy` = the admin, audited as `impersonate`, deleted when the run ends), so `ctx.auth.session_id()` works and queries are scoped exactly as for the user. The user must exist (`not_found`). Every run is audited as `run_function` (`details`: path, kind, asUser, sessionId, args with secrets redacted, outcome): a mutation's entry is written in the mutation's own transaction (`outcome: "success"`), a failed mutation gets its own entry (`"error"`), queries and actions are recorded before they run (`"started"`). A failing function is a result with `success: false`, not an error. |
| `users` | query | `{ search?, paginationOpts }` → `PaginationResult<{ _id, _creationTime, email, emailVerified, name, image, providers, hasPassword, sessions }>`; newest first, or by email prefix with `search`. |
| `user_sessions` | query | `{ user }` → `[{ _id, _creationTime, expiresAt, lastUsedAt, userAgent, ip, expired, impersonatedBy }]` (no token hashes; `impersonatedBy` is the admin id of an impersonation session, else null). |
| `revoke_session` | mutation | `{ session }` → `null`. Deletes the session and its rotated refresh tokens; its sockets get `AuthError` at once (live revocation). |
| `delete_user` | mutation | `{ user }` → `{ sessions, accounts, credentials, verificationCodes }`: the user and everything auth keeps for them, in one transaction. App documents referencing the user are left alone. |
| `me` | query | `{}` → `{ _id, email }` of the signed-in admin. |

Write errors (schema validation, unknown table, missing document) are user errors with codes `invalid_document` /
`not_found`, carrying the engine's message (e.g. ``inserted document … does not match table `todos`; nothing was
written: …``).

**Audited.** `insert_document`, `patch_document`, `replace_document`, `delete_documents` (one entry per document),
`revoke_session` and `delete_user` append an `_audit_log` entry in their own transaction (§4.1.5), so an entry exists
exactly for the writes that committed.

**Secrets.** In every `_`-prefixed table, a field whose name ends in `hash` or `encrypted` (case-insensitive:
`passwordHash`, `refreshTokenHash`, `tokenHash`, `codeHash`, `_settings.encrypted`) is replaced by `"••••"` in
everything the dashboard returns (audit log diffs included), and indexes
over such a field are `usable: false` and refused by `documents` (a range filter on them would let an admin
binary-search a hash).

**System tables are read-only** through the generic writes (`read_only_table`), with one exception: in `_users`,
`name`, `image` and `emailVerified` can be changed through `patch_document` / `replace_document` (any other changed
field: `read_only_field`; `image` must be an http(s) URL). Users and sessions are removed with `delete_user` and
`revoke_session`, which keep the auth invariants (no orphaned credentials, accounts, or rotated-token rows).

### 4.1.1 Files page functions

`crates/ferrix/src/storage/mod.rs` defines the Files page backend, admin-only like §4.1 (`unauthenticated` /
`forbidden`). References: `storage` in `packages/dashboard/src/lib/api.ts`.

| Path `_system/storage:…` | Kind | Args → result |
|---|---|---|
| `files` | query | `{ paginationOpts }` → `PaginationResult<FileSummary>`, newest first, live. `FileSummary` = `{ _id, _creationTime, sha256, size, contentType, createdBy, filename, url, downloadUrl }`. In private mode `url` is a signed URL (valid one to two hours, stable within the hour) that already has a query string: append variants with `&` (`&thumb=96x96&fit=cover` for a preview), or use `downloadUrl` (`download=1` appended correctly). |
| `file` | query | `{ id }` → `FileSummary \| null`. `invalid_id` for a malformed id. |
| `delete_file` | mutation | `{ id }` → `null` (`ctx.storage.delete`). `not_found` if missing. |
| `generate_upload_url` | mutation | `{}` → an upload URL (the dashboard's upload button; `createdBy` = the admin). |
| `sweep_orphans` | internal action | `{}` → `{ scanned, deleted, thumbnails }`: the orphan sweep, run by the system cron `_storage_sweep` (run it now with `_system/scheduler:run_cron_now { name: "_storage_sweep" }`). |

Thumbnails for previews: `<url>?thumb=WxH&fit=cover` (or `&thumb=…` on a signed URL), see docs/PROTOCOL.md.
### 4.1.2 Scheduler tables and system functions

Defined in `crates/ferrix/src/scheduler` with the normal macros (§1.7).

**`_scheduled_jobs`** (`ScheduledJob`): `{ path, kind: "mutation" | "action", args, runAt, state: "pending" |
"inProgress" | "success" | "failed" | "canceled", attempts, maxAttempts, result: string | null (the return value as
JSON text), error: string | null (both cut to 4 KiB), scheduledAt, startedAt, completedAt, origin: string | null (the
scheduling function's path, "cron:<name>" or "dashboard") }`, with indexes `by_state_run_at = [state, runAt,
scheduledAt]` and `by_completed_at = [completedAt]`. Mutation jobs never show `inProgress`.

**`_crons`** (`Cron`): `{ name, schedule (the expression, or "every 5m"), timezone (IANA name, null for UTC), path,
args, lastRun, nextRun, lastJob }`, indexes `by_name` and `by_next_run`. Names starting with `_` are system crons.

| Path `_system/scheduler:…` | Kind | Args → result |
|---|---|---|
| `schedule` | internal mutation | `{ path, args, runAt, maxAttempts, origin? }` → job id. `ctx.scheduler` in actions. |
| `cancel` | internal mutation | `{ id, reason? }` → `"canceled" \| "alreadyFinished"`. `ctx.scheduler.cancel` in actions. |
| `jobs` | query (admin) | `{ state?, paginationOpts }` → `PaginationResult<Doc<"_scheduled_jobs">>`. With `state`: that state through `by_state_run_at`, pending/in-progress soonest first, finished ones most recent first; without: newest first. Live. |
| `cancel_job` | mutation (admin) | `{ id }` → `"canceled" \| "alreadyFinished"`. Pending jobs and **actions in progress** (signaled, then aborted after `cancel_grace`; §1.7); `error` = `"Canceled from the dashboard by admin:<id>"` (+ `" (while running)"`, then how the action ended). |
| `run_job_now` | mutation (admin) | `{ id }` → the id of the job that will run: a pending job becomes due now (same id); a finished one is run again as a new job (same path and args, origin `dashboard`). `running` for an action in progress. |
| `crons` | query (admin) | `{}` → `[{ _id, name, schedule, timezone, system, path, args, lastRun, nextRun, lastJob, lastJobState }]`: the app's crons by name, then the system crons (`system: true`, e.g. `_storage_sweep`). Live. |
| `run_cron_now` | mutation (admin) | `{ name, force? }` → the id of the job enqueued now (origin `cron:<name>`), which becomes the cron's `lastRun` / `lastJob` (`nextRun` unchanged). `running` while the cron's previous job is pending or in progress, unless `force: true`; `not_found` for an unknown name. Works for system crons. |

The admin functions start with `ctx.auth.require_admin()` like §4.1; `invalid_id` / `not_found` for bad ids. The
dashboard's references are in `packages/dashboard/src/lib/api.ts` (`scheduler`), its Scheduler page at `/_/scheduler`.

### 4.1.3 Migration table and system functions

Defined in `crates/ferrix/src/migrations` with the normal macros (§1.9).

**`_migrations`** (`MigrationRecord`), one row per migration that has run at least once (a declared migration without
a row is pending): `{ name, table, state: "pending" | "running" | "done" | "failed" | "canceled", cursor: string |
null (a pagination cursor over the table's by_creation_time index), processed, changed, startedAt, finishedAt, error
(cut to 4 KiB), failedDocument, schemaCheck: string | null ("passed" or "failed: <report>") }`, index `by_name`.
Read-only through the dashboard's generic writes; codegen leaves it out.

| Path `_system/migrations:…` | Kind | Args → result |
|---|---|---|
| `list` | query (admin) | `{}` → `MigrationSummary[]`: every declared migration in name (= run) order, then rows of migrations no longer declared. `MigrationSummary` = `{ name, table, declared, batchSize: number \| null, state, processed, changed, documents (the table's current count, for a progress bar), startedAt, finishedAt, error, failedDocument, schemaCheck, tableMigrating }`. Live (reruns with each batch and each write to the migrated tables). |
| `status` | query (admin) | `{ name }` → `MigrationSummary \| null`. |
| `run` | action (admin) | `{ name?, dryRun?, restart? }` → `{ started: string[], reports: MigrationReport[] }`. Without `name`: every migration that isn't done, in name order. Real runs continue in the background (`started`; follow them with `list`); `reports` then holds the ones that didn't start (`alreadyDone`, `alreadyRunning`). Dry runs run to the end and return a report each (`outcome: "dryRun"` or `"failed"`, counts, up to 5 `examples` `{ id, before, after }`). `not_found` for an unknown name. |
| `cancel` | mutation (admin) | `{ name }` → `"canceled"` (it stops after its current batch; the next run resumes) or `"notRunning"`. |
| `start` | internal mutation | `{ name, restart }` → `"started" \| "alreadyRunning" \| "alreadyDone"`. |
| `run_batch` | internal mutation | `{ name, dryRun, cursor? }` → `{ ran, state, processed, changed, done, cursor, schemaCheck, examples }`: one batch (§1.9). |
| `fail` | internal mutation | `{ name, error, document? }` → `null`: marks the migration failed. |

The admin functions start with `ctx.auth.require_admin()` like §4.1.

### 4.1.4 Settings (`_settings`, `_system/settings:*`)

Defined in `crates/ferrix/src/settings`. Admin-only (`unauthenticated` / `forbidden`). They are **actions** (they
need the server's configuration and keys), so the dashboard calls them with `useAction` / `client.action`; there are
no hand-written references in `@ferrix/dashboard` yet.

**`_settings`** (`Setting`): `{ key, value, encrypted: string | null, updatedAt, updatedBy }`, index `by_key`. One row
per key: `oauth`, `smtp`, `redirectUrls`, `signups`, `accessTokenTtlSecs`, `sessionTtlSecs`, `corsOrigins`. `value`
holds the non-secret JSON; `encrypted` the secret part (the OAuth client secrets as a JSON map by provider id, or the
SMTP password), sealed as `v1.<base64url(nonce ‖ ciphertext ‖ tag)>` with XChaCha20-Poly1305 under a key derived
from `auth_secret.key` (BLAKE3 KDF, context `ferrix settings v1 …`), a random 192-bit nonce per write, and the row's
key as associated data. The data browser shows `encrypted` as `"••••"`; generic writes are refused (`read_only_table`).
A row that can't be decrypted (another `auth_secret.key`) is ignored with an error in the log: its providers are off
until the secret is entered again.

| Path `_system/settings:…` | Kind | Args → result |
|---|---|---|
| `get` | action | `{}` → `SettingsView` |
| `update` | action | `{ oauth?, smtp?, redirectUrls?, signups?, accessTokenTtlSecs?, sessionTtlSecs?, corsOrigins?, clear? }` → `SettingsView`. Omitted fields are unchanged; `clear: ["smtp", …]` resets keys to their defaults; `oauth` replaces the list of providers kept in settings. Validated as a whole before anything is written (`invalid_settings` with a message): provider ids `[a-z0-9_-]{1,40}`, unique; `kind` `github` / `google` / `oidc` (the latter needs an https `issuer`, http only for loopback or in `--dev`); a client id; a secret to be enabled; scopes without spaces; SMTP `smtp://` / `smtps://` URL with a host and a valid `from` mailbox (lettre must accept the transport); absolute http(s) redirect URLs; `accessTokenTtlSecs` 30 s–24 h, `sessionTtlSecs` 60 s–400 days; CORS entries that are bare origins (`https://app.example.com`) or `*`; at most 20 providers and 50 list entries. Writes the rows and an `_audit_log` entry (`settings_update`, masked before/after of the changed keys, `details.keys`) in one transaction, then swaps in the new configuration before returning. |
| `send_test_email` | action | `{ to, smtp? }` → `null`. Sends a test email through the mailer in effect, or through the given (unsaved) `SmtpInput` (a missing password means the stored one). `email_not_configured`, or `email_failed` with the transport's error (the password is masked out of it). 30 s timeout. |

```ts
type OAuthKind = "github" | "google" | "oidc";
type SettingsView = {
  oauth: { id: string; kind: OAuthKind; issuer: string | null; clientId: string; clientSecret: "••••" | null;
           enabled: boolean; scopes: string[] | null; locked: boolean }[];    // locked: a provider with this id is configured in code
  smtp: { url: string /* no password */; from: string; password: "••••" | null } | null;
  redirectUrls: string[] | null; signups: boolean | null; accessTokenTtlSecs: number | null;
  sessionTtlSecs: number | null; corsOrigins: string[] | null;               // null: not set in _settings
  locked: string[];      // fields the configuration fixes: "signups" | "accessTokenTtlSecs" | "sessionTtlSecs" | "redirectUrls" | "smtp" | "oauth.<id>"
  effective: { signups: boolean; accessTokenTtlSecs: number; sessionTtlSecs: number; redirectUrls: string[];
               oauthProviders: string[]; emailConfigured: boolean; corsOrigins: string[] | null /* null: any */ };
};
type OAuthProviderInput = { id: string; kind: OAuthKind; issuer?: string | null; clientId: string;
                            clientSecret?: string | null /* omitted, null or "••••": keep; else replace */;
                            enabled: boolean; scopes?: string[] | null };
type SmtpInput = { url: string /* a password in the URL is moved out of it */; from: string;
                   password?: string | null /* omitted, null or "••••": keep; "": none */ };
```

Precedence: §1.5 ("Runtime settings and precedence"); `locked` and `effective` show its result. Secrets are only ever
accepted, never returned, and never logged: the execution log stores system functions' arguments with every
`password` / `secret` / `token` field and every URL password replaced by `"••••"` (`settings::redact_args`).

### 4.1.5 Audit log (`_audit_log`, `_system/audit:list`)

Defined in `crates/ferrix/src/audit.rs`. **`_audit_log`** (`AuditEntry`): `{ admin: string | null, adminEmail,
action, targetTable, targetId, before, after, details, at }` with indexes `by_at = [at]`, `by_admin = [admin, at]`,
`by_action = [action, at]`, `by_table = [targetTable, at]`, `by_target = [targetId, at]`. Actions: `insert`, `patch`,
`replace`, `delete` (target: the document; `before` / `after`: the changed fields only for updates, the whole document
for inserts and deletes), `revoke_session` (`before`: the session, hashes masked), `delete_user` (`before`: the user,
`details`: the counts), `settings_update`, `impersonate` (target: the user; `details`: `purpose` (`run_function` or
`open_app`), `sessionId`, `expiresAt`), `run_function` (`details` as in §4.1). Secrets are masked by the §4.1 rule.

Each entry is written in the same transaction as the write it describes. The table is a system table, so the
dashboard's generic writes refuse it, and no function edits or deletes entries; the only deletion is retention:
entries older than `AuthConfig::audit_retention` (90 days) are swept at startup and hourly (`ferrix::audit::sweep`).

| Path | Kind | Args → result |
|---|---|---|
| `_system/audit:list` | query (admin) | `{ admin?, action?, table?, target?, since?, until?, paginationOpts }` → `PaginationResult<Doc<"_audit_log">>`, newest first, live. The most selective filter picks the index (`target` > `admin` > `action` > `table` > time); the others are applied while scanning; `since` / `until` bound `at` (ms, inclusive). |

### 4.2 Admin HTTP endpoints

The execution log and server stats live outside the reactive database, so they are plain HTTP. Every endpoint
requires `Authorization: Bearer <access token>` with the `adm` claim: 401 `unauthenticated` without a token, 401
`invalid_token` for an invalid, expired or revoked one, 403 `forbidden` for a user's token. Errors have the auth
endpoints' shape; responses carry `Cache-Control: no-store`. The backup, export and import endpoints are defined in
`crates/ferrix/src/ops/http.rs` (see §4.4 for what the archives and files contain).

| Endpoint | Response |
|---|---|
| `GET /api/admin/backups` | `{ target, backups: BackupInfo[] }`, newest first. `target` is where backups are kept (`--backup-to`, default `<data dir>/backups`). `BackupInfo` = `{ name, bytes, createdAt (ms), scheduled (a `ferrix-auto-*` archive from `--backup-every`; only those are rotated), ts (commit ts of the snapshot, or null when unknown) }`. |
| `POST /api/admin/backups` | Body (optional) `{ includeAux?, includeFiles? }` → 201 `BackupInfo` once the archive is stored. Runs while the server serves (writes are not blocked); one backup at a time per process (a second request waits). `includeSecret: true` → 400 `invalid_request` (`auth_secret.key` is only ever included by the CLI). 500 `server_error` with the reason (e.g. S3 credentials) on failure. |
| `GET /api/admin/backups/{name}` | The archive, streamed: `Content-Type: application/zstd`, `Content-Length`, `Content-Disposition: attachment; filename="<name>"`. 404 `not_found`; 400 `invalid_name` for a name that isn't `ferrix-….tar.zst`. |
| `DELETE /api/admin/backups/{name}` | `{ deleted: true }`; 404 `not_found`; 400 `invalid_name`. |
| `GET /api/admin/export/{table}?format=jsonl\|csv` | The table, streamed from one read transaction (a consistent snapshot; never blocks writes), in id order. JSONL (`application/x-ndjson`): one stored document per line. CSV (`text/csv`): header `_id,_creationTime,<other top-level fields, sorted>`, RFC 4180 quoting, CRLF line ends; strings as is, `null`/missing empty, other values as JSON. `Content-Disposition: attachment; filename="<table>.<format>"`. In system tables, secret fields are masked as in §4.1. 404 `not_found` for an unknown table, 400 `invalid_request` for another format. |
| `POST /api/admin/import/{table}?mode=insert\|upsert\|replace` | Body: JSONL (≤ 256 MiB, else 413 `too_large`) → `ImportReport` = `{ tables: [{ table, documents, replaced, deleted }], files, missingFiles, remappedIds }`. App tables only (400 `read_only_table` for `_`-prefixed ones; 404 `not_found` for unknown tables). Every line is parsed and validated before anything is written (400 `import_failed` with up to 20 `<line>: <error>` messages); then batches of 500 documents per transaction through the normal write path, so the dashboard's live views update. `insert` (default) refuses ids that exist; `upsert` replaces them; `replace` deletes every document of the table first. Ids and `_creationTime` are kept; lines without `_id` get a new one. |
| `GET /metrics` | Not under `/api/admin`: Prometheus text (`text/plain; version=0.0.4`), only when the server runs with `--metrics` (404 otherwise); with `--metrics-token`, `Authorization: Bearer <token>` is required (401). Metrics are listed in `crates/ferrix/src/ops/metrics.rs`. |
| `GET /api/admin/logs?<LogFilter>` | `{ entries: LogEntry[] }`, newest first. Query parameters: `pathPrefix`, `kind`, `outcome`, `since`, `limit` (≤ 1000), `beforeId`, `identity`, `excludeSystem`. 400 for malformed ones. |
| `GET /api/admin/logs/stream?<LogFilter>` | `text/event-stream`: `event: ready` once, then `event: log` (`data`: a `LogEntry` as JSON) for each new matching entry, `event: lagged` (`data`: how many entries were skipped because the client fell behind), keep-alive comments every 15s, `event: expired` right before the stream ends when the access token expires (reconnect with a fresh token), and `event: revoked` (`data`: `session has ended`) when it ends because the admin's session ended (sign-out, revocation). |
| `POST /api/admin/impersonate` | Body `{ user, ttlSeconds?, redirectTo? }` → `{ accessToken, refreshToken, expiresIn, userId, sessionId, expiresAt, url }`. Mints a session of `user` (default 15 minutes, 1–3600 s, 400 `invalid_request` otherwise) marked `impersonatedBy` the admin, with a fixed expiry (refreshing never extends it; access tokens never outlive it), audited as `impersonate` in the same transaction. With `redirectTo` (checked against the redirect allowlist, 400 `invalid_redirect`), `url` is that app URL with `#ferrix_access_token=…&ferrix_refresh_token=…&ferrix_expires_in=…&ferrix_user_id=…`, which `handleRedirectCallback()` consumes: open it in a private window. 404 `not_found` for an unknown user, 403 `impersonation_disabled` with `AuthConfig::impersonation(false)`. |
| `GET /api/admin/stats?since=<ms>` | `{ since, functions: FunctionStats[], overview: { now, startedAt, version, dev, ts, commitTime, tables, documents, registeredFunctions, sockets, subscriptions, sharedQueries, droppedLogEntries, files: [{ name, bytes }] } }`. `subscriptions` counts (socket, queryId) pairs, `sharedQueries` the distinct queries the server actually reruns. `since` defaults to 24 hours ago; `files` are `data.db`, `aux.db` and their WAL files. |

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
`crates/ferrix/dashboard-dist` (embedded with `rust-embed`): existing files as is (`assets/*` cached immutably),
everything else as `index.html` (client-side routes such as `/_/data/todos?doc=<id>` and `/_/setup#token=…`), with a
restrictive Content-Security-Policy and `X-Frame-Options: DENY`. Without the feature, only a minimal `/_/setup` page
is served; the system functions and `/api/admin/*` exist either way.

### 4.4 Operations: backups, export and import (`crate::ops`)

**Backup archives** (`ferrix-<UTC yyyymmddThhmmss.mmmZ>.tar.zst`; scheduled ones `ferrix-auto-…`): a zstd-compressed
tar of `data.db` (a `VACUUM INTO` snapshot: one read transaction, so a consistent copy at one commit ts that never
blocks the writer), optionally `aux.db` (same), `storage/<storage id>` for the blob of every `_storage` document of
the snapshot (local storage only), `auth_secret.key` (CLI `--include-secret` only), and last `manifest.json`:
`{ format: "ferrix-backup", version: 1, createdAt, ferrixVersion, ts, storage: "included" | "excluded" | "remote",
missingBlobs, files: [{ path, bytes, sha256 }] }`.

**Scheduled backups**: `serve --backup-every 6h --backup-keep 7 [--backup-to LOCATION] [--backup-with-aux]
[--backup-with-files]` declares the interval cron `ferrix-backup` running the internal system action
`_system/ops/backup:run_scheduled` with args `{ schedule: { to, keep, includeAux, includeFiles } }` (it shows on the
Scheduler page like any cron; "run now" works). Each run stores a `ferrix-auto-*` archive and deletes the oldest
scheduled ones beyond `keep`; manual archives are never rotated.

**Restore** (`ferrix restore <archive | s3://bucket/key>`, offline) takes `<dir>/ferrix.lock` (held by `serve` and
`import`, so it refuses while a server runs), extracts next to the data, checks entries against the manifest
(unexpected, missing or altered entries are refused), runs `PRAGMA integrity_check`, opens the database with the
binary's tables (schema check) and runs `verify_indexes` (`--skip-verify` skips the last two). Only then are
`data.db`(`-wal`, `-shm`), `aux.db`, persisted HNSW indexes (`vector_index_<id>.hnsw`) and, if restored,
`auth_secret.key` moved to `<dir>/pre-restore-<time>/` and the restored files put in place; blobs are added to the
storage directory.

A backup holds the whole database, system tables included (`_users`, `_settings`, `_audit_log`, ...); secrets in
`_settings` are encrypted with a key derived from `auth_secret.key`, so they are readable after a restore only with
the same key file. Persisted HNSW vector indexes (`vector_index_<id>.hnsw`) are caches and are not backed up:
they are rebuilt from the restored database at startup.

**Export directories** (`ferrix export`): `<table>.jsonl` or `<table>.csv` per table (all app tables by default;
`--tables` may name system tables), `files/<storage id>` for exported `_storage` documents, and `manifest.json`:
`{ format: "ferrix-export", version: 1, documentFormat, exportedAt, ts, ferrixVersion, tables: [{ name, number,
documents, file }], catalog: { <table>: <number> }, files, missingFiles }`. **Import** (`ferrix import`, offline;
or the HTTP endpoint above) reads JSONL: an export directory (ids of tables whose number differs in the target are
rewritten everywhere in the imported documents using `catalog`, and `files/` are stored), a directory of
`<table>.jsonl`, or one file (`--table`). The CLI may write system tables; validation, batching and modes as above.

```rust
// crate::ops (selected)
pub struct OpsOptions { pub metrics: MetricsOptions /* enabled, token */, pub backup_target: Option<String> } // AppOptions::ops
backup::create(&app, &BackupSpec { include_aux, include_files, include_secret }, &BackupTarget, scheduled) -> Result<BackupInfo>;
backup::restore(archive: &Path, dir: &Path, &RestoreOptions { storage_dir, skip_verify }) -> Result<RestoreReport>;
BackupTarget::{parse(location, data_dir), for_app(&app), list(), open(name), download(name, dest), delete(name), rotate(keep)};
export::export_to_dir(data_dir, Option<BlobStore>, out, &ExportOptions { tables, format, include_files }) -> Result<ExportManifest>;
export::import_path(&app, path, table: Option<&str>, &ImportOptions { mode, batch_size, allow_system_tables }) -> Result<ImportReport>;
DirLock::acquire(dir) -> Result<DirLock>;     // <dir>/ferrix.lock
web::router(app, &WebOptions { static_files, routes }) -> axum::Router;   // what `serve` serves
tls::start(router, &Listen::{Http(addr) | Acme { https, redirect, acme } | CertFiles { https, redirect, cert, key }}) -> Result<RunningServer>;
```

**Builder**: `Ferrix::static_dir(dir)`, `Ferrix::embed_static::<A: rust_embed::RustEmbed>()` (derive with
`#[derive(ferrix::rust_embed::Embed)] #[crate_path = "ferrix::rust_embed"]`) and `Ferrix::routes(axum::Router)`.
Routing and caching rules are in `crates/ferrix/src/ops/web.rs`: Ferrix's routes and the app's routes first, static
files as the fallback (never under `/api` or `/_`), SPA fallback to `index.html`, `no-cache` HTML, immutable hashed
assets, precompressed `.br`/`.gz`, ETags.


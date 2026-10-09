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
// prelude = Id, Doc, Field, Index, Order, QueryCtx, MutationCtx, ActionCtx, Result, Error, UserError,
//           table, query, mutation, action, internal_query, internal_mutation, internal_action, json!, Ferrix

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
- Every arg type must be `Serialize + DeserializeOwned + ts_rs::TS + Send + 'static`.
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
    pub ts: fn() -> String,   // inline TS object type of the table struct, for codegen (`ts_inline::<Self>()`)
}   // PartialEq compares name, indexes and the rendered `ts()`, never the fn pointer
pub struct IndexDef { pub name: &'static str, pub fields: Vec<&'static str> }   // JSON field names

/// Typed handle for a field (JSON name). V is the Rust value type.
pub struct Field<T, V> { /* name: &'static str, PhantomData<fn() -> (T, V)> */ }
impl<T, V> Field<T, V> { pub const fn new(name: &'static str) -> Self; pub fn name(&self) -> &'static str; }
// Copy + Clone regardless of T, V (implement manually).

/// Typed handle for an index.
pub struct Index<T> { /* name: &'static str, PhantomData */ }
impl<T> Index<T> { pub const fn new(name: &'static str) -> Self; pub fn name(&self) -> &'static str; }

pub enum Order { Asc, Desc }
```

### 1.2 Contexts and database API

```rust
pub struct QueryCtx    { pub db: QueryDb,    pub auth: Auth, /* private */ }
pub struct MutationCtx { pub db: MutationDb, pub auth: Auth, /* private */ }
pub struct ActionCtx   { pub auth: Auth, /* private: runtime handle */ }
// All three contexts are Send + Sync. QueryCtx/MutationCtx::now() -> f64 is the ms timestamp of the snapshot or commit.

pub struct Auth { /* phase 0: always unauthenticated */ }
impl Auth { pub fn user_id(&self) -> Option<&str>; }

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
impl<'a, T: Table> Query<'a, T> {
    pub fn with_index(self, index: Index<T>, range: impl FnOnce(IndexRange<T>) -> IndexRange<T>) -> Self; // default: by_creation_time, full range
    pub fn order(self, order: Order) -> Self;                                   // default Asc
    pub fn filter(self, pred: impl Fn(&Doc<T>) -> bool + Send + Sync + 'a) -> Self;
    pub async fn collect(self) -> Result<Vec<Doc<T>>>;
    pub async fn take(self, n: usize) -> Result<Vec<Doc<T>>>;
    pub async fn first(self) -> Result<Option<Doc<T>>>;
    pub async fn unique(self) -> Result<Option<Doc<T>>>;                       // Err if > 1 match
}
impl<T> IndexRange<T> {   // fields must follow the index field order: eq* then at most one lower and one upper bound on the next field
    pub fn eq<V: Serialize>(self, f: Field<T, V>, v: V) -> Self;
    pub fn gt<V: Serialize>(self, f: Field<T, V>, v: V) -> Self;   // also gte, lt, lte
}
impl ActionCtx {
    pub async fn run_query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output>;
    pub async fn run_mutation<F: MutationFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output>;
    pub async fn run_action<F: ActionFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output>;
}
// Built-in indexes on every table: "by_id" (point lookups) and "by_creation_time" (fields ["_creationTime"]).
// Every index key implicitly ends with _id, so keys are unique.
// Each table also gets the field handles `_id: Field<Self, Id<Self>>` and `_creation_time: Field<Self, f64>`
// (generated by #[table]), plus `by_creation_time: Index<Self>`.
```

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
```

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
                                                                 // ('static: codegen also walks T's dependencies)
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
        ], ts: || ::ferrix::__private::ts_inline::<Todo>() }
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

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::FUNCTIONS)]
#[linkme(crate = ::ferrix::__private::linkme)]
static __FERRIX_FN_list: ::ferrix::__private::FunctionDef = ::ferrix::__private::FunctionDef {
    module_path: module_path!(),
    name: "list",
    visibility: ::ferrix::__private::Visibility::Public,     // Internal for internal_*
    handler: ::ferrix::__private::Handler::Query(|ctx, args| ::std::boxed::Box::pin(async move {
        let a: __ferrix_args_list = ::ferrix::__private::decode_args("list", args)?;
        let out = list(&ctx, a.done, a.limit).await?;            // value namespace => the user's fn
        ::ferrix::__private::encode_output(out)
    })),
    ts: || (::ferrix::__private::ts_inline::<__ferrix_args_list>(),
            ::ferrix::__private::ts_inline::<Vec<Doc<Todo>>>()),
};
```
- `Output` is the `T` in `Result<T>`. The macro must handle `Result<T>`, `ferrix::Result<T>` and
  `anyhow::Result<T>`: it takes the first generic argument of the last path segment named `Result`. Anything else is
  a compile error.
- `optional_fields = nullable` makes `Option<T>` args optional in TS (`done?: boolean | null`); serde already treats a missing `Option` field as `None`.
- Zero params: `pub struct __ferrix_args_list {}` and `impl From<()>`.
- Compile errors (with spans) for: a non-async fn, a missing ctx param, a ctx type that doesn't match the macro kind
  (match on the last path segment: `QueryCtx` / `MutationCtx` / `ActionCtx`), `self` params, non-identifier
  patterns, generics, and a missing return type.
- Mutations use `Handler::Mutation` + `impl MutationFunction`; actions use `Handler::Action` + `impl ActionFunction`.

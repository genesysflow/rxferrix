//! Test-only mock of the `ferrix` runtime.
//!
//! It defines exactly the items from `docs/CONTRACT.md` §1–§2 that the macro expansions reference, with the
//! same paths and signatures, so the expansions of `ferrix-macros` can be type-checked and executed before the
//! real runtime exists. Behaviour is stubbed: contexts carry no database, and the `db` APIs are omitted.

use std::fmt;
use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub type Error = anyhow::Error;
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserError {
    pub code: String,
    pub message: String,
    pub data: serde_json::Value,
}
impl UserError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), data: serde_json::Value::Null }
    }
}
impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for UserError {}

// ---------------------------------------------------------------------------------------------------------------
// Id / Doc

pub struct Id<T> {
    raw: [u8; 16],
    _t: PhantomData<fn() -> T>,
}
impl<T> Id<T> {
    pub fn from_raw(raw: [u8; 16]) -> Self {
        Self { raw, _t: PhantomData }
    }
    pub fn table_number(&self) -> u32 {
        u32::from_be_bytes([self.raw[0], self.raw[1], self.raw[2], self.raw[3]])
    }
    pub fn as_str(&self) -> String {
        self.raw.iter().map(|b| format!("{b:02x}")).collect()
    }
}
impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Id<T> {}
impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<T> Eq for Id<T> {}
impl<T> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.as_str())
    }
}
impl<T> Serialize for Id<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_str())
    }
}
impl<'de, T> Deserialize<'de> for Id<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() != 32 {
            return Err(serde::de::Error::custom("bad id"));
        }
        let mut raw = [0u8; 16];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(serde::de::Error::custom)?;
        }
        Ok(Id::from_raw(raw))
    }
}
impl<T: Table> ts_rs::TS for Id<T> {
    type WithoutGenerics = Self;
    type OptionInnerType = Self;
    fn name(_: &ts_rs::Config) -> String {
        format!("Id<\"{}\">", T::NAME)
    }
    fn inline(cfg: &ts_rs::Config) -> String {
        Self::name(cfg)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc<T> {
    #[serde(rename = "_id")]
    pub id: Id<T>,
    #[serde(rename = "_creationTime")]
    pub creation_time: f64,
    #[serde(flatten)]
    pub value: T,
}
impl<T> std::ops::Deref for Doc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
impl<T: Table> ts_rs::TS for Doc<T> {
    type WithoutGenerics = Self;
    type OptionInnerType = Self;
    fn name(_: &ts_rs::Config) -> String {
        format!("Doc<\"{}\">", T::NAME)
    }
    fn inline(cfg: &ts_rs::Config) -> String {
        Self::name(cfg)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Tables

pub trait Table: Serialize + DeserializeOwned + ts_rs::TS + Clone + Send + Sync + 'static {
    const NAME: &'static str;
    fn table_def() -> TableDef;
}

#[derive(Debug, Clone)]
pub struct TableDef {
    pub name: &'static str,
    pub indexes: Vec<IndexDef>,
    pub search_indexes: Vec<SearchIndexDef>,
    pub vector_indexes: Vec<VectorIndexDef>,
    pub ts: fn() -> String,
    pub validate: fn(&serde_json::Value) -> std::result::Result<(), String>,
    pub shape: fn() -> shape::Shape,
    pub aggregates: Vec<AggregateDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateDef {
    pub name: &'static str,
    pub group_fields: Vec<&'static str>,
    pub sum_fields: Vec<&'static str>,
}

pub struct Aggregate<T> {
    name: &'static str,
    _t: PhantomData<fn() -> T>,
}
impl<T> Aggregate<T> {
    pub const fn new(name: &'static str) -> Self {
        Self { name, _t: PhantomData }
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
}
impl<T> Clone for Aggregate<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Aggregate<T> {}

/// `ferrix::Bytes` (for `shape`).
pub struct Bytes(pub Vec<u8>);

/// The real `ferrix::shape`, so expansions use exactly its items.
#[path = "../../../../ferrix/src/shape.rs"]
pub mod shape;
impl PartialEq for TableDef {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.indexes == other.indexes
            && self.search_indexes == other.search_indexes
            && self.vector_indexes == other.vector_indexes
            && self.aggregates == other.aggregates
            && (self.ts)() == (other.ts)()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexDef {
    pub name: &'static str,
    pub fields: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchIndexDef {
    pub name: &'static str,
    pub search_field: &'static str,
    pub filter_fields: Vec<&'static str>,
    pub tokenizer: Tokenizer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenizerKind {
    Unicode61,
    Porter,
    Trigram,
    Snowball,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tokenizer {
    pub kind: TokenizerKind,
    pub language: Option<&'static str>,
    pub remove_diacritics: bool,
}

impl Tokenizer {
    pub const fn unicode61() -> Self {
        Tokenizer { kind: TokenizerKind::Unicode61, language: None, remove_diacritics: true }
    }
    pub const fn porter(language: &'static str) -> Self {
        Tokenizer { kind: TokenizerKind::Porter, language: Some(language), remove_diacritics: true }
    }
    pub const fn trigram() -> Self {
        Tokenizer { kind: TokenizerKind::Trigram, language: None, remove_diacritics: true }
    }
    pub const fn snowball(language: &'static str) -> Self {
        Tokenizer { kind: TokenizerKind::Snowball, language: Some(language), remove_diacritics: true }
    }
    pub const fn keep_diacritics(self) -> Self {
        Tokenizer { remove_diacritics: false, ..self }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorIndexDef {
    pub name: &'static str,
    pub vector_field: &'static str,
    pub dimensions: usize,
    pub filter_fields: Vec<&'static str>,
    pub kind: VectorIndexKind,
    pub hnsw: HnswParams,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
}

impl HnswParams {
    pub const DEFAULT: HnswParams = HnswParams { m: 16, ef_construction: 128, ef_search: 128 };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VectorIndexKind {
    #[default]
    Exact,
    Hnsw,
}

pub struct SearchIndex<T> {
    name: &'static str,
    _t: PhantomData<fn() -> T>,
}
impl<T> SearchIndex<T> {
    pub const fn new(name: &'static str) -> Self {
        Self { name, _t: PhantomData }
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
}
impl<T> Clone for SearchIndex<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SearchIndex<T> {}

pub struct VectorIndex<T> {
    name: &'static str,
    _t: PhantomData<fn() -> T>,
}
impl<T> VectorIndex<T> {
    pub const fn new(name: &'static str) -> Self {
        Self { name, _t: PhantomData }
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
}
impl<T> Clone for VectorIndex<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for VectorIndex<T> {}

pub struct Field<T, V> {
    name: &'static str,
    _t: PhantomData<fn() -> (T, V)>,
}
impl<T, V> Field<T, V> {
    pub const fn new(name: &'static str) -> Self {
        Self { name, _t: PhantomData }
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
}
impl<T, V> Clone for Field<T, V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T, V> Copy for Field<T, V> {}

pub struct Index<T> {
    name: &'static str,
    _t: PhantomData<fn() -> T>,
}
impl<T> Index<T> {
    pub const fn new(name: &'static str) -> Self {
        Self { name, _t: PhantomData }
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
}
impl<T> Clone for Index<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Index<T> {}

pub enum Order {
    Asc,
    Desc,
}

// ---------------------------------------------------------------------------------------------------------------
// Contexts (stubs: no db)

#[derive(Default)]
pub struct Auth {
    _p: (),
}
impl Auth {
    pub fn user_id(&self) -> Option<&str> {
        None
    }
}

#[non_exhaustive]
pub struct QueryCtx {
    pub auth: Auth,
}
#[non_exhaustive]
pub struct MutationCtx {
    pub auth: Auth,
}
#[non_exhaustive]
pub struct ActionCtx {
    pub auth: Auth,
}
impl QueryCtx {
    pub fn mock() -> Self {
        Self { auth: Auth::default() }
    }
    pub fn now(&self) -> f64 {
        1.0
    }
}
impl MutationCtx {
    pub fn mock() -> Self {
        Self { auth: Auth::default() }
    }
    pub fn now(&self) -> f64 {
        2.0
    }
}
impl ActionCtx {
    pub fn mock() -> Self {
        Self { auth: Auth::default() }
    }
    /// Mock: runs the query's registered handler in-process.
    pub async fn run_query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output> {
        let def = __private::find(&F::path()).expect("query not registered");
        let __private::Handler::Query(h) = def.handler else { panic!("not a query") };
        let out = h(QueryCtx::mock(), serde_json::to_value(args.into())?).await?;
        Ok(serde_json::from_value(out)?)
    }
    pub async fn run_mutation<F: MutationFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output> {
        let def = __private::find(&F::path()).expect("mutation not registered");
        let __private::Handler::Mutation(h) = def.handler else { panic!("not a mutation") };
        let out = h(MutationCtx::mock(), serde_json::to_value(args.into())?).await?;
        Ok(serde_json::from_value(out)?)
    }
    pub async fn run_action<F: ActionFunction>(&self, args: impl Into<F::Args>) -> Result<F::Output> {
        let def = __private::find(&F::path()).expect("action not registered");
        let __private::Handler::Action(h) = def.handler else { panic!("not an action") };
        let out = h(ActionCtx::mock(), serde_json::to_value(args.into())?).await?;
        Ok(serde_json::from_value(out)?)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Function references

pub trait FunctionRef: Send + Sync + 'static {
    type Args: Serialize + DeserializeOwned + Send + 'static;
    type Output: Serialize + DeserializeOwned + Send + 'static;
    fn path() -> String;
}
pub trait QueryFunction: FunctionRef {}
pub trait MutationFunction: FunctionRef {}
pub trait ActionFunction: FunctionRef {}
pub trait SchedulableFunction: FunctionRef {}

pub mod prelude {
    pub use crate::migrations::DocChange;
    pub use crate::{ActionCtx, Doc, Error, Field, Id, Index, MutationCtx, Order, QueryCtx, Result, UserError};
}

/// `#[migration]` items (CONTRACT.md §1.9).
pub mod migrations {
    #[derive(Debug, Clone, PartialEq)]
    pub enum DocChange {
        Keep,
        Patch(serde_json::Value),
        Replace(serde_json::Value),
        Delete,
    }

    #[diagnostic::on_unimplemented(
        message = "a #[migration] must return `Result<Option<serde_json::Value>>` (a patch or None), `Result<DocChange>` or `Result<()>`, not `Result<{Self}>`",
        label = "not a migration result"
    )]
    pub trait MigrationOutput {
        fn into_change(self) -> DocChange;
    }
    impl MigrationOutput for DocChange {
        fn into_change(self) -> DocChange {
            self
        }
    }
    impl MigrationOutput for Option<serde_json::Value> {
        fn into_change(self) -> DocChange {
            self.map_or(DocChange::Keep, DocChange::Patch)
        }
    }
    impl MigrationOutput for () {
        fn into_change(self) -> DocChange {
            DocChange::Keep
        }
    }

    pub trait MigrationRef: 'static {
        fn name() -> String;
        fn table() -> &'static str;
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Macro-only items

#[doc(hidden)]
pub mod __private {
    pub use ::futures::future::BoxFuture;
    pub use ::linkme;
    pub use ::serde;
    pub use ::serde_json;
    pub use ::ts_rs;

    use crate::{ActionCtx, MutationCtx, QueryCtx, Result, TableDef};
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    #[linkme::distributed_slice]
    pub static TABLES: [fn() -> TableDef];

    #[linkme::distributed_slice]
    pub static FUNCTIONS: [FunctionDef];

    #[linkme::distributed_slice]
    pub static MIGRATIONS: [MigrationDef];

    pub type MigrationHandler =
        for<'a> fn(&'a MutationCtx, serde_json::Value) -> BoxFuture<'a, Result<crate::migrations::DocChange>>;

    pub struct MigrationDef {
        pub module_path: &'static str,
        pub fn_name: &'static str,
        pub name: Option<&'static str>,
        pub table: &'static str,
        pub batch_size: usize,
        pub after: &'static [&'static str],
        pub handler: MigrationHandler,
    }
    impl MigrationDef {
        pub fn name(&self) -> String {
            match self.name {
                Some(n) => n.to_string(),
                None => function_path(self.module_path, self.fn_name),
            }
        }
    }

    pub fn decode_migration_doc<T: DeserializeOwned>(doc: serde_json::Value) -> Result<T> {
        serde_json::from_value(doc).map_err(|e| anyhow::anyhow!("the document does not deserialize: {e}"))
    }

    pub fn migration_output<O: crate::migrations::MigrationOutput>(out: O) -> Result<crate::migrations::DocChange> {
        Ok(out.into_change())
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FunctionKind {
        Query,
        Mutation,
        Action,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Visibility {
        Public,
        Internal,
    }

    pub type QueryHandler = fn(QueryCtx, serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value>>;
    pub type MutationHandler = fn(MutationCtx, serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value>>;
    pub type ActionHandler = fn(ActionCtx, serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value>>;

    pub enum Handler {
        Query(QueryHandler),
        Mutation(MutationHandler),
        Action(ActionHandler),
    }
    impl Handler {
        pub fn kind(&self) -> FunctionKind {
            match self {
                Handler::Query(_) => FunctionKind::Query,
                Handler::Mutation(_) => FunctionKind::Mutation,
                Handler::Action(_) => FunctionKind::Action,
            }
        }
    }

    pub struct FunctionDef {
        pub module_path: &'static str,
        pub name: &'static str,
        pub visibility: Visibility,
        pub dev_only: bool,
        pub handler: Handler,
        pub ts: fn() -> (String, String),
    }

    pub fn function_path(module_path: &str, name: &str) -> String {
        let mut parts = module_path.split("::").skip(1).peekable();
        if parts.peek().is_none() {
            return name.to_owned();
        }
        format!("{}:{}", parts.collect::<Vec<_>>().join("/"), name)
    }

    pub fn find(path: &str) -> Option<&'static FunctionDef> {
        FUNCTIONS.iter().find(|f| function_path(f.module_path, f.name) == path)
    }

    pub fn decode_args<A: DeserializeOwned>(name: &str, args: serde_json::Value) -> Result<A> {
        serde_json::from_value(args).map_err(|e| anyhow::anyhow!("Invalid args for {name}: {e}"))
    }

    pub fn encode_output<O: Serialize>(out: O) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(out)?)
    }

    pub fn ts_inline<T: ts_rs::TS + ?Sized + 'static>() -> String {
        T::inline(&ts_rs::Config::default())
    }

    pub fn ts_output<T: ts_rs::TS + ?Sized + 'static>() -> String {
        T::name(&ts_rs::Config::default())
    }

    #[diagnostic::on_unimplemented(
        message = "the search field of a #[search_index] must be a `String` or `Option<String>`, not `{Self}`",
        label = "not a string field"
    )]
    pub trait SearchFieldType {}
    impl SearchFieldType for String {}
    impl SearchFieldType for Option<String> {}
    pub const fn assert_search_field<T: SearchFieldType + ?Sized>() {}

    #[diagnostic::on_unimplemented(
        message = "the field of a #[vector_index] must be a `Vec<f32>` or `Option<Vec<f32>>`, not `{Self}`",
        label = "not a `Vec<f32>` field"
    )]
    pub trait VectorFieldType {}
    impl VectorFieldType for Vec<f32> {}
    impl VectorFieldType for Option<Vec<f32>> {}
    pub const fn assert_vector_field<T: VectorFieldType + ?Sized>() {}

    #[diagnostic::on_unimplemented(
        message = "a sum field of #[aggregate] must be a number or an `Option` of one, not `{Self}`",
        label = "not a numeric field"
    )]
    pub trait SumFieldType {}
    macro_rules! sum_fields {
        ($($t:ty),*) => { $(impl SumFieldType for $t {} impl SumFieldType for Option<$t> {})* };
    }
    sum_fields!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);
    pub const fn assert_sum_field<T: SumFieldType + ?Sized>() {}

    pub fn validate_doc<T: crate::Table>(doc: &serde_json::Value) -> std::result::Result<(), String> {
        <crate::Doc<T> as serde::Deserialize>::deserialize(doc).map(drop).map_err(|e| e.to_string())
    }
}

//! Schema types: tables, documents, field and index handles.

use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::id::Id;

/// Name of the built-in point-lookup index present on every table.
pub const BY_ID: &str = "by_id";
/// Name of the built-in index on `_creationTime` present on every table.
pub const BY_CREATION_TIME: &str = "by_creation_time";
/// JSON name of the id system field.
pub const ID_FIELD: &str = "_id";
/// JSON name of the creation time system field.
pub const CREATION_TIME_FIELD: &str = "_creationTime";

/// A table: a Rust struct whose values are stored as documents. Implemented by `#[table]`.
pub trait Table: Serialize + DeserializeOwned + ts_rs::TS + Clone + Send + Sync + 'static {
    const NAME: &'static str;
    fn table_def() -> TableDef;
}

/// Runtime description of a table, registered in `__private::TABLES`.
#[derive(Debug, Clone)]
pub struct TableDef {
    pub name: &'static str,
    /// User-defined indexes (the built-in ones are implicit).
    pub indexes: Vec<IndexDef>,
    /// Full-text search indexes (`#[search_index]`, SQLite FTS5).
    pub search_indexes: Vec<SearchIndexDef>,
    /// Vector indexes (`#[vector_index]`, sqlite-vec).
    pub vector_indexes: Vec<VectorIndexDef>,
    /// The inline TS object type of the table struct (without system fields), for codegen.
    pub ts: fn() -> String,
    /// Checks that a stored document (system fields included) deserializes as `Doc<T>`; `Err` carries the serde
    /// error. Used by the startup schema check (`__private::validate_doc::<T>`).
    pub validate: fn(&serde_json::Value) -> Result<(), String>,
    /// The JSON shape of the table's fields (without system fields), for the analytics copy (`crate::shape`).
    pub shape: fn() -> crate::shape::Shape,
    /// Live aggregates (`#[aggregate]`): counts and sums per group, maintained in every write (`crate::db::aggregate`).
    pub aggregates: Vec<AggregateDef>,
}

/// Compares the schema (name and indexes) and the rendered TS type. Function pointers themselves (`ts`, `validate`)
/// are not compared: their addresses are not meaningful.
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
impl Eq for TableDef {}

/// A live aggregate (`#[aggregate(name, group = [..], sum = [..])]`): per group of `group_fields` values, the number of
/// documents and the sums of `sum_fields`. All names are JSON field names; no group fields = one group for the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateDef {
    pub name: &'static str,
    pub group_fields: Vec<&'static str>,
    pub sum_fields: Vec<&'static str>,
}

/// A full-text search index: one string field searched with SQLite FTS5, plus fields usable for equality filters.
/// All names are JSON field names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchIndexDef {
    pub name: &'static str,
    pub search_field: &'static str,
    pub filter_fields: Vec<&'static str>,
    /// How the text is split into terms (`#[search_index(.., tokenizer = "porter")]`). Changing it rebuilds the index.
    pub tokenizer: Tokenizer,
}

/// The FTS5 tokenizer of a search index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenizerKind {
    /// Words (Unicode letters and digits), case-insensitive. The default.
    Unicode61,
    /// Words reduced to their English stem (Porter): "running" matches "run".
    Porter,
    /// Every 3-character sequence: matches substrings anywhere in a word ("ell" matches "hello").
    Trigram,
    /// Words reduced to their stem by a Snowball stemmer for the index's `language` (one of
    /// [`Tokenizer::SNOWBALL_LANGUAGES`]): "copiii" matches "copiilor" in Romanian, "chantait" matches "chanter" in
    /// French. A custom FTS5 tokenizer (`db::search::stem`) wrapping `unicode61`.
    Snowball,
}

/// A search index's tokenizer: its kind, the stemming language (`Porter` and `Snowball`) and whether diacritics are
/// folded (`café` = `cafe`, the default) or kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tokenizer {
    pub kind: TokenizerKind,
    /// `Some("english")` for `Porter` (the only language FTS5's stemmer supports), one of
    /// [`Tokenizer::SNOWBALL_LANGUAGES`] for `Snowball`, `None` otherwise.
    pub language: Option<&'static str>,
    pub remove_diacritics: bool,
}

impl Tokenizer {
    /// Languages accepted by [`Tokenizer::porter`].
    pub const LANGUAGES: &'static [&'static str] = &["english"];
    /// Languages accepted by [`Tokenizer::snowball`] (the stemmers of the `rust-stemmers` crate). Keep in sync with
    /// the list in `ferrix-macros`.
    pub const SNOWBALL_LANGUAGES: &'static [&'static str] = &[
        "arabic",
        "danish",
        "dutch",
        "english",
        "finnish",
        "french",
        "german",
        "greek",
        "hungarian",
        "italian",
        "norwegian",
        "portuguese",
        "romanian",
        "russian",
        "spanish",
        "swedish",
        "tamil",
        "turkish",
    ];

    /// `tokenizer = "unicode61"` (the default).
    pub const fn unicode61() -> Self {
        Tokenizer { kind: TokenizerKind::Unicode61, language: None, remove_diacritics: true }
    }

    /// `tokenizer = "porter", language = "english"`.
    pub const fn porter(language: &'static str) -> Self {
        Tokenizer { kind: TokenizerKind::Porter, language: Some(language), remove_diacritics: true }
    }

    /// `tokenizer = "trigram"`.
    pub const fn trigram() -> Self {
        Tokenizer { kind: TokenizerKind::Trigram, language: None, remove_diacritics: true }
    }

    /// `tokenizer = "snowball", language = "<language>"` (one of [`Tokenizer::SNOWBALL_LANGUAGES`], checked at
    /// startup).
    pub const fn snowball(language: &'static str) -> Self {
        Tokenizer { kind: TokenizerKind::Snowball, language: Some(language), remove_diacritics: true }
    }

    /// `diacritics = "keep"`: `café` and `cafe` are different terms.
    pub const fn keep_diacritics(self) -> Self {
        Tokenizer { remove_diacritics: false, ..self }
    }
}

impl Default for Tokenizer {
    fn default() -> Self {
        Tokenizer::unicode61()
    }
}

/// A vector index over a `Vec<f32>` (or `Option<Vec<f32>>`) field of exactly `dimensions` elements, plus fields
/// usable for equality filters. All names are JSON field names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorIndexDef {
    pub name: &'static str,
    pub vector_field: &'static str,
    pub dimensions: usize,
    pub filter_fields: Vec<&'static str>,
    /// Exact (sqlite-vec brute force, the default) or approximate (an HNSW graph on top of it).
    pub kind: VectorIndexKind,
    /// The HNSW graph's parameters (`#[vector_index(.., kind = "hnsw", m = 16, ef_construction = 128, ef_search =
    /// 128)]`). Must be [`HnswParams::DEFAULT`] for exact indexes. Changing `m` or `ef_construction` rebuilds the
    /// graph (and its saved file); `ef_search` only changes how searches run.
    pub hnsw: HnswParams,
}

/// The parameters of an HNSW vector index (docs/CONTRACT.md §1.8). Ranges are checked at startup (and by the macro).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct HnswParams {
    /// Links per node on the upper layers (layer 0 allows `2 * m`). More links: better recall, more memory, slower
    /// builds. [`HnswParams::M_RANGE`].
    pub m: usize,
    /// Width of the candidate list when inserting a node. Higher: a better graph, slower builds.
    /// [`HnswParams::EF_CONSTRUCTION_RANGE`].
    pub ef_construction: usize,
    /// Default search width (at least the number of candidates a search asks for). Higher: better recall, slower
    /// searches. Per search: `VectorSearchOpts::ef(n)`. [`HnswParams::EF_SEARCH_RANGE`].
    pub ef_search: usize,
}

impl HnswParams {
    /// `m = 16, ef_construction = 128, ef_search = 128`.
    pub const DEFAULT: HnswParams = HnswParams { m: 16, ef_construction: 128, ef_search: 128 };
    pub const M_RANGE: std::ops::RangeInclusive<usize> = 4..=64;
    pub const EF_CONSTRUCTION_RANGE: std::ops::RangeInclusive<usize> = 16..=2048;
    /// Also the range of `VectorSearchOpts::ef`.
    pub const EF_SEARCH_RANGE: std::ops::RangeInclusive<usize> = 1..=4096;

    /// An error message if a parameter is out of range.
    pub fn check(&self) -> Result<(), String> {
        for (name, value, range) in [
            ("m", self.m, Self::M_RANGE),
            ("ef_construction", self.ef_construction, Self::EF_CONSTRUCTION_RANGE),
            ("ef_search", self.ef_search, Self::EF_SEARCH_RANGE),
        ] {
            if !range.contains(&value) {
                return Err(format!("`{name}` must be between {} and {}, got {value}", range.start(), range.end()));
            }
        }
        Ok(())
    }
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams::DEFAULT
    }
}

/// How a vector index answers nearest-neighbour searches (`#[vector_index(.., kind = "hnsw")]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorIndexKind {
    /// sqlite-vec `vec0`: an exact brute-force scan. Fine up to roughly 100k vectors.
    #[default]
    Exact,
    /// An in-memory HNSW graph (approximate, much faster on large sets), kept consistent with the committed `vec0`
    /// table, which stays the source of truth (docs/CONTRACT.md §1.8).
    Hnsw,
}

/// A user-defined index: a name and the JSON names of the indexed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    pub name: &'static str,
    pub fields: Vec<&'static str>,
}

/// Typed handle for a document field. `V` is the Rust value type, so range builders type-check their values.
pub struct Field<T, V> {
    name: &'static str,
    _p: PhantomData<fn() -> (T, V)>,
}

impl<T, V> Field<T, V> {
    pub const fn new(name: &'static str) -> Self {
        Field { name, _p: PhantomData }
    }
    /// The JSON name of the field.
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
impl<T, V> std::fmt::Debug for Field<T, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Field({})", self.name)
    }
}

/// Typed handle for an index of table `T`.
pub struct Index<T> {
    name: &'static str,
    _p: PhantomData<fn() -> T>,
}

impl<T> Index<T> {
    pub const fn new(name: &'static str) -> Self {
        Index { name, _p: PhantomData }
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
impl<T> std::fmt::Debug for Index<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Index({})", self.name)
    }
}

/// Typed handle for a live aggregate of table `T` (`#[aggregate]`); see `QueryDb::aggregate`.
pub struct Aggregate<T> {
    name: &'static str,
    _p: PhantomData<fn() -> T>,
}

impl<T> Aggregate<T> {
    pub const fn new(name: &'static str) -> Self {
        Aggregate { name, _p: PhantomData }
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
impl<T> std::fmt::Debug for Aggregate<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Aggregate({})", self.name)
    }
}

/// Typed handle for a full-text search index of table `T` (`#[search_index]`); see `Query::with_search_index`.
pub struct SearchIndex<T> {
    name: &'static str,
    _p: PhantomData<fn() -> T>,
}

impl<T> SearchIndex<T> {
    pub const fn new(name: &'static str) -> Self {
        SearchIndex { name, _p: PhantomData }
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
impl<T> std::fmt::Debug for SearchIndex<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SearchIndex({})", self.name)
    }
}

/// Typed handle for a vector index of table `T` (`#[vector_index]`); see `ActionCtx::vector_search`.
pub struct VectorIndex<T> {
    name: &'static str,
    _p: PhantomData<fn() -> T>,
}

impl<T> VectorIndex<T> {
    pub const fn new(name: &'static str) -> Self {
        VectorIndex { name, _p: PhantomData }
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
impl<T> std::fmt::Debug for VectorIndex<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VectorIndex({})", self.name)
    }
}

/// Scan order of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

/// A stored document: the system fields plus the user's struct, flattened.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Doc<T> {
    #[serde(rename = "_id")]
    pub id: Id<T>,
    /// Milliseconds since the unix epoch (sub-millisecond precision).
    #[serde(rename = "_creationTime")]
    pub creation_time: f64,
    #[serde(flatten)]
    pub value: T,
}

impl<T> Deref for Doc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Doc<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: PartialEq> PartialEq for Doc<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.creation_time == other.creation_time && self.value == other.value
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

    /// `#[serde(flatten)] doc: Doc<T>` in an `#[object]` struct: the struct's TS type is `{ ...its fields } &
    /// Doc<"table">`.
    fn inline_flattened(cfg: &ts_rs::Config) -> String {
        Self::name(cfg)
    }
}

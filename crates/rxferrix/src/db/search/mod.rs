//! Full-text search (SQLite FTS5) and vector search (sqlite-vec `vec0`) indexes.
//!
//! ## Storage
//!
//! Search and vector indexes ("special" indexes) are listed in the system table `_search_indexes`
//! (`index_id, table_number, name, definition`), next to `_indexes`. Their ids live in the upper half of the
//! [`IndexId`] space (`>= SPECIAL_INDEX_BASE`), so they never collide with ordinary index ids and can appear in read
//! sets and commit entries like any other index (see "Reactivity").
//!
//! - A **search index** `n` (`id = SPECIAL_INDEX_BASE + n`) is the FTS5 table `_search_<n>(t, f0 UNINDEXED, ...)`:
//!   `t` is the search field's text, `f<i>` the hex of the memcomparable encoding (`keys::encode_opt`) of the i-th
//!   filter field (so equality on the column is equality of values, of any JSON type). The FTS rowid maps to the
//!   document through `_search_<n>_docs(rowid INTEGER PRIMARY KEY, doc_id BLOB UNIQUE)`. Documents whose search field
//!   is missing, null or not a string are not indexed.
//! - A **vector index** `n` is the `vec0` virtual table `_vector_<n>(doc_id TEXT PRIMARY KEY, v float[dims]
//!   distance_metric=cosine, f0 TEXT, ...)`, with the filter fields as vec0 metadata columns (same hex encoding).
//!   Documents whose vector field is missing or null are not indexed.
//!
//! ## Transactional maintenance
//!
//! [`SpecialWrites::prepare`] computes, for one document write, the entries of every special index of the table
//! before and after (failing early, before anything is written, e.g. on a vector of the wrong dimension), and
//! [`SpecialWrites::apply`] updates the FTS / vec0 tables in the same SQLite write transaction as the document. Both
//! the typed and the untyped write paths go through `WriteState::write`, which calls them, so search results are
//! always consistent with document reads, and a rolled-back mutation leaves no trace in them.
//!
//! ## Catalog
//!
//! [`reconcile_table`] runs inside the catalog's startup transaction: new indexes are created and backfilled,
//! indexes whose definition changed (fields, dimensions, tokenizer version) are dropped, recreated and backfilled,
//! and indexes removed from code are dropped with their storage.
//!
//! ## Reactivity: per-filter-value keys
//!
//! A search index's id is a synthetic index id whose key space is laid out so that a search with an `eq` filter
//! reads only the keys of documents with that filter value:
//!
//! ```text
//! key = [filter column i: 1 byte][memcomparable encoding of the document's value of filter field i][doc id: 16 bytes]
//! ```
//!
//! Every write that changes a document's entry in a search index (its text or a filter value; inserting or deleting
//! an indexed document always does) adds, for the old entry and for the new one, one such key per filter field to
//! the commit entry's writes (an index without filter fields writes `[doc id]`). A search query records:
//! - with at least one `eq` filter: the prefix range `[i][enc(v)]` of the `eq` with the lowest filter column `i` (any
//!   one of them is enough: a document matching all filters has a key under each prefix); writes to documents whose
//!   old and new values of field `i` both differ from `v` fall outside it;
//! - without filters: the full range `[b"", ∞)`, which contains every key of the index;
//! - and a point read on `by_id` for every document it examined (changes to its other fields).
//!
//! This is exact because the ranking of a document depends only on that document and the query (see [`score`]):
//! documents outside the filter can neither enter the results nor move the documents inside it. The encoding of a
//! value is self-delimiting, so the prefix of one value never contains the keys of another, and the interval index
//! of the subscription manager stabs each written key like any index key. No change to the read-set or write-log
//! types is needed.
//!
//! Vector indexes use the same key layout (an entry's filter values are the vec0 metadata columns), so a query's
//! `ctx.db.vector_search` reads the prefixes of its filter values, or the full range ([`vector`]). An HNSW vector
//! index additionally writes the bare `(vector index id, doc id)` for every changed entry (16 bytes; filter keys are
//! longer), which is how its in-memory graph catches up with commits ([`ann`]). That key can only make a query rerun
//! more often, never less.

pub mod ann;
pub mod hnsw;
pub mod paginate;
pub mod score;
pub mod stem;
pub mod text;
pub mod vector;

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db::catalog::{Catalog, IndexId, TableInfo};
use crate::db::storage;
use crate::id::DocId;
use crate::keys;
use crate::schema::{
    BY_CREATION_TIME, BY_ID, CREATION_TIME_FIELD, HnswParams, ID_FIELD, TableDef, Tokenizer, TokenizerKind,
    VectorIndexKind,
};

pub use text::{MAX_SEARCH_RESULTS, RawSearchQuery, SearchFilter, SearchQuery};
pub use vector::{MAX_VECTOR_RESULTS, VectorFilter, VectorSearchOpts};

/// Register the SQLite extensions search needs (sqlite-vec, the `rxferrix_score` FTS5 function and the
/// `rxferrix_snowball` FTS5 tokenizer) for every connection opened afterwards in this process. Idempotent.
pub fn register_extensions() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        vector::register_sqlite_vec();
        score::register_auto_extension();
    });
}

/// Ids of search and vector indexes start here (the upper half of the `IndexId` space).
pub const SPECIAL_INDEX_BASE: IndexId = 1 << 31;
/// At most this many filter fields per index (sqlite-vec supports 16 metadata columns).
pub const MAX_FILTER_FIELDS: usize = 16;
/// Largest supported vector dimension (sqlite-vec's limit).
pub const MAX_DIMENSIONS: usize = 8192;
/// Bumped when the FTS5 table layout or tokenizer changes; stored definitions with another version are rebuilt.
const TEXT_VERSION: u32 = 1;
/// Same, for the vec0 layout.
const VECTOR_VERSION: u32 = 1;

const DDL: &str = "CREATE TABLE IF NOT EXISTS _search_indexes (index_id INTEGER PRIMARY KEY, table_number INTEGER NOT NULL, \
                   name TEXT NOT NULL, definition TEXT NOT NULL, UNIQUE(table_number, name))";

/// The stored definition of a search or vector index (JSON in `_search_indexes.definition`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum SpecialIndexKind {
    Search {
        search_field: String,
        filter_fields: Vec<String>,
        version: u32,
        /// Missing in definitions stored before tokenizers were configurable: the default.
        #[serde(default)]
        tokenizer: TokenizerConfig,
    },
    Vector {
        vector_field: String,
        dimensions: usize,
        filter_fields: Vec<String>,
        version: u32,
        /// Exact (`vec0` only) or HNSW (`vec0` plus an in-memory graph). Missing in older definitions: exact.
        #[serde(default)]
        method: VectorIndexKind,
        /// The HNSW parameters (the defaults for exact indexes). Missing in older definitions: the defaults.
        #[serde(default)]
        hnsw: HnswParams,
    },
}

/// A search index's tokenizer as stored in its definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenizerConfig {
    pub kind: TokenizerKind,
    #[serde(default)]
    pub language: Option<String>,
    pub remove_diacritics: bool,
}

impl Default for TokenizerConfig {
    fn default() -> Self {
        TokenizerConfig::from(Tokenizer::default())
    }
}

impl From<Tokenizer> for TokenizerConfig {
    fn from(t: Tokenizer) -> Self {
        TokenizerConfig {
            kind: t.kind,
            language: t.language.map(str::to_string),
            remove_diacritics: t.remove_diacritics,
        }
    }
}

impl TokenizerConfig {
    /// The FTS5 `tokenize` option.
    pub fn fts5(&self) -> String {
        match self.kind {
            TokenizerKind::Unicode61 => {
                format!("unicode61 remove_diacritics {}", if self.remove_diacritics { 2 } else { 0 })
            }
            TokenizerKind::Porter => {
                format!("porter unicode61 remove_diacritics {}", if self.remove_diacritics { 2 } else { 0 })
            }
            TokenizerKind::Trigram => format!("trigram remove_diacritics {}", u8::from(self.remove_diacritics)),
            TokenizerKind::Snowball => format!(
                "{} {} remove_diacritics {}",
                stem::TOKENIZER,
                self.language.as_deref().unwrap_or("english"),
                if self.remove_diacritics { 2 } else { 0 }
            ),
        }
    }

    /// `"unicode61"`, `"porter"`, `"trigram"` or `"snowball"`.
    pub fn name(&self) -> &'static str {
        match self.kind {
            TokenizerKind::Unicode61 => "unicode61",
            TokenizerKind::Porter => "porter",
            TokenizerKind::Trigram => "trigram",
            TokenizerKind::Snowball => "snowball",
        }
    }
}

/// A search or vector index of a table, as stored in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchIndexInfo {
    /// `>= SPECIAL_INDEX_BASE`. Search queries record reads on this id; writes changing an entry write it.
    pub id: IndexId,
    pub name: String,
    pub kind: SpecialIndexKind,
}

impl SearchIndexInfo {
    pub fn is_vector(&self) -> bool {
        matches!(self.kind, SpecialIndexKind::Vector { .. })
    }

    pub fn filter_fields(&self) -> &[String] {
        match &self.kind {
            SpecialIndexKind::Search { filter_fields, .. } | SpecialIndexKind::Vector { filter_fields, .. } => {
                filter_fields
            }
        }
    }

    /// The searched (text) or embedded (vector) field.
    pub fn field(&self) -> &str {
        match &self.kind {
            SpecialIndexKind::Search { search_field, .. } => search_field,
            SpecialIndexKind::Vector { vector_field, .. } => vector_field,
        }
    }

    /// The tokenizer of a search index (`None` for vector indexes).
    pub fn tokenizer(&self) -> Option<&TokenizerConfig> {
        match &self.kind {
            SpecialIndexKind::Search { tokenizer, .. } => Some(tokenizer),
            SpecialIndexKind::Vector { .. } => None,
        }
    }

    /// Is this a vector index with an HNSW graph?
    pub fn is_hnsw(&self) -> bool {
        matches!(self.kind, SpecialIndexKind::Vector { method: VectorIndexKind::Hnsw, .. })
    }

    /// The HNSW parameters of a vector index (`None` for search indexes).
    pub fn hnsw_params(&self) -> Option<HnswParams> {
        match self.kind {
            SpecialIndexKind::Vector { hnsw, .. } => Some(hnsw),
            SpecialIndexKind::Search { .. } => None,
        }
    }

    fn number(&self) -> u32 {
        self.id - SPECIAL_INDEX_BASE
    }

    /// The FTS5 / vec0 table holding the entries.
    pub(crate) fn storage_table(&self) -> String {
        match self.kind {
            SpecialIndexKind::Search { .. } => format!("\"_search_{}\"", self.number()),
            SpecialIndexKind::Vector { .. } => format!("\"_vector_{}\"", self.number()),
        }
    }

    /// For search indexes: the rowid → doc id map.
    pub(crate) fn docs_table(&self) -> String {
        format!("\"_search_{}_docs\"", self.number())
    }
}

/// A search or vector index as the dashboard lists it (`_system/dashboard:tables`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct SearchIndexSummary {
    pub name: String,
    /// `"search"` or `"vector"`.
    pub kind: String,
    /// The searched text field or the vector field (JSON name).
    pub field: String,
    pub filter_fields: Vec<String>,
    /// Vector indexes only.
    pub dimensions: Option<usize>,
    /// Search indexes only: `"unicode61"`, `"porter"`, `"trigram"` or `"snowball"`.
    pub tokenizer: Option<String>,
    /// Search indexes with a stemming tokenizer (`porter`, `snowball`): the language.
    pub language: Option<String>,
    /// Search indexes only: whether diacritics are folded (`café` = `cafe`).
    pub remove_diacritics: Option<bool>,
    /// Vector indexes only: `"exact"` or `"hnsw"`.
    pub method: Option<String>,
    /// HNSW vector indexes only: the graph's parameters.
    pub hnsw: Option<HnswParams>,
}

/// The search and vector indexes of a table, sorted by name.
pub fn summaries(table: &TableInfo) -> Vec<SearchIndexSummary> {
    let mut out: Vec<SearchIndexSummary> = table
        .search_indexes
        .iter()
        .map(|i| SearchIndexSummary {
            name: i.name.clone(),
            kind: if i.is_vector() { "vector" } else { "search" }.to_string(),
            field: i.field().to_string(),
            filter_fields: i.filter_fields().to_vec(),
            dimensions: match i.kind {
                SpecialIndexKind::Vector { dimensions, .. } => Some(dimensions),
                SpecialIndexKind::Search { .. } => None,
            },
            tokenizer: i.tokenizer().map(|t| t.name().to_string()),
            language: i.tokenizer().and_then(|t| t.language.clone()),
            remove_diacritics: i.tokenizer().map(|t| t.remove_diacritics),
            method: match i.kind {
                SpecialIndexKind::Vector { method: VectorIndexKind::Exact, .. } => Some("exact".to_string()),
                SpecialIndexKind::Vector { method: VectorIndexKind::Hnsw, .. } => Some("hnsw".to_string()),
                SpecialIndexKind::Search { .. } => None,
            },
            hnsw: if i.is_hnsw() { i.hnsw_params() } else { None },
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The `TableDef` view of a table's search and vector indexes, as stored kinds.
fn desired(def: &TableDef) -> Vec<(String, SpecialIndexKind)> {
    let strings = |v: &[&'static str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut out: Vec<(String, SpecialIndexKind)> = def
        .search_indexes
        .iter()
        .map(|s| {
            let kind = SpecialIndexKind::Search {
                search_field: s.search_field.to_string(),
                filter_fields: strings(&s.filter_fields),
                version: TEXT_VERSION,
                tokenizer: TokenizerConfig::from(s.tokenizer),
            };
            (s.name.to_string(), kind)
        })
        .collect();
    out.extend(def.vector_indexes.iter().map(|v| {
        let kind = SpecialIndexKind::Vector {
            vector_field: v.vector_field.to_string(),
            dimensions: v.dimensions,
            filter_fields: strings(&v.filter_fields),
            version: VECTOR_VERSION,
            method: v.kind,
            hnsw: v.hnsw,
        };
        (v.name.to_string(), kind)
    }));
    out
}

/// Check the search and vector index definitions of a table (called from the catalog's `validate_defs`).
pub(crate) fn validate_def(def: &TableDef) -> anyhow::Result<()> {
    let mut names: HashSet<&str> = def.indexes.iter().map(|i| i.name).collect();
    names.insert(BY_ID);
    names.insert(BY_CREATION_TIME);
    let is_system = |f: &str| f == ID_FIELD || f == CREATION_TIME_FIELD;
    let check_filters = |name: &str, field: &str, filters: &[&'static str]| -> anyhow::Result<()> {
        if filters.len() > MAX_FILTER_FIELDS {
            bail!("table `{}`: index `{name}` has more than {MAX_FILTER_FIELDS} filter fields", def.name);
        }
        let mut seen = HashSet::new();
        for f in filters {
            if !seen.insert(*f) || *f == field || is_system(f) || f.is_empty() {
                bail!("table `{}`: index `{name}` has an invalid or duplicate filter field `{f}`", def.name);
            }
        }
        Ok(())
    };
    for s in &def.search_indexes {
        if !names.insert(s.name) {
            bail!("table `{}`: duplicate or reserved index name `{}`", def.name, s.name);
        }
        if s.search_field.is_empty() || is_system(s.search_field) {
            bail!("table `{}`: search index `{}` has an invalid search field `{}`", def.name, s.name, s.search_field);
        }
        match (s.tokenizer.kind, s.tokenizer.language) {
            (TokenizerKind::Porter, Some(l)) if Tokenizer::LANGUAGES.contains(&l) => {}
            (TokenizerKind::Snowball, Some(l)) if Tokenizer::SNOWBALL_LANGUAGES.contains(&l) => {}
            (kind @ (TokenizerKind::Porter | TokenizerKind::Snowball), l) => {
                let (name, supported) = match kind {
                    TokenizerKind::Porter => ("porter", Tokenizer::LANGUAGES),
                    _ => ("snowball", Tokenizer::SNOWBALL_LANGUAGES),
                };
                bail!(
                    "table `{}`: search index `{}`: unsupported stemming language {l:?} for the {name} tokenizer (supported: {supported:?})",
                    def.name,
                    s.name,
                )
            }
            (_, Some(_)) => bail!(
                "table `{}`: search index `{}`: a language only applies to the porter and snowball tokenizers",
                def.name,
                s.name
            ),
            (_, None) => {}
        }
        check_filters(s.name, s.search_field, &s.filter_fields)?;
    }
    for v in &def.vector_indexes {
        if !names.insert(v.name) {
            bail!("table `{}`: duplicate or reserved index name `{}`", def.name, v.name);
        }
        if v.vector_field.is_empty() || is_system(v.vector_field) {
            bail!("table `{}`: vector index `{}` has an invalid field `{}`", def.name, v.name, v.vector_field);
        }
        if !(1..=MAX_DIMENSIONS).contains(&v.dimensions) {
            bail!("table `{}`: vector index `{}` must have 1 to {MAX_DIMENSIONS} dimensions", def.name, v.name);
        }
        if let Err(e) = v.hnsw.check() {
            bail!("table `{}`: vector index `{}`: {e}", def.name, v.name);
        }
        if v.kind == VectorIndexKind::Exact && v.hnsw != HnswParams::DEFAULT {
            bail!(
                "table `{}`: vector index `{}`: HNSW parameters (m, ef_construction, ef_search) only apply to kind = \"hnsw\"",
                def.name,
                v.name
            );
        }
        check_filters(v.name, v.vector_field, &v.filter_fields)?;
    }
    Ok(())
}

pub(crate) fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(DDL)?;
    Ok(())
}

/// The search and vector indexes stored for every table, by table number.
pub(crate) fn load(conn: &Connection) -> anyhow::Result<BTreeMap<u32, Vec<SearchIndexInfo>>> {
    let mut out: BTreeMap<u32, Vec<SearchIndexInfo>> = BTreeMap::new();
    let mut stmt =
        conn.prepare("SELECT index_id, table_number, name, definition FROM _search_indexes ORDER BY index_id")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let definition: String = r.get(3)?;
        let kind: SpecialIndexKind =
            serde_json::from_str(&definition).context("corrupt definition in _search_indexes")?;
        out.entry(r.get(1)?).or_default().push(SearchIndexInfo { id: r.get(0)?, name: r.get(2)?, kind });
    }
    Ok(out)
}

/// Bring the search and vector indexes of one table in line with `def`. Runs inside the catalog's reconciliation
/// transaction, after the table's ordinary indexes.
pub(crate) fn reconcile_table(conn: &Connection, def: &TableDef, table_number: u32) -> anyhow::Result<()> {
    let current = load(conn)?.remove(&table_number).unwrap_or_default();
    let desired = desired(def);
    for (name, kind) in &desired {
        let existing = current.iter().find(|c| &c.name == name);
        if existing.is_some_and(|c| &c.kind == kind) {
            continue;
        }
        // Switching a vector index between exact and HNSW, or changing its HNSW parameters, keeps its vec0 table
        // (the graph is built from it; a saved graph built with other parameters is discarded when it loads).
        if let Some(old) = existing
            && same_storage(&old.kind, kind)
        {
            tracing::info!(table = def.name, index = %name, "vector index method or HNSW parameters changed");
            conn.execute(
                "UPDATE _search_indexes SET definition = ?1 WHERE index_id = ?2",
                params![serde_json::to_string(kind)?, old.id],
            )?;
            continue;
        }
        let id = match existing {
            Some(old) => {
                tracing::info!(table = def.name, index = %name, "search index definition changed; rebuilding");
                drop_storage(conn, old)?;
                conn.execute(
                    "UPDATE _search_indexes SET definition = ?1 WHERE index_id = ?2",
                    params![serde_json::to_string(kind)?, old.id],
                )?;
                old.id
            }
            None => {
                let max: Option<u32> = conn.query_row("SELECT MAX(index_id) FROM _search_indexes", [], |r| r.get(0))?;
                let id = max.unwrap_or(SPECIAL_INDEX_BASE).max(SPECIAL_INDEX_BASE) + 1;
                conn.execute(
                    "INSERT INTO _search_indexes (index_id, table_number, name, definition) VALUES (?1, ?2, ?3, ?4)",
                    params![id, table_number, name, serde_json::to_string(kind)?],
                )?;
                id
            }
        };
        let info = SearchIndexInfo { id, name: name.clone(), kind: kind.clone() };
        create_storage(conn, &info)?;
        backfill(conn, def.name, table_number, &info)?;
    }
    for old in &current {
        if !desired.iter().any(|(n, _)| n == &old.name) {
            tracing::info!(table = def.name, index = %old.name, "search index removed; dropping it");
            drop_storage(conn, old)?;
            conn.execute("DELETE FROM _search_indexes WHERE index_id = ?1", [old.id])?;
        }
    }
    Ok(())
}

/// Do two definitions share the same storage, i.e. differ only in the vector search method and HNSW parameters?
fn same_storage(a: &SpecialIndexKind, b: &SpecialIndexKind) -> bool {
    match (a, b) {
        (SpecialIndexKind::Vector { .. }, SpecialIndexKind::Vector { method, hnsw, .. }) => {
            let mut a = a.clone();
            if let SpecialIndexKind::Vector { method: m, hnsw: h, .. } = &mut a {
                *m = *method;
                *h = *hnsw;
            }
            &a == b
        }
        _ => false,
    }
}

/// Drop, recreate and backfill one search or vector index (e.g. after a key encoding change, which changes the
/// encoded filter columns). Recreated with the index's stored definition, so its tokenizer is kept. An HNSW index's
/// graph follows: its saved file is validated against the rebuilt `vec0` table at startup (filter values that no
/// longer match are re-inserted).
pub(crate) fn rebuild(conn: &Connection, table: &str, table_number: u32, info: &SearchIndexInfo) -> anyhow::Result<()> {
    drop_storage(conn, info)?;
    create_storage(conn, info)?;
    backfill(conn, table, table_number, info)
}

fn create_storage(conn: &Connection, info: &SearchIndexInfo) -> anyhow::Result<()> {
    let filters: Vec<String> = (0..info.filter_fields().len()).map(|i| format!("f{i}")).collect();
    match &info.kind {
        SpecialIndexKind::Search { tokenizer, .. } => {
            let cols: String = filters.iter().map(|f| format!(", {f} UNINDEXED")).collect();
            // Prefix indexes speed up the last term's prefix match; trigram queries are substring matches instead.
            let prefix = if tokenizer.kind == TokenizerKind::Trigram { "" } else { ", prefix = '2 3'" };
            conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE {} USING fts5(t{cols}, tokenize = '{}'{prefix});
                 CREATE TABLE {} (rowid INTEGER PRIMARY KEY, doc_id BLOB NOT NULL UNIQUE);",
                info.storage_table(),
                tokenizer.fts5(),
                info.docs_table()
            ))?;
        }
        SpecialIndexKind::Vector { dimensions, .. } => {
            let cols: String = filters.iter().map(|f| format!(", {f} text")).collect();
            conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE {} USING vec0(doc_id text primary key, v float[{dimensions}] distance_metric=cosine{cols});",
                info.storage_table()
            ))?;
        }
    }
    Ok(())
}

fn drop_storage(conn: &Connection, info: &SearchIndexInfo) -> anyhow::Result<()> {
    conn.execute_batch(&format!("DROP TABLE IF EXISTS {};", info.storage_table()))?;
    if !info.is_vector() {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {};", info.docs_table()))?;
    }
    Ok(())
}

fn backfill(conn: &Connection, table: &str, table_number: u32, info: &SearchIndexInfo) -> anyhow::Result<()> {
    let docs = storage::table_documents(conn, table_number)?;
    if !docs.is_empty() {
        tracing::info!(table, index = %info.name, docs = docs.len(), "backfilling search index");
    }
    let mut skipped = 0usize;
    for (id, json) in docs {
        let doc: Value = serde_json::from_str(&json)?;
        let doc = doc.as_object().context("stored document is not an object")?;
        match Entry::of(info, doc) {
            Ok(Some(entry)) => entry.insert(conn, info, &DocId(id))?,
            Ok(None) => {}
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(
            table,
            index = %info.name,
            skipped,
            "documents whose vector does not have the index's dimensions were not indexed; fix them to make them searchable"
        );
    }
    Ok(())
}

/// The hex of the memcomparable encoding of a filter value: equal strings exactly when the values are equal.
pub(crate) fn encode_filter_value(v: Option<&Value>) -> String {
    let mut out = Vec::new();
    keys::encode_opt(v, &mut out);
    data_encoding::HEXLOWER.encode(&out)
}

/// The start of the synthetic keys of documents whose filter column `col` holds the value with this hex encoding:
/// `[col][encoded value]` (see "Reactivity" in the module docs).
pub(crate) fn filter_key_prefix(col: usize, hex: &str) -> Vec<u8> {
    let mut key = vec![col as u8];
    key.extend(data_encoding::HEXLOWER.decode(hex.as_bytes()).expect("filter values are stored as lowercase hex"));
    key
}

/// The synthetic keys a write of document `id` with this search or vector entry adds to the commit (see
/// "Reactivity").
fn search_keys(entry: &Entry, id: &DocId, out: &mut Vec<Vec<u8>>) {
    let (Entry::Text { filters, .. } | Entry::Vector { filters, .. }) = entry;
    if filters.is_empty() {
        out.push(id.0.to_vec());
    }
    for (col, hex) in filters.iter().enumerate() {
        let mut key = filter_key_prefix(col, hex);
        key.extend_from_slice(&id.0);
        out.push(key);
    }
}

/// One document's entry in a search or vector index.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Text { text: String, filters: Vec<String> },
    Vector { vector: Vec<f32>, filters: Vec<String> },
}

impl Entry {
    /// The entry of `doc`, `None` if it is not indexed. Errors only for a vector of the wrong shape.
    fn of(info: &SearchIndexInfo, doc: &Map<String, Value>) -> anyhow::Result<Option<Entry>> {
        let filters = || info.filter_fields().iter().map(|f| encode_filter_value(doc.get(f))).collect();
        match &info.kind {
            SpecialIndexKind::Search { search_field, .. } => Ok(match doc.get(search_field) {
                Some(Value::String(text)) => Some(Entry::Text { text: text.clone(), filters: filters() }),
                _ => None,
            }),
            SpecialIndexKind::Vector { vector_field, dimensions, .. } => match doc.get(vector_field) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Array(items)) => {
                    if items.len() != *dimensions {
                        bail!(
                            "field `{vector_field}` has {} dimensions, but vector index `{}` expects {dimensions}",
                            items.len(),
                            info.name
                        );
                    }
                    let vector = items
                        .iter()
                        .map(|v| v.as_f64().map(|f| f as f32).filter(|f| f.is_finite()))
                        .collect::<Option<Vec<f32>>>()
                        .with_context(|| format!("field `{vector_field}` must be an array of finite numbers"))?;
                    Ok(Some(Entry::Vector { vector, filters: filters() }))
                }
                Some(_) => bail!("field `{vector_field}` of vector index `{}` must be an array of numbers", info.name),
            },
        }
    }

    fn insert(&self, conn: &Connection, info: &SearchIndexInfo, id: &DocId) -> anyhow::Result<()> {
        match self {
            Entry::Text { text, filters } => {
                let rowid: i64 = match conn
                    .prepare_cached(&format!("SELECT rowid FROM {} WHERE doc_id = ?1", info.docs_table()))?
                    .query_row([&id.0[..]], |r| r.get(0))
                    .optional()?
                {
                    Some(r) => r,
                    None => {
                        conn.prepare_cached(&format!("INSERT INTO {} (doc_id) VALUES (?1)", info.docs_table()))?
                            .execute([&id.0[..]])?;
                        conn.last_insert_rowid()
                    }
                };
                let placeholders: String = (0..filters.len()).map(|i| format!(", ?{}", i + 3)).collect();
                let cols: String = (0..filters.len()).map(|i| format!(", f{i}")).collect();
                let sql =
                    format!("INSERT INTO {} (rowid, t{cols}) VALUES (?1, ?2{placeholders})", info.storage_table());
                let mut stmt = conn.prepare_cached(&sql)?;
                let mut values: Vec<rusqlite::types::Value> = vec![rowid.into(), text.clone().into()];
                values.extend(filters.iter().map(|f| f.clone().into()));
                stmt.execute(rusqlite::params_from_iter(values))?;
            }
            Entry::Vector { vector, filters } => {
                let placeholders: String = (0..filters.len()).map(|i| format!(", ?{}", i + 3)).collect();
                let cols: String = (0..filters.len()).map(|i| format!(", f{i}")).collect();
                let sql =
                    format!("INSERT INTO {} (doc_id, v{cols}) VALUES (?1, ?2{placeholders})", info.storage_table());
                let mut stmt = conn.prepare_cached(&sql)?;
                let mut values: Vec<rusqlite::types::Value> = vec![id.encode().into(), vector::to_blob(vector).into()];
                values.extend(filters.iter().map(|f| f.clone().into()));
                stmt.execute(rusqlite::params_from_iter(values))?;
            }
        }
        Ok(())
    }

    /// Remove the entry of `id`. With `keep_rowid` (a search entry about to be re-inserted), the rowid mapping is
    /// kept so the document keeps its rowid (its tie-break position among equal ranks).
    fn delete(info: &SearchIndexInfo, conn: &Connection, id: &DocId, keep_rowid: bool) -> anyhow::Result<()> {
        if info.is_vector() {
            conn.prepare_cached(&format!("DELETE FROM {} WHERE doc_id = ?1", info.storage_table()))?
                .execute([id.encode()])?;
            return Ok(());
        }
        let rowid: Option<i64> = conn
            .prepare_cached(&format!("SELECT rowid FROM {} WHERE doc_id = ?1", info.docs_table()))?
            .query_row([&id.0[..]], |r| r.get(0))
            .optional()?;
        if let Some(rowid) = rowid {
            conn.prepare_cached(&format!("DELETE FROM {} WHERE rowid = ?1", info.storage_table()))?.execute([rowid])?;
            if !keep_rowid {
                conn.prepare_cached(&format!("DELETE FROM {} WHERE rowid = ?1", info.docs_table()))?
                    .execute([rowid])?;
            }
        }
        Ok(())
    }
}

/// The search and vector index updates of one document write: computed (and validated) before anything is
/// written, applied in the same transaction as the document.
pub(crate) struct SpecialWrites<'a> {
    changes: Vec<(&'a SearchIndexInfo, Option<Entry>, Option<Entry>)>,
}

impl<'a> SpecialWrites<'a> {
    /// Compare the entries of the old and new version of a document (`None` = absent) in every search and vector
    /// index of `table`. Fails, before anything was written, if the new version can't be indexed (a vector with
    /// the wrong number of dimensions).
    pub(crate) fn prepare(
        table: &'a TableInfo,
        old: Option<&Map<String, Value>>,
        new: Option<&Map<String, Value>>,
    ) -> anyhow::Result<Self> {
        let mut changes = Vec::new();
        for info in &table.search_indexes {
            let new_entry = match new.map(|d| Entry::of(info, d)).transpose() {
                Ok(entry) => entry.flatten(),
                // A vector that was already unindexable and is left unchanged (e.g. written before the index's
                // dimensions changed, then skipped by the backfill) stays unindexed rather than blocking other edits.
                Err(_) if old.is_some_and(|o| o.get(info.field()) == new.and_then(|n| n.get(info.field()))) => None,
                Err(e) => return Err(e),
            };
            // The old version was accepted when it was written (or skipped by a backfill): never fail on it.
            let old_entry = old.and_then(|d| Entry::of(info, d).ok().flatten());
            if old_entry != new_entry {
                changes.push((info, old_entry, new_entry));
            }
        }
        Ok(SpecialWrites { changes })
    }

    /// Update the FTS / vec0 tables and record the synthetic keys of every changed search or vector entry (the old
    /// entry's and the new one's, see "Reactivity"), plus `(index id, doc id)` for every changed HNSW vector entry
    /// (how its graph catches up, [`ann`]).
    pub(crate) fn apply(
        self,
        conn: &Connection,
        id: DocId,
        writes: &mut Vec<(IndexId, Vec<u8>)>,
    ) -> anyhow::Result<()> {
        for (info, old, new) in self.changes {
            if old.is_some() {
                Entry::delete(info, conn, &id, new.is_some())?;
            }
            if let Some(entry) = &new {
                entry.insert(conn, info, &id)?;
            }
            let mut keys = Vec::new();
            for entry in old.iter().chain(&new) {
                search_keys(entry, &id, &mut keys);
            }
            if info.is_hnsw() {
                keys.push(id.0.to_vec());
            }
            keys.sort();
            keys.dedup();
            writes.extend(keys.into_iter().map(|k| (info.id, k)));
        }
        Ok(())
    }
}

/// Recompute every search and vector index from the documents and compare with what is stored (at one snapshot).
/// Part of `Database::verify_indexes`. Returns a description of each mismatch.
pub(crate) fn verify(conn: &Connection, catalog: &Catalog) -> anyhow::Result<Vec<String>> {
    let mut problems = Vec::new();
    let mut known: HashSet<String> = HashSet::new();
    for table in catalog.tables() {
        if table.search_indexes.is_empty() {
            continue;
        }
        let docs = storage::table_documents(conn, table.number)?;
        for info in &table.search_indexes {
            known.insert(info.storage_table().trim_matches('"').to_string());
            let mut expected: Vec<(String, Entry)> = Vec::new();
            for (id, json) in &docs {
                let doc: Value = serde_json::from_str(json)?;
                let doc = doc.as_object().context("document is not an object")?;
                if let Ok(Some(entry)) = Entry::of(info, doc) {
                    expected.push((DocId(*id).encode(), entry));
                }
            }
            expected.sort_by(|a, b| a.0.cmp(&b.0));
            let actual = stored_entries(conn, info)?;
            if actual != expected {
                let differing = expected.iter().filter(|e| !actual.contains(e)).count()
                    + actual.iter().filter(|a| !expected.contains(a)).count();
                problems.push(format!(
                    "table `{}` {} index `{}`: expected {} entries, found {} ({differing} differ)",
                    table.name,
                    if info.is_vector() { "vector" } else { "search" },
                    info.name,
                    expected.len(),
                    actual.len()
                ));
            }
            if !info.is_vector() {
                let (fts, map): (i64, i64) = conn.query_row(
                    &format!(
                        "SELECT (SELECT COUNT(*) FROM {}), (SELECT COUNT(*) FROM {})",
                        info.storage_table(),
                        info.docs_table()
                    ),
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if fts != map {
                    problems.push(format!(
                        "table `{}` search index `{}`: {fts} FTS rows but {map} rowid mappings",
                        table.name, info.name
                    ));
                }
            }
        }
    }
    // Storage tables of indexes that no longer exist.
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND (name LIKE '\\_search\\_%' ESCAPE '\\' OR name LIKE '\\_vector\\_%' ESCAPE '\\')")?;
    let names: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for name in names {
        let base = name.strip_prefix("_search_").or_else(|| name.strip_prefix("_vector_")).unwrap_or("");
        let is_main = !base.is_empty() && base.bytes().all(|b| b.is_ascii_digit());
        if is_main && !known.contains(&name) && name != "_search_indexes" {
            problems.push(format!("storage table `{name}` belongs to no search or vector index"));
        }
    }
    Ok(problems)
}

/// The stored entries of an index as `(doc id string, entry)`, sorted by id.
fn stored_entries(conn: &Connection, info: &SearchIndexInfo) -> anyhow::Result<Vec<(String, Entry)>> {
    let n = info.filter_fields().len();
    let cols: String = (0..n).map(|i| format!(", s.f{i}")).collect();
    let mut out = Vec::new();
    if info.is_vector() {
        let mut stmt = conn.prepare(&format!("SELECT s.doc_id, s.v{cols} FROM {} s", info.storage_table()))?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let blob: Vec<u8> = r.get(1)?;
            let filters = (0..n).map(|i| r.get::<_, String>(i + 2)).collect::<Result<_, _>>()?;
            out.push((r.get::<_, String>(0)?, Entry::Vector { vector: vector::from_blob(&blob), filters }));
        }
    } else {
        let mut stmt = conn.prepare(&format!(
            "SELECT m.doc_id, s.t{cols} FROM {} s JOIN {} m ON m.rowid = s.rowid",
            info.storage_table(),
            info.docs_table()
        ))?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id: Vec<u8> = r.get(0)?;
            let id: [u8; 16] = id.try_into().map_err(|_| anyhow::anyhow!("corrupt doc id in search index"))?;
            let filters = (0..n).map(|i| r.get::<_, String>(i + 2)).collect::<Result<_, _>>()?;
            out.push((DocId(id).encode(), Entry::Text { text: r.get(1)?, filters }));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::db::Database;
    use crate::db::query::MutationDb;
    use crate::schema::{TableDef, VectorIndexDef};

    fn def(dimensions: usize) -> TableDef {
        TableDef {
            name: "vt",
            indexes: vec![],
            search_indexes: vec![],
            vector_indexes: vec![VectorIndexDef {
                name: "v",
                vector_field: "e",
                dimensions,
                filter_fields: vec![],
                kind: Default::default(),
                hnsw: Default::default(),
            }],
            ts: || "{ }".into(),
            validate: |_| Ok(()),
            shape: crate::shape::Shape::unknown,
            aggregates: vec![],
        }
    }

    async fn insert(db: &Database, doc: Value) -> String {
        let txn = db.begin_write().await.unwrap();
        let id = MutationDb::new(txn.clone()).insert_raw("vt", doc).await.unwrap();
        txn.commit_with(|_, _| Ok(())).await.unwrap();
        id
    }

    async fn patch(db: &Database, id: &str, patch: Value) -> crate::Result<()> {
        let txn = db.begin_write().await?;
        let result = MutationDb::new(txn.clone()).patch_raw(id, patch).await;
        match result {
            Ok(()) => txn.commit_with(|_, _| Ok(())).await.map(drop),
            Err(e) => {
                txn.rollback();
                Err(e)
            }
        }
    }

    /// After the dimensions change, old vectors are skipped by the backfill; their documents stay editable as long
    /// as the vector itself is left alone, and a new vector must have the new dimensions.
    #[tokio::test]
    async fn unchanged_unindexable_vectors_do_not_block_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path(), &[def(2)], 1).unwrap();
        let id = insert(&db, json!({ "e": [1.0, 2.0], "x": 1 })).await;
        drop(db);

        let db = Database::open(dir.path(), &[def(3)], 1).unwrap();
        assert!(db.verify_indexes().await.unwrap().is_empty());
        patch(&db, &id, json!({ "x": 2 })).await.unwrap();
        patch(&db, &id, json!({ "e": [1.0, 2.0] })).await.unwrap();
        let err = patch(&db, &id, json!({ "e": [1.0] })).await.unwrap_err();
        assert!(err.to_string().contains("expects 3"), "{err}");
        patch(&db, &id, json!({ "e": [1.0, 2.0, 3.0] })).await.unwrap();
        let err = patch(&db, &id, json!({ "e": [1.0, 2.0] })).await.unwrap_err();
        assert!(err.to_string().contains("expects 3"), "{err}");
        assert!(db.verify_indexes().await.unwrap().is_empty());
    }
}

//! Vector search with sqlite-vec: `ctx.vector_search(T::idx, &embedding, VectorSearchOpts::new(10))` in actions.
//!
//! sqlite-vec is compiled into the binary (the `sqlite-vec` crate, built against the bundled SQLite) and registered
//! for every connection with `sqlite3_auto_extension` ([`register_extension`]). Each vector index is a `vec0`
//! virtual table using the cosine distance; vec0's KNN is an exact (brute-force) scan, so results equal a brute-force
//! computation. Filter fields are vec0 metadata columns, so filters are applied during the scan (the `limit` nearest
//! *matching* documents are returned, not the nearest documents filtered afterwards).
//!
//! An HNSW index (`kind = "hnsw"`) answers from its in-memory graph instead ([`super::ann`]): approximate, with
//! filters applied after the graph search (oversampled), and exact again while the graph is not ready.
//!
//! Vector search is only available in actions (`ActionCtx` is the only context with `vector_search`): embeddings come
//! from external APIs, which only actions may call, and keeping it out of queries keeps queries cheap and
//! deterministic. It reads the latest committed snapshot; actions have no transaction, so the documents may change
//! before the action reads them (`ctx.run_query`).

use std::marker::PhantomData;
use std::sync::Once;

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::Value;

use crate::db::search::{SpecialIndexKind, encode_filter_value};
use crate::id::{DocId, Id};
use crate::runtime::ActionCtx;
use crate::schema::{Field, Table, VectorIndex};

/// The most results one vector search returns.
pub const MAX_VECTOR_RESULTS: usize = 256;
/// The most `or` branches a [`VectorFilter`] may have.
pub const MAX_FILTER_BRANCHES: usize = 64;

/// Register sqlite-vec (and the other search extensions, see [`super::register_extensions`]) for every SQLite
/// connection opened afterwards in this process. Idempotent.
pub fn register_extension() {
    super::register_extensions();
}

pub(super) fn register_sqlite_vec() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        type Init = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;
        // SAFETY: `sqlite3_vec_init` is the extension's entry point, whose real signature is `Init` (the crate
        // declares it without parameters). With SQLITE_CORE it uses the linked SQLite directly and ignores the api
        // routines pointer. Registering an auto extension is thread-safe.
        unsafe {
            let init: Init = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(Some(init));
        }
    });
}

/// A vector as vec0 stores it: little-endian f32s.
pub(crate) fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub(crate) fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// Equality filters on a vector index's filter fields: an OR of ANDs of `field == value`.
///
/// ```ignore
/// VectorFilter::eq(Doc::channel, c)                                   // channel == c
/// VectorFilter::eq(Doc::channel, a).or(VectorFilter::eq(Doc::channel, b))   // channel == a || channel == b
/// VectorFilter::any_of(Doc::channel, [a, b])                         // same
/// VectorFilter::eq(Doc::channel, c).and(VectorFilter::eq(Doc::lang, "en"))
/// ```
pub struct VectorFilter<T> {
    /// Disjunction of conjunctions of `(field, value)`.
    branches: Vec<Vec<(&'static str, Result<Value, String>)>>,
    _p: PhantomData<fn() -> T>,
}

impl<T> Clone for VectorFilter<T> {
    fn clone(&self) -> Self {
        VectorFilter { branches: self.branches.clone(), _p: PhantomData }
    }
}

impl<T> VectorFilter<T> {
    /// `field == value`.
    pub fn eq<V: Serialize>(field: Field<T, V>, value: V) -> Self {
        let value = serde_json::to_value(value).map_err(|e| e.to_string());
        VectorFilter { branches: vec![vec![(field.name(), value)]], _p: PhantomData }
    }

    /// `field` equals any of `values` (no values: matches nothing).
    pub fn any_of<V: Serialize>(field: Field<T, V>, values: impl IntoIterator<Item = V>) -> Self {
        let branches = values
            .into_iter()
            .map(|v| vec![(field.name(), serde_json::to_value(v).map_err(|e| e.to_string()))])
            .collect();
        VectorFilter { branches, _p: PhantomData }
    }

    /// Either filter matches.
    pub fn or(mut self, other: Self) -> Self {
        self.branches.extend(other.branches);
        self
    }

    /// Both filters match.
    pub fn and(self, other: Self) -> Self {
        let mut branches = Vec::new();
        for a in &self.branches {
            for b in &other.branches {
                branches.push(a.iter().chain(b).cloned().collect());
            }
        }
        VectorFilter { branches, _p: PhantomData }
    }
}

/// Options of [`ActionCtx::vector_search`].
pub struct VectorSearchOpts<T> {
    /// How many results to return, 1 to [`MAX_VECTOR_RESULTS`]. Default 10.
    pub limit: usize,
    /// Only documents matching this filter. Default: all documents.
    pub filter: Option<VectorFilter<T>>,
    /// On an HNSW index, search exactly (the `vec0` scan) instead of the graph. No effect on exact indexes.
    pub exact: bool,
}

impl<T> VectorSearchOpts<T> {
    pub fn new(limit: usize) -> Self {
        VectorSearchOpts { limit, filter: None, exact: false }
    }

    pub fn filter(mut self, filter: VectorFilter<T>) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Search exactly even on an HNSW index (e.g. to measure its recall).
    pub fn exact(mut self) -> Self {
        self.exact = true;
        self
    }
}

impl<T> Default for VectorSearchOpts<T> {
    fn default() -> Self {
        Self::new(10)
    }
}

impl<T> Clone for VectorSearchOpts<T> {
    fn clone(&self) -> Self {
        VectorSearchOpts { limit: self.limit, filter: self.filter.clone(), exact: self.exact }
    }
}

impl ActionCtx {
    /// The `opts.limit` documents of `T` whose vector field is most similar to `vector` (cosine similarity, in
    /// `[-1, 1]`, higher is more similar), best first, among those matching `opts.filter`. Documents without a
    /// vector are never returned. Errors if `vector` doesn't have the index's dimensions, if `opts.limit` is not in
    /// `1..=MAX_VECTOR_RESULTS`, or if the filter uses a field that is not one of the index's filter fields.
    ///
    /// Reads the latest committed state. Load the documents with `ctx.run_query` (they may have changed since).
    pub async fn vector_search<T: Table>(
        &self,
        index: VectorIndex<T>,
        vector: impl AsRef<[f32]>,
        opts: VectorSearchOpts<T>,
    ) -> crate::Result<Vec<(Id<T>, f32)>> {
        let vector = vector.as_ref();
        let catalog = self.app.db().catalog().clone();
        let table = catalog.table(T::NAME)?;
        let name = index.name();
        let info = table
            .search_indexes
            .iter()
            .find(|i| i.name == name)
            .with_context(|| format!("vector index `{name}` not found on table `{}`", table.name))?
            .clone();
        let SpecialIndexKind::Vector { dimensions, filter_fields, .. } = &info.kind else {
            bail!("`{name}` on table `{}` is a search index, not a vector index", table.name);
        };
        if vector.len() != *dimensions {
            bail!(
                "vector_search: the query vector has {} dimensions, but vector index `{name}` of table `{}` has {dimensions}",
                vector.len(),
                table.name
            );
        }
        if vector.iter().any(|f| !f.is_finite()) {
            bail!("vector_search: the query vector contains a NaN or infinite value");
        }
        if !(1..=MAX_VECTOR_RESULTS).contains(&opts.limit) {
            bail!("vector_search: limit must be between 1 and {MAX_VECTOR_RESULTS}, got {}", opts.limit);
        }
        // Resolve the filter into branches of (column, encoded value); `None` = no filter (one unconstrained branch).
        let branches: Vec<Vec<(usize, String)>> = match &opts.filter {
            None => vec![Vec::new()],
            Some(f) => {
                if f.branches.len() > MAX_FILTER_BRANCHES {
                    bail!("vector_search: a filter may have at most {MAX_FILTER_BRANCHES} `or` branches");
                }
                let mut out = Vec::new();
                'branch: for branch in &f.branches {
                    let mut conds: Vec<(usize, String)> = Vec::new();
                    for (field, value) in branch {
                        let Some(col) = filter_fields.iter().position(|f| f == field) else {
                            bail!("vector_search: `{field}` is not a filter field of vector index `{name}` (filter fields: {filter_fields:?})");
                        };
                        let value = value.as_ref().map_err(|e| anyhow::anyhow!("vector_search: cannot serialize filter value: {e}"))?;
                        let encoded = encode_filter_value(Some(value));
                        match conds.iter().find(|(c, _)| *c == col) {
                            Some((_, v)) if *v != encoded => continue 'branch, // field == a AND field == b: no match
                            Some(_) => {}
                            None => conds.push((col, encoded)),
                        }
                    }
                    out.push(conds);
                }
                out
            }
        };
        let limit = opts.limit;
        let snapshot = self.app.db().snapshot().await?;
        // HNSW: catch the graph up to this snapshot, then search it (exact while the graph isn't ready).
        if !opts.exact
            && let Some(ann) = self.app.db().ann().index(info.id).filter(|a| a.is_ready()).cloned()
        {
            let log = self.app.db().ann().log().clone();
            let query = vector.to_vec();
            let snapshot = snapshot.clone();
            let branches = branches.clone();
            let found = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                snapshot.with_conn(|conn| ann.catch_up(conn, snapshot.ts, &log))?;
                Ok(ann.search(&query, limit, &branches))
            })
            .await??;
            if let Some(found) = found {
                return Ok(found.into_iter().map(|(id, score)| (Id::from_doc_id(id), score)).collect());
            }
        }
        let blob = to_blob(vector);
        let storage = info.storage_table();
        let rows = tokio::task::spawn_blocking(move || {
            snapshot.with_conn(|conn| {
                let mut best: Vec<(String, f64)> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for conds in &branches {
                    let where_eqs: String =
                        conds.iter().enumerate().map(|(i, (col, _))| format!(" AND f{col} = ?{}", i + 3)).collect();
                    let sql = format!(
                        "SELECT doc_id, distance FROM {storage} WHERE v MATCH ?1 AND k = ?2{where_eqs} ORDER BY distance"
                    );
                    let mut stmt = conn.prepare_cached(&sql)?;
                    let mut values: Vec<rusqlite::types::Value> = vec![blob.clone().into(), (limit as i64).into()];
                    values.extend(conds.iter().map(|(_, v)| v.clone().into()));
                    let found = stmt
                        .query_map(rusqlite::params_from_iter(values), |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?;
                    for row in found {
                        let (id, distance) = row?;
                        // A document can match several branches; keep it once.
                        if seen.insert(id.clone()) {
                            best.push((id, distance));
                        }
                    }
                }
                best.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                best.truncate(limit);
                Ok(best)
            })
        })
        .await??;
        rows.into_iter()
            .map(|(id, distance)| {
                let id = DocId::decode(&id).map_err(|_| anyhow::anyhow!("corrupt id in vector index `{name}`"))?;
                Ok((Id::from_doc_id(id), (1.0 - distance) as f32))
            })
            .collect()
    }
}

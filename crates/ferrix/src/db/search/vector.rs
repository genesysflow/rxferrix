//! Vector search with sqlite-vec: `ctx.db.vector_search(T::idx, &embedding, VectorSearchOpts::new(10))` in queries
//! and mutations (exact, reactive), and `ctx.vector_search(..)` in actions (the latest committed state, HNSW when the
//! index has a graph).
//!
//! sqlite-vec is compiled into the binary (the `sqlite-vec` crate, built against the bundled SQLite) and registered
//! for every connection with `sqlite3_auto_extension` ([`register_extension`]). Each vector index is a `vec0`
//! virtual table using the cosine distance; vec0's KNN is an exact (brute-force) scan, so results equal a brute-force
//! computation. Filter fields are vec0 metadata columns, so filters are applied during the scan (the `limit` nearest
//! *matching* documents are returned, not the nearest documents filtered afterwards).
//!
//! An HNSW index (`kind = "hnsw"`) answers action searches from its in-memory graph instead ([`super::ann`]):
//! approximate, with filters applied after the graph search (oversampled), and exact again while the graph is not
//! ready.
//!
//! ## Which one to use
//!
//! - **`ctx.db.vector_search` (queries, mutations)**: always the exact `vec0` scan at the function's snapshot (or in
//!   the mutation's transaction, seeing its own writes), also on HNSW indexes. So it is deterministic (the same
//!   snapshot gives the same results; ties are broken by document id) and a subscribed query is **reactive**: it
//!   reruns when a write changes its results, e.g. when a nearer vector is inserted. Use it when the query vector is
//!   already known to the query (stored on a document, sent by the client, a "more like this" over a stored
//!   embedding). Cost: a brute-force scan per run (about 18 ms per k=10 search over 50k × 384 vectors, 75–120 ms at
//!   200k), on every rerun.
//! - **`ctx.vector_search` (actions)**: for vectors that come from outside (an embedding API, which only actions may
//!   call), and for large indexes where the HNSW graph's ~0.4 ms approximate search matters. Not reactive (actions
//!   have no read set) and not transactional: it reads the latest committed state, and the documents may change
//!   before the action loads them (`ctx.run_query`).
//!
//! Why queries never use the graph: an HNSW search is deterministic for a fixed graph, but the graph is not a
//! function of the snapshot. It is updated after commits, rebuilt in the background (tombstones) and loaded from a
//! file at startup, so two runs at the same snapshot could walk different graphs and return different neighbours,
//! and a rerun could change its result without any write to the data it depends on. The exact scan depends only on
//! the snapshot.
//!
//! ## Reactivity (queries)
//!
//! A vector index's id is a synthetic index id, laid out like a search index's (see "Reactivity" in [`super`]): every
//! write that changes a document's entry (its vector or a filter value; inserting or deleting a document with a
//! vector) writes `[filter column i][memcomparable encoding of the value of filter field i][doc id]` for each filter
//! field, for the old entry and the new one (`[doc id]` for an index without filter fields). A query's search records,
//! for each `or` branch of its filter, the prefix range of the branch's equality on the lowest filter column, or the
//! full range for an unfiltered search (a branch without conditions). The results are a function of the entries that
//! match the filter: an entry that matches a branch before or after a write has a key under that branch's prefix, so
//! every write that can change the results is in the read set, and writes to documents outside the filter are not.
//! Branches that can't match (`field == a AND field == b`) read nothing. No `by_id` points are recorded: the results
//! are `(id, score)` pairs, which depend only on the entries; loading the documents with `ctx.db.get` records them.

use std::marker::PhantomData;
use std::sync::Once;

use anyhow::{Context, bail};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::db::catalog::Catalog;
use crate::db::query::QueryDb;
use crate::db::readset::KeyRange;
use crate::db::search::{SearchIndexInfo, SpecialIndexKind, encode_filter_value, filter_key_prefix};
use crate::id::{DocId, Id};
use crate::keys;
use crate::runtime::ActionCtx;
use crate::schema::{Field, HnswParams, Table, VectorIndex};

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

/// Options of [`QueryDb::vector_search`] and [`ActionCtx::vector_search`].
pub struct VectorSearchOpts<T> {
    /// How many results to return, 1 to [`MAX_VECTOR_RESULTS`]. Default 10.
    pub limit: usize,
    /// Only documents matching this filter. Default: all documents.
    pub filter: Option<VectorFilter<T>>,
    /// Actions on an HNSW index: search exactly (the `vec0` scan) instead of the graph. No effect on exact indexes
    /// and in queries (always exact).
    pub exact: bool,
    /// Actions on an HNSW index: the search width for this search (default: the index's `ef_search`), in
    /// `HnswParams::EF_SEARCH_RANGE`. Higher: better recall, slower. No effect on exact indexes and in queries.
    pub ef: Option<usize>,
}

impl<T> VectorSearchOpts<T> {
    pub fn new(limit: usize) -> Self {
        VectorSearchOpts { limit, filter: None, exact: false, ef: None }
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

    /// Search an HNSW index with width `ef` (instead of its `ef_search`): trade speed for recall per search.
    pub fn ef(mut self, ef: usize) -> Self {
        self.ef = Some(ef);
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
        VectorSearchOpts { limit: self.limit, filter: self.filter.clone(), exact: self.exact, ef: self.ef }
    }
}

/// A checked vector search: the index, the filter as branches of `(filter column, encoded value)` (an OR of ANDs;
/// `[[]]` = no filter, `[]` = matches nothing) and the limit.
struct VectorPlan {
    info: SearchIndexInfo,
    branches: Vec<Vec<(usize, String)>>,
    limit: usize,
}

impl VectorPlan {
    fn new<T: Table>(
        catalog: &Catalog,
        index: VectorIndex<T>,
        vector: &[f32],
        opts: &VectorSearchOpts<T>,
    ) -> anyhow::Result<VectorPlan> {
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
        if let Some(ef) = opts.ef
            && !HnswParams::EF_SEARCH_RANGE.contains(&ef)
        {
            let range = HnswParams::EF_SEARCH_RANGE;
            bail!("vector_search: ef must be between {} and {}, got {ef}", range.start(), range.end());
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
                            bail!(
                                "vector_search: `{field}` is not a filter field of vector index `{name}` (filter fields: {filter_fields:?})"
                            );
                        };
                        let value = value
                            .as_ref()
                            .map_err(|e| anyhow::anyhow!("vector_search: cannot serialize filter value: {e}"))?;
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
        Ok(VectorPlan { info, branches, limit: opts.limit })
    }

    /// The synthetic key ranges a query's search depends on (see "Reactivity" in the module docs): per branch, the
    /// prefix of its equality on the lowest filter column, or everything for a branch without conditions.
    fn read_ranges(&self) -> Vec<KeyRange> {
        let mut out = Vec::new();
        for branch in &self.branches {
            match branch.iter().min_by_key(|(col, _)| *col) {
                None => return vec![KeyRange { start: Vec::new(), end: None }],
                Some((col, hex)) => {
                    let start = filter_key_prefix(*col, hex);
                    let end = keys::prefix_successor(&start);
                    out.push(KeyRange { start, end });
                }
            }
        }
        out.sort_by(|a, b| a.start.cmp(&b.start));
        out.dedup();
        out
    }

    /// The exact search: vec0's KNN per branch, merged, best first (ties by document id), at most `limit`.
    fn exact(&self, conn: &Connection, vector: &[f32]) -> anyhow::Result<Vec<(DocId, f32)>> {
        let blob = to_blob(vector);
        let storage = self.info.storage_table();
        let mut best: Vec<(String, f64)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for conds in &self.branches {
            let where_eqs: String =
                conds.iter().enumerate().map(|(i, (col, _))| format!(" AND f{col} = ?{}", i + 3)).collect();
            let sql = format!(
                "SELECT doc_id, distance FROM {storage} WHERE v MATCH ?1 AND k = ?2{where_eqs} ORDER BY distance"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut values: Vec<rusqlite::types::Value> = vec![blob.clone().into(), (self.limit as i64).into()];
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
        best.truncate(self.limit);
        best.into_iter()
            .map(|(id, distance)| {
                let id = DocId::decode(&id)
                    .map_err(|_| anyhow::anyhow!("corrupt id in vector index `{}`", self.info.name))?;
                Ok((id, (1.0 - distance) as f32))
            })
            .collect()
    }
}

impl QueryDb {
    /// The `opts.limit` documents of `T` whose vector field is most similar to `vector` (cosine similarity, in
    /// `[-1, 1]`, higher is more similar), best first (ties by id), among those matching `opts.filter`, as `(id,
    /// score)`. Documents without a vector are never returned. Same errors as [`ActionCtx::vector_search`].
    ///
    /// Always an **exact** search (the `vec0` scan, also on HNSW indexes; `opts.exact` and `opts.ef` have no effect)
    /// at this context's snapshot, so it is deterministic and, in a query, reactive: a subscription reruns when a
    /// write changes the entries matching the filter (only those; see the module docs). Load the documents with
    /// `ctx.db.get(id)`.
    ///
    /// ```ignore
    /// #[query]
    /// pub async fn similar(ctx: &QueryCtx, id: Id<Message>) -> Result<Vec<Doc<Message>>> {
    ///     let Some(msg) = ctx.db.get(id).await? else { return Ok(vec![]) };
    ///     let Some(embedding) = &msg.embedding else { return Ok(vec![]) };
    ///     let opts = VectorSearchOpts::new(6).filter(VectorFilter::eq(Message::channel, msg.channel.clone()));
    ///     let mut out = Vec::new();
    ///     for (other, _score) in ctx.db.vector_search(Message::by_embedding, embedding, opts).await? {
    ///         if other != id && let Some(doc) = ctx.db.get(other).await? { out.push(doc); }
    ///     }
    ///     Ok(out)
    /// }
    /// ```
    pub async fn vector_search<T: Table>(
        &self,
        index: VectorIndex<T>,
        vector: impl AsRef<[f32]>,
        opts: VectorSearchOpts<T>,
    ) -> crate::Result<Vec<(Id<T>, f32)>> {
        let vector = vector.as_ref();
        let plan = VectorPlan::new(self.catalog(), index, vector, &opts)?;
        for range in plan.read_ranges() {
            self.record(plan.info.id, range);
        }
        let found = self.with_conn(|conn| plan.exact(conn, vector))?;
        Ok(found.into_iter().map(|(id, score)| (Id::from_doc_id(id), score)).collect())
    }
}

impl ActionCtx {
    /// The `opts.limit` documents of `T` whose vector field is most similar to `vector` (cosine similarity, in
    /// `[-1, 1]`, higher is more similar), best first, among those matching `opts.filter`. Documents without a
    /// vector are never returned. Errors if `vector` doesn't have the index's dimensions, if `opts.limit` is not in
    /// `1..=MAX_VECTOR_RESULTS`, if `opts.ef` is out of range, or if the filter uses a field that is not one of the
    /// index's filter fields.
    ///
    /// Reads the latest committed state; on an HNSW index, searches its graph (approximate) unless `opts.exact`.
    /// Load the documents with `ctx.run_query` (they may have changed since). For a deterministic, reactive search
    /// inside a query, use [`QueryDb::vector_search`] (see the module docs for when to use which).
    pub async fn vector_search<T: Table>(
        &self,
        index: VectorIndex<T>,
        vector: impl AsRef<[f32]>,
        opts: VectorSearchOpts<T>,
    ) -> crate::Result<Vec<(Id<T>, f32)>> {
        let vector = vector.as_ref();
        let plan = VectorPlan::new(self.app.db().catalog(), index, vector, &opts)?;
        let snapshot = self.app.db().snapshot().await?;
        // HNSW: catch the graph up to this snapshot, then search it (exact while the graph isn't ready).
        if !opts.exact
            && let Some(ann) = self.app.db().ann().index(plan.info.id).filter(|a| a.is_ready()).cloned()
        {
            let log = self.app.db().ann().log().clone();
            let query = vector.to_vec();
            let snapshot = snapshot.clone();
            let branches = plan.branches.clone();
            let (limit, ef) = (plan.limit, opts.ef);
            let found = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                snapshot.with_conn(|conn| ann.catch_up(conn, snapshot.ts, &log))?;
                Ok(ann.search(&query, limit, &branches, ef))
            })
            .await??;
            if let Some(found) = found {
                return Ok(found.into_iter().map(|(id, score)| (Id::from_doc_id(id), score)).collect());
            }
        }
        let query = vector.to_vec();
        let found = tokio::task::spawn_blocking(move || snapshot.with_conn(|conn| plan.exact(conn, &query))).await??;
        Ok(found.into_iter().map(|(id, score)| (Id::from_doc_id(id), score)).collect())
    }
}

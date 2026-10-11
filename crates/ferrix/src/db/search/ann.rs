//! Approximate vector indexes: `#[vector_index(.., kind = "hnsw")]`.
//!
//! An HNSW vector index is an ordinary vector index (its `vec0` table is written in the mutation's transaction and
//! stays the source of truth) plus an in-memory [`Graph`] that answers `ctx.vector_search` approximately. The graph is
//! an accelerator: whenever it isn't ready (still building at startup, or failed), searches use the exact `vec0` scan.
//!
//! ## Consistent with committed data
//!
//! The graph is updated *after* commit, from the write log: every write that changes a document's entry in an HNSW
//! index adds `(index id, doc id)` to its commit entry ([`super::SpecialWrites::apply`]). Catching up to a snapshot
//! at ts `S` reads the doc ids written in `(applied, S]` from the log, reads those documents' rows from `vec0` in the
//! snapshot, and applies them (a vector or filter change is a delete plus an insert), then sets `applied = S`. If the
//! log no longer covers `(applied, S]` (it keeps 5 minutes / 100k commits), the whole `vec0` table is diffed instead.
//! A background thread catches up after commits; a search catches up to its own snapshot first, so it always sees
//! every commit up to the snapshot it started from (or a later one, if the graph is already ahead).
//!
//! ## Deletes: tombstones and rebuilds
//!
//! A deleted (or changed) document's node stays in the graph as a tombstone: it still routes searches but is never
//! returned. When tombstones exceed a quarter of the nodes (and 256), the background thread rebuilds the graph from
//! the live nodes on all cores while searches keep using the old one; changes applied meanwhile are journaled and
//! replayed onto the new graph before it is swapped in.
//!
//! ## Filters: post-filtering with oversampling
//!
//! Each node keeps its filter values, and each `(filter field, value)` has a posting set of live nodes. A filtered
//! search first estimates how many live documents match (from the posting sets). If at most [`BRUTE_FORCE_MAX`] do,
//! it computes their distances exactly (always correct, and faster than a graph search at that size). Otherwise it
//! asks the graph for `k' = k · live / matching · 1.25` neighbours (at least `k`), keeps those that are live and match,
//! and retries with `4k'` (up to three times, then the exact fallback) if fewer than `k` remain. Unfiltered searches
//! are the case `matching = live`.
//!
//! ## Persistence
//!
//! Indexes with at least [`persist_threshold`] (10 000) live vectors are saved to `<data dir>/vector_index_<id>.hnsw`
//! after the initial build, after rebuilds, and after every `max(1000, live / 10)` changes (written to a temporary
//! file, then renamed). The file holds the graph, the vectors and each node's document id, filter values and a hash of
//! its vector, followed by a blake3 checksum of the rest. At startup it is loaded if its checksum, format and index
//! definition match (dimensions, filter fields, and the graph's `m` and `ef_construction`: a file built with other
//! parameters is discarded, so changing them rebuilds the graph), then **validated against the table**: the whole `vec0` table is scanned (at one snapshot) and
//! every document whose vector hash or filter values differ from its node, that has no node, or whose node has no
//! document, is applied like a catch-up. A missing, stale or corrupt file only costs a rebuild. Smaller indexes are
//! rebuilt at every startup.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension};

use crate::db::catalog::{Catalog, IndexId};
use crate::db::search::hnsw::{Graph, Params, dot, normalized};
use crate::db::search::vector::from_blob;
use crate::db::search::{SearchIndexInfo, SpecialIndexKind};
use crate::db::storage;
use crate::id::DocId;
use crate::schema::HnswParams;
use crate::writelog::WriteLog;

/// Filtered searches whose matching documents number at most this many are computed exactly.
pub const BRUTE_FORCE_MAX: usize = 4096;
/// Default search width on layer 0 (at least the number of candidates asked for; per index:
/// `#[vector_index(.., ef_search = N)]`, per search: `VectorSearchOpts::ef`). At 200k × 384 with a low intrinsic
/// dimension, 128 gives recall@10 ≈ 0.95 in about 0.5 ms (see `search::perf_hnsw`).
pub const EF_SEARCH: usize = crate::schema::HnswParams::DEFAULT.ef_search;
/// Rebuild once tombstones exceed this fraction of the nodes...
const REBUILD_RATIO: f64 = 0.25;
/// ...and this many.
const REBUILD_MIN: usize = 256;
/// How often the background thread checks for commits.
const POLL: Duration = Duration::from_millis(20);
const MAGIC: &[u8; 8] = b"FXANN\0\0\x01";

static PERSIST_THRESHOLD: AtomicUsize = AtomicUsize::new(10_000);

/// Indexes with at least this many live vectors are persisted next to `data.db`.
pub fn persist_threshold() -> usize {
    PERSIST_THRESHOLD.load(Ordering::Relaxed)
}

/// Change [`persist_threshold`] for this process (tests).
#[doc(hidden)]
pub fn set_persist_threshold(n: usize) {
    PERSIST_THRESHOLD.store(n, Ordering::Relaxed);
}

/// The HNSW indexes of one database.
///
/// Dropping it (with the `Database`) stops the background thread and waits for it, so its reader connection is
/// closed before the drop returns: a later open of the same database in this process never races with it (the last
/// connection to close checkpoints the WAL under an exclusive lock).
pub struct AnnIndexes {
    inner: Arc<Inner>,
    /// Dropped to stop the background thread (it waits on the receiving end between polls).
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AnnIndexes {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
            && thread.join().is_err()
        {
            tracing::error!("the vector index thread panicked");
        }
    }
}

struct Inner {
    indexes: HashMap<IndexId, Arc<AnnIndex>>,
    log: Arc<WriteLog>,
}

impl AnnIndexes {
    /// Start maintaining the HNSW indexes of `catalog`: a background thread loads or builds each graph, then follows
    /// the write log. Without HNSW indexes, does nothing.
    pub(crate) fn open(dir: &Path, catalog: &Catalog, log: Arc<WriteLog>) -> AnnIndexes {
        let mut indexes = HashMap::new();
        for table in catalog.tables() {
            for info in table.search_indexes.iter().filter(|i| i.is_hnsw()) {
                indexes.insert(info.id, Arc::new(AnnIndex::new(dir, &table.name, info)));
            }
        }
        remove_stale_files(dir, &indexes);
        let inner = Arc::new(Inner { indexes, log });
        let (mut stop, mut thread) = (None, None);
        if !inner.indexes.is_empty() {
            let weak = Arc::downgrade(&inner);
            let db_path = dir.join("data.db");
            let (tx, rx) = mpsc::channel();
            let spawned =
                std::thread::Builder::new().name("ferrix-ann".into()).spawn(move || background(weak, db_path, rx));
            match spawned {
                Ok(handle) => (stop, thread) = (Some(tx), Some(handle)),
                Err(e) => {
                    tracing::error!("could not start the vector index thread ({e}); vector searches will be exact")
                }
            }
        }
        AnnIndexes { inner, stop, thread }
    }

    /// The HNSW index with this id, if it is one.
    pub fn index(&self, id: IndexId) -> Option<&Arc<AnnIndex>> {
        self.inner.indexes.get(&id)
    }

    pub(crate) fn log(&self) -> &Arc<WriteLog> {
        &self.inner.log
    }

    /// Catch every ready graph up to the snapshot `conn` reads (at `ts`) and compare it with the `vec0` tables.
    /// Part of `Database::verify_indexes`.
    pub(crate) fn verify(&self, conn: &Connection, ts: u64) -> anyhow::Result<Vec<String>> {
        let mut problems = Vec::new();
        for index in self.inner.indexes.values() {
            if !index.is_ready() {
                continue;
            }
            index.catch_up(conn, ts, &self.inner.log)?;
            let mut expected: Vec<(DocId, u64, Vec<String>)> =
                scan(conn, index)?.into_iter().map(|r| (r.doc, r.hash, r.filters)).collect();
            expected.sort_by_key(|e| e.0);
            let state = index.state.read().unwrap_or_else(|e| e.into_inner());
            let Some(state) = state.as_ref() else { continue };
            let mut actual: Vec<(DocId, u64, Vec<String>)> = state
                .by_doc
                .iter()
                .map(|(doc, &n)| (*doc, state.nodes[n as usize].hash, state.nodes[n as usize].filters.clone()))
                .collect();
            actual.sort_by_key(|e| e.0);
            if actual != expected {
                problems.push(format!(
                    "table `{}` HNSW index `{}`: graph has {} live vectors, vec0 has {} (or they differ)",
                    index.table,
                    index.name,
                    actual.len(),
                    expected.len()
                ));
            }
            let postings: usize = state.postings.iter().map(|p| p.values().map(HashSet::len).sum::<usize>()).sum();
            if postings != state.by_doc.len() * index.filter_count {
                problems
                    .push(format!("table `{}` HNSW index `{}`: filter postings out of sync", index.table, index.name));
            }
        }
        Ok(problems)
    }
}

/// Delete `vector_index_<id>.hnsw` files that belong to no HNSW index (removed, or switched to exact).
fn remove_stale_files(dir: &Path, indexes: &HashMap<IndexId, Arc<AnnIndex>>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_prefix("vector_index_").and_then(|n| n.strip_suffix(".hnsw")) else { continue };
        if id.parse::<IndexId>().is_ok_and(|id| !indexes.contains_key(&id)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// What a status query reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnStatus {
    /// The graph answers searches (otherwise they are exact).
    pub ready: bool,
    /// Nodes in the graph, tombstones included.
    pub nodes: usize,
    /// Live vectors.
    pub live: usize,
    pub tombstones: usize,
    /// The commit ts the graph reflects.
    pub applied: u64,
}

/// One HNSW index.
pub struct AnnIndex {
    pub id: IndexId,
    table: String,
    name: String,
    dims: usize,
    filter_count: usize,
    /// `m` and `ef_construction` build the graph; `ef_search` is the default search width.
    params: HnswParams,
    /// The vec0 table.
    storage: String,
    file: PathBuf,
    /// `None` until the graph is loaded or built (searches are exact meanwhile).
    state: RwLock<Option<State>>,
    /// Held while changing what the graph reflects (catch-up, rebuild swap), so `applied` only moves forward.
    catch_up: Mutex<()>,
    /// While a rebuild runs: the changes applied to the old graph since its live nodes were copied.
    journal: Mutex<Option<Vec<Op>>>,
    /// Changes since the last save.
    unsaved: AtomicUsize,
    rebuilding: AtomicBool,
}

struct State {
    graph: Graph,
    nodes: Vec<NodeMeta>,
    by_doc: HashMap<DocId, u32>,
    /// Per filter column: value (hex of its memcomparable encoding) → live nodes with it.
    postings: Vec<HashMap<String, HashSet<u32>>>,
    applied: u64,
    tombstones: usize,
}

#[derive(Clone)]
struct NodeMeta {
    doc: DocId,
    filters: Vec<String>,
    /// blake3 of the vector as stored in vec0 (first 8 bytes).
    hash: u64,
    live: bool,
}

/// A document's row in the vec0 table.
#[derive(Clone)]
struct Row {
    doc: DocId,
    vector: Vec<f32>,
    filters: Vec<String>,
    hash: u64,
}

/// A change to apply: the document's current row, or its absence.
#[derive(Clone)]
enum Op {
    Upsert(Row),
    Delete(DocId),
}

fn vector_hash(blob: &[u8]) -> u64 {
    u64::from_le_bytes(blake3::hash(blob).as_bytes()[..8].try_into().expect("8 bytes"))
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

impl AnnIndex {
    fn new(dir: &Path, table: &str, info: &SearchIndexInfo) -> AnnIndex {
        let SpecialIndexKind::Vector { dimensions, filter_fields, hnsw, .. } = &info.kind else {
            unreachable!("only vector indexes are HNSW indexes")
        };
        AnnIndex {
            id: info.id,
            table: table.to_string(),
            name: info.name.clone(),
            dims: *dimensions,
            filter_count: filter_fields.len(),
            params: *hnsw,
            storage: info.storage_table(),
            file: dir.join(format!("vector_index_{}.hnsw", info.id)),
            state: RwLock::new(None),
            catch_up: Mutex::new(()),
            journal: Mutex::new(None),
            unsaved: AtomicUsize::new(0),
            rebuilding: AtomicBool::new(false),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.state.read().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// The index's parameters (from its definition).
    pub fn params(&self) -> HnswParams {
        self.params
    }

    /// The `(m, ef_construction)` the current graph was built with (`None` while it isn't ready).
    pub fn graph_params(&self) -> Option<(usize, usize)> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        state.as_ref().map(|s| (s.graph.params().m, s.graph.params().ef_construction))
    }

    /// The graph parameters of this index.
    fn build_params(&self) -> Params {
        Params { m: self.params.m, ef_construction: self.params.ef_construction }
    }

    pub fn status(&self) -> AnnStatus {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match state.as_ref() {
            None => AnnStatus { ready: false, nodes: 0, live: 0, tombstones: 0, applied: 0 },
            Some(s) => AnnStatus {
                ready: true,
                nodes: s.graph.len(),
                live: s.by_doc.len(),
                tombstones: s.tombstones,
                applied: s.applied,
            },
        }
    }

    /// Wait until the graph is ready (or `timeout` passes); returns whether it is.
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while !self.is_ready() {
            if start.elapsed() > timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }

    // ----------------------------------------------------------------------------------------------- loading

    /// Load (and validate) or build the graph at the snapshot `conn` is in.
    fn initialize(&self, conn: &Connection, ts: u64) -> anyhow::Result<()> {
        let _guard = self.catch_up.lock().unwrap_or_else(|e| e.into_inner());
        let started = Instant::now();
        let loaded = if self.file.exists() {
            match self.load_file() {
                Ok(state) => Some(state),
                Err(e) => {
                    tracing::warn!(table = %self.table, index = %self.name, "ignoring the saved HNSW graph: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let rows = scan(conn, self)?;
        let mut state = if let Some(mut state) = loaded {
            // Validate against the table: apply every difference between the saved nodes and the vec0 rows.
            let mut seen: HashSet<DocId> = HashSet::new();
            let mut ops = Vec::new();
            for row in rows {
                seen.insert(row.doc);
                if !state.has(&row) {
                    ops.push(Op::Upsert(row));
                }
            }
            ops.extend(state.by_doc.keys().filter(|d| !seen.contains(d)).map(|d| Op::Delete(*d)));
            let changed = ops.len();
            state.apply(self, ops);
            tracing::info!(
                table = %self.table, index = %self.name, live = state.by_doc.len(), changed,
                elapsed_ms = started.elapsed().as_millis() as u64, "loaded the saved HNSW graph"
            );
            self.unsaved.store(changed, Ordering::Relaxed);
            state
        } else {
            let state = State::build(self, rows);
            tracing::info!(
                table = %self.table, index = %self.name, live = state.by_doc.len(),
                elapsed_ms = started.elapsed().as_millis() as u64, "built the HNSW graph"
            );
            self.unsaved.store(usize::MAX / 2, Ordering::Relaxed);
            state
        };
        state.applied = ts;
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = Some(state);
        Ok(())
    }

    // ----------------------------------------------------------------------------------------------- catch-up

    /// Bring the graph up to the snapshot `conn` reads, at commit `ts`. No-op if it is not ready or already there.
    pub(crate) fn catch_up(&self, conn: &Connection, ts: u64, log: &WriteLog) -> anyhow::Result<()> {
        let _guard = self.catch_up.lock().unwrap_or_else(|e| e.into_inner());
        let applied = match self.state.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(s) => s.applied,
            None => return Ok(()),
        };
        if ts <= applied {
            return Ok(());
        }
        // A snapshot can see a commit a moment before its log entry is appended.
        let deadline = Instant::now() + Duration::from_secs(1);
        while log.latest() < ts && Instant::now() < deadline {
            std::thread::sleep(Duration::from_micros(200));
        }
        let ops: Vec<Op> = match log.entries_between(applied, ts) {
            Some(entries) => {
                let docs: BTreeSet<[u8; 16]> = entries
                    .iter()
                    .flat_map(|e| e.writes.iter())
                    .filter(|(id, _)| *id == self.id)
                    .filter_map(|(_, key)| <[u8; 16]>::try_from(key.as_slice()).ok())
                    .collect();
                let mut ops = Vec::with_capacity(docs.len());
                for doc in docs {
                    let doc = DocId(doc);
                    ops.push(match read_row(conn, self, doc)? {
                        Some(row) => Op::Upsert(row),
                        None => Op::Delete(doc),
                    });
                }
                ops
            }
            None => {
                // The log no longer covers the gap: diff the whole table.
                let rows = scan(conn, self)?;
                let state = self.state.read().unwrap_or_else(|e| e.into_inner());
                let Some(state) = state.as_ref() else { return Ok(()) };
                let seen: HashSet<DocId> = rows.iter().map(|r| r.doc).collect();
                let mut ops: Vec<Op> = rows.into_iter().filter(|r| !state.has(r)).map(Op::Upsert).collect();
                ops.extend(state.by_doc.keys().filter(|d| !seen.contains(d)).map(|d| Op::Delete(*d)));
                ops
            }
        };
        if !ops.is_empty()
            && let Some(journal) = self.journal.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            journal.extend(ops.iter().cloned());
        }
        self.unsaved.fetch_add(ops.len(), Ordering::Relaxed);
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = state.as_mut() {
            state.apply(self, ops);
            state.applied = ts;
        }
        Ok(())
    }

    // ----------------------------------------------------------------------------------------------- search

    /// The `limit` live documents nearest to `vector` among those matching `branches` (an OR of ANDs of `(filter
    /// column, hex value)`; `[[]]` matches everything), best first, as `(doc id, cosine similarity)`, searching
    /// with width `ef` (default: the index's `ef_search`). `None` if the graph is not ready.
    pub(crate) fn search(
        &self,
        vector: &[f32],
        limit: usize,
        branches: &[Vec<(usize, String)>],
        ef: Option<usize>,
    ) -> Option<Vec<(DocId, f32)>> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        let state = state.as_ref()?;
        Some(state.search(vector, limit, branches, ef.unwrap_or(self.params.ef_search)))
    }

    // ----------------------------------------------------------------------------------------------- rebuild

    fn needs_rebuild(&self) -> bool {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        state
            .as_ref()
            .is_some_and(|s| s.tombstones >= REBUILD_MIN && s.tombstones as f64 >= REBUILD_RATIO * s.graph.len() as f64)
    }

    /// Rebuild the graph from its live nodes (dropping tombstones). Searches use the old graph meanwhile.
    pub fn rebuild(&self) {
        if self.rebuilding.swap(true, Ordering::SeqCst) {
            return;
        }
        let started = Instant::now();
        // Copy the live nodes at a consistent point (no catch-up in between) and start journaling.
        let (rows, applied) = {
            let _guard = self.catch_up.lock().unwrap_or_else(|e| e.into_inner());
            let state = self.state.read().unwrap_or_else(|e| e.into_inner());
            let Some(state) = state.as_ref() else {
                self.rebuilding.store(false, Ordering::SeqCst);
                return;
            };
            let mut live: Vec<(&DocId, &u32)> = state.by_doc.iter().collect();
            live.sort_by_key(|(_, n)| **n);
            let rows: Vec<Row> = live
                .into_iter()
                .map(|(doc, &n)| {
                    let meta = &state.nodes[n as usize];
                    Row {
                        doc: *doc,
                        vector: state.graph.vector(n).to_vec(),
                        filters: meta.filters.clone(),
                        hash: meta.hash,
                    }
                })
                .collect();
            *self.journal.lock().unwrap_or_else(|e| e.into_inner()) = Some(Vec::new());
            (rows, state.applied)
        };
        let mut fresh = State::build(self, rows);
        fresh.applied = applied;
        {
            let _guard = self.catch_up.lock().unwrap_or_else(|e| e.into_inner());
            let ops = self.journal.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
            let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
            if let Some(old) = state.as_ref() {
                fresh.apply(self, ops);
                fresh.applied = old.applied;
                *state = Some(fresh);
            }
        }
        self.unsaved.store(usize::MAX / 2, Ordering::Relaxed);
        self.rebuilding.store(false, Ordering::SeqCst);
        tracing::info!(table = %self.table, index = %self.name, elapsed_ms = started.elapsed().as_millis() as u64, "rebuilt the HNSW graph");
    }

    // ----------------------------------------------------------------------------------------------- persistence

    fn needs_save(&self) -> bool {
        let live = {
            let state = self.state.read().unwrap_or_else(|e| e.into_inner());
            match state.as_ref() {
                Some(s) => s.by_doc.len(),
                None => return false,
            }
        };
        live >= persist_threshold() && self.unsaved.load(Ordering::Relaxed) >= (live / 10).max(1000).min(live.max(1))
    }

    /// Write the graph to its file (a temporary file renamed over it).
    pub fn save(&self) -> anyhow::Result<()> {
        static SAVES: AtomicUsize = AtomicUsize::new(0);
        let n = SAVES.fetch_add(1, Ordering::Relaxed);
        let tmp = self.file.with_extension(format!("hnsw.tmp{}-{n}", std::process::id()));
        {
            let state = self.state.read().unwrap_or_else(|e| e.into_inner());
            let Some(state) = state.as_ref() else { return Ok(()) };
            let file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            let mut w = HashingWriter { inner: BufWriter::new(file), hasher: blake3::Hasher::new() };
            w.write_all(MAGIC)?;
            w.write_all(&self.id.to_le_bytes())?;
            w.write_all(&(self.dims as u32).to_le_bytes())?;
            w.write_all(&(self.filter_count as u32).to_le_bytes())?;
            w.write_all(&state.applied.to_le_bytes())?;
            w.write_all(&(state.nodes.len() as u64).to_le_bytes())?;
            for meta in &state.nodes {
                w.write_all(&meta.doc.0)?;
                w.write_all(&[u8::from(meta.live)])?;
                w.write_all(&meta.hash.to_le_bytes())?;
                for f in &meta.filters {
                    w.write_all(&(f.len() as u32).to_le_bytes())?;
                    w.write_all(f.as_bytes())?;
                }
            }
            state.graph.write_to(&mut w)?;
            let digest = w.hasher.finalize();
            w.inner.write_all(digest.as_bytes())?;
            w.inner.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            self.unsaved.store(0, Ordering::Relaxed);
        }
        std::fs::rename(&tmp, &self.file).with_context(|| format!("renaming {}", tmp.display()))?;
        Ok(())
    }

    fn load_file(&self) -> anyhow::Result<State> {
        let file = std::fs::File::open(&self.file)?;
        let mut r = HashingReader { inner: BufReader::with_capacity(1 << 20, file), hasher: blake3::Hasher::new() };
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        ensure!(&magic == MAGIC, "unknown format");
        let mut u32b = [0u8; 4];
        let mut u64b = [0u8; 8];
        let mut read_u32 = |r: &mut HashingReader<_>| -> anyhow::Result<u32> {
            r.read_exact(&mut u32b)?;
            Ok(u32::from_le_bytes(u32b))
        };
        ensure!(read_u32(&mut r)? == self.id, "saved for another index");
        ensure!(read_u32(&mut r)? as usize == self.dims, "saved with other dimensions");
        ensure!(read_u32(&mut r)? as usize == self.filter_count, "saved with other filter fields");
        r.read_exact(&mut u64b)?;
        let applied = u64::from_le_bytes(u64b);
        r.read_exact(&mut u64b)?;
        let n = u64::from_le_bytes(u64b) as usize;
        ensure!(n <= u32::MAX as usize, "too many nodes");
        let mut nodes = Vec::with_capacity(n.min(1 << 24));
        for _ in 0..n {
            let mut doc = [0u8; 16];
            r.read_exact(&mut doc)?;
            let mut live = [0u8; 1];
            r.read_exact(&mut live)?;
            r.read_exact(&mut u64b)?;
            let hash = u64::from_le_bytes(u64b);
            let mut filters = Vec::with_capacity(self.filter_count);
            for _ in 0..self.filter_count {
                let len = read_u32(&mut r)? as usize;
                ensure!(len <= 1 << 20, "filter value too long");
                let mut s = vec![0u8; len];
                r.read_exact(&mut s)?;
                filters.push(String::from_utf8(s).context("filter value is not UTF-8")?);
            }
            ensure!(live[0] <= 1, "bad node flag");
            nodes.push(NodeMeta { doc: DocId(doc), filters, hash, live: live[0] == 1 });
        }
        let graph = Graph::read_from(&mut r)?;
        ensure!(graph.params() == self.build_params(), "saved with other HNSW parameters (m, ef_construction)");
        let digest = r.hasher.finalize();
        let mut stored = [0u8; 32];
        r.inner.read_exact(&mut stored).context("missing checksum")?;
        ensure!(digest.as_bytes() == &stored, "checksum mismatch");
        ensure!(r.inner.read(&mut [0u8; 1])? == 0, "trailing bytes");
        ensure!(graph.len() == n && graph.dims() == self.dims, "graph does not match its nodes");
        let mut state = State {
            graph,
            nodes: Vec::new(),
            by_doc: HashMap::new(),
            postings: vec![HashMap::new(); self.filter_count],
            applied,
            tombstones: 0,
        };
        for (i, meta) in nodes.iter().enumerate() {
            if meta.live {
                ensure!(state.by_doc.insert(meta.doc, i as u32).is_none(), "duplicate document");
                for (col, v) in meta.filters.iter().enumerate() {
                    state.postings[col].entry(v.clone()).or_default().insert(i as u32);
                }
            } else {
                state.tombstones += 1;
            }
        }
        state.nodes = nodes;
        Ok(state)
    }
}

impl State {
    /// A fresh graph over `rows` (built on all cores).
    fn build(index: &AnnIndex, rows: Vec<Row>) -> State {
        let mut state = State {
            graph: Graph::new(index.dims, index.build_params(), u64::from(index.id) ^ 0x5eed),
            nodes: Vec::new(),
            by_doc: HashMap::new(),
            postings: vec![HashMap::new(); index.filter_count],
            applied: 0,
            tombstones: 0,
        };
        state.apply(index, rows.into_iter().map(Op::Upsert).collect());
        state
    }

    /// Is `row` exactly what the graph holds for its document?
    fn has(&self, row: &Row) -> bool {
        self.by_doc.get(&row.doc).is_some_and(|&n| {
            let meta = &self.nodes[n as usize];
            meta.hash == row.hash && meta.filters == row.filters
        })
    }

    fn kill(&mut self, node: u32) {
        let meta = &mut self.nodes[node as usize];
        if !meta.live {
            return;
        }
        meta.live = false;
        self.by_doc.remove(&meta.doc);
        for (col, v) in meta.filters.iter().enumerate() {
            if let Some(set) = self.postings[col].get_mut(v) {
                set.remove(&node);
                if set.is_empty() {
                    self.postings[col].remove(v);
                }
            }
        }
        self.tombstones += 1;
    }

    /// Apply changes: deletes become tombstones; new or changed rows become new nodes, linked on all cores when there
    /// are many.
    fn apply(&mut self, index: &AnnIndex, ops: Vec<Op>) {
        let first_new = self.graph.len();
        for op in ops {
            match op {
                Op::Delete(doc) => {
                    if let Some(&n) = self.by_doc.get(&doc) {
                        self.kill(n);
                    }
                }
                Op::Upsert(row) => {
                    if self.has(&row) {
                        continue;
                    }
                    if let Some(&n) = self.by_doc.get(&row.doc) {
                        self.kill(n);
                    }
                    if row.vector.len() != index.dims {
                        continue;
                    }
                    let node = self.graph.push(&row.vector);
                    for (col, v) in row.filters.iter().enumerate() {
                        self.postings[col].entry(v.clone()).or_default().insert(node);
                    }
                    self.by_doc.insert(row.doc, node);
                    self.nodes.push(NodeMeta { doc: row.doc, filters: row.filters, hash: row.hash, live: true });
                }
            }
        }
        let new = self.graph.len() - first_new;
        let graph = &self.graph;
        if new < 64 {
            for n in first_new..graph.len() {
                graph.link(n as u32);
            }
        } else {
            let next = AtomicUsize::new(first_new);
            std::thread::scope(|s| {
                for _ in 0..threads().min(new) {
                    s.spawn(|| {
                        loop {
                            let n = next.fetch_add(1, Ordering::Relaxed);
                            if n >= graph.len() {
                                break;
                            }
                            graph.link(n as u32);
                        }
                    });
                }
            });
        }
    }

    fn matches(&self, node: u32, branches: &[Vec<(usize, String)>]) -> bool {
        let meta = &self.nodes[node as usize];
        meta.live && branches.iter().any(|b| b.iter().all(|(col, v)| meta.filters.get(*col) == Some(v)))
    }

    /// An upper bound on the live documents matching `branches`.
    fn estimate(&self, branches: &[Vec<(usize, String)>]) -> usize {
        let live = self.by_doc.len();
        let n: usize = branches
            .iter()
            .map(|b| {
                b.iter()
                    .map(|(col, v)| self.postings.get(*col).and_then(|p| p.get(v)).map_or(0, HashSet::len))
                    .min()
                    .unwrap_or(live)
            })
            .sum();
        n.min(live)
    }

    fn search(&self, vector: &[f32], limit: usize, branches: &[Vec<(usize, String)>], ef: usize) -> Vec<(DocId, f32)> {
        let live = self.by_doc.len();
        let matching = self.estimate(branches);
        if matching == 0 || limit == 0 {
            return Vec::new();
        }
        let accept = |n: u32| self.matches(n, branches);
        let mut hits: Option<Vec<(f32, u32)>> = None;
        if matching > BRUTE_FORCE_MAX {
            let mut k = ((limit as f64) * (live as f64 / matching as f64) * 1.25).ceil() as usize;
            k = k.clamp(limit, live.max(limit));
            for _ in 0..3 {
                let found = self.graph.search(vector, k, ef.max(k), accept);
                if found.len() >= limit || k >= self.graph.len() {
                    hits = Some(found);
                    break;
                }
                k = (k * 4).min(self.graph.len());
            }
        }
        // Few matches (or the graph search kept coming up short): compute the matching documents exactly.
        let mut hits = hits.unwrap_or_else(|| self.brute_force(vector, branches));
        hits.truncate(limit);
        hits.into_iter().map(|(d, n)| (self.nodes[n as usize].doc, 1.0 - d)).collect()
    }

    /// Exact distances to every live node matching `branches`, closest first.
    fn brute_force(&self, vector: &[f32], branches: &[Vec<(usize, String)>]) -> Vec<(f32, u32)> {
        let q = normalized(vector);
        let mut candidates: HashSet<u32> = HashSet::new();
        for branch in branches {
            // The smallest posting set of the branch (or every live node, without conditions), checked fully.
            let smallest = branch
                .iter()
                .map(|(col, v)| self.postings.get(*col).and_then(|p| p.get(v)))
                .min_by_key(|s| s.map_or(0, |s| s.len()));
            match smallest {
                None => candidates.extend(self.by_doc.values().copied()),
                Some(None) => {}
                Some(Some(set)) => {
                    candidates.extend(set.iter().copied().filter(|&n| self.matches(n, std::slice::from_ref(branch))))
                }
            }
        }
        let mut out: Vec<(f32, u32)> =
            candidates.into_iter().map(|n| (1.0 - dot(&q, self.graph.vector(n)), n)).collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        out
    }
}

/// Every row of the index's vec0 table.
fn scan(conn: &Connection, index: &AnnIndex) -> anyhow::Result<Vec<Row>> {
    let cols: String = (0..index.filter_count).map(|i| format!(", f{i}")).collect();
    let mut stmt = conn.prepare(&format!("SELECT doc_id, v{cols} FROM {}", index.storage))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(row_of(r, index, None)?);
    }
    Ok(out)
}

/// The vec0 row of one document, if it has one.
fn read_row(conn: &Connection, index: &AnnIndex, doc: DocId) -> anyhow::Result<Option<Row>> {
    let cols: String = (0..index.filter_count).map(|i| format!(", f{i}")).collect();
    let mut stmt = conn.prepare_cached(&format!("SELECT doc_id, v{cols} FROM {} WHERE doc_id = ?1", index.storage))?;
    let row = stmt.query_row([doc.encode()], |r| Ok(row_of(r, index, Some(doc)))).optional()?;
    row.transpose()
}

fn row_of(r: &rusqlite::Row<'_>, index: &AnnIndex, doc: Option<DocId>) -> anyhow::Result<Row> {
    let doc = match doc {
        Some(d) => d,
        None => DocId::decode(&r.get::<_, String>(0)?).map_err(|_| anyhow::anyhow!("corrupt id in vector index"))?,
    };
    let blob: Vec<u8> = r.get(1)?;
    let filters = (0..index.filter_count).map(|i| r.get::<_, String>(i + 2)).collect::<Result<_, _>>()?;
    Ok(Row { doc, hash: vector_hash(&blob), vector: from_blob(&blob), filters })
}

/// The background thread: load or build every graph, then follow commits, rebuild and save as needed. Exits (closing
/// its connection) as soon as `stop` disconnects, i.e. when the `AnnIndexes` is dropped, which waits for it.
fn background(weak: Weak<Inner>, db_path: PathBuf, stop: mpsc::Receiver<()>) {
    let stopped = || matches!(stop.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    let conn = match storage::open_reader(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("vector index thread: {e:#}; vector searches will be exact");
            return;
        }
    };
    // In a read transaction, at the snapshot's ts.
    let at_snapshot = |f: &mut dyn FnMut(u64) -> anyhow::Result<()>| -> anyhow::Result<()> {
        conn.execute_batch("BEGIN")?;
        let result = storage::read_meta_u64(&conn, storage::META_TS).and_then(&mut *f);
        conn.execute_batch("COMMIT")?;
        result
    };
    let mut retry_at: HashMap<IndexId, Instant> = HashMap::new();
    let mut seen = 0u64;
    loop {
        if stopped() {
            return;
        }
        let Some(inner) = weak.upgrade() else { return };
        let latest = inner.log.latest();
        let mut indexes: Vec<&Arc<AnnIndex>> = inner.indexes.values().collect();
        indexes.sort_by_key(|i| i.id);
        let result = at_snapshot(&mut |ts| {
            for index in &indexes {
                if index.is_ready() {
                    if ts > seen {
                        index.catch_up(&conn, ts, &inner.log)?;
                    }
                } else if retry_at.get(&index.id).is_none_or(|t| Instant::now() >= *t)
                    && let Err(e) = index.initialize(&conn, ts)
                {
                    tracing::error!(table = %index.table, index = %index.name, "could not build the HNSW graph: {e:#}");
                    retry_at.insert(index.id, Instant::now() + Duration::from_secs(5));
                }
            }
            seen = seen.max(ts);
            Ok(())
        });
        if let Err(e) = result {
            tracing::warn!("vector index thread: {e:#}");
        }
        for index in &indexes {
            if stopped() {
                return;
            }
            if index.needs_rebuild() {
                index.rebuild();
            }
            if index.needs_save()
                && let Err(e) = index.save()
            {
                tracing::warn!(table = %index.table, index = %index.name, "could not save the HNSW graph: {e:#}");
            }
        }
        drop(indexes);
        drop(inner);
        // Wait for the next commit (or a few polls), returning as soon as the database goes away.
        let start = Instant::now();
        loop {
            if !matches!(stop.recv_timeout(POLL), Err(mpsc::RecvTimeoutError::Timeout)) {
                return;
            }
            let Some(inner) = weak.upgrade() else { return };
            if inner.log.latest() > latest.max(seen) || start.elapsed() > Duration::from_secs(1) {
                break;
            }
        }
    }
}

struct HashingWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct HashingReader<R: Read> {
    inner: R,
    hasher: blake3::Hasher,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

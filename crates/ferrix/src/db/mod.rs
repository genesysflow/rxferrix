//! The database: one writer connection, a pool of reader connections, the catalog and the write log.
//!
//! ## Concurrency model
//!
//! * **Writes** are serialized through a single read-write connection behind a `tokio::sync::Mutex`. A mutation holds
//!   the lock for its whole duration and runs inside `BEGIN IMMEDIATE`, so mutations are serializable without any
//!   conflict detection or retries. Its reads go through the same connection and therefore see its own writes.
//! * **Reads** use read-only connections. A [`Snapshot`] is a read transaction: `BEGIN` followed by a read of
//!   `_meta.ts`. The first read pins the WAL snapshot, and because the writer updates `_meta.ts` in the same transaction
//!   as the data, the value read is exactly the commit ts the snapshot reflects. One snapshot can serve several
//!   `QueryCtx` at once (each recording its own read set): the sync worker uses this to rerun many queries at one
//!   consistent ts.
//!
//! rusqlite connections are `Send` but not `Sync`, so each is wrapped in a `std::sync::Mutex` that is only ever held
//! for the duration of synchronous SQLite calls, never across an `.await`.

pub mod catalog;
pub mod pagination;
pub mod query;
pub mod readset;
pub mod storage;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use rusqlite::Connection;
use serde_json::{Map, Value};
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

use crate::db::catalog::{Catalog, IndexId, TableInfo};
use crate::id::DocId;
use crate::keys;
use crate::schema::{CREATION_TIME_FIELD, ID_FIELD, TableDef};
use crate::writelog::{CommitEntry, WriteLog};

/// Default number of reader connections.
pub const DEFAULT_READERS: usize = 6;

pub struct Database {
    writer: Arc<tokio::sync::Mutex<Connection>>,
    readers: Arc<ReaderPool>,
    catalog: Arc<Catalog>,
    write_log: Arc<WriteLog>,
}

impl Database {
    /// Open `<dir>/data.db`, creating it if needed, and reconcile the catalog with `tables`.
    pub fn open(dir: &Path, tables: &[TableDef], readers: usize) -> anyhow::Result<Database> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating data dir {}", dir.display()))?;
        let path = dir.join("data.db");
        let mut writer = storage::open_writer(&path)?;
        let catalog = catalog::reconcile(&mut writer, tables)?;
        let ts = storage::read_meta_u64(&writer, storage::META_TS)?;
        let readers = readers.max(1);
        Ok(Database {
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            readers: Arc::new(ReaderPool { path, conns: Mutex::new(Vec::new()), permits: Arc::new(Semaphore::new(readers)) }),
            catalog: Arc::new(catalog),
            write_log: Arc::new(WriteLog::new(ts)),
        })
    }

    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    pub fn write_log(&self) -> &Arc<WriteLog> {
        &self.write_log
    }

    /// Open a read snapshot at the latest committed ts.
    pub async fn snapshot(&self) -> anyhow::Result<Arc<Snapshot>> {
        let permit = self.readers.permits.clone().acquire_owned().await?;
        let conn = match self.readers.conns.lock().unwrap().pop() {
            Some(c) => c,
            None => storage::open_reader(&self.readers.path)?,
        };
        // BEGIN is deferred: the snapshot is pinned by the first read, which is the ts read itself.
        let begun = (|| -> anyhow::Result<(u64, f64)> {
            conn.execute_batch("BEGIN")?;
            let ts = storage::read_meta_u64(&conn, storage::META_TS)?;
            let time = storage::read_meta_f64(&conn, storage::META_TIME)?;
            Ok((ts, time))
        })();
        let (ts, time) = match begun {
            Ok(v) => v,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        };
        Ok(Arc::new(Snapshot { conn: Mutex::new(Some(conn)), pool: self.readers.clone(), ts, time, _permit: permit }))
    }

    /// Recompute every stored index from the documents and compare with `index_entries` (at one snapshot).
    /// Returns a description of each mismatch; empty means consistent. Meant for tests and offline checks.
    pub async fn verify_indexes(&self) -> anyhow::Result<Vec<String>> {
        let snapshot = self.snapshot().await?;
        let catalog = self.catalog.clone();
        snapshot.with_conn(|conn| {
            let mut problems = Vec::new();
            for table in catalog.tables() {
                let docs = storage::table_documents(conn, table.number)?;
                for index in table.stored_indexes() {
                    let mut expected: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                    for (id, json) in &docs {
                        let doc: Value = serde_json::from_str(json)?;
                        let obj = doc.as_object().context("document is not an object")?;
                        expected.push((keys::index_key(obj, &index.fields, id), id.to_vec()));
                    }
                    expected.sort();
                    let mut stmt =
                        conn.prepare("SELECT key, doc_id FROM index_entries WHERE index_id = ?1 ORDER BY key")?;
                    let actual: Vec<(Vec<u8>, Vec<u8>)> = stmt
                        .query_map([index.id], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<Result<_, _>>()?;
                    if actual != expected {
                        problems.push(format!(
                            "table `{}` index `{}`: expected {} entries, found {} (or contents differ)",
                            table.name,
                            index.name,
                            expected.len(),
                            actual.len()
                        ));
                    }
                }
            }
            // Entries pointing at indexes that no longer exist.
            let orphans: i64 = conn.query_row(
                "SELECT COUNT(*) FROM index_entries WHERE index_id NOT IN (SELECT index_id FROM _indexes)",
                [],
                |r| r.get(0),
            )?;
            if orphans > 0 {
                problems.push(format!("{orphans} index entries belong to no index"));
            }
            Ok(problems)
        })
    }

    /// Lock the writer and begin a write transaction.
    pub async fn begin_write(&self) -> anyhow::Result<Arc<WriteTxn>> {
        let guard = self.writer.clone().lock_owned().await;
        // A previous transaction can only still be open if its future was dropped mid-flight *and* the rollback in
        // `WriteState::drop` failed; be defensive anyway.
        if !guard.is_autocommit() {
            guard.execute_batch("ROLLBACK")?;
        }
        guard.execute_batch("BEGIN IMMEDIATE")?;
        let begun = (|| -> anyhow::Result<(u64, f64, f64)> {
            Ok((
                storage::read_meta_u64(&guard, storage::META_TS)?,
                storage::read_meta_f64(&guard, storage::META_TIME)?,
                storage::read_meta_f64(&guard, storage::META_LAST_CREATION_TIME)?,
            ))
        })();
        let (prev_ts, last_time, last_creation_time) = match begun {
            Ok(v) => v,
            Err(e) => {
                let _ = guard.execute_batch("ROLLBACK");
                return Err(e);
            }
        };
        let now = wall_clock_ms().max(last_time);
        let state = WriteState { conn: guard, prev_ts, now, last_creation_time, writes: Vec::new() };
        Ok(Arc::new(WriteTxn {
            state: Mutex::new(Some(state)),
            catalog: self.catalog.clone(),
            write_log: self.write_log.clone(),
        }))
    }
}

pub fn wall_clock_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

struct ReaderPool {
    path: PathBuf,
    conns: Mutex<Vec<Connection>>,
    permits: Arc<Semaphore>,
}

/// A consistent read-only view of the database at commit `ts`. Ends the read transaction when dropped.
pub struct Snapshot {
    conn: Mutex<Option<Connection>>,
    pool: Arc<ReaderPool>,
    /// The commit ts this snapshot reflects.
    pub ts: u64,
    /// Wall-clock ms of that commit (0 before the first commit).
    pub time: f64,
    _permit: OwnedSemaphorePermit,
}

impl Snapshot {
    pub(crate) fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<R>) -> anyhow::Result<R> {
        let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.as_ref().context("snapshot already closed")?;
        f(conn)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let conn = self.conn.get_mut().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(conn) = conn {
            // Ending the read transaction releases the WAL snapshot so checkpoints can progress.
            if conn.execute_batch("COMMIT").is_ok() || conn.is_autocommit() {
                self.pool.conns.lock().unwrap_or_else(|e| e.into_inner()).push(conn);
            }
            // Otherwise the connection is in an unknown state: drop it; the pool opens a new one on demand.
        }
    }
}

/// An open write transaction. Shared (via `Arc`) between the runtime and the `MutationCtx` handed to user code.
pub struct WriteTxn {
    state: Mutex<Option<WriteState>>,
    pub(crate) catalog: Arc<Catalog>,
    write_log: Arc<WriteLog>,
}

pub(crate) struct WriteState {
    conn: OwnedMutexGuard<Connection>,
    prev_ts: u64,
    now: f64,
    last_creation_time: f64,
    writes: Vec<(IndexId, Vec<u8>)>,
}

impl Drop for WriteState {
    fn drop(&mut self) {
        // Reached without commit on error, panic, or when the mutation future is dropped: undo everything.
        if !self.conn.is_autocommit()
            && let Err(e) = self.conn.execute_batch("ROLLBACK")
        {
            tracing::error!("rollback failed: {e}");
        }
    }
}

impl WriteTxn {
    pub(crate) fn with_state<R>(&self, f: impl FnOnce(&mut WriteState) -> anyhow::Result<R>) -> anyhow::Result<R> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = guard.as_mut().context("the mutation's transaction has already finished")?;
        f(state)
    }

    /// The commit time of this transaction (wall clock at begin, never earlier than the previous commit).
    pub fn now(&self) -> f64 {
        self.with_state(|s| Ok(s.now)).unwrap_or(0.0)
    }

    /// Commit. Returns the new ts, or the current ts if nothing was written (then ts is not bumped).
    pub(crate) fn commit(&self) -> anyhow::Result<u64> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut state = state.context("the mutation's transaction has already finished")?;
        if state.writes.is_empty() {
            state.conn.execute_batch("COMMIT")?;
            return Ok(state.prev_ts);
        }
        let ts = state.prev_ts + 1;
        storage::write_meta(&state.conn, storage::META_TS, (ts as i64).into())?;
        storage::write_meta(&state.conn, storage::META_TIME, state.now.into())?;
        storage::write_meta(&state.conn, storage::META_LAST_CREATION_TIME, state.last_creation_time.into())?;
        state.conn.execute_batch("COMMIT")?;
        // Still holding the writer lock (inside `state.conn`), so log entries are appended in ts order.
        self.write_log.append(CommitEntry { ts, writes: std::mem::take(&mut state.writes) });
        Ok(ts)
    }

    /// Roll back (also happens implicitly on drop).
    pub(crate) fn rollback(&self) {
        drop(self.state.lock().unwrap_or_else(|e| e.into_inner()).take());
    }
}

impl WriteState {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    fn next_creation_time(&mut self) -> f64 {
        // Strictly increasing, so `by_creation_time` preserves insertion order even within one mutation.
        let t = self.now.max(self.last_creation_time.next_up());
        self.last_creation_time = t;
        t
    }

    fn load(&self, table: &TableInfo, id: DocId) -> anyhow::Result<Map<String, Value>> {
        check_table(table, id)?;
        let json = storage::get_document(&self.conn, &id.0)?.with_context(|| format!("document {id} not found"))?;
        match serde_json::from_str(&json)? {
            Value::Object(m) => Ok(m),
            _ => bail!("stored document {id} is not an object"),
        }
    }

    /// Write a new version of a document (`new = None` deletes it), maintaining every index and recording the old
    /// and new keys for the write log.
    fn write(
        &mut self,
        table: &TableInfo,
        id: DocId,
        old: Option<&Map<String, Value>>,
        new: Option<&Map<String, Value>>,
    ) -> anyhow::Result<()> {
        let ts = self.prev_ts + 1;
        for index in table.stored_indexes() {
            let old_key = old.map(|d| keys::index_key(d, &index.fields, &id.0));
            let new_key = new.map(|d| keys::index_key(d, &index.fields, &id.0));
            if old_key != new_key {
                if let Some(k) = &old_key {
                    storage::delete_index_entry(&self.conn, index.id, k)?;
                }
                if let Some(k) = &new_key {
                    storage::insert_index_entry(&self.conn, index.id, k, &id.0)?;
                }
            }
            // Record the key even when it didn't change: the document's content did, and a query that read this key
            // through this index must be invalidated.
            match (old_key, new_key) {
                (Some(o), Some(n)) if o == n => self.writes.push((index.id, o)),
                (o, n) => self.writes.extend(o.into_iter().chain(n).map(|k| (index.id, k))),
            }
        }
        self.writes.push((table.by_id().id, id.0.to_vec()));
        match new {
            Some(doc) => storage::put_document(&self.conn, &id.0, table.number, &serde_json::to_string(doc)?, ts)?,
            None => storage::delete_document(&self.conn, &id.0)?,
        }
        Ok(())
    }

    /// Insert a document from user fields; adds `_id` and `_creationTime`.
    pub(crate) fn insert(&mut self, table: &TableInfo, mut fields: Map<String, Value>) -> anyhow::Result<DocId> {
        reject_system_fields(&fields)?;
        let id = DocId::new_random(table.number);
        anyhow::ensure!(storage::get_document(&self.conn, &id.0)?.is_none(), "id collision");
        fields.insert(ID_FIELD.into(), Value::String(id.encode()));
        let ct = self.next_creation_time();
        fields.insert(CREATION_TIME_FIELD.into(), serde_json::Number::from_f64(ct).context("bad time")?.into());
        self.write(table, id, None, Some(&fields))?;
        Ok(id)
    }

    /// Replace all user fields of a document, keeping its system fields.
    pub(crate) fn replace(&mut self, table: &TableInfo, id: DocId, mut fields: Map<String, Value>) -> anyhow::Result<()> {
        reject_system_fields(&fields)?;
        let old = self.load(table, id)?;
        for sys in [ID_FIELD, CREATION_TIME_FIELD] {
            if let Some(v) = old.get(sys) {
                fields.insert(sys.into(), v.clone());
            }
        }
        self.write(table, id, Some(&old), Some(&fields))
    }

    /// Shallow-merge `patch` into a document. Returns the merged document so the caller can validate it.
    pub(crate) fn patch(
        &mut self,
        table: &TableInfo,
        id: DocId,
        patch: Map<String, Value>,
        validate: impl FnOnce(&Map<String, Value>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        reject_system_fields(&patch)?;
        let old = self.load(table, id)?;
        let mut new = old.clone();
        new.extend(patch);
        validate(&new)?;
        self.write(table, id, Some(&old), Some(&new))
    }

    pub(crate) fn delete(&mut self, table: &TableInfo, id: DocId) -> anyhow::Result<()> {
        let old = self.load(table, id)?;
        self.write(table, id, Some(&old), None)
    }
}

/// Ids carry their table number; using one with the wrong table is an error rather than a silent miss.
pub(crate) fn check_table(table: &TableInfo, id: DocId) -> anyhow::Result<()> {
    if id.table_number() != table.number {
        bail!(
            "id {id} belongs to table number {}, not to table `{}` (number {})",
            id.table_number(),
            table.name,
            table.number
        );
    }
    Ok(())
}

fn reject_system_fields(fields: &Map<String, Value>) -> anyhow::Result<()> {
    for sys in [ID_FIELD, CREATION_TIME_FIELD] {
        if fields.contains_key(sys) {
            bail!("system field `{sys}` cannot be written");
        }
    }
    Ok(())
}

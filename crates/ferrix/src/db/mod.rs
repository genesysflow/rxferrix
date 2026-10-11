//! The database: one writer connection, a pool of reader connections, the catalog and the write log.
//!
//! ## Concurrency model
//!
//! * **Writes** are serialized through a single read-write connection behind a `tokio::sync::Mutex`. A mutation holds
//!   the lock for its whole duration, so mutations are serializable without any conflict detection or retries. Its
//!   reads go through the same connection and therefore see its own writes.
//! * **Group commit.** Mutations that queue for the writer share one SQLite transaction (a *batch*): each runs in its
//!   own savepoint (rolled back alone if it fails), and whoever releases the lock commits the batch unless another
//!   write is already waiting to continue it (up to [`MAX_BATCH`] mutations or [`MAX_BATCH_AGE`]). A mutation's caller
//!   gets its result only after the batch commits, and every mutation still gets its own ts; the commit, the `_meta`
//!   update and the page writes to the WAL are paid once per batch instead of once per mutation. A mutation known to
//!   be slow (over [`SLOW_WRITE`] lately, per path) commits the batch before it starts instead of joining it, so it
//!   doesn't hold the others' results back. See [`WriterShared`].
//! * **Reads** use read-only connections. A [`Snapshot`] is a read transaction: `BEGIN` followed by a read of
//!   `_meta.ts`. The first read pins the WAL snapshot, and because the writer updates `_meta.ts` in the same transaction
//!   as the data, the value read is exactly the commit ts the snapshot reflects. One snapshot can serve several
//!   `QueryCtx` at once (each recording its own read set): the sync worker uses this to rerun many queries at one
//!   consistent ts.
//!
//! rusqlite connections are `Send` but not `Sync`, so each is wrapped in a `std::sync::Mutex` that is only ever held
//! for the duration of synchronous SQLite calls, never across an `.await`.

pub mod aggregate;
pub mod catalog;
pub mod checkpoint;
pub mod dedup;
pub mod journal;
pub mod pagination;
pub mod query;
pub mod readset;
pub mod schema_check;
pub mod search;
pub mod storage;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Group commit: the most mutations one batch takes before it commits, even with more waiting.
pub const MAX_BATCH: usize = 64;
/// Group commit: a batch older than this (since its first mutation began) commits at the next release, so the first
/// mutation's caller isn't kept waiting by a long queue.
pub const MAX_BATCH_AGE: Duration = Duration::from_millis(2);
/// Group commit: a write transaction whose label (the mutation's path) has recently taken longer than this commits
/// the open batch before it starts, so the mutations already in the batch don't wait for it.
pub const SLOW_WRITE: Duration = Duration::from_micros(500);
/// A mutation holding the writer this long is logged as a warning (at most once a minute per path): every other write
/// waited for it.
pub const WARN_WRITE: Duration = Duration::from_millis(100);

/// Fields drop in order: the HNSW thread is stopped (and its connection closed) first, the writer closes last, so
/// the read-write connection is the last one and checkpoints the WAL when the database is dropped.
pub struct Database {
    /// HNSW vector indexes (in-memory graphs following the write log).
    ann: search::ann::AnnIndexes,
    /// Checkpoints the WAL off the commit path; stopped before the writer closes.
    _checkpointer: checkpoint::Checkpointer,
    readers: Arc<ReaderPool>,
    catalog: Arc<Catalog>,
    write_log: Arc<WriteLog>,
    clock: Arc<Clock>,
    /// The change journal's switch (`journal`; on with `serve --replicate-to`).
    journal: Arc<journal::Journal>,
    writer: Arc<WriterShared>,
}

/// Options for [`Database::open_with`].
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Number of reader connections.
    pub readers: usize,
    /// Skip validating existing documents against changed table types (see [`schema_check`]).
    pub skip_schema_check: bool,
    /// Journal every commit's document changes from the start ([`journal`]; replication).
    pub journal: bool,
    /// Wait for each commit to reach the disk (SQLite `synchronous = FULL`) instead of only the OS (`NORMAL`, the
    /// default: a process crash loses nothing, a power loss may lose the last commits). With group commit this is one
    /// fsync per batch of mutations.
    pub fsync_commits: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions { readers: DEFAULT_READERS, skip_schema_check: false, journal: false, fsync_commits: false }
    }
}

impl Database {
    /// Open `<dir>/data.db`, creating it if needed, and reconcile the catalog with `tables`.
    pub fn open(dir: &Path, tables: &[TableDef], readers: usize) -> anyhow::Result<Database> {
        Self::open_with(dir, tables, &OpenOptions { readers, ..Default::default() })
    }

    /// Like [`Database::open`]. Refuses to open if existing documents don't match changed table types, unless
    /// `options.skip_schema_check` (see [`schema_check`]).
    pub fn open_with(dir: &Path, tables: &[TableDef], options: &OpenOptions) -> anyhow::Result<Database> {
        Self::open_with_migrations(dir, tables, options, &[])
    }

    /// Like [`Database::open_with`], with the declared migrations as `(name, table)` pairs: a table whose documents
    /// don't match its type still opens while migrations for it are pending (not `done` in `_migrations`), as a
    /// *migrating* table ([`Catalog::is_migrating`]).
    pub fn open_with_migrations(
        dir: &Path,
        tables: &[TableDef],
        options: &OpenOptions,
        migrations: &[(String, String)],
    ) -> anyhow::Result<Database> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating data dir {}", dir.display()))?;
        configure_sqlite();
        search::vector::register_extension();
        let path = dir.join("data.db");
        let mut writer = storage::open_writer(&path)?;
        if options.fsync_commits {
            writer.pragma_update(None, "synchronous", "FULL")?;
        }
        dedup::ensure_schema(&writer)?;
        writer.execute_batch(journal::DDL)?;
        let catalog = catalog::reconcile(&mut writer, tables)?;
        let coverage = crate::migrations::coverage(&writer, &catalog, migrations)?;
        for table in schema_check::run(&writer, &catalog, tables, options.skip_schema_check, &coverage)? {
            catalog.set_migrating(&table, true);
        }
        let ts = storage::read_meta_u64(&writer, storage::META_TS)?;
        let readers = options.readers.max(1);
        let write_log = Arc::new(WriteLog::new(ts));
        let ann = search::ann::AnnIndexes::open(dir, &catalog, write_log.clone());
        let journal = Arc::new(journal::Journal::default());
        journal.set_enabled(options.journal);
        if options.journal {
            // Rows a previous run didn't ship count towards the bound from the start.
            journal.set_pending_bytes(journal::stats(&writer)?.map(|s| s.3).unwrap_or(0));
        }
        let checkpointer = checkpoint::Checkpointer::start(&path)?;
        checkpoint::install_hook(&writer, checkpointer.signal());
        Ok(Database {
            writer: Arc::new(WriterShared {
                lock: Arc::new(tokio::sync::Mutex::new(Writer {
                    conn: writer,
                    batch: None,
                    checkpoint: checkpointer.signal().clone(),
                })),
                waiting: AtomicUsize::new(0),
                durations: Mutex::new(std::collections::HashMap::new()),
                write_log: write_log.clone(),
                journal: journal.clone(),
            }),
            journal,
            readers: Arc::new(ReaderPool {
                path,
                conns: Mutex::new(Vec::new()),
                permits: Arc::new(Semaphore::new(readers)),
                max: readers,
            }),
            catalog: Arc::new(catalog),
            write_log,
            clock: Arc::new(Clock::default()),
            ann,
            _checkpointer: checkpointer,
        })
    }

    /// The HNSW vector indexes ([`search::ann`]).
    pub fn ann(&self) -> &search::ann::AnnIndexes {
        &self.ann
    }

    /// The clock that write transactions (`MutationCtx::now`) and the scheduler read. Tests can move it forward.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// The same clock, for services that outlive a borrow of the database (file storage's URL expiries).
    pub(crate) fn shared_clock(&self) -> Arc<Clock> {
        self.clock.clone()
    }

    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    pub fn write_log(&self) -> &Arc<WriteLog> {
        &self.write_log
    }

    /// The change journal's switch and size ([`journal`]).
    pub fn journal(&self) -> &Arc<journal::Journal> {
        &self.journal
    }

    /// Run `f` on the writer connection in its own `BEGIN IMMEDIATE` transaction while holding the writer lock (so
    /// no mutation is in flight), committed if `f` succeeds. For bookkeeping outside the document model that must
    /// not race with commits (the replicator's trims and `_meta` keys, switching the journal). Doesn't bump the ts.
    pub(crate) async fn with_writer<R>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<R>) -> anyhow::Result<R> {
        let mut guard = self.writer.lock().await;
        // A batch of mutations left for this holder to continue: commit it first.
        let finished = self.writer.commit_batch(&mut guard);
        finish_members(finished);
        if !guard.is_autocommit() {
            guard.execute_batch("ROLLBACK")?;
        }
        guard.execute_batch("BEGIN IMMEDIATE")?;
        let result = match f(&guard) {
            Ok(r) => guard.execute_batch("COMMIT").map(|()| r).map_err(Into::into),
            Err(e) => {
                let _ = guard.execute_batch("ROLLBACK");
                Err(e)
            }
        };
        self.writer.release(guard);
        result
    }

    /// Re-apply one journal entry ([`journal`]) as its own commit, with its ts and time: the documents are written
    /// through the write path (indexes, search and vector entries maintained, the write log appended), unvalidated.
    /// The database must be exactly at `entry.ts - 1`. Used by point-in-time restores (`crate::ops::replicate`).
    pub async fn replay(&self, entry: &journal::JournalEntry) -> anyhow::Result<u64> {
        let changes = entry.parse_changes()?;
        let txn = self.begin_write().await?;
        txn.with_state(|s| {
            anyhow::ensure!(
                s.prev_ts + 1 == entry.ts,
                "journal gap: the database is at ts {} but the next journal entry is ts {}",
                s.prev_ts,
                entry.ts
            );
            s.now = entry.time;
            s.force_ts_bump = true;
            for change in &changes {
                let table = self
                    .catalog
                    .table_by_number(change.id.table_number())
                    .with_context(|| format!("journal entry ts {}: no table number {}", entry.ts, change.id.table_number()))?
                    .clone();
                anyhow::ensure!(
                    table.name == change.table,
                    "journal entry ts {}: table number {} is `{}` here but `{}` in the journal",
                    entry.ts,
                    table.number,
                    table.name,
                    change.table
                );
                s.apply_change(&table, change.id, change.doc.as_ref())?;
            }
            Ok(())
        })?;
        let ts = txn.commit_with(|_, _| Ok(())).await?;
        anyhow::ensure!(ts == entry.ts, "replaying journal entry ts {} committed ts {ts}", entry.ts);
        Ok(ts)
    }

    /// Open a read snapshot at the latest committed ts.
    pub async fn snapshot(&self) -> anyhow::Result<Arc<Snapshot>> {
        let permit = self.readers.permits.clone().acquire_owned().await?;
        self.open_snapshot(permit)
    }

    /// Like [`snapshot`](Self::snapshot), but `None` instead of waiting when every reader is busy.
    pub fn try_snapshot(&self) -> Option<anyhow::Result<Arc<Snapshot>>> {
        let permit = self.readers.permits.clone().try_acquire_owned().ok()?;
        Some(self.open_snapshot(permit))
    }

    /// Number of read-only connections (snapshots that can be open at once).
    pub fn readers(&self) -> usize {
        self.readers.max
    }

    fn open_snapshot(&self, permit: OwnedSemaphorePermit) -> anyhow::Result<Arc<Snapshot>> {
        let conn = match self.readers.conns.lock().unwrap().pop() {
            Some(c) => c,
            None => storage::open_reader(&self.readers.path)?,
        };
        // BEGIN is deferred: the snapshot is pinned by the first read, which is the ts read itself.
        let begun = (|| -> anyhow::Result<(u64, f64)> {
            storage::exec_cached(&conn, "BEGIN")?;
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
                        expected.push((keys::index_key(obj, &index.key_fields, id), id.to_vec()));
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
            problems.extend(search::verify(conn, &catalog)?);
            problems.extend(aggregate::verify(conn, &catalog.tables().cloned().collect::<Vec<_>>())?);
            problems.extend(self.ann.verify(conn, snapshot.ts)?);
            Ok(problems)
        })
    }

    /// Lock the writer and begin a write transaction: a savepoint in the open batch, or in a new one (group commit).
    pub async fn begin_write(&self) -> anyhow::Result<Arc<WriteTxn>> {
        self.begin_write_as("").await
    }

    /// [`Database::begin_write`] for a mutation: `label` (its path) keys how long its transactions take, so a slow
    /// one commits the open batch first instead of joining it ([`SLOW_WRITE`]).
    pub async fn begin_write_as(&self, label: &str) -> anyhow::Result<Arc<WriteTxn>> {
        let guard = self.writer.lock().await;
        let mut conn = WriterGuard {
            guard: Some(guard),
            shared: self.writer.clone(),
            undo: Undo::Nothing,
            label: label.to_string(),
            began: Instant::now(),
        };
        if conn.writer().batch.is_some() && self.writer.expected(label) > SLOW_WRITE {
            let finished = self.writer.commit_batch(conn.writer_mut());
            finish_members(finished);
        }
        if conn.writer().batch.is_none() {
            let batch = conn.writer().begin_batch()?;
            conn.writer_mut().batch = Some(batch);
            // Nothing else is in the batch yet: undoing this transaction is rolling back the whole SQLite transaction,
            // which spares the savepoint's two statements when there is no queue to batch with.
            conn.undo = Undo::Transaction;
        } else {
            storage::exec_cached(&conn, "SAVEPOINT mutation")?;
            conn.undo = Undo::Savepoint;
        }
        // Timed from here: committing the batch before it (above), beginning a new one and finishing a checkpoint
        // are not this transaction's work, and counting them would make a fast mutation look slow (and the rule
        // above commit every batch before it, which, timed in, would keep it looking slow).
        conn.began = Instant::now();
        let batch = conn.writer().batch.as_ref().expect("a batch is open");
        let (prev_ts, last_time, last_creation_time) = (batch.ts, batch.time, batch.last_creation_time);
        let now = self.clock.now_ms().max(last_time);
        let state = WriteState {
            conn,
            prev_ts,
            now,
            last_creation_time,
            writes: Vec::new(),
            catalog: self.catalog.clone(),
            // Decided here, under the writer lock (the switch only changes under it): all or nothing per transaction.
            journal: self.journal.is_enabled().then(journal::Changes::default),
            force_ts_bump: false,
        };
        Ok(Arc::new(WriteTxn {
            state: Mutex::new(Some(state)),
            catalog: self.catalog.clone(),
            on_commit: Mutex::new(Vec::new()),
        }))
    }
}

/// The writer connection, and the batch of mutations it has open (group commit).
pub(crate) struct Writer {
    conn: Connection,
    batch: Option<Batch>,
    /// What the connection's WAL hook nudges ([`checkpoint::install_hook`]); dropped after `conn`.
    checkpoint: Arc<checkpoint::Signal>,
}

impl std::ops::Deref for Writer {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl Writer {
    fn begin_batch(&self) -> anyhow::Result<Batch> {
        // Open only if a previous holder's commit and its rollback both failed; be defensive anyway.
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("ROLLBACK")?;
        }
        // The checkpoint thread copied most of the WAL; with no write in progress, this one completes, so the
        // transaction below starts the WAL over (`checkpoint`, module docs).
        checkpoint::finish_if_asked(&self.conn, &self.checkpoint);
        storage::exec_cached(&self.conn, "BEGIN IMMEDIATE")?;
        let begun = (|| -> anyhow::Result<(u64, f64, f64)> {
            Ok((
                storage::read_meta_u64(&self.conn, storage::META_TS)?,
                storage::read_meta_f64(&self.conn, storage::META_TIME)?,
                storage::read_meta_f64(&self.conn, storage::META_LAST_CREATION_TIME)?,
            ))
        })();
        match begun {
            Ok((ts, time, last_creation_time)) => {
                Ok(Batch { begin_ts: ts, ts, time, last_creation_time, started: Instant::now(), members: Vec::new() })
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // A batch nobody committed (only if a release path failed): undo it; its callers already got an error.
        if self.batch.is_some() && !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        checkpoint::remove_hook(&self.conn);
    }
}

/// An open SQLite transaction holding the mutations that finished in it but aren't committed yet.
struct Batch {
    /// `_meta.ts` when the batch began.
    begin_ts: u64,
    /// The ts, commit time and last creation time after the last mutation in the batch: what the next one continues
    /// from, and what `_meta` gets at commit.
    ts: u64,
    time: f64,
    last_creation_time: f64,
    started: Instant,
    members: Vec<Member>,
}

/// A mutation waiting for its batch to commit.
struct Member {
    /// Its write-log entry (None if it wrote nothing), appended once the batch commits.
    entry: Option<CommitEntry>,
    journaled: u64,
    on_commit: Vec<Box<dyn FnOnce() + Send>>,
    done: tokio::sync::oneshot::Sender<Result<(), String>>,
    /// Set by [`WriterShared::commit_batch`].
    result: Result<(), String>,
}

/// The writer lock, and who is waiting for it. Group commit's rule: a holder releasing the lock commits the open batch
/// unless another write is already waiting for the lock (and the batch is below [`MAX_BATCH`] and [`MAX_BATCH_AGE`]);
/// then that one continues it. A waiter that gives up (its future dropped), or a release that races with one, commits
/// a batch nobody will continue ([`WriterShared::commit_orphaned`]).
pub(crate) struct WriterShared {
    lock: Arc<tokio::sync::Mutex<Writer>>,
    /// Tasks between asking for the lock and getting it.
    waiting: AtomicUsize,
    /// Per label: a moving average of how long its write transactions hold the lock (µs), and when one was last
    /// reported for holding it too long ([`WARN_WRITE`]).
    durations: Mutex<std::collections::HashMap<String, (f64, Option<Instant>)>>,
    write_log: Arc<WriteLog>,
    journal: Arc<journal::Journal>,
}

impl WriterShared {
    /// How long a write transaction labelled `label` is expected to take (zero if never seen).
    fn expected(&self, label: &str) -> Duration {
        let durations = self.durations.lock().unwrap_or_else(|e| e.into_inner());
        Duration::from_secs_f64(durations.get(label).map_or(0.0, |d| d.0) / 1e6)
    }

    fn record_duration(&self, label: &str, took: Duration) {
        let mut durations = self.durations.lock().unwrap_or_else(|e| e.into_inner());
        let micros = took.as_secs_f64() * 1e6;
        let entry = durations.entry(label.to_string()).or_insert((micros, None));
        // Weighted towards recent runs, so a function that got slow (or fast) is noticed within a few calls.
        entry.0 += (micros - entry.0) / 4.0;
        if took >= WARN_WRITE && !label.is_empty() && entry.1.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
            entry.1 = Some(Instant::now());
            tracing::warn!(
                mutation = label,
                held_ms = took.as_millis() as u64,
                "a mutation held the database writer for {} ms: every other write waited for it. Keep mutations short \
                 (read less, or split the work into several mutations, scheduled or run from an action)",
                took.as_millis()
            );
        }
    }

    async fn lock(self: &Arc<Self>) -> OwnedMutexGuard<Writer> {
        struct Waiting<'a> {
            shared: &'a WriterShared,
            acquired: bool,
        }
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.shared.waiting.fetch_sub(1, Ordering::SeqCst);
                if !self.acquired {
                    // Given up: a holder may have left its batch open for us.
                    self.shared.commit_orphaned();
                }
            }
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let mut waiting = Waiting { shared: self, acquired: false };
        let guard = self.lock.clone().lock_owned().await;
        waiting.acquired = true;
        guard
    }

    /// End a turn at the writer: commit the batch, or leave it open for a waiting write to continue.
    fn release(&self, mut guard: OwnedMutexGuard<Writer>) {
        let young = |b: &Batch| b.members.len() < MAX_BATCH && b.started.elapsed() < MAX_BATCH_AGE;
        let continue_batch = guard.batch.as_ref().is_some_and(young) && self.waiting.load(Ordering::SeqCst) > 0;
        if !continue_batch {
            let finished = self.commit_batch(&mut guard);
            drop(guard);
            finish_members(finished);
            return;
        }
        drop(guard);
        // The waiter may have given up between our check and the release.
        self.commit_orphaned();
    }

    /// Commit an open batch if nobody is waiting to continue it and the lock is free.
    fn commit_orphaned(&self) {
        if self.waiting.load(Ordering::SeqCst) > 0 {
            return;
        }
        if let Ok(mut guard) = self.lock.clone().try_lock_owned()
            && guard.batch.is_some()
        {
            let finished = self.commit_batch(&mut guard);
            drop(guard);
            finish_members(finished);
        }
    }

    /// Commit the open batch (if any): `_meta` gets the batch's last ts, then `COMMIT`; the members' write-log entries
    /// are appended in ts order, still under the lock. On failure the whole batch rolls back and every member gets the
    /// error. Returns the members, for [`finish_members`] (after releasing the lock).
    fn commit_batch(&self, writer: &mut Writer) -> Vec<Member> {
        let Some(mut batch) = writer.batch.take() else { return Vec::new() };
        let committed = (|| -> anyhow::Result<()> {
            if batch.ts != batch.begin_ts {
                storage::write_meta(&writer.conn, storage::META_TS, (batch.ts as i64).into())?;
                storage::write_meta(&writer.conn, storage::META_TIME, batch.time.into())?;
                storage::write_meta(&writer.conn, storage::META_LAST_CREATION_TIME, batch.last_creation_time.into())?;
            }
            storage::exec_cached(&writer.conn, "COMMIT")
        })();
        match committed {
            Ok(()) => {
                for member in &mut batch.members {
                    self.journal.add_pending_bytes(member.journaled);
                    if let Some(entry) = member.entry.take() {
                        self.write_log.append(entry);
                    }
                }
            }
            Err(e) => {
                tracing::error!("commit failed: {e:#}");
                if !writer.conn.is_autocommit()
                    && let Err(e) = writer.conn.execute_batch("ROLLBACK")
                {
                    tracing::error!("rollback after a failed commit failed: {e}");
                }
                for member in &mut batch.members {
                    member.result = Err(format!("commit failed: {e:#}"));
                    member.on_commit.clear();
                }
            }
        }
        batch.members
    }
}

/// Run committed members' `on_commit` hooks and wake every member's caller.
fn finish_members(members: Vec<Member>) {
    for member in members {
        if member.result.is_ok() {
            for hook in member.on_commit {
                hook();
            }
        }
        // The caller may be gone (its future dropped while waiting); its writes are committed regardless.
        let _ = member.done.send(member.result);
    }
}

/// The writer lock as held by one write transaction, with its savepoint. Dropped without [`WriterGuard::finish`]
/// (an error, a panic, the mutation future dropped), it rolls back to the savepoint, undoing only this transaction,
/// and releases the lock ([`WriterShared::release`]).
pub(crate) struct WriterGuard {
    guard: Option<OwnedMutexGuard<Writer>>,
    shared: Arc<WriterShared>,
    undo: Undo,
    /// For [`WriterShared::record_duration`].
    label: String,
    began: Instant,
}

/// How a [`WriterGuard`] undoes its transaction.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Undo {
    /// Finished (or nothing to undo).
    Nothing,
    /// `ROLLBACK TO` its savepoint: earlier mutations in the batch stay.
    Savepoint,
    /// It began the batch: `ROLLBACK` the SQLite transaction (and close the batch).
    Transaction,
}

impl std::ops::Deref for WriterGuard {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.writer().conn
    }
}

impl WriterGuard {
    fn writer(&self) -> &Writer {
        self.guard.as_ref().expect("held until dropped")
    }

    fn writer_mut(&mut self) -> &mut Writer {
        self.guard.as_mut().expect("held until dropped")
    }

    /// Undo this transaction's writes (only them: earlier mutations in the batch stay).
    fn rollback_savepoint(&mut self) {
        match std::mem::replace(&mut self.undo, Undo::Nothing) {
            Undo::Nothing => {}
            Undo::Transaction => {
                let writer = self.writer_mut();
                writer.batch = None;
                if let Err(e) = writer.conn.execute_batch("ROLLBACK") {
                    tracing::error!("rollback failed: {e}");
                }
            }
            Undo::Savepoint => {
                let undone = storage::exec_cached(self, "ROLLBACK TO mutation")
                    .and_then(|()| storage::exec_cached(self, "RELEASE mutation"));
                if let Err(e) = undone {
                    // The batch's transaction is in an unknown state: roll all of it back (its members get the error).
                    tracing::error!("rollback to savepoint failed: {e:#}");
                    let writer = self.writer_mut();
                    let _ = writer.conn.execute_batch("ROLLBACK");
                    if let Some(batch) = writer.batch.take() {
                        for mut member in batch.members {
                            member.result = Err(format!("rolled back: {e:#}"));
                            member.on_commit.clear();
                            finish_members(vec![member]);
                        }
                    }
                }
            }
        }
    }

    /// Keep this transaction's writes: release the savepoint into the batch, add the member, and release the lock.
    fn finish(mut self, member: Member, ts: u64, time: f64, last_creation_time: f64) -> anyhow::Result<()> {
        if self.undo == Undo::Savepoint {
            storage::exec_cached(&self, "RELEASE mutation")?;
        }
        self.undo = Undo::Nothing;
        let batch = self.writer_mut().batch.as_mut().expect("a batch is open");
        if member.entry.is_some() {
            batch.ts = ts;
            batch.time = time;
            batch.last_creation_time = last_creation_time;
        }
        batch.members.push(member);
        Ok(())
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.rollback_savepoint();
        self.shared.record_duration(&self.label, self.began.elapsed());
        if let Some(guard) = self.guard.take() {
            self.shared.release(guard);
        }
    }
}

/// Process-wide SQLite settings for parallel readers, applied once before the first connection.
///
/// Two global mutexes in SQLite serialize reader connections that run on different threads: the memory statistics
/// (`SQLITE_CONFIG_MEMSTATUS`, taken on every allocation) and, when SQLite is built with
/// `SQLITE_ENABLE_MEMORY_MANAGEMENT` (rusqlite's bundled build defines it), the page cache that all connections then
/// share. With both, N readers ran no faster than one. Memory statistics are turned off here (if SQLite is not
/// initialized yet; the workspace also builds it with `SQLITE_DEFAULT_MEMSTATUS=0`); the shared page cache can only
/// be avoided at build time, so its presence is logged (see `.cargo/config.toml`, which `ferrix new` gives every app).
fn configure_sqlite() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: called before this process opens any connection (unless the embedding app did; then SQLite is
        // initialized and this returns SQLITE_MISUSE, which is harmless). The variadic argument is the C int it expects.
        unsafe {
            rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0 as std::ffi::c_int);
        }
        // SAFETY: a NUL-terminated string literal.
        let shared_cache = unsafe { rusqlite::ffi::sqlite3_compileoption_used(c"ENABLE_MEMORY_MANAGEMENT".as_ptr()) };
        if shared_cache != 0 {
            tracing::warn!(
                "SQLite was built with SQLITE_ENABLE_MEMORY_MANAGEMENT: its reader connections share one page cache behind \
                 a global mutex, so queries won't run faster in parallel. Build with \
                 LIBSQLITE3_FLAGS=\"-USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DEFAULT_MEMSTATUS=0\": apps created with \
                 `ferrix new` set it in their .cargo/config.toml; copy that file into this app's directory."
            );
        }
    });
}

/// The wall clock plus an offset that only ever grows. The offset is 0 in production; tests advance it
/// (`TestApp::advance_time`) to make scheduled jobs and crons due without waiting.
#[derive(Debug, Default)]
pub struct Clock {
    offset_ms: std::sync::atomic::AtomicU64,
}

impl Clock {
    /// Milliseconds since the epoch, as seen by mutations and the scheduler.
    pub fn now_ms(&self) -> f64 {
        wall_clock_ms() + self.offset().as_secs_f64() * 1000.0
    }

    /// Move this clock forward by `by`.
    pub fn advance(&self, by: std::time::Duration) {
        self.offset_ms.fetch_add(by.as_millis() as u64, std::sync::atomic::Ordering::SeqCst);
    }

    /// How far this clock is ahead of the wall clock.
    pub fn offset(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.offset_ms.load(std::sync::atomic::Ordering::SeqCst))
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
    max: usize,
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
            if storage::exec_cached(&conn, "COMMIT").is_ok() || conn.is_autocommit() {
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
    /// Run after a successful `COMMIT`, dropped on rollback (see [`WriteTxn::on_commit`]).
    on_commit: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

pub(crate) struct WriteState {
    /// The writer lock and this transaction's savepoint; dropping the state rolls back to it.
    conn: WriterGuard,
    prev_ts: u64,
    now: f64,
    last_creation_time: f64,
    writes: Vec<(IndexId, Vec<u8>)>,
    /// For [`Catalog::is_migrating`].
    catalog: Arc<Catalog>,
    /// The document changes to journal at commit, when journaling ([`journal`]).
    journal: Option<journal::Changes>,
    /// Commit a new ts even without writes (a replayed journal entry whose writes cancel out here).
    force_ts_bump: bool,
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

    /// Run `f` once this transaction has committed (never if it rolls back). Used for side effects outside the
    /// database that must follow the commit, e.g. deleting a stored file's blob. `f` runs synchronously right after
    /// `COMMIT`, so it should only hand work off (e.g. `tokio::spawn`).
    pub(crate) fn on_commit(&self, f: impl FnOnce() + Send + 'static) {
        self.on_commit.lock().unwrap_or_else(|e| e.into_inner()).push(Box::new(f));
    }

    /// Commit, running `before_commit(conn, commit_ts)` inside the transaction first (used to record the mutation's
    /// response for idempotency atomically with its writes). Returns the new ts, or the current ts if no document
    /// was written (then ts is not bumped), once the batch holding this transaction has committed (group commit:
    /// see the module docs). If that commit fails, so does this, and nothing of this transaction is kept.
    pub(crate) async fn commit_with(
        &self,
        before_commit: impl FnOnce(&Connection, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<u64> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut state = state.context("the mutation's transaction has already finished")?;
        let (done, committed) = tokio::sync::oneshot::channel();
        let on_commit = std::mem::take(&mut *self.on_commit.lock().unwrap_or_else(|e| e.into_inner()));
        let ts = if state.writes.is_empty() && !state.force_ts_bump {
            before_commit(&state.conn, state.prev_ts)?;
            let member = Member { entry: None, journaled: 0, on_commit, done, result: Ok(()) };
            let (ts, now, lct) = (state.prev_ts, state.now, state.last_creation_time);
            state.take_conn().finish(member, ts, now, lct)?;
            ts
        } else {
            let ts = state.prev_ts + 1;
            before_commit(&state.conn, ts)?;
            // In the same SQLite transaction as the writes and the ts bump: journaled exactly once, or not at all.
            let journaled = match state.journal.take() {
                Some(changes) => {
                    let encoded = changes.encode();
                    journal::insert(&state.conn, ts, state.now, &encoded)?;
                    encoded.len() as u64
                }
                None => 0,
            };
            let entry = Some(CommitEntry { ts, writes: std::mem::take(&mut state.writes) });
            let member = Member { entry, journaled, on_commit, done, result: Ok(()) };
            let (now, lct) = (state.now, state.last_creation_time);
            state.take_conn().finish(member, ts, now, lct)?;
            ts
        };
        match committed.await {
            Ok(Ok(())) => Ok(ts),
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => bail!("the write batch was dropped before it committed"),
        }
    }

    /// Roll back (also happens implicitly on drop).
    pub(crate) fn rollback(&self) {
        drop(self.state.lock().unwrap_or_else(|e| e.into_inner()).take());
    }

    /// Roll back, then run `f` while still holding the writer lock. `f`'s writes are kept: they commit with the
    /// batch (or at once, if this transaction began it), without waiting for it here.
    pub(crate) fn rollback_then(&self, f: impl FnOnce(&Connection) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut state = state.context("the mutation's transaction has already finished")?;
        state.conn.rollback_savepoint();
        f(&state.conn)
    }
}

impl WriteState {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The writer guard, for [`WriterGuard::finish`]; this state is spent.
    fn take_conn(self) -> WriterGuard {
        self.conn
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
        // Validated before anything is written (e.g. a vector of the wrong dimension).
        let special = search::SpecialWrites::prepare(table, old, new)?;
        let aggregates = aggregate::Writes::prepare(table, old, new);
        for index in table.stored_indexes() {
            let old_key = old.map(|d| keys::index_key(d, &index.key_fields, &id.0));
            let new_key = new.map(|d| keys::index_key(d, &index.key_fields, &id.0));
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
        special.apply(&self.conn, id, &mut self.writes)?;
        aggregates.apply(&self.conn, &mut self.writes)?;
        if table.name == <crate::storage::StoredFile as crate::schema::Table>::NAME {
            // Deletion times of files, for keeping their blobs (`StorageConfig::keep_deleted`).
            if new.is_some() {
                self.conn.prepare_cached("DELETE FROM _storage_deleted WHERE id = ?1")?.execute([&id.0[..]])?;
            } else if old.is_some() {
                self.conn
                    .prepare_cached("INSERT OR REPLACE INTO _storage_deleted (id, deleted_at) VALUES (?1, ?2)")?
                    .execute(rusqlite::params![&id.0[..], self.now])?;
            }
        }
        let json = match new {
            Some(doc) => {
                let json = serde_json::to_string(doc)?;
                if old.is_some() {
                    storage::put_document(&self.conn, &id.0, table.number, &json, ts)?;
                } else {
                    storage::insert_document(&self.conn, &id.0, table.number, &json, ts)?;
                }
                Some(json)
            }
            None => {
                storage::delete_document(&self.conn, &id.0)?;
                None
            }
        };
        if let Some(changes) = &mut self.journal {
            changes.record(&table.name, &id.0, old.is_some(), json);
        }
        Ok(())
    }

    /// Write a journaled document state as is (`None`: delete it if it exists), keeping every index up to date.
    /// Used to replay the journal ([`Database::replay`]); no validation (the document was valid when written).
    fn apply_change(&mut self, table: &TableInfo, id: DocId, doc: Option<&Map<String, Value>>) -> anyhow::Result<()> {
        check_table(table, id)?;
        let old = match storage::get_document(&self.conn, &id.0)? {
            Some(_) => Some(self.load(table, id)?),
            None => None,
        };
        if let Some(t) = doc.and_then(|d| d.get(CREATION_TIME_FIELD)).and_then(Value::as_f64) {
            self.last_creation_time = self.last_creation_time.max(t);
        }
        match (old, doc) {
            (None, None) => Ok(()),
            (old, new) => self.write(table, id, old.as_ref(), new),
        }
    }

    /// Insert a document from user fields; adds `_id` and `_creationTime`. `validate` sees the complete document
    /// (system fields included) before anything is written.
    pub(crate) fn insert(
        &mut self,
        table: &TableInfo,
        fields: Map<String, Value>,
        validate: impl FnOnce(&Map<String, Value>) -> anyhow::Result<()>,
    ) -> anyhow::Result<DocId> {
        self.insert_with_id(table, DocId::new_random(table.number), fields, validate)
    }

    /// [`WriteState::insert`] with an id chosen by the caller (e.g. a stored file whose id names its blob, which is
    /// written before the document). The id must belong to `table` and not exist yet.
    pub(crate) fn insert_with_id(
        &mut self,
        table: &TableInfo,
        id: DocId,
        mut fields: Map<String, Value>,
        validate: impl FnOnce(&Map<String, Value>) -> anyhow::Result<()>,
    ) -> anyhow::Result<DocId> {
        reject_system_fields(&fields)?;
        check_table(table, id)?;
        // (An existing id fails the document insert in `write`: no lookup needed here.)
        fields.insert(ID_FIELD.into(), Value::String(id.encode()));
        let last_creation_time = self.last_creation_time;
        let ct = self.next_creation_time();
        fields.insert(CREATION_TIME_FIELD.into(), serde_json::Number::from_f64(ct).context("bad time")?.into());
        if let Err(e) = validate(&fields) {
            self.last_creation_time = last_creation_time;
            return Err(e);
        }
        self.write(table, id, None, Some(&fields))?;
        Ok(id)
    }

    /// Replace all user fields of a document, keeping its system fields. `validate` sees the complete new document.
    pub(crate) fn replace(
        &mut self,
        table: &TableInfo,
        id: DocId,
        mut fields: Map<String, Value>,
        validate: impl Fn(&Map<String, Value>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        reject_system_fields(&fields)?;
        let old = self.load(table, id)?;
        for sys in [ID_FIELD, CREATION_TIME_FIELD] {
            if let Some(v) = old.get(sys) {
                fields.insert(sys.into(), v.clone());
            }
        }
        self.validate_update(table, &old, &fields, validate)?;
        self.write(table, id, Some(&old), Some(&fields))
    }

    /// Shallow-merge `patch` into a document. Returns the merged document so the caller can validate it.
    pub(crate) fn patch(
        &mut self,
        table: &TableInfo,
        id: DocId,
        patch: Map<String, Value>,
        validate: impl Fn(&Map<String, Value>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        reject_system_fields(&patch)?;
        let old = self.load(table, id)?;
        let mut new = old.clone();
        new.extend(patch);
        self.validate_update(table, &old, &new, validate)?;
        self.write(table, id, Some(&old), Some(&new))
    }

    /// Validate the new version of an existing document. In a migrating table ([`Catalog::is_migrating`]), a document
    /// that didn't match the table's type before the write may still not match after it (the migrations will rewrite
    /// it); a document that matched must still match.
    fn validate_update(
        &self,
        table: &TableInfo,
        old: &Map<String, Value>,
        new: &Map<String, Value>,
        validate: impl Fn(&Map<String, Value>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        match validate(new) {
            Err(_) if self.catalog.is_migrating(&table.name) && validate(old).is_err() => {
                tracing::debug!(table = %table.name, "lenient write to a document of a migrating table");
                Ok(())
            }
            r => r,
        }
    }

    pub(crate) fn delete(&mut self, table: &TableInfo, id: DocId) -> anyhow::Result<()> {
        let old = self.load(table, id)?;
        self.write(table, id, Some(&old), None)
    }

    /// Write a document with a given id and, optionally, creation time (`ops::export` imports): inserted, or, with
    /// `overwrite`, replacing an existing document with that id (keeping its creation time when none is given).
    /// `validate` sees the complete document. Returns whether a document was replaced.
    pub(crate) fn put_imported(
        &mut self,
        table: &TableInfo,
        id: DocId,
        mut fields: Map<String, Value>,
        creation_time: Option<f64>,
        overwrite: bool,
        validate: impl FnOnce(&Map<String, Value>) -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        reject_system_fields(&fields)?;
        check_table(table, id)?;
        let old = match storage::get_document(&self.conn, &id.0)? {
            None => None,
            Some(_) if !overwrite => bail!("document {id} already exists in `{}`", table.name),
            Some(_) => Some(self.load(table, id)?),
        };
        let ct = match (creation_time, &old) {
            (Some(t), _) => {
                anyhow::ensure!(t.is_finite(), "`_creationTime` must be a finite number");
                // Later inserts must still get strictly greater creation times.
                self.last_creation_time = self.last_creation_time.max(t);
                t
            }
            (None, Some(o)) => o.get(CREATION_TIME_FIELD).and_then(Value::as_f64).unwrap_or(self.now),
            (None, None) => self.next_creation_time(),
        };
        fields.insert(ID_FIELD.into(), Value::String(id.encode()));
        fields.insert(CREATION_TIME_FIELD.into(), serde_json::Number::from_f64(ct).context("bad time")?.into());
        validate(&fields)?;
        self.write(table, id, old.as_ref(), Some(&fields))?;
        Ok(old.is_some())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_mutations_are_reported_once_a_minute() {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with(dir.path(), &[], &OpenOptions::default()).unwrap();
        let buf = Buf::default();
        let make = buf.clone();
        let subscriber = tracing_subscriber::fmt().with_writer(move || make.clone()).with_ansi(false).finish();
        tracing::subscriber::with_default(subscriber, || {
            db.writer.record_duration("app:slow", Duration::from_millis(150));
            db.writer.record_duration("app:slow", Duration::from_millis(300)); // within the minute: not again
            db.writer.record_duration("app:fast", Duration::from_millis(5));
            db.writer.record_duration("", Duration::from_millis(500)); // internal writes have no label
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(out.matches("held the database writer").count(), 1, "{out}");
        assert!(out.contains("app:slow") && out.contains("150 ms"), "{out}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wal_is_checkpointed_off_the_commit_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with(dir.path(), &[], &OpenOptions::default()).unwrap();
        let autocheckpoint: i64 =
            db.with_writer(|c| Ok(c.query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))?)).await.unwrap();
        assert_eq!(autocheckpoint, 0, "the writer never checkpoints itself");
        // ~3 pages per commit: the WAL passes the hook's threshold several times.
        for i in 0..600i64 {
            // Different all through each time, so all of its overflow pages are rewritten.
            let pad = format!("{i:08}").repeat(1_000);
            db.with_writer(|c| {
                storage::write_meta(c, "pad", pad.into())?;
                storage::write_meta(c, "probe", i.into())
            })
            .await
            .unwrap();
        }
        // Nothing checkpoints on the commit path, so only the background thread can have moved commits into the main
        // file: read a copy of it alone (without the WAL).
        let copy = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // A copy taken while the checkpoint thread writes can be torn: then it just doesn't count yet.
            std::fs::copy(dir.path().join("data.db"), copy.path().join("main.db")).unwrap();
            let in_main = Connection::open(copy.path().join("main.db"))
                .map_or(0, |main| storage::read_meta_u64(&main, "probe").unwrap_or(0));
            if in_main >= 300 {
                break;
            }
            assert!(Instant::now() < deadline, "the main file has only commits up to probe = {in_main}");
            std::thread::sleep(Duration::from_millis(20));
        }
        let wal = std::fs::metadata(dir.path().join("data.db-wal")).map_or(0, |m| m.len());
        assert!(wal <= 2 * 1000 * 4096 + 64 * 1024, "the WAL grew to {wal} bytes");
        // Closing still checkpoints and removes the WAL (the writer closes last), and the data is all there.
        drop(db);
        assert!(!dir.path().join("data.db-wal").exists(), "WAL left behind");
        let db = Database::open_with(dir.path(), &[], &OpenOptions::default()).unwrap();
        let v = db.with_writer(|c| storage::read_meta_u64(c, "probe")).await.unwrap();
        assert_eq!(v, 599);
    }

    #[tokio::test]
    async fn fsync_commits_sets_synchronous_full() {
        for (fsync, expected) in [(false, 1), (true, 2)] {
            let dir = tempfile::tempdir().unwrap();
            let options = OpenOptions { fsync_commits: fsync, ..Default::default() };
            let db = Database::open_with(dir.path(), &[], &options).unwrap();
            // 1 = NORMAL, 2 = FULL.
            let level = |c: &Connection| Ok(c.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))?);
            let level = db.with_writer(level).await.unwrap();
            assert_eq!(level, expected, "fsync_commits = {fsync}");
        }
    }
}

//! The database: one writer connection, a pool of reader connections, the catalog and the write log.
//!
//! ## Concurrency model
//!
//! * **Writes** are serialized through a single read-write connection behind a `tokio::sync::Mutex`. A mutation holds
//!   the lock for its whole duration, so mutations are serializable without any conflict detection or retries. Its
//!   reads go through the same connection and therefore see its own writes.
//! * **Group commit.** Mutations that queue for the writer share one SQLite transaction (a *batch*): each runs in its
//!   own savepoint (rolled back alone if it fails), and whoever releases the lock commits the batch unless another
//!   write is already waiting to continue it (up to [`MAX_BATCH`] mutations, and for [`BATCH_AGE`] or longer where
//!   commits are slow, see [`BATCH_AGE_COMMITS`]). A mutation's caller
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
mod batch_guard;
pub mod catalog;
pub mod checkpoint;
pub mod dedup;
pub mod floats;
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

/// Default number of reader connections: one and a half per core (as `std::thread::available_parallelism`, which
/// follows container CPU limits, counts them; `rxferrix serve`'s runtime has a worker thread per core).
///
/// A query holds a reader for its whole run, so with as many slow queries running as there are readers, every other
/// query waits; and the SQLite calls run on the runtime's worker threads, so with far more readers than threads, a
/// saturated CPU leaves slow queries crawling (each `.await` puts them behind every runnable fast one). Measured with
/// `bench_readers` (integration tests, 2026-10-15) on 2, 4, 8 and 16 threads, half as many slow queries (tens of ms)
/// as threads running next to fast ones: 1.5 readers per thread kept both going (8 threads: 109k fast queries/s at
/// p50 0.3 ms and 148 slow/s), one per thread starved the fast ones once the slow ones took every reader (2.5k/s at
/// p50 13 ms with 6 readers on 8 threads, the fixed default before), and two per thread the slow ones (4/s). It also
/// matches what the subscription manager asks for (`sync::subscriptions`: a fast-lane runner per thread, plus half
/// as many slow-lane ones). Readers are opened when first needed.
pub fn default_readers() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores + cores / 2).max(2)
}
/// Default [`OpenOptions::writer_cache_size`]: the page cache of the connection all writes go through (SQLite's own
/// default is 2 MB). Measured with `bench_indexes` and `bench_writes` (integration tests, 2026-10-15), 32 MiB made
/// inserts into a table with 32 indexes 48% faster, patches of all their fields 56%, and 10-insert mutations 10%,
/// and halved the p99 of read-then-insert mutations; one-insert mutations didn't change. 64 MiB gained nothing more,
/// and made inserts at random places of a 200k-document table 8% slower (32 MiB: 3%). Pages are only allocated as
/// they are used. Only the writer gets it: a reader's cache starts over whenever another connection has committed.
pub const DEFAULT_WRITER_CACHE_SIZE: u64 = 32 << 20;

/// Group commit: the most mutations one batch takes before it commits, even with more waiting. A bound on what one
/// commit holds; how long a batch stays open ([`BATCH_AGE`]) is what normally ends it.
///
/// It was 256 before: with more mutations queued than that (1024 writers of one-insert mutations, or sockets with
/// many calls in flight), batches filled in about a millisecond and the writer spent most of its time committing them
/// (`bench_writes`, 2026-11-24, btrfs with fsync, 1024 writers, medians of three runs: 46.4k mutations/s at p50 22 ms
/// and p99 27 ms with 256; 68.2k/s at p50 15 ms and p99 19 ms with this bound and [`BATCH_AGE_COMMITS`]; unchanged
/// at 64 and 256 writers).
pub const MAX_BATCH: usize = 4096;
/// Group commit: a batch older than this (since its first mutation began) commits at the next release, so the first
/// mutation's caller isn't kept waiting by a long queue. Longer where commits are slow: see [`BATCH_AGE_COMMITS`].
pub const BATCH_AGE: Duration = Duration::from_millis(4);
/// Group commit: a batch may stay open this many times as long as the last commit took, if that is longer than
/// [`BATCH_AGE`] (and up to [`MAX_BATCH_AGE`]). A commit has a cost that doesn't depend on the batch (the fsync, about
/// 3 ms on an encrypted btrfs NVMe SSD) and one per page written, which grows slower than the batch (mutations of
/// one batch often write the same pages): with a fixed 4 ms, a writer that always had a queue spent more time
/// committing than running mutations. With this, commits take about a fifth of its time where their cost is mostly
/// the fsync, and where commits are fast (no fsync, a fast disk) batches stay as short as before.
///
/// Measured with `bench_transfer` (2026-11-24, 64 sockets with 40 calls in flight each, btrfs with fsync; medians of
/// three runs): transfers between accounts picked uniformly went from 17.4k/s (batches of 230 open for 4 ms, then
/// 7 ms in the commit) to 28.1k/s (1800 open for 29 ms, 22 ms in the commit), with p50 latency from 135 to 66 ms and
/// p99 unchanged (16 later runs: 59 to 106 ms from run to run, 80 ms their median, at 26.9k/s); between a few hot accounts from 34.3k to 52.2k/s. A fixed 16 ms gave 23.7k and 50.1k/s, a fixed
/// 32 ms 27.9k and 45.8k/s, twice the last commit 27.9k and 43.0k/s. With a queue no longer than a batch (64 sockets
/// with one call in flight, `bench_writes` with up to 256 writers) nothing changes: those batches end when nobody is
/// waiting.
pub const BATCH_AGE_COMMITS: u32 = 4;
/// Group commit: the longest a batch stays open, however slow commits are. Like [`BATCH_AGE`], it is checked when a
/// mutation finishes: one that runs long keeps the batch open until it ends.
pub const MAX_BATCH_AGE: Duration = Duration::from_millis(32);
/// Group commit: a write transaction whose label (the mutation's path) has recently taken longer than this commits
/// the open batch before it starts, so the mutations already in the batch don't wait for it.
pub const SLOW_WRITE: Duration = Duration::from_micros(500);
/// A mutation holding the writer this long is logged as a warning (at most once a minute per path): every other write
/// waited for it.
pub const WARN_WRITE: Duration = Duration::from_millis(100);
/// An index key longer than this is logged as a warning (at most once an hour per index, and when an index is
/// backfilled): with the index id, the document id and the record header, its `index_entries` row nears the most of
/// a row a 4 KiB B-tree page holds (1002 bytes, SQLite's `maxLocal`), past which the rest goes to an overflow page of
/// its own. In `bench_index_key_length` (integration tests), that made scans of 100 entries 10x slower and lookups 2x.
pub const WARN_KEY_BYTES: usize = 960;

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
    /// Wait for each commit to reach the disk (SQLite `synchronous = FULL`, the default) instead of only the OS
    /// (`NORMAL`: a process crash loses nothing, a power loss may lose the last commits). With group commit this is
    /// one fsync per batch of mutations.
    pub fsync_commits: bool,
    /// The most memory, in bytes, the writer connection's page cache may use ([`DEFAULT_WRITER_CACHE_SIZE`]).
    pub writer_cache_size: u64,
    /// The schema check already ran on this database file (a takeover checked its copy before swapping it in,
    /// `crate::ops::takeover`): skip it, and open these tables as *migrating*.
    pub prechecked: Option<Vec<String>>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            readers: default_readers(),
            skip_schema_check: false,
            journal: false,
            fsync_commits: true,
            writer_cache_size: DEFAULT_WRITER_CACHE_SIZE,
            prechecked: None,
        }
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
        crate::ops::backup::create_dir_all_synced(dir)
            .with_context(|| format!("creating data dir {}", dir.display()))?;
        configure_sqlite();
        search::vector::register_extension();
        let path = dir.join("data.db");
        let mut writer = storage::open_writer(&path)?;
        if options.fsync_commits {
            writer.pragma_update(None, "synchronous", "FULL")?;
        }
        // A negative cache_size is in KiB.
        writer.pragma_update(
            None,
            "cache_size",
            -i64::try_from(options.writer_cache_size >> 10).unwrap_or(i64::MAX),
        )?;
        let responses = dedup::Index::open(&writer)?;
        writer.execute_batch(journal::DDL)?;
        let catalog = catalog::reconcile(&mut writer, tables)?;
        let migrating = match &options.prechecked {
            Some(migrating) => migrating.clone(),
            None => {
                let coverage = crate::migrations::coverage(&writer, &catalog, migrations)?;
                schema_check::run(&writer, &catalog, tables, options.skip_schema_check, &coverage)?
            }
        };
        for table in migrating {
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
        let batch_guard = Arc::new(batch_guard::BatchGuard::default());
        batch_guard::install(&writer, &batch_guard);
        Ok(Database {
            writer: Arc::new(WriterShared {
                lock: Arc::new(tokio::sync::Mutex::new(Writer {
                    conn: writer,
                    batch: None,
                    committed_meta: None,
                    responses,
                    last_commit: Duration::ZERO,
                    checkpoint: checkpointer.signal().clone(),
                    batch_guard,
                })),
                waiting: AtomicUsize::new(0),
                durations: Mutex::new(std::collections::HashMap::new()),
                write_log: write_log.clone(),
                journal: journal.clone(),
                sealed: std::sync::atomic::AtomicBool::new(false),
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

    /// Refuse every write from now on, once the ones in flight have committed: what a server does before it lets go
    /// of the data directory, so a background task can't commit after another process took over (`serve`,
    /// `crate::ops::takeover`). Returns the ts of the last commit.
    pub async fn seal(&self) -> anyhow::Result<u64> {
        let mut guard = self.writer.lock().await;
        self.writer.sealed.store(true, Ordering::SeqCst);
        let finished = self.writer.commit_batch(&mut guard);
        finish_members(finished);
        // (Still in a transaction if that commit and its rollback both failed: the ts read below would be one that
        // isn't committed.)
        let ts = (|| {
            if !guard.is_autocommit() {
                guard.execute_batch("ROLLBACK")?;
            }
            storage::read_meta_u64(&guard, storage::META_TS)
        })();
        self.writer.release(guard);
        ts
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
        if self.writer.sealed.load(Ordering::SeqCst) {
            self.writer.release(guard);
            anyhow::bail!("the database is closed for writes (the server is shutting down)");
        }
        // A batch of mutations left for this holder to continue: commit it first.
        let finished = self.writer.commit_batch(&mut guard);
        finish_members(finished);
        if !guard.is_autocommit() {
            guard.execute_batch("ROLLBACK")?;
        }
        // `f` may write anything, the batch `_meta` values included.
        guard.committed_meta = None;
        guard.execute_batch("BEGIN IMMEDIATE")?;
        let result = match f(&guard) {
            Ok(r) => guard.execute_batch("COMMIT").map(|()| r).map_err(|e| {
                // A commit can fail and leave its transaction open: what `f` wrote must not be read below.
                if !guard.is_autocommit() {
                    let _ = guard.execute_batch("ROLLBACK");
                }
                e.into()
            }),
            Err(e) => {
                let _ = guard.execute_batch("ROLLBACK");
                Err(e)
            }
        };
        // `f` may have imported mutation responses ([`dedup::import`]). (Not read while a transaction is still open,
        // which takes a commit and its rollback both failing: the connection would show what isn't committed.)
        let writer = &mut *guard;
        let resynced = if writer.conn.is_autocommit() {
            writer.responses.resync(&writer.conn)
        } else {
            writer.responses.mark_stale();
            Err(anyhow::anyhow!("a transaction was left open on the writer connection"))
        };
        self.writer.release(guard);
        resynced.context("reloading the recorded mutation responses")?;
        result
    }

    /// Re-apply one journal entry ([`journal`]) as its own commit, with its ts and time: the documents are written
    /// through the write path (indexes, search and vector entries maintained, the write log appended), unvalidated.
    /// The database must be exactly at `entry.ts - 1`. Used by point-in-time restores (`crate::ops::replicate`).
    pub async fn replay(&self, entry: &journal::JournalEntry) -> anyhow::Result<u64> {
        self.replay_with(entry, false).await
    }

    /// Like [`replay`](Self::replay), but every document written must match its table's type (unless the table is
    /// migrating): a takeover catching up on what the old server wrote after the copy (`crate::ops::takeover`).
    pub async fn replay_checked(&self, entry: &journal::JournalEntry) -> anyhow::Result<u64> {
        self.replay_with(entry, true).await
    }

    async fn replay_with(&self, entry: &journal::JournalEntry, validate: bool) -> anyhow::Result<u64> {
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
                    .with_context(|| {
                        format!("journal entry ts {}: no table number {}", entry.ts, change.id.table_number())
                    })?
                    .clone();
                anyhow::ensure!(
                    table.name == change.table,
                    "journal entry ts {}: table number {} is `{}` here but `{}` in the journal",
                    entry.ts,
                    table.number,
                    table.name,
                    change.table
                );
                if validate
                    && let (Some(doc), Some(def)) = (change.doc.as_ref(), table.def.as_ref())
                    && !self.catalog.is_migrating(&table.name)
                    && let Err(e) = (def.validate)(&Value::Object(doc.clone()))
                {
                    anyhow::bail!(
                        "a `{}` document written at ts {} doesn't match the table's type: {e}",
                        table.name,
                        entry.ts
                    );
                }
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
                for index in table.stored_indexes() {
                    // One pass over the documents per index: the expected keys are held, the documents aren't.
                    let mut expected: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                    storage::for_each_document(conn, table.number, |id, json| {
                        let doc: Value = serde_json::from_str(json)?;
                        let obj = doc.as_object().context("document is not an object")?;
                        expected.push((keys::index_key(obj, &index.key_fields, &id), id.to_vec()));
                        Ok(())
                    })?;
                    expected.sort();
                    let mut stmt =
                        conn.prepare("SELECT key, doc_id FROM index_entries WHERE index_id = ?1 ORDER BY key")?;
                    let actual: Vec<(Vec<u8>, Vec<u8>)> =
                        stmt.query_map([index.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
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
        if self.writer.sealed.load(Ordering::SeqCst) {
            self.writer.release(guard);
            anyhow::bail!("the database is closed for writes (the server is shutting down)");
        }
        let mut conn = WriterGuard {
            guard: Some(guard),
            shared: self.writer.clone(),
            undo: Undo::Nothing,
            label: label.to_string(),
            began: Instant::now(),
        };
        // (A batch that lost its transaction isn't left open for a waiter, see `release`; checked here all the same,
        // since joining one would begin a transaction of this write's own.)
        if conn.writer().batch.is_some() && (self.writer.expected(label) > SLOW_WRITE || conn.writer().batch_lost()) {
            let finished = self.writer.commit_batch(conn.writer_mut());
            finish_members(finished);
        }
        if conn.writer().batch.is_none() {
            if conn.writer().responses.is_stale() {
                // Not before the recorded responses are known again: a repeat would run a second time.
                let writer = conn.writer_mut();
                if !writer.conn.is_autocommit() {
                    writer.conn.execute_batch("ROLLBACK")?;
                }
                writer.responses.resync(&writer.conn).context("loading the recorded mutation responses")?;
            }
            let batch = conn.writer().begin_batch()?;
            conn.writer_mut().open_batch(batch);
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
    /// The batch `_meta` values (ts, time, last creation time) as last committed through this writer, so a batch
    /// begins without reading them; `None` when unknown (after [`Database::with_writer`], which may write them).
    committed_meta: Option<(u64, f64, f64)>,
    /// The mutation responses on record, and the open batch's ([`dedup`]).
    responses: dedup::Index,
    /// How long the last batch took to commit, for [`BATCH_AGE_COMMITS`].
    last_commit: Duration,
    /// What the connection's WAL hook nudges ([`checkpoint::install_hook`]); dropped after `conn`.
    checkpoint: Arc<checkpoint::Signal>,
    /// What the connection's commit and rollback hooks read ([`batch_guard`]): whether `batch` is open, and whether
    /// its transaction is gone. Dropped after `conn`.
    batch_guard: Arc<batch_guard::BatchGuard>,
}

impl std::ops::Deref for Writer {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl Writer {
    /// The batch that [`Writer::begin_batch`] began is open from here on: nothing but its commit may commit
    /// ([`batch_guard`]).
    fn open_batch(&mut self, batch: Batch) {
        self.batch_guard.opened();
        self.batch = Some(batch);
    }

    /// The open batch, which ends here: its `COMMIT` or a `ROLLBACK` follows.
    fn close_batch(&mut self) -> Option<Batch> {
        self.batch_guard.closed();
        self.batch.take()
    }

    /// SQLite ended the transaction of the batch that is open, or that [`Writer::close_batch`] just closed (it does
    /// on a full disk, an I/O error, when out of memory): the batch's writes are gone, and none of it can be kept.
    fn batch_lost(&self) -> bool {
        self.batch_guard.lost() || self.conn.is_autocommit()
    }

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
            if let Some(meta) = self.committed_meta {
                return Ok(meta);
            }
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
        self.batch_guard.closed();
        if self.batch.is_some() && !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        checkpoint::remove_hook(&self.conn);
        batch_guard::remove(&self.conn);
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
/// unless another write is already waiting for the lock (and the batch is below [`MAX_BATCH`] and younger than
/// [`BATCH_AGE`], or than [`BATCH_AGE_COMMITS`] times the last commit);
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
    /// Set by [`Database::seal`]: no write transaction may begin any more.
    sealed: std::sync::atomic::AtomicBool,
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
        let age = (guard.last_commit * BATCH_AGE_COMMITS).clamp(BATCH_AGE, MAX_BATCH_AGE);
        let young = |b: &Batch| b.members.len() < MAX_BATCH && b.started.elapsed() < age;
        let continue_batch = guard.batch.as_ref().is_some_and(young) && self.waiting.load(Ordering::SeqCst) > 0;
        // A batch whose transaction SQLite ended fails here (`commit_batch`), not in the hands of the next write.
        if !continue_batch || guard.batch_lost() {
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
    /// are appended in ts order, still under the lock. The members' recorded responses ([`dedup`]) are written right
    /// before the `COMMIT`, as one row. On failure the whole batch rolls back and every member gets the error. Returns
    /// the members, for [`finish_members`] (after releasing the lock).
    fn commit_batch(&self, writer: &mut Writer) -> Vec<Member> {
        let Some(mut batch) = writer.close_batch() else { return Vec::new() };
        let started = Instant::now();
        let committed = (|| -> anyhow::Result<dedup::Written> {
            // What follows would be written without the batch's writes: a recorded response for a mutation that
            // wasn't applied.
            anyhow::ensure!(!writer.batch_lost(), "{LOST}");
            if batch.ts != batch.begin_ts {
                storage::write_batch_meta(&writer.conn, batch.ts, batch.time, batch.last_creation_time)?;
            }
            let responses = writer.responses.write_pending(&writer.conn)?;
            storage::exec_cached(&writer.conn, "COMMIT")?;
            Ok(responses)
        })();
        match committed {
            Ok(responses) => {
                writer.last_commit = started.elapsed();
                writer.responses.committed(responses);
                writer.committed_meta = Some((batch.ts, batch.time, batch.last_creation_time));
                for member in &batch.members {
                    self.journal.add_pending_bytes(member.journaled);
                }
                self.write_log.append_all(batch.members.iter_mut().filter_map(|m| m.entry.take()));
            }
            Err(e) => {
                tracing::error!("commit failed: {e:#}");
                writer.committed_meta = None;
                writer.responses.rolled_back();
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

/// The error of a batch whose transaction SQLite ended ([`Writer::batch_lost`]).
const LOST: &str = "SQLite ended the transaction (a full disk or an I/O error does that): nothing of it was kept";

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
                writer.close_batch();
                writer.responses.rolled_back();
                // (Nothing to roll back if SQLite ended the transaction itself, unless a statement after that left
                // one open: `batch_guard`.)
                if !writer.conn.is_autocommit()
                    && let Err(e) = writer.conn.execute_batch("ROLLBACK")
                {
                    tracing::error!("rollback failed: {e}");
                }
            }
            Undo::Savepoint => {
                let undone = if self.writer().batch_lost() {
                    Err(anyhow::anyhow!("{LOST}"))
                } else {
                    storage::exec_cached(self, "ROLLBACK TO mutation")
                        .and_then(|()| storage::exec_cached(self, "RELEASE mutation"))
                };
                if let Err(e) = undone {
                    // The batch's transaction is gone or in an unknown state: roll all of it back (its members get
                    // the error).
                    tracing::error!("rollback to savepoint failed: {e:#}");
                    let writer = self.writer_mut();
                    let batch = writer.close_batch();
                    if !writer.conn.is_autocommit() {
                        let _ = writer.conn.execute_batch("ROLLBACK");
                    }
                    writer.responses.rolled_back();
                    if let Some(batch) = batch {
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

    /// Keep this transaction's writes: release the savepoint into the batch, add the member (and the response to
    /// record for it, [`dedup`]), and release the lock.
    fn finish(
        mut self,
        member: Member,
        (ts, time, last_creation_time): (u64, f64, f64),
        response: Option<dedup::Record>,
    ) -> anyhow::Result<()> {
        // This transaction's writes are gone with the batch's (dropping `self` fails the batch).
        anyhow::ensure!(!self.writer().batch_lost(), "{LOST}");
        if self.undo == Undo::Savepoint {
            storage::exec_cached(&self, "RELEASE mutation")?;
        }
        self.undo = Undo::Nothing;
        let writer = self.writer_mut();
        let batch = writer.batch.as_mut().expect("a batch is open");
        if member.entry.is_some() {
            batch.ts = ts;
            batch.time = time;
            batch.last_creation_time = last_creation_time;
        }
        batch.members.push(member);
        if let Some(response) = response {
            writer.responses.add(response);
        }
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
/// `SQLITE_ENABLE_MEMORY_MANAGEMENT` (rusqlite's bundled build defined it up to libsqlite3-sys 0.38), the page cache that all connections then
/// share. With both, N readers ran no faster than one. Memory statistics are turned off here (if SQLite is not
/// initialized yet; the workspace also builds it with `SQLITE_DEFAULT_MEMSTATUS=0`); the shared page cache can only
/// be avoided at build time, so its presence is logged (see `.cargo/config.toml`, which `rxferrix new` gives every app).
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
                 `rxferrix new` set it in their .cargo/config.toml; copy that file into this app's directory."
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
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

    /// Commit. Returns the new ts, or the current ts if no document was written (then ts is not bumped), once the
    /// batch holding this transaction has committed (group commit: see the module docs). If that commit fails, so
    /// does this, and nothing of this transaction is kept. `before_commit(conn, commit_ts)` runs inside the
    /// transaction first.
    pub(crate) async fn commit_with(
        &self,
        before_commit: impl FnOnce(&Connection, u64) -> anyhow::Result<()>,
    ) -> anyhow::Result<u64> {
        self.commit_inner(|conn, ts| before_commit(conn, ts).map(|()| None)).await
    }

    /// [`WriteTxn::commit_with`] for a mutation that returned `value`: with `key`, its response is recorded for
    /// idempotency ([`dedup`]) in the same SQLite transaction as its writes.
    pub(crate) async fn commit_recording(
        &self,
        key: Option<&dedup::IdempotencyKey>,
        value: &Value,
    ) -> anyhow::Result<u64> {
        self.commit_inner(|_, ts| key.map(|key| dedup::Record::success(key, value, ts)).transpose()).await
    }

    async fn commit_inner(
        &self,
        before_commit: impl FnOnce(&Connection, u64) -> anyhow::Result<Option<dedup::Record>>,
    ) -> anyhow::Result<u64> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut state = state.context("the mutation's transaction has already finished")?;
        let (done, committed) = tokio::sync::oneshot::channel();
        let on_commit = std::mem::take(&mut *self.on_commit.lock().unwrap_or_else(|e| e.into_inner()));
        let ts = if state.writes.is_empty() && !state.force_ts_bump {
            let response = before_commit(&state.conn, state.prev_ts)?;
            let member = Member { entry: None, journaled: 0, on_commit, done, result: Ok(()) };
            let meta = (state.prev_ts, state.now, state.last_creation_time);
            state.take_conn().finish(member, meta, response)?;
            meta.0
        } else {
            let ts = state.prev_ts + 1;
            let response = before_commit(&state.conn, ts)?;
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
            let meta = (ts, state.now, state.last_creation_time);
            state.take_conn().finish(member, meta, response)?;
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

    /// Roll back, then record `error` as the response for `key` ([`dedup`]) while still holding the writer lock, so
    /// a retry gets the same error. It commits with the batch, without waiting for it here, or at once if no batch
    /// is open (this transaction began it).
    pub(crate) fn rollback_recording(&self, key: &dedup::IdempotencyKey, error: &crate::Error) -> anyhow::Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut state = state.context("the mutation's transaction has already finished")?;
        state.conn.rollback_savepoint();
        let record = dedup::Record::error(key, error)?;
        let writer = state.conn.writer_mut();
        writer.responses.add(record);
        if writer.batch.is_none() {
            writer.responses.commit_alone(&writer.conn)?;
        }
        Ok(())
    }
}

impl WriteState {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The response on record for `key` ([`dedup`]), from a batch that committed or from the open one.
    pub(crate) fn recorded_response(
        &self,
        key: &dedup::IdempotencyKey,
    ) -> anyhow::Result<Option<dedup::StoredResponse>> {
        self.conn.writer().responses.lookup(&self.conn, key)
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
    /// and new keys for the write log. All or nothing: a failure part way (a SQLite error after some index entries
    /// changed) is rolled back to before this write, so a mutation that catches the error and goes on can't commit
    /// a half-written document.
    fn write(
        &mut self,
        table: &TableInfo,
        id: DocId,
        old: Option<&Map<String, Value>>,
        new: Option<&Map<String, Value>>,
    ) -> anyhow::Result<()> {
        let writes_before = self.writes.len();
        self.conn.prepare_cached("SAVEPOINT rxferrix_write")?.execute([])?;
        match self.write_unguarded(table, id, old, new) {
            Ok(()) => {
                self.conn.prepare_cached("RELEASE rxferrix_write")?.execute([])?;
                Ok(())
            }
            Err(e) => {
                let undone = self
                    .conn
                    .execute_batch("ROLLBACK TO rxferrix_write; RELEASE rxferrix_write")
                    .context("rolling back a failed write");
                self.writes.truncate(writes_before);
                undone?;
                Err(e)
            }
        }
    }

    /// [`WriteState::write`] without its savepoint.
    fn write_unguarded(
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
                    self.catalog.check_key_len(&table.name, index, k);
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
        self.insert_with_id(table, DocId::new_ordered(table.number, self.now), fields, validate)
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
        table.floats_as_floats(&mut fields);
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
        table.floats_as_floats(&mut fields);
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
        table.floats_as_floats(&mut new);
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
        table.floats_as_floats(&mut fields);
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

    #[tokio::test]
    async fn batches_see_meta_written_outside_them() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with(dir.path(), &[], &OpenOptions::default()).unwrap();
        // A write transaction without writes commits at the current ts; this one also leaves the writer's copy set.
        assert_eq!(db.begin_write().await.unwrap().commit_with(|_, _| Ok(())).await.unwrap(), 0);
        db.with_writer(|c| storage::write_meta(c, storage::META_TS, 41i64.into())).await.unwrap();
        assert_eq!(db.begin_write().await.unwrap().commit_with(|_, _| Ok(())).await.unwrap(), 41);
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

    #[tokio::test]
    async fn writer_cache_size_sets_the_writers_cache() {
        for (size, expected_kib) in [(DEFAULT_WRITER_CACHE_SIZE, 32 << 10), (100 << 20, 100 << 10)] {
            let dir = tempfile::tempdir().unwrap();
            let options = OpenOptions { writer_cache_size: size, ..Default::default() };
            let db = Database::open_with(dir.path(), &[], &options).unwrap();
            let cache = |c: &Connection| Ok(c.query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0))?);
            assert_eq!(db.with_writer(cache).await.unwrap(), -expected_kib);
        }
    }

    /// A database with a table `t` and a raw table `u` whose key 1 is taken: inserting it again with `OR ROLLBACK`
    /// makes SQLite end the open transaction by itself, as it does on a full disk or an I/O error
    /// ([`lose_transaction`]).
    async fn lossy(journal: bool) -> (tempfile::TempDir, Arc<Database>) {
        let def = crate::schema::TableDef {
            name: "t",
            indexes: vec![],
            search_indexes: vec![],
            vector_indexes: vec![],
            ts: || "{ }".into(),
            validate: |_| Ok(()),
            shape: crate::shape::Shape::unknown,
            aggregates: vec![],
        };
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with(dir.path(), &[def], &OpenOptions { journal, ..Default::default() }).unwrap();
        db.with_writer(|c| Ok(c.execute_batch("CREATE TABLE u (k PRIMARY KEY); INSERT INTO u VALUES (1);")?))
            .await
            .unwrap();
        // Batches stay open for a waiting write as long as they may, so the tests' writes share one.
        db.writer.lock.lock().await.last_commit = MAX_BATCH_AGE;
        (dir, Arc::new(db))
    }

    fn lose_transaction(txn: &WriteTxn) {
        let lost = txn.with_state(|s| Ok(s.conn().execute("INSERT OR ROLLBACK INTO u VALUES (1)", [])?));
        assert!(lost.is_err());
        assert!(txn.with_state(|s| Ok(s.conn().is_autocommit())).unwrap(), "SQLite ended the transaction");
    }

    async fn insert(txn: &Arc<WriteTxn>) -> crate::Result<String> {
        query::MutationDb::new(txn.clone()).insert_raw("t", serde_json::json!({})).await
    }

    fn key(request_id: u64) -> dedup::IdempotencyKey {
        dedup::IdempotencyKey { session_id: "s".into(), request_id }
    }

    /// What is durable, as another connection sees it: documents, journal rows, rows of `u`, the `_meta` ts.
    fn durable(dir: &Path) -> (i64, i64, i64, u64) {
        let conn = Connection::open(dir.join("data.db")).unwrap();
        let count = |table: &str| conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap();
        let ts = storage::read_meta_u64(&conn, storage::META_TS).unwrap();
        (count("documents"), count("_journal"), count("u"), ts)
    }

    async fn recorded(db: &Database, request_id: u64) -> bool {
        let txn = db.begin_write().await.unwrap();
        let response = txn.with_state(|s| s.recorded_response(&key(request_id))).unwrap();
        txn.rollback();
        response.is_some()
    }

    async fn wait_for_a_waiter(db: &Database) {
        while db.writer.waiting.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    }

    /// One more document, journal row and ts than `before`.
    fn one_more((documents, journal, raw, ts): (i64, i64, i64, u64), journaled: bool) -> (i64, i64, i64, u64) {
        (documents + 1, journal + journaled as i64, raw, ts + 1)
    }

    #[tokio::test]
    async fn a_mutation_that_goes_on_after_sqlite_ended_its_transaction_keeps_nothing() {
        let (dir, db) = lossy(true).await;
        let before = durable(dir.path());
        let txn = db.begin_write().await.unwrap();
        insert(&txn).await.unwrap();
        lose_transaction(&txn);
        // It doesn't stop at the error: a document, a statement of its own (as the file storage runs them), a commit.
        let _ = insert(&txn).await;
        let _ = txn.with_state(|s| Ok(s.conn().execute("INSERT INTO u VALUES (2)", [])?));
        let _ = insert(&txn).await;
        let committed = txn.commit_recording(Some(&key(1)), &serde_json::json!("done")).await;
        assert!(committed.is_err(), "told that it committed: {committed:?}");
        assert_eq!(durable(dir.path()), before);
        assert!(!recorded(&db, 1).await);
        // The next one is on its own, at the next ts.
        let txn = db.begin_write().await.unwrap();
        insert(&txn).await.unwrap();
        assert_eq!(txn.commit_recording(Some(&key(2)), &serde_json::json!("done")).await.unwrap(), before.3 + 1);
        assert_eq!(durable(dir.path()), one_more(before, true));
        assert!(recorded(&db, 2).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiting_mutation_does_not_continue_a_batch_sqlite_ended() {
        // Without the journal, the first one runs no statement after losing the transaction.
        let (dir, db) = lossy(false).await;
        let before = durable(dir.path());
        let first = db.begin_write().await.unwrap();
        insert(&first).await.unwrap();
        let second = tokio::spawn({
            let db = db.clone();
            async move {
                // Under a label of its own: a write expected to be slow commits the open batch before it begins.
                let txn = db.begin_write_as("second").await?;
                insert(&txn).await?;
                txn.commit_recording(Some(&key(2)), &serde_json::json!("second")).await
            }
        });
        wait_for_a_waiter(&db).await;
        lose_transaction(&first);
        let first = first.commit_recording(Some(&key(1)), &serde_json::json!("first")).await;
        let second = second.await.unwrap();
        assert!(first.is_err(), "told that it committed: {first:?}");
        assert!(!recorded(&db, 1).await);
        // The second is told it committed exactly when its writes are there; and they are, in a batch of its own.
        let expected = if second.is_ok() { one_more(before, false) } else { before };
        assert_eq!(durable(dir.path()), expected, "{second:?}");
        assert_eq!(recorded(&db, 2).await, second.is_ok());
        assert_eq!(second.ok(), Some(before.3 + 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_joined_mutation_that_loses_the_transaction_fails_its_batch() {
        let (dir, db) = lossy(true).await;
        let before = durable(dir.path());
        let first = db.begin_write().await.unwrap();
        insert(&first).await.unwrap();
        let second = tokio::spawn({
            let db = db.clone();
            async move { db.begin_write_as("second").await.unwrap() }
        });
        wait_for_a_waiter(&db).await;
        let first =
            tokio::spawn(async move { first.commit_recording(Some(&key(1)), &serde_json::json!("first")).await });
        let second = second.await.unwrap();
        // (On a machine too slow for that, the first one's batch committed before the second began.)
        let joined = second.with_state(|s| Ok(s.conn.undo == Undo::Savepoint)).unwrap();
        lose_transaction(&second);
        let _ = insert(&second).await;
        let second = second.commit_recording(Some(&key(2)), &serde_json::json!("second")).await;
        let first = first.await.unwrap();
        assert!(second.is_err(), "told that it committed: {second:?}");
        assert_eq!(first.is_ok(), !joined, "{first:?}");
        assert_eq!(durable(dir.path()), if joined { before } else { one_more(before, true) });
        assert_eq!((recorded(&db, 1).await, recorded(&db, 2).await), (!joined, false));
        // The writer goes on from what is there.
        let txn = db.begin_write().await.unwrap();
        insert(&txn).await.unwrap();
        let ts = txn.commit_with(|_, _| Ok(())).await.unwrap();
        assert_eq!(ts, before.3 + 1 + !joined as u64);
    }

    fn response(request_id: u64) -> (i64, dedup::Record) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        (now, dedup::Record::success(&key(request_id), &serde_json::json!("carried"), 1).unwrap())
    }

    /// `with_writer`'s commit can fail and leave its transaction open (a deferred constraint, here): what it wrote
    /// is rolled back before the recorded responses are read again, not found by them and gone later.
    #[tokio::test]
    async fn bookkeeping_whose_commit_fails_leaves_nothing() {
        let (dir, db) = lossy(false).await;
        db.writer
            .lock
            .lock()
            .await
            .execute_batch(
                "PRAGMA foreign_keys = ON;
             CREATE TABLE parent (id PRIMARY KEY);
             CREATE TABLE child (parent REFERENCES parent DEFERRABLE INITIALLY DEFERRED);",
            )
            .unwrap();
        let before = durable(dir.path());
        let failed = db
            .with_writer(|c| {
                dedup::import(c, vec![response(7)])?;
                c.execute("INSERT INTO child VALUES (1)", [])?;
                Ok(())
            })
            .await;
        let failed = format!("{:#}", failed.unwrap_err());
        assert!(failed.contains("FOREIGN KEY"), "{failed}");
        assert!(db.writer.lock.lock().await.is_autocommit(), "rolled back at once");
        assert!(!recorded(&db, 7).await);
        let txn = db.begin_write().await.unwrap();
        insert(&txn).await.unwrap();
        txn.commit_with(|_, _| Ok(())).await.unwrap();
        assert_eq!(durable(dir.path()), one_more(before, false));
    }

    /// The recorded responses couldn't be read again after an import: no mutation begins until they can, since a
    /// repeat of a mutation whose response was imported would run a second time.
    #[tokio::test]
    async fn no_write_begins_while_the_recorded_responses_are_unknown() {
        let (_dir, db) = lossy(false).await;
        // The import commits; reading the table after it fails (it is gone, standing in for a read error).
        let imported = db
            .with_writer(|c| {
                dedup::import(c, vec![response(7)])?;
                Ok(c.execute_batch("ALTER TABLE _mutation_responses RENAME TO elsewhere")?)
            })
            .await;
        assert!(imported.is_err());
        assert!(db.begin_write().await.is_err());
        db.writer.lock.lock().await.execute_batch("ALTER TABLE elsewhere RENAME TO _mutation_responses").unwrap();
        assert!(recorded(&db, 7).await);
    }
}

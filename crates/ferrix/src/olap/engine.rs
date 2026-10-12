//! The DuckDB copy (feature `olap`): open and lock down, rebuild tables from snapshots, apply journal batches, run
//! sandboxed queries. See the module docs of `crate::olap` and docs/CONTRACT.md §1.11.
//!
//! Tables in `olap.duckdb`: one per copied ferrix table, plus `_ferrix_meta` (`copy_id`, `applied_ts`: the copy
//! reflects every commit up to it), `_ferrix_tables` (each table's fingerprint and applied ts) and `_ferrix_stage`
//! (documents on their way in, empty between batches). All work runs on blocking threads, one step at a time
//! (`running`); queries use their own connections and see whole transactions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use duckdb::{Config, Connection, params};
use serde_json::{Map, Value};
use tokio::sync::{Semaphore, watch};

use super::columns::{TablePlan, copied, ident, literal};
use super::{
    OlapColumn, OlapExport, OlapExportFormat, OlapOptions, OlapResult, OlapStatus, OlapTableStatus, OlapValue,
};
use crate::UserError;
use crate::db::journal::{self, Consumer};
use crate::db::storage;
use crate::runtime::{App, WeakApp};

const STAGE: &str = "_ferrix_stage";
/// Journal rows read per batch, and their bytes.
const BATCH_ENTRIES: usize = 2000;
const BATCH_BYTES: usize = 32 << 20;
/// Documents staged at a time while rebuilding a table.
const REBUILD_CHUNK: usize = 50_000;
/// How often the journal is released (rows trimmed) while applying.
const RELEASE_EVERY: Duration = Duration::from_secs(1);
/// Commits released per hold of the writer lock: writes wait while rows are deleted, so a big backlog goes in steps.
const RELEASE_CHUNK: u64 = 2048;
/// How often the waiting journal's size is compared with `max_lag_bytes`.
const LAG_CHECK_EVERY: Duration = Duration::from_secs(5);

struct TableState {
    plan: TablePlan,
    number: u32,
    /// The copy of this table reflects every commit up to this ts.
    applied: u64,
    /// Rebuild it from a snapshot before applying more.
    rebuild: Option<String>,
}

enum Phase {
    Building(String),
    Ready,
    Failed(String),
}

struct State {
    phase: Phase,
    tables: Vec<TableState>,
    /// Every table reflects every commit up to this ts.
    cursor: u64,
    /// Rebuild every table (no copy yet, a restore, a journal gap, fallen behind).
    rebuild_all: Option<String>,
    rows_applied: u64,
    rebuilds: u64,
    last_release: Option<Instant>,
    /// The newest ts released to the journal (rows up to it may be gone).
    released: u64,
    last_lag_check: Option<Instant>,
}

impl State {
    /// Whether queries see a consistent copy: no rebuild pending and every table at the cursor.
    fn queryable(&self) -> bool {
        self.rebuild_all.is_none() && self.tables.iter().all(|t| t.rebuild.is_none() && t.applied == self.cursor)
    }
}

pub(super) struct Engine {
    options: OlapOptions,
    path: PathBuf,
    copy_id: String,
    /// The connection that writes (rebuilds and batches), used by one step at a time.
    writer: Arc<Mutex<Connection>>,
    /// Queries clone their connections from this one.
    base: Mutex<Connection>,
    state: Mutex<State>,
    running: tokio::sync::Mutex<()>,
    applied: watch::Sender<u64>,
    permits: Arc<Semaphore>,
}

fn unavailable(why: impl std::fmt::Display) -> crate::Error {
    UserError::new("olap_unavailable", format!("The analytics copy is not available yet: {why}")).into()
}

fn query_failed(e: impl std::fmt::Display) -> crate::Error {
    UserError::new("olap_query_failed", e.to_string()).into()
}

/// Open `olap.duckdb`, recreating it if it can't be opened (it is derived data).
fn open_file(path: &Path, options: &OlapOptions) -> anyhow::Result<Connection> {
    let config = || -> anyhow::Result<Config> {
        let mut c = Config::default();
        if let Some(threads) = options.threads {
            c = c.with("threads", threads.max(1).to_string())?;
        }
        if let Some(bytes) = options.memory_limit {
            c = c.with("memory_limit", format!("{bytes}B"))?;
        }
        Ok(c)
    };
    match Connection::open_with_flags(path, config()?) {
        Ok(conn) => Ok(conn),
        Err(e) => {
            tracing::warn!("olap: {} can't be opened ({e}); starting a new copy", path.display());
            for p in [path.to_path_buf(), path.with_extension("duckdb.wal")] {
                let _ = std::fs::remove_file(p);
            }
            Ok(Connection::open_with_flags(path, config()?)?)
        }
    }
}

fn meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT v FROM _ferrix_meta WHERE k = ?")?;
    let mut rows = stmt.query([key])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute("INSERT OR REPLACE INTO _ferrix_meta VALUES (?, ?)", [key, value])?;
    Ok(())
}

impl Engine {
    pub(super) async fn open(app: &App) -> crate::Result<Arc<Engine>> {
        let options = app.options().olap.clone();
        let path = app.dir().join(super::FILE);
        let mut plans: Vec<(TablePlan, u32)> = app
            .db()
            .catalog()
            .tables()
            .filter(|t| copied(&t.name))
            .filter_map(|t| t.def.as_ref().map(|d| (TablePlan::new(&t.name, &(d.shape)()), t.number)))
            .collect();
        plans.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let snapshot = app.db().snapshot().await?;
        let (sqlite_id, sqlite_ts) = snapshot.with_conn(|c| {
            let id: Option<String> =
                c.query_row("SELECT v FROM _meta WHERE k = ?1", [journal::META_OLAP_ID], |r| r.get(0)).ok();
            Ok((id, storage::read_meta_u64(c, storage::META_TS)?))
        })?;
        drop(snapshot);
        let opened = {
            let (path, options) = (path.clone(), options.clone());
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let conn = open_file(&path, &options)?;
                conn.execute_batch(&format!(
                    "CREATE TABLE IF NOT EXISTS _ferrix_meta (k VARCHAR PRIMARY KEY, v VARCHAR);
                     CREATE TABLE IF NOT EXISTS _ferrix_tables (name VARCHAR PRIMARY KEY, fingerprint VARCHAR, applied_ts BIGINT);
                     CREATE TABLE IF NOT EXISTS {STAGE} (tbl VARCHAR, id VARCHAR, ts BIGINT, doc VARCHAR, extra VARCHAR);
                     DELETE FROM {STAGE};"
                ))?;
                let copy_id = meta(&conn, "copy_id")?;
                let cursor = meta(&conn, "applied_ts")?.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let mut existing = HashMap::new();
                {
                    let mut stmt = conn.prepare("SELECT name, fingerprint, applied_ts FROM _ferrix_tables")?;
                    let mut rows = stmt.query([])?;
                    while let Some(r) = rows.next()? {
                        existing.insert(r.get::<_, String>(0)?, (r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64));
                    }
                }
                Ok((conn, copy_id, cursor, existing))
            })
            .await??
        };
        let (conn, duck_id, duck_cursor, existing) = opened;

        // What to rebuild.
        let mut rebuild_all = match (&duck_id, &sqlite_id) {
            (None, _) => Some("there is no copy yet".to_string()),
            (Some(d), Some(s)) if d == s => None,
            (Some(_), _) => Some("the database was restored or replaced since the copy was made".to_string()),
        };
        if rebuild_all.is_none() && duck_cursor > sqlite_ts {
            rebuild_all = Some(format!("the copy is at commit {duck_cursor}, beyond the database's {sqlite_ts}"));
        }
        let copy_id = match (&rebuild_all, duck_id) {
            (None, Some(id)) => id,
            _ => crate::id::DocId::new_random(0).encode(),
        };
        let tables: Vec<TableState> = plans
            .into_iter()
            .map(|(plan, number)| {
                let (applied, rebuild) = match existing.get(&plan.name) {
                    _ if rebuild_all.is_some() => (0, None),
                    None => (0, Some("it is new".to_string())),
                    Some((fp, _)) if *fp != plan.fingerprint => (0, Some("its shape changed".to_string())),
                    Some((_, applied)) => (*applied, None),
                };
                TableState { plan, number, applied, rebuild }
            })
            .collect();
        // Tables the copy no longer has.
        let keep: Vec<String> = tables.iter().map(|t| t.plan.name.clone()).collect();
        let stale: Vec<String> = existing.keys().filter(|n| !keep.contains(n)).cloned().collect();
        let (conn, base) = {
            let options = options.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<(Connection, Connection)> {
                for name in &stale {
                    conn.execute_batch(&format!(
                        "BEGIN; DROP TABLE IF EXISTS {}; DELETE FROM _ferrix_tables WHERE name = {}; COMMIT;",
                        ident(name),
                        literal(name)
                    ))?;
                }
                // The sandbox, for every connection of this database: no files, network or extensions, and the
                // configuration can't be changed back (`query` only runs single SELECTs on top of this).
                let _ = &options;
                conn.execute_batch(
                    "SET enable_external_access = false;
                     SET autoinstall_known_extensions = false;
                     SET autoload_known_extensions = false;
                     SET lock_configuration = true;",
                )?;
                let base = conn.try_clone()?;
                Ok((conn, base))
            })
            .await??
        };
        let building = rebuild_all.clone().or_else(|| tables.iter().find_map(|t| t.rebuild.clone()));
        let cursor = if rebuild_all.is_some() { 0 } else { duck_cursor };
        tracing::info!(
            file = %path.display(),
            tables = tables.len(),
            cursor,
            rebuild = building.as_deref().unwrap_or("no"),
            "olap: analytics copy opened"
        );
        let (applied, _) = watch::channel(cursor);
        Ok(Arc::new(Engine {
            permits: Arc::new(Semaphore::new(options.max_concurrent_queries.max(1))),
            options,
            path,
            copy_id,
            writer: Arc::new(Mutex::new(conn)),
            base: Mutex::new(base),
            state: Mutex::new(State {
                phase: match &building {
                    Some(why) => Phase::Building(why.clone()),
                    None => Phase::Ready,
                },
                tables,
                cursor,
                rebuild_all,
                rows_applied: 0,
                rebuilds: 0,
                last_release: None,
                released: cursor,
                last_lag_check: None,
            }),
            running: tokio::sync::Mutex::new(()),
            applied,
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn request_rebuild(&self, why: &str) {
        let mut st = self.state();
        st.rebuild_all = Some(why.to_string());
        st.phase = Phase::Building(why.to_string());
    }

    /// One unit of work: a pending rebuild, else one batch of journal rows. Returns whether there may be more to do.
    async fn step(&self, app: &App) -> crate::Result<bool> {
        let (all, some) = {
            let st = self.state();
            let some: Vec<usize> =
                st.tables.iter().enumerate().filter(|(_, t)| t.rebuild.is_some()).map(|(i, _)| i).collect();
            (st.rebuild_all.clone(), some)
        };
        if let Some(why) = all {
            let n = self.state().tables.len();
            self.rebuild(app, (0..n).collect(), true, &why).await?;
            return Ok(true);
        }
        if !some.is_empty() {
            let why = self.state().tables[some[0]].rebuild.clone().unwrap_or_default();
            self.rebuild(app, some, false, &why).await?;
            return Ok(true);
        }
        let more = self.apply(app).await?;
        let mut st = self.state();
        if st.queryable() {
            st.phase = Phase::Ready;
        }
        Ok(more)
    }

    /// Rebuild the tables at `indexes` from one snapshot. `full`: every table, and the copy's cursor moves to the
    /// snapshot (the journal before it is released first, so a copy that fell behind lets it go).
    async fn rebuild(&self, app: &App, indexes: Vec<usize>, full: bool, why: &str) -> crate::Result<()> {
        tracing::info!(tables = indexes.len(), full, reason = why, "olap: rebuilding");
        self.state().phase = Phase::Building(why.to_string());
        let (id, journal) = (self.copy_id.clone(), app.db().journal().clone());
        let released = app
            .db()
            .with_writer(move |conn| {
                storage::write_meta(conn, journal::META_OLAP_ID, rusqlite::types::Value::Text(id))?;
                if !full {
                    return Ok(None);
                }
                let ts = storage::read_meta_u64(conn, storage::META_TS)?;
                journal::release(conn, &journal, Consumer::Olap, ts)?;
                Ok(Some(ts))
            })
            .await?;
        if let Some(ts) = released {
            self.state().released = ts;
        }
        let snapshot = app.db().snapshot().await?;
        let ts = snapshot.ts;
        if full {
            let (writer, copy_id) = (self.writer.clone(), self.copy_id.clone());
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let conn = writer.lock().unwrap_or_else(|e| e.into_inner());
                set_meta(&conn, "copy_id", &copy_id)
            })
            .await??;
        }
        for i in indexes {
            let (plan, number) = {
                let st = self.state();
                (st.tables[i].plan.clone(), st.tables[i].number)
            };
            let (writer, snap) = (self.writer.clone(), snapshot.clone());
            let rows = tokio::task::spawn_blocking(move || rebuild_table(&writer, &snap, &plan, number, ts)).await??;
            let mut st = self.state();
            st.tables[i].applied = ts;
            st.tables[i].rebuild = None;
            st.rows_applied += rows;
        }
        self.state().rebuilds += 1;
        drop(snapshot);
        if full {
            let writer = self.writer.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let conn = writer.lock().unwrap_or_else(|e| e.into_inner());
                set_meta(&conn, "applied_ts", &ts.to_string())
            })
            .await??;
            {
                let mut st = self.state();
                st.cursor = ts;
                st.rebuild_all = None;
            }
            self.applied.send_replace(ts);
            self.release(app, ts).await?;
        }
        Ok(())
    }

    /// Apply the next batch of journal rows. Returns whether it applied any.
    async fn apply(&self, app: &App) -> crate::Result<bool> {
        let cursor = self.state().cursor;
        let snapshot = app.db().snapshot().await?;
        if snapshot.ts <= cursor {
            return Ok(false);
        }
        let check_lag = self.state().last_lag_check.is_none_or(|t| t.elapsed() >= LAG_CHECK_EVERY);
        if check_lag {
            self.state().last_lag_check = Some(Instant::now());
        }
        let (applied_by_table, plans): (HashMap<String, u64>, Vec<TablePlan>) = {
            let st = self.state();
            (
                st.tables.iter().map(|t| (t.plan.name.clone(), t.applied)).collect(),
                st.tables.iter().map(|t| t.plan.clone()).collect(),
            )
        };
        // Reading and parsing the journal is CPU work: off the async threads, where the writers' tasks run.
        let max_lag = self.options.max_lag_bytes;
        let batch =
            tokio::task::spawn_blocking(move || read_batch(&snapshot, cursor, check_lag, max_lag, &applied_by_table))
                .await??;
        let (latest_state, last) = match batch {
            Batch::Rebuild(why) => {
                self.request_rebuild(&why);
                return Ok(true);
            }
            Batch::Changes { latest_state, last } => (latest_state, last),
        };
        let writer = self.writer.clone();
        let n = tokio::task::spawn_blocking(move || apply_batch(&writer, &plans, latest_state, last)).await??;
        {
            let mut st = self.state();
            for t in &mut st.tables {
                t.applied = t.applied.max(last);
            }
            st.cursor = last;
            st.rows_applied += n;
        }
        self.applied.send_replace(last);
        // Releasing takes the writer lock: once a second, not after every batch.
        if self.state().last_release.is_none_or(|t| t.elapsed() >= RELEASE_EVERY) {
            self.release(app, last).await?;
        }
        Ok(true)
    }

    /// The journal rows up to `ts` are applied: let them go (if replication doesn't need them), at most
    /// [`RELEASE_CHUNK`] commits per hold of the writer lock.
    async fn release(&self, app: &App, ts: u64) -> crate::Result<()> {
        let mut from = self.state().released.min(ts);
        loop {
            let to = (from + RELEASE_CHUNK).min(ts);
            let journal = app.db().journal().clone();
            app.db().with_writer(move |conn| journal::release(conn, &journal, Consumer::Olap, to)).await?;
            from = to;
            self.state().released = to;
            if to >= ts {
                break;
            }
            tokio::task::yield_now().await;
        }
        self.state().last_release = Some(Instant::now());
        Ok(())
    }

    /// Bring the copy up to the newest commit now.
    pub(super) async fn sync(&self, app: &App) -> crate::Result<()> {
        let _one = self.running.lock().await;
        for _ in 0..10_000 {
            let more = match self.step(app).await {
                Ok(more) => more,
                Err(e) => {
                    self.state().phase = Phase::Failed(format!("{e:#}"));
                    return Err(e);
                }
            };
            let caught_up = {
                let st = self.state();
                st.queryable() && st.cursor >= app.db().write_log().latest()
            };
            if caught_up && !more {
                let cursor = self.state().cursor;
                return self.release(app, cursor).await;
            }
        }
        anyhow::bail!("olap: the copy didn't catch up")
    }

    pub(super) async fn query(&self, sql: &str, params: Vec<OlapValue>, at_least: u64) -> crate::Result<OlapResult> {
        let (sql, max_rows) = (sql.to_string(), self.options.max_result_rows);
        self.sandboxed(at_least, self.options.query_timeout, move |conn| run_query(conn, &sql, params, max_rows)).await
    }

    pub(super) async fn export(
        &self,
        sql: &str,
        params: Vec<OlapValue>,
        format: OlapExportFormat,
        path: PathBuf,
    ) -> crate::Result<OlapExport> {
        let (sql, max_rows) = (sql.to_string(), self.options.max_export_rows);
        self.sandboxed(0, self.options.export_timeout, move |conn| {
            run_export(conn, &sql, params, format, &path, max_rows)
        })
        .await
    }

    /// Run `f` on a connection of its own once the copy has applied commit `at_least` and is consistent, with at most
    /// `max_concurrent_queries` at once, interrupting it after `timeout`.
    async fn sandboxed<T: Send + 'static>(
        &self,
        at_least: u64,
        timeout: Duration,
        f: impl FnOnce(&Connection) -> crate::Result<T> + Send + 'static,
    ) -> crate::Result<T> {
        if at_least > *self.applied.borrow() {
            let mut rx = self.applied.subscribe();
            let reached = tokio::time::timeout(self.options.query_timeout, rx.wait_for(|ts| *ts >= at_least)).await;
            if !matches!(reached, Ok(Ok(_))) {
                return Err(UserError::new(
                    "olap_timeout",
                    format!(
                        "The analytics copy didn't reach commit {at_least} within {:?}",
                        self.options.query_timeout
                    ),
                )
                .into());
            }
        }
        {
            let st = self.state();
            if !st.queryable() {
                let why = match &st.phase {
                    Phase::Building(why) => format!("it is being built ({why})"),
                    Phase::Failed(e) => format!("building it failed ({e}); retrying"),
                    Phase::Ready => "a table is being rebuilt".into(),
                };
                return Err(unavailable(why));
            }
        }
        let _permit = self.permits.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!(e))?;
        let conn = self.base.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        let interrupt = conn.interrupt_handle();
        let mut task = tokio::task::spawn_blocking(move || f(&conn));
        match tokio::time::timeout(timeout, &mut task).await {
            Ok(joined) => joined.map_err(|e| anyhow::anyhow!(e))?,
            Err(_) => {
                interrupt.interrupt();
                let _ = task.await;
                Err(UserError::new("olap_timeout", format!("The analytics query ran longer than {timeout:?}")).into())
            }
        }
    }

    pub(super) async fn status(&self, app: &App) -> OlapStatus {
        let latest = app.db().write_log().latest();
        let (state, detail, cursor, rows_applied, rebuilds, plans) = {
            let st = self.state();
            let (state, detail) = match &st.phase {
                Phase::Building(why) => ("building", Some(why.clone())),
                Phase::Ready => ("ready", None),
                Phase::Failed(e) => ("failed", Some(e.clone())),
            };
            (
                state.to_string(),
                detail,
                st.cursor,
                st.rows_applied,
                st.rebuilds,
                st.tables.iter().map(|t| t.plan.clone()).collect::<Vec<_>>(),
            )
        };
        let applied_time = match app.db().snapshot().await {
            Ok(s) => s
                .with_conn(|c| {
                    Ok(c.query_row("SELECT time FROM _journal WHERE ts = ?1", [cursor as i64], |r| r.get::<_, f64>(0))
                        .ok())
                })
                .ok()
                .flatten(),
            Err(_) => None,
        };
        let now = app.db().clock().now_ms();
        let conn = self.base.lock().unwrap_or_else(|e| e.into_inner()).try_clone();
        let tables = match conn {
            Ok(conn) => tokio::task::spawn_blocking(move || {
                plans
                    .iter()
                    .map(|p| OlapTableStatus {
                        name: p.name.clone(),
                        rows: conn
                            .query_row(&format!("SELECT count(*) FROM {}", ident(&p.name)), [], |r| r.get::<_, i64>(0))
                            .unwrap_or(0) as u64,
                        columns: p
                            .columns
                            .iter()
                            .map(|c| OlapColumn { name: c.name.clone(), type_name: c.sql_type.clone() })
                            .collect(),
                    })
                    .collect()
            })
            .await
            .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let size_bytes = [self.path.clone(), self.path.with_extension("duckdb.wal")]
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        OlapStatus {
            state,
            detail,
            applied_ts: cursor,
            latest_ts: latest,
            applied_time: applied_time.unwrap_or(0.0),
            lag_ms: if cursor >= latest { 0.0 } else { applied_time.map_or(0.0, |t| (now - t).max(0.0)) },
            tables,
            size_bytes,
            rows_applied,
            rebuilds,
        }
    }
}

/// Follow the database: look for new commits every `apply_every`, rebuild or apply until caught up; back off on
/// errors. Ends when the app is dropped.
pub(super) async fn run(engine: Arc<Engine>, weak: WeakApp) {
    let mut failures = 0u32;
    loop {
        let delay = if failures == 0 {
            engine.options.apply_every
        } else {
            (Duration::from_secs(1) * 2u32.saturating_pow(failures.min(6))).min(Duration::from_secs(60))
        };
        tokio::time::sleep(delay).await;
        let Some(app) = weak.upgrade() else { return };
        let _one = engine.running.lock().await;
        let mut result = Ok(());
        for _ in 0..64 {
            match engine.step(&app).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        match result {
            Ok(()) => failures = 0,
            Err(e) => {
                failures += 1;
                tracing::error!("olap: {e:#}");
                engine.state().phase = Phase::Failed(format!("{e:#}"));
            }
        }
    }
}

/// What the next journal rows mean for the copy.
enum Batch {
    /// The final state of each document they wrote (tables already past a row skip it), and the last ts.
    Changes { latest_state: HashMap<(String, String), (u64, Option<String>)>, last: u64 },
    /// The copy can't follow: rebuild it, for this reason.
    Rebuild(String),
}

/// Read and parse the journal rows after `cursor` on `snapshot` (blocking).
fn read_batch(
    snapshot: &crate::db::Snapshot,
    cursor: u64,
    check_lag: bool,
    max_lag: u64,
    applied_by_table: &HashMap<String, u64>,
) -> crate::Result<Batch> {
    if check_lag {
        let waiting: i64 = snapshot.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(octet_length(changes)), 0) FROM _journal WHERE ts > ?1",
                [cursor as i64],
                |r| r.get(0),
            )?)
        })?;
        if waiting as u64 > max_lag {
            return Ok(Batch::Rebuild(format!(
                "it fell behind by {} MiB of changes (--olap-max-lag)",
                waiting as u64 >> 20
            )));
        }
    }
    let entries = snapshot.with_conn(|c| journal::read_after(c, cursor, BATCH_ENTRIES, BATCH_BYTES))?;
    let Some(first) = entries.first() else {
        return Ok(Batch::Rebuild(format!(
            "the journal has no commits after {cursor} although the database is at {} (journaling was off)",
            snapshot.ts
        )));
    };
    if first.ts != cursor + 1 {
        return Ok(Batch::Rebuild(format!(
            "the journal starts at commit {} but the copy needs {} (rows were dropped)",
            first.ts,
            cursor + 1
        )));
    }
    let last = entries.last().map_or(cursor, |e| e.ts);
    let mut latest_state = HashMap::new();
    for entry in &entries {
        for change in entry.parse_raw_changes()? {
            if applied_by_table.get(&change.table).is_some_and(|applied| entry.ts > *applied) {
                latest_state.insert((change.table, change.id), (entry.ts, change.doc));
            }
        }
    }
    Ok(Batch::Changes { latest_state, last })
}

/// Stage `rows` (`(table, id, ts, doc, extra)`) in `_ferrix_stage`.
fn stage(
    conn: &Connection,
    rows: impl IntoIterator<Item = (String, String, u64, Option<String>, Option<String>)>,
) -> anyhow::Result<()> {
    let mut appender = conn.appender(STAGE)?;
    for (tbl, id, ts, doc, extra) in rows {
        appender.append_row(params![tbl, id, ts as i64, doc, extra])?;
    }
    appender.flush()?;
    Ok(())
}

/// Build `plan`'s table from `snapshot` under a temporary name and swap it in. Returns the documents copied.
fn rebuild_table(
    writer: &Mutex<Connection>,
    snapshot: &crate::db::Snapshot,
    plan: &TablePlan,
    number: u32,
    ts: u64,
) -> anyhow::Result<u64> {
    let conn = writer.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = format!("{}__ferrix_rebuild", plan.name);
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {}; {}; DELETE FROM {STAGE};",
        ident(&tmp),
        plan.create_sql(&tmp)
    ))?;
    let insert = plan.insert_sql(&tmp, STAGE, "doc IS NOT NULL");
    let flush = |conn: &Connection| -> anyhow::Result<()> {
        conn.execute_batch(&format!("{insert}; DELETE FROM {STAGE};"))?;
        Ok(())
    };
    let total = snapshot.with_conn(|sq| {
        let mut stmt = sq.prepare("SELECT json, ts FROM documents WHERE table_number = ?1")?;
        let mut rows = stmt.query([number])?;
        let mut chunk = Vec::with_capacity(REBUILD_CHUNK);
        let mut total = 0u64;
        while let Some(r) = rows.next()? {
            let (doc, doc_ts): (String, i64) = (r.get(0)?, r.get(1)?);
            let extra = plan.extra(&doc);
            chunk.push((plan.name.clone(), String::new(), doc_ts as u64, Some(doc), extra));
            total += 1;
            if chunk.len() >= REBUILD_CHUNK {
                stage(&conn, chunk.drain(..))?;
                flush(&conn)?;
            }
        }
        stage(&conn, chunk.drain(..))?;
        flush(&conn)?;
        Ok(total)
    })?;
    conn.execute_batch(&format!(
        "BEGIN;
         DROP TABLE IF EXISTS {name};
         ALTER TABLE {tmp} RENAME TO {name};
         INSERT OR REPLACE INTO _ferrix_tables VALUES ({lname}, {fp}, {ts});
         COMMIT;",
        name = ident(&plan.name),
        tmp = ident(&tmp),
        lname = literal(&plan.name),
        fp = literal(&plan.fingerprint),
    ))
    .with_context(|| format!("swapping in the rebuilt copy of `{}`", plan.name))?;
    Ok(total)
}

/// Apply the final states of a batch's documents, in one transaction that also moves every table to `last`.
fn apply_batch(
    writer: &Mutex<Connection>,
    plans: &[TablePlan],
    changes: HashMap<(String, String), (u64, Option<String>)>,
    last: u64,
) -> anyhow::Result<u64> {
    let conn = writer.lock().unwrap_or_else(|e| e.into_inner());
    let by_name: HashMap<&str, &TablePlan> = plans.iter().map(|p| (p.name.as_str(), p)).collect();
    let mut touched: Vec<&str> = Vec::new();
    let mut rows = Vec::with_capacity(changes.len());
    for ((table, id), (ts, doc)) in changes {
        let Some(plan) = by_name.get(table.as_str()) else { continue };
        if !touched.contains(&plan.name.as_str()) {
            touched.push(plan.name.as_str());
        }
        let extra = doc.as_deref().and_then(|d| plan.extra(d));
        rows.push((table, id, ts, doc, extra));
    }
    let n = rows.len() as u64;
    conn.execute_batch(&format!("BEGIN; DELETE FROM {STAGE};"))?;
    let result = (|| -> anyhow::Result<()> {
        stage(&conn, rows)?;
        for name in &touched {
            let plan = by_name[name];
            let filter = format!("tbl = {}", literal(name));
            conn.execute_batch(&format!(
                "DELETE FROM {t} WHERE _id IN (SELECT id FROM {STAGE} WHERE {filter}); {insert};",
                t = ident(name),
                insert = plan.insert_sql(name, STAGE, &format!("{filter} AND doc IS NOT NULL")),
            ))?;
        }
        conn.execute(
            "UPDATE _ferrix_tables SET applied_ts = ? WHERE applied_ts < ?",
            params![last as i64, last as i64],
        )?;
        set_meta(&conn, "applied_ts", &last.to_string())?;
        conn.execute_batch(&format!("DELETE FROM {STAGE}; COMMIT;"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result.map(|()| n)
}

/// Accept `sql` only if it is exactly one SELECT (DuckDB's own parser: `json_serialize_sql` serializes SELECTs only).
fn check_select(conn: &Connection, sql: &str) -> crate::Result<()> {
    let out: String =
        conn.query_row("SELECT json_serialize_sql(?::VARCHAR)", [sql], |r| r.get(0)).map_err(query_failed)?;
    let parsed: Value = serde_json::from_str(&out).map_err(query_failed)?;
    let statements = parsed["statements"].as_array().map_or(0, Vec::len);
    if parsed["error"] == Value::Bool(true) || statements != 1 {
        let detail =
            parsed["error_message"].as_str().filter(|m| !m.is_empty()).map(|m| format!(" ({m})")).unwrap_or_default();
        return Err(query_failed(format!(
            "Analytics queries run exactly one SELECT statement (WITH ... SELECT included){detail}"
        )));
    }
    Ok(())
}

fn type_name(t: &duckdb::core::LogicalTypeHandle) -> String {
    t.get_alias().unwrap_or_else(|| format!("{:?}", t.id()).to_uppercase())
}

/// Inside the query's transaction: refuse while tables are at different commits (a rebuild was just swapped in); the
/// commit the copy reflects.
fn consistent_as_of(conn: &Connection) -> crate::Result<u64> {
    let (lo, hi): (Option<i64>, Option<i64>) =
        conn.query_row("SELECT min(applied_ts), max(applied_ts) FROM _ferrix_tables", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    if lo != hi {
        return Err(unavailable("a table is being rebuilt"));
    }
    Ok(meta(conn, "applied_ts")?.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0))
}

/// Run `f` in a read transaction that is rolled back; an interrupted statement is a timeout.
fn in_read_transaction<T>(conn: &Connection, sql: &str, f: impl FnOnce(u64) -> crate::Result<T>) -> crate::Result<T> {
    check_select(conn, sql)?;
    conn.execute_batch("BEGIN TRANSACTION")?;
    let result = consistent_as_of(conn).and_then(f);
    let _ = conn.execute_batch("ROLLBACK");
    result.map_err(|e| {
        let interrupted = e.chain().any(|c| c.to_string().contains("Interrupt"));
        if interrupted {
            UserError::new("olap_timeout", "The analytics query was interrupted (it ran too long)").into()
        } else {
            e
        }
    })
}

fn run_export(
    conn: &Connection,
    sql: &str,
    params: Vec<OlapValue>,
    format: OlapExportFormat,
    path: &Path,
    max_rows: u64,
) -> crate::Result<OlapExport> {
    use std::io::Write;
    let too_many = || query_failed(format!("The export has more than {max_rows} rows: add a LIMIT"));
    in_read_transaction(conn, sql, |as_of| {
        let mut stmt = conn.prepare(sql).map_err(query_failed)?;
        let params: Vec<duckdb::types::Value> = params.into_iter().map(super::values::param).collect();
        let file = std::io::BufWriter::new(std::fs::File::create(path)?);
        match format {
            OlapExportFormat::Csv => {
                let mut out = file;
                let mut rows = stmt.query(duckdb::params_from_iter(params)).map_err(query_failed)?;
                let st = rows.as_ref().context("a statement")?;
                let n = st.column_count();
                let names = st.column_names();
                let types: Vec<String> = (0..n).map(|i| type_name(&st.column_logical_type(i))).collect();
                crate::ops::export::write_csv_record(&mut out, names.iter().cloned())?;
                let mut count = 0u64;
                while let Some(row) = rows.next().map_err(query_failed)? {
                    count += 1;
                    if count > max_rows {
                        return Err(too_many());
                    }
                    let mut fields = Vec::with_capacity(n);
                    for (i, t) in types.iter().enumerate() {
                        let v: duckdb::types::Value = row.get(i).map_err(query_failed)?;
                        fields.push(crate::ops::export::csv_value(Some(&super::values::to_json(v, t == "JSON"))));
                    }
                    crate::ops::export::write_csv_record(&mut out, fields)?;
                }
                out.flush()?;
                let columns =
                    names.into_iter().zip(types).map(|(name, type_name)| OlapColumn { name, type_name }).collect();
                Ok(OlapExport { rows: count, columns, as_of_ts: as_of })
            }
            OlapExportFormat::Parquet => {
                let batches = stmt.stream_arrow(duckdb::params_from_iter(params)).map_err(query_failed)?;
                let schema = batches.get_schema();
                let columns = schema
                    .fields()
                    .iter()
                    .map(|f| OlapColumn { name: f.name().clone(), type_name: f.data_type().to_string() })
                    .collect();
                let props = parquet::file::properties::WriterProperties::builder()
                    .set_compression(parquet::basic::Compression::ZSTD(Default::default()))
                    .build();
                let mut writer =
                    parquet::arrow::ArrowWriter::try_new(file, schema, Some(props)).map_err(anyhow::Error::from)?;
                // The iterator panics when DuckDB fails to fetch a chunk (an interrupt, for one).
                let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> crate::Result<u64> {
                    let mut count = 0u64;
                    for batch in batches {
                        count += batch.num_rows() as u64;
                        if count > max_rows {
                            return Err(too_many());
                        }
                        writer.write(&batch).map_err(anyhow::Error::from)?;
                    }
                    Ok(count)
                }));
                let count = match written {
                    Ok(r) => r?,
                    Err(panic) => {
                        let msg = panic.downcast_ref::<String>().cloned().unwrap_or_default();
                        return Err(anyhow::anyhow!("fetching the result failed: {msg}"));
                    }
                };
                writer.close().map_err(anyhow::Error::from)?;
                Ok(OlapExport { rows: count, columns, as_of_ts: as_of })
            }
        }
    })
}

fn run_query(conn: &Connection, sql: &str, params: Vec<OlapValue>, max_rows: usize) -> crate::Result<OlapResult> {
    in_read_transaction(conn, sql, |as_of| {
        let mut stmt = conn.prepare(sql).map_err(query_failed)?;
        let params: Vec<duckdb::types::Value> = params.into_iter().map(super::values::param).collect();
        let mut rows = stmt.query(duckdb::params_from_iter(params)).map_err(query_failed)?;
        let st = rows.as_ref().context("a statement")?;
        let n = st.column_count();
        let names = st.column_names();
        let types: Vec<String> = (0..n).map(|i| type_name(&st.column_logical_type(i))).collect();
        let json_typed: Vec<bool> = types.iter().map(|t| t == "JSON").collect();
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(query_failed)? {
            if out.len() >= max_rows {
                return Err(query_failed(format!("The query returned more than {max_rows} rows: add a LIMIT")));
            }
            let mut obj = Map::with_capacity(n);
            for i in 0..n {
                let v: duckdb::types::Value = row.get(i).map_err(query_failed)?;
                obj.insert(names[i].clone(), super::values::to_json(v, json_typed[i]));
            }
            out.push(obj);
        }
        let columns = names.into_iter().zip(types).map(|(name, type_name)| OlapColumn { name, type_name }).collect();
        Ok(OlapResult { columns, rows: out, as_of_ts: as_of })
    })
}

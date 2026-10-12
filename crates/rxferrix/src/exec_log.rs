//! The execution log: one row per function execution, in `<dir>/aux.db` (table `_logs`).
//!
//! A separate SQLite file keeps logging off the writer lock of `data.db`. Executions are recorded with
//! [`Logs::record`], which never blocks: entries go through a bounded channel to a background thread that inserts
//! them in batches (one transaction per batch). When the channel is full, entries are dropped and counted
//! ([`Logs::dropped`]). After each batch is written, its entries (now with ids) are broadcast to live-tail
//! subscribers ([`Logs::subscribe`]).
//!
//! Retention ([`LogRetention`], from [`AppOptions`](crate::AppOptions)): rows older than `max_age`, and the oldest
//! rows beyond `max_rows`, are deleted at startup and then every `trim_interval`.
//!
//! The dashboard reads it through [`App::logs`](crate::App::logs): [`Logs::query`] (filtered, newest first,
//! paginated by id), [`Logs::subscribe`] (live tail), [`Logs::stats`] (per-function calls, errors, p50/p95) and
//! [`Logs::timeseries`] (the same aggregates per time bucket, for the dashboard's charts).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use crate::error::{ClientError, ErrorKind};
use crate::private::FunctionKind;

/// Bytes of JSON args kept per entry.
pub const MAX_ARGS_BYTES: usize = 2048;
/// Capacity of the channel between executions and the writer thread.
const CHANNEL_CAPACITY: usize = 8192;
/// Entries inserted per transaction at most.
const BATCH: usize = 512;
/// Capacity of the live-tail broadcast; slow subscribers see `RecvError::Lagged`.
const BROADCAST_CAPACITY: usize = 1024;
/// Default and maximum `LogFilter::limit`.
pub const DEFAULT_QUERY_LIMIT: usize = 100;
pub const MAX_QUERY_LIMIT: usize = 1000;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS _logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,      -- ms since the unix epoch, when the execution started
    path TEXT NOT NULL,
    kind TEXT NOT NULL,              -- query | mutation | action
    identity TEXT,                   -- summary of the caller's identity; NULL when anonymous
    duration_us INTEGER NOT NULL,
    outcome TEXT NOT NULL,           -- success | userError | serverError
    error TEXT,                      -- the full error message (never redacted here)
    log_lines TEXT NOT NULL,         -- JSON array of strings
    args TEXT NOT NULL,              -- JSON, truncated to MAX_ARGS_BYTES
    args_truncated INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS _logs_by_timestamp ON _logs (timestamp);
CREATE INDEX IF NOT EXISTS _logs_by_path ON _logs (path, id);
";

/// How long execution log rows are kept. Part of [`AppOptions`](crate::AppOptions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRetention {
    /// Rows older than this are deleted. Default: 7 days.
    pub max_age: Duration,
    /// At most this many rows are kept (the oldest are deleted first). Default: 200 000.
    pub max_rows: u64,
    /// How often trimming runs (it also runs at startup). Default: 1 minute.
    pub trim_interval: Duration,
}

impl Default for LogRetention {
    fn default() -> Self {
        LogRetention {
            max_age: Duration::from_secs(7 * 24 * 3600),
            max_rows: 200_000,
            trim_interval: Duration::from_secs(60),
        }
    }
}

/// How an execution ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogOutcome {
    Success,
    /// A [`UserError`](crate::UserError), invalid args, or an unknown function: the caller's fault.
    UserError,
    /// Anything else (a bug, a panic, a timeout, a failed commit).
    ServerError,
}

impl LogOutcome {
    /// The serialized name: `"success"`, `"userError"` or `"serverError"`.
    pub fn as_str(self) -> &'static str {
        match self {
            LogOutcome::Success => "success",
            LogOutcome::UserError => "userError",
            LogOutcome::ServerError => "serverError",
        }
    }

    fn parse(s: &str) -> LogOutcome {
        match s {
            "success" => LogOutcome::Success,
            "userError" => LogOutcome::UserError,
            _ => LogOutcome::ServerError,
        }
    }

    /// Classify an execution result.
    pub fn of<T>(result: &crate::Result<T>) -> LogOutcome {
        match result {
            Ok(_) => LogOutcome::Success,
            Err(e) => match ClientError::from_anyhow(e, true).kind {
                ErrorKind::Server => LogOutcome::ServerError,
                ErrorKind::User | ErrorKind::InvalidArgs | ErrorKind::NotFound => LogOutcome::UserError,
            },
        }
    }
}

fn parse_kind(s: &str) -> FunctionKind {
    match s {
        "mutation" => FunctionKind::Mutation,
        "action" => FunctionKind::Action,
        _ => FunctionKind::Query,
    }
}

/// One function execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    /// Increasing row id (0 until the entry is written).
    pub id: i64,
    /// When the execution started, ms since the unix epoch.
    pub timestamp: i64,
    pub path: String,
    #[serde(with = "kind_serde")]
    pub kind: FunctionKind,
    /// A summary of the caller's identity (`None` when anonymous). Opaque: for display and filtering only.
    pub identity: Option<String>,
    pub duration_us: u64,
    pub outcome: LogOutcome,
    /// The full error message (with context) when the execution failed. Never redacted: this is server-side data.
    pub error: Option<String>,
    /// Captured log lines (see [`crate::log`]), including those of nested `ctx.run_*` calls.
    pub log_lines: Vec<String>,
    /// The args as JSON, cut to at most [`MAX_ARGS_BYTES`] bytes (then `args_truncated` is set, and the string may
    /// not be valid JSON).
    pub args: String,
    pub args_truncated: bool,
}

mod kind_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::private::FunctionKind;

    pub fn serialize<S: Serializer>(k: &FunctionKind, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(k.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<FunctionKind, D::Error> {
        Ok(super::parse_kind(&String::deserialize(d)?))
    }
}

/// Serialize `args` and cut the result to [`MAX_ARGS_BYTES`] at a char boundary. Returns (json, truncated).
pub fn truncate_args(args: &Value) -> (String, bool) {
    let mut s = serde_json::to_string(args).unwrap_or_default();
    if s.len() <= MAX_ARGS_BYTES {
        return (s, false);
    }
    let mut cut = MAX_ARGS_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    (s, true)
}

/// Filter for [`Logs::query`]. Every field is optional; `Default` returns the newest [`DEFAULT_QUERY_LIMIT`] rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogFilter {
    /// Only paths starting with this (e.g. `"todos:"` or `"todos:list"`).
    pub path_prefix: Option<String>,
    /// Only this function (`"todos:add"`, not `"todos:add_later"`).
    pub path: Option<String>,
    #[serde(with = "opt_kind_serde")]
    pub kind: Option<FunctionKind>,
    pub outcome: Option<LogOutcome>,
    /// Only executions that started at or after this time (ms since the epoch).
    pub since: Option<i64>,
    /// At most this many rows (default [`DEFAULT_QUERY_LIMIT`], capped at [`MAX_QUERY_LIMIT`]).
    pub limit: Option<usize>,
    /// Only rows with `id < before_id`: pass the last id of the previous page to page backwards.
    pub before_id: Option<i64>,
    /// Only executions by this caller: an [`LogEntry::identity`] value (`"user:<id>"`, `"admin:<id>"`), or
    /// `"anonymous"` for anonymous callers.
    pub identity: Option<String>,
    /// Leave out rxferrix's own functions (paths under `_system/`, e.g. the dashboard's queries).
    pub exclude_system: bool,
}

impl LogFilter {
    /// Whether `entry` passes every condition except `limit` and `before_id` (used to filter the live tail the same
    /// way [`Logs::query`] filters history).
    pub fn matches(&self, entry: &LogEntry) -> bool {
        self.path_prefix.as_deref().is_none_or(|p| entry.path.starts_with(p))
            && self.path.as_deref().is_none_or(|p| entry.path == p)
            && self.kind.is_none_or(|k| entry.kind == k)
            && self.outcome.is_none_or(|o| entry.outcome == o)
            && self.since.is_none_or(|t| entry.timestamp >= t)
            && self.identity.as_deref().is_none_or(|i| entry.identity.as_deref().unwrap_or(ANONYMOUS) == i)
            && !(self.exclude_system && crate::private::is_system_path(&entry.path))
    }
}

/// The [`LogFilter::identity`] value matching anonymous callers.
pub const ANONYMOUS: &str = "anonymous";

mod opt_kind_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::private::FunctionKind;

    pub fn serialize<S: Serializer>(k: &Option<FunctionKind>, s: S) -> Result<S::Ok, S::Error> {
        match k {
            Some(k) => s.serialize_some(k.as_str()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<FunctionKind>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.map(|s| super::parse_kind(&s)))
    }
}

/// Per-function aggregates over a time window ([`Logs::stats`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionStats {
    pub path: String,
    #[serde(with = "kind_serde")]
    pub kind: FunctionKind,
    pub calls: u64,
    /// `user_errors + server_errors`.
    pub errors: u64,
    pub user_errors: u64,
    pub server_errors: u64,
    /// Median duration (nearest rank).
    pub p50_us: u64,
    /// 95th percentile duration (nearest rank).
    pub p95_us: u64,
}

/// Most buckets one [`Logs::timeseries`] call may produce.
pub const MAX_TIMESERIES_BUCKETS: i64 = 1500;

/// What [`Logs::timeseries`] aggregates: executions of `path` (or of every non-system function when `None`) that
/// started in `[since, until)`, in buckets of `bucket_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeseriesSpec {
    pub path: Option<String>,
    /// ms since the epoch, inclusive. Rounded down to a multiple of `bucket_ms`, so buckets are aligned.
    pub since: i64,
    /// ms since the epoch, exclusive.
    pub until: i64,
    pub bucket_ms: i64,
}

/// One time bucket of [`Logs::timeseries`]: the executions that started in `[t, t + bucket)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimePoint {
    /// The bucket's start, ms since the epoch.
    pub t: i64,
    pub calls: u64,
    /// `user_errors + server_errors`.
    pub errors: u64,
    pub user_errors: u64,
    pub server_errors: u64,
    /// Median duration (nearest rank).
    pub p50_us: u64,
    /// 95th percentile duration (nearest rank).
    pub p95_us: u64,
    pub max_us: u64,
}

enum Msg {
    Entry(Box<LogEntry>),
    /// Reply once everything sent before this message is written.
    Flush(oneshot::Sender<()>),
    /// Trim now, then reply.
    Trim(oneshot::Sender<()>),
}

struct Shared {
    path: PathBuf,
    live: broadcast::Sender<LogEntry>,
    dropped: AtomicU64,
    /// Lazily opened read-only connection for queries.
    reader: Mutex<Option<Connection>>,
}

/// Handle to the execution log. Cheap to clone. The writer thread stops when every handle is dropped.
#[derive(Clone)]
pub struct Logs {
    tx: SyncSender<Msg>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Logs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logs").field("path", &self.shared.path).finish()
    }
}

impl Logs {
    /// Open (creating if needed) the log database at `path` and start the writer thread.
    pub fn open(path: &Path, retention: LogRetention) -> anyhow::Result<Logs> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        anyhow::ensure!(mode.eq_ignore_ascii_case("wal"), "could not enable WAL mode on aux.db (got {mode})");
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // A trim deletes many rows at once, growing the WAL well past the automatic checkpoint's 4 MB; without a limit
        // the file stays that size after the WAL restarts (112 MB after an hour of heavy traffic).
        conn.pragma_update(None, "journal_size_limit", 16i64 << 20)?;
        conn.execute_batch(DDL)?;
        trim(&conn, &retention)?;

        let (live, _) = broadcast::channel(BROADCAST_CAPACITY);
        let shared =
            Arc::new(Shared { path: path.to_path_buf(), live, dropped: AtomicU64::new(0), reader: Mutex::new(None) });
        let (tx, rx) = std::sync::mpsc::sync_channel(CHANNEL_CAPACITY);
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("rxferrix-exec-log".into())
            .spawn(move || writer_loop(conn, rx, thread_shared, retention))
            .context("spawning the execution log writer")?;
        Ok(Logs { tx, shared })
    }

    /// Record an execution. Never blocks: if the writer is behind, the entry is dropped and counted.
    pub fn record(&self, entry: LogEntry) {
        match self.tx.try_send(Msg::Entry(Box::new(entry))) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Entries dropped because the writer could not keep up, since startup.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Live tail: every entry, after it has been written (so it carries its id).
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.shared.live.subscribe()
    }

    /// Wait until every entry recorded before this call is written.
    pub async fn flush(&self) -> anyhow::Result<()> {
        self.round_trip(Msg::Flush).await
    }

    /// Apply the retention limits now (normally periodic), after writing what is queued.
    pub async fn trim_now(&self) -> anyhow::Result<()> {
        self.round_trip(Msg::Trim).await
    }

    async fn round_trip(&self, msg: fn(oneshot::Sender<()>) -> Msg) -> anyhow::Result<()> {
        let (done_tx, done_rx) = oneshot::channel();
        let tx = self.tx.clone();
        // A blocking send: unlike entries, control messages must not be dropped.
        tokio::task::spawn_blocking(move || tx.send(msg(done_tx)))
            .await?
            .map_err(|_| anyhow::anyhow!("execution log writer stopped"))?;
        done_rx.await.map_err(|_| anyhow::anyhow!("execution log writer stopped"))
    }

    async fn with_reader<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> anyhow::Result<R> + Send + 'static,
    ) -> anyhow::Result<R> {
        let shared = self.shared.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.reader.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                let conn = Connection::open_with_flags(
                    &shared.path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                conn.pragma_update(None, "busy_timeout", 5000)?;
                *guard = Some(conn);
            }
            f(guard.as_ref().expect("opened above"))
        })
        .await?
    }

    /// Entries matching `filter`, newest first.
    pub async fn query(&self, filter: LogFilter) -> anyhow::Result<Vec<LogEntry>> {
        self.with_reader(move |conn| query(conn, &filter)).await
    }

    /// Per-function calls, errors and p50/p95 duration for executions that started at or after `since`
    /// (ms since the epoch), sorted by path.
    pub async fn stats(&self, since: i64) -> anyhow::Result<Vec<FunctionStats>> {
        self.with_reader(move |conn| stats(conn, since)).await
    }

    /// Calls, errors and p50/p95/max duration per time bucket (oldest first; only buckets with executions), for the
    /// executions [`TimeseriesSpec`] selects. Fails if the spec is empty or inverted, or asks for more than
    /// [`MAX_TIMESERIES_BUCKETS`] buckets.
    pub async fn timeseries(&self, spec: TimeseriesSpec) -> anyhow::Result<Vec<TimePoint>> {
        anyhow::ensure!(spec.bucket_ms > 0, "the bucket must be positive");
        anyhow::ensure!(spec.until > spec.since, "`until` must be after `since`");
        let buckets = bucket_count(spec.since, spec.until, spec.bucket_ms);
        anyhow::ensure!(
            buckets <= MAX_TIMESERIES_BUCKETS as i128,
            "{buckets} buckets requested; at most {MAX_TIMESERIES_BUCKETS} (use a larger bucket or a shorter range)"
        );
        let since = spec.since.max(0).div_euclid(spec.bucket_ms) * spec.bucket_ms;
        self.with_reader(move |conn| timeseries(conn, &TimeseriesSpec { since, ..spec })).await
    }
}

fn writer_loop(mut conn: Connection, rx: Receiver<Msg>, shared: Arc<Shared>, retention: LogRetention) {
    let mut next_trim = Instant::now() + retention.trim_interval;
    let mut batch: Vec<LogEntry> = Vec::new();
    let mut replies: Vec<(oneshot::Sender<()>, bool)> = Vec::new();
    loop {
        let timeout = next_trim.saturating_duration_since(Instant::now());
        let first = match rx.recv_timeout(timeout) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut want_trim = first.is_none();
        let mut next = first;
        while let Some(msg) = next {
            match msg {
                Msg::Entry(e) => batch.push(*e),
                Msg::Flush(reply) => replies.push((reply, false)),
                Msg::Trim(reply) => replies.push((reply, true)),
            }
            if batch.len() >= BATCH {
                break;
            }
            next = rx.try_recv().ok();
        }
        if !batch.is_empty() {
            match insert_batch(&mut conn, &mut batch) {
                Ok(()) => {
                    for e in batch.drain(..) {
                        let _ = shared.live.send(e);
                    }
                }
                Err(e) => {
                    tracing::warn!("execution log: dropping {} entries: {e:#}", batch.len());
                    shared.dropped.fetch_add(batch.len() as u64, Ordering::Relaxed);
                    batch.clear();
                }
            }
        }
        want_trim |= replies.iter().any(|(_, trim)| *trim) || Instant::now() >= next_trim;
        if want_trim {
            if let Err(e) = trim(&conn, &retention) {
                tracing::warn!("execution log: trimming failed: {e:#}");
            }
            next_trim = Instant::now() + retention.trim_interval;
        }
        for (reply, _) in replies.drain(..) {
            let _ = reply.send(());
        }
    }
}

fn insert_batch(conn: &mut Connection, batch: &mut [LogEntry]) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO _logs (timestamp, path, kind, identity, duration_us, outcome, error, log_lines, args, args_truncated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for e in batch.iter_mut() {
            stmt.execute(params![
                e.timestamp,
                e.path,
                e.kind.as_str(),
                e.identity,
                e.duration_us as i64,
                e.outcome.as_str(),
                e.error,
                serde_json::to_string(&e.log_lines)?,
                e.args,
                e.args_truncated,
            ])?;
            e.id = tx.last_insert_rowid();
        }
    }
    tx.commit()?;
    Ok(())
}

fn trim(conn: &Connection, retention: &LogRetention) -> anyhow::Result<()> {
    let cutoff = crate::db::wall_clock_ms() as i64 - retention.max_age.as_millis() as i64;
    let by_age = conn.execute("DELETE FROM _logs WHERE timestamp < ?1", [cutoff])?;
    // Ids increase with insertion, so "the newest max_rows" is "id > the max_rows-th largest id".
    let by_count = conn.execute(
        "DELETE FROM _logs WHERE id <= (SELECT id FROM _logs ORDER BY id DESC LIMIT 1 OFFSET ?1)",
        [retention.max_rows as i64],
    )?;
    if by_age + by_count > 0 {
        tracing::debug!(by_age, by_count, "execution log trimmed");
    }
    Ok(())
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<LogEntry> {
    let kind: String = r.get(3)?;
    let outcome: String = r.get(6)?;
    let lines: String = r.get(8)?;
    Ok(LogEntry {
        id: r.get(0)?,
        timestamp: r.get(1)?,
        path: r.get(2)?,
        kind: parse_kind(&kind),
        identity: r.get(4)?,
        duration_us: r.get::<_, i64>(5)? as u64,
        outcome: LogOutcome::parse(&outcome),
        error: r.get(7)?,
        log_lines: serde_json::from_str(&lines).unwrap_or_default(),
        args: r.get(9)?,
        args_truncated: r.get(10)?,
    })
}

fn query(conn: &Connection, f: &LogFilter) -> anyhow::Result<Vec<LogEntry>> {
    let mut sql = String::from(
        "SELECT id, timestamp, path, kind, identity, duration_us, outcome, error, log_lines, args, args_truncated
         FROM _logs WHERE 1 = 1",
    );
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    // Appends `clause` with every `?` replaced by the number of the parameter `v`.
    let mut arg = |sql: &mut String, clause: &str, v: rusqlite::types::Value| {
        args.push(v);
        sql.push_str(&clause.replace('?', &format!("?{}", args.len())));
    };
    if let Some(prefix) = &f.path_prefix {
        arg(&mut sql, " AND substr(path, 1, length(?)) = ?", prefix.clone().into());
    }
    if let Some(path) = &f.path {
        arg(&mut sql, " AND path = ?", path.clone().into());
    }
    if let Some(kind) = f.kind {
        arg(&mut sql, " AND kind = ?", kind.as_str().to_string().into());
    }
    if let Some(outcome) = f.outcome {
        arg(&mut sql, " AND outcome = ?", outcome.as_str().to_string().into());
    }
    if let Some(since) = f.since {
        arg(&mut sql, " AND timestamp >= ?", since.into());
    }
    if let Some(before) = f.before_id {
        arg(&mut sql, " AND id < ?", before.into());
    }
    match f.identity.as_deref() {
        None => {}
        Some(ANONYMOUS) => sql.push_str(" AND identity IS NULL"),
        Some(identity) => arg(&mut sql, " AND identity = ?", identity.to_string().into()),
    }
    if f.exclude_system {
        // `is_system_path`: the first path segment is exactly `_system`.
        sql.push_str(" AND NOT (path = '_system' OR substr(path, 1, 8) IN ('_system/', '_system:'))");
    }
    let limit = f.limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT);
    arg(&mut sql, " ORDER BY id DESC LIMIT ?", (limit as i64).into());
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), row_to_entry)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn stats(conn: &Connection, since: i64) -> anyhow::Result<Vec<FunctionStats>> {
    let mut stmt =
        conn.prepare("SELECT path, kind, outcome, duration_us FROM _logs WHERE timestamp >= ?1 ORDER BY path")?;
    let mut rows = stmt.query([since])?;
    let mut out: Vec<(FunctionStats, Vec<u64>)> = Vec::new();
    while let Some(r) = rows.next()? {
        let path: String = r.get(0)?;
        let kind: String = r.get(1)?;
        let outcome = LogOutcome::parse(&r.get::<_, String>(2)?);
        let duration = r.get::<_, i64>(3)? as u64;
        if out.last().is_none_or(|(s, _)| s.path != path) {
            let s = FunctionStats {
                path,
                kind: parse_kind(&kind),
                calls: 0,
                errors: 0,
                user_errors: 0,
                server_errors: 0,
                p50_us: 0,
                p95_us: 0,
            };
            out.push((s, Vec::new()));
        }
        let (s, durations) = out.last_mut().expect("pushed above");
        s.calls += 1;
        match outcome {
            LogOutcome::Success => {}
            LogOutcome::UserError => s.user_errors += 1,
            LogOutcome::ServerError => s.server_errors += 1,
        }
        durations.push(duration);
    }
    Ok(out
        .into_iter()
        .map(|(mut s, mut d)| {
            d.sort_unstable();
            s.errors = s.user_errors + s.server_errors;
            s.p50_us = percentile(&d, 0.50);
            s.p95_us = percentile(&d, 0.95);
            s
        })
        .collect())
}

/// How many buckets of `bucket_ms` (> 0) cover `[since, until)` once `since` is rounded down to a bucket boundary
/// (in `i128`: no overflow for any inputs).
pub fn bucket_count(since: i64, until: i64, bucket_ms: i64) -> i128 {
    let (since, until, bucket) = (since as i128, until as i128, bucket_ms.max(1) as i128);
    let start = since.div_euclid(bucket) * bucket;
    (until - start + bucket - 1).div_euclid(bucket)
}

fn timeseries(conn: &Connection, spec: &TimeseriesSpec) -> anyhow::Result<Vec<TimePoint>> {
    let mut sql =
        String::from("SELECT timestamp, outcome, duration_us FROM _logs WHERE timestamp >= ?1 AND timestamp < ?2");
    let mut args: Vec<rusqlite::types::Value> = vec![spec.since.into(), spec.until.into()];
    match &spec.path {
        Some(path) => {
            sql.push_str(" AND path = ?3");
            args.push(path.clone().into());
        }
        None => sql.push_str(" AND NOT (path = '_system' OR substr(path, 1, 8) IN ('_system/', '_system:'))"),
    }
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let mut buckets: std::collections::BTreeMap<i64, (TimePoint, Vec<u64>)> = std::collections::BTreeMap::new();
    while let Some(r) = rows.next()? {
        let timestamp: i64 = r.get(0)?;
        let outcome = LogOutcome::parse(&r.get::<_, String>(1)?);
        let duration = r.get::<_, i64>(2)?.max(0) as u64;
        let t = spec.since + (timestamp - spec.since) / spec.bucket_ms * spec.bucket_ms;
        let (point, durations) = buckets.entry(t).or_insert_with(|| {
            (
                TimePoint { t, calls: 0, errors: 0, user_errors: 0, server_errors: 0, p50_us: 0, p95_us: 0, max_us: 0 },
                Vec::new(),
            )
        });
        point.calls += 1;
        match outcome {
            LogOutcome::Success => {}
            LogOutcome::UserError => point.user_errors += 1,
            LogOutcome::ServerError => point.server_errors += 1,
        }
        durations.push(duration);
    }
    Ok(buckets
        .into_values()
        .map(|(mut p, mut d)| {
            d.sort_unstable();
            p.errors = p.user_errors + p.server_errors;
            p.p50_us = percentile(&d, 0.50);
            p.p95_us = percentile(&d, 0.95);
            p.max_us = d.last().copied().unwrap_or(0);
            p
        })
        .collect())
}

/// Nearest-rank percentile of sorted values (0 for none).
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: FunctionKind, outcome: LogOutcome, duration_us: u64, timestamp: i64) -> LogEntry {
        LogEntry {
            id: 0,
            timestamp,
            path: path.into(),
            kind,
            identity: None,
            duration_us,
            outcome,
            error: (outcome != LogOutcome::Success).then(|| "boom".into()),
            log_lines: vec!["[INFO] hi".into()],
            args: "{}".into(),
            args_truncated: false,
        }
    }

    #[test]
    fn args_truncation() {
        let (s, t) = truncate_args(&serde_json::json!({ "a": 1 }));
        assert_eq!((s.as_str(), t), (r#"{"a":1}"#, false));
        let (s, t) = truncate_args(&serde_json::json!({ "a": "é".repeat(MAX_ARGS_BYTES) }));
        assert!(t && s.len() <= MAX_ARGS_BYTES && s.starts_with(r#"{"a":"é"#));
    }

    #[test]
    fn percentiles() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!((percentile(&v, 0.5), percentile(&v, 0.95)), (50, 95));
        assert_eq!((percentile(&[7], 0.5), percentile(&[7], 0.95)), (7, 7));
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[tokio::test]
    async fn write_query_tail_stats_and_trim() {
        let dir = tempfile::tempdir().unwrap();
        let retention = LogRetention { max_rows: 5, ..Default::default() };
        let logs = Logs::open(&dir.path().join("aux.db"), retention).unwrap();
        let mut tail = logs.subscribe();
        let now = crate::db::wall_clock_ms() as i64;
        let old = now - 8 * 24 * 3600 * 1000;
        logs.record(entry("todos:list", FunctionKind::Query, LogOutcome::Success, 10, now));
        logs.record(entry("todos:add", FunctionKind::Mutation, LogOutcome::UserError, 30, now));
        logs.record(entry("todos:add", FunctionKind::Mutation, LogOutcome::Success, 20, now));
        logs.record(entry("other:x", FunctionKind::Action, LogOutcome::ServerError, 5, old));
        logs.flush().await.unwrap();

        let first = tail.recv().await.unwrap();
        assert_eq!((first.id, first.path.as_str()), (1, "todos:list"));

        let all = logs.query(LogFilter::default()).await.unwrap();
        assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), vec![4, 3, 2, 1], "newest first");
        assert_eq!(all[3].log_lines, vec!["[INFO] hi"]);
        let adds = logs.query(LogFilter { path_prefix: Some("todos:a".into()), ..Default::default() }).await.unwrap();
        assert_eq!(adds.len(), 2);
        let exact = LogFilter { path: Some("todos:add".into()), ..Default::default() };
        assert_eq!(logs.query(exact.clone()).await.unwrap().len(), 2);
        assert!(exact.matches(&entry("todos:add", FunctionKind::Mutation, LogOutcome::Success, 1, now)));
        assert!(!exact.matches(&entry("todos:add_later", FunctionKind::Mutation, LogOutcome::Success, 1, now)));
        let none = LogFilter { path: Some("todos:a".into()), ..Default::default() };
        assert!(logs.query(none).await.unwrap().is_empty());
        let errs = logs
            .query(LogFilter {
                kind: Some(FunctionKind::Mutation),
                outcome: Some(LogOutcome::UserError),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(errs.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2]);
        let page = logs.query(LogFilter { before_id: Some(3), limit: Some(1), ..Default::default() }).await.unwrap();
        assert_eq!(page.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2]);
        let recent = logs.query(LogFilter { since: Some(now - 1000), ..Default::default() }).await.unwrap();
        assert_eq!(recent.len(), 3);

        let stats = logs.stats(now - 1000).await.unwrap();
        assert_eq!(stats.len(), 2);
        let add = &stats[0];
        assert_eq!(
            (add.path.as_str(), add.kind, add.calls, add.errors, add.user_errors, add.p50_us, add.p95_us),
            ("todos:add", FunctionKind::Mutation, 2, 1, 1, 20, 30)
        );

        // Retention: the 8-day-old row goes by age, then the oldest rows beyond max_rows = 5.
        logs.trim_now().await.unwrap();
        assert_eq!(logs.query(LogFilter::default()).await.unwrap().len(), 3);
        for i in 0..10 {
            logs.record(entry("todos:list", FunctionKind::Query, LogOutcome::Success, i, now));
        }
        logs.trim_now().await.unwrap();
        let left = logs.query(LogFilter::default()).await.unwrap();
        assert_eq!(left.iter().map(|e| e.id).collect::<Vec<_>>(), vec![14, 13, 12, 11, 10]);
        assert_eq!(logs.dropped(), 0);

        // The serialized form the dashboard sees.
        let json = serde_json::to_value(&left[0]).unwrap();
        assert_eq!(json["kind"], "query");
        assert_eq!(json["outcome"], "success");
        assert!(json.get("durationUs").is_some() && json.get("logLines").is_some());
        let filter: LogFilter = serde_json::from_value(serde_json::json!({ "kind": "mutation", "limit": 5 })).unwrap();
        assert_eq!((filter.kind, filter.limit), (Some(FunctionKind::Mutation), Some(5)));
    }

    #[tokio::test]
    async fn timeseries_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let logs = Logs::open(&dir.path().join("aux.db"), LogRetention::default()).unwrap();
        // Recent, aligned to 5 minutes so the buckets are predictable.
        let base = (crate::db::wall_clock_ms() as i64 / 300_000 - 3) * 300_000;
        let add =
            |d: u64, at: i64, outcome: LogOutcome| entry("todos:add", FunctionKind::Mutation, outcome, d, base + at);
        // Bucket 0: 1..=20 µs (one user error); bucket 2: 100 and 300 µs (a server error); nothing in bucket 1.
        for d in 1..=20 {
            logs.record(add(d, 1000 + d as i64, if d == 7 { LogOutcome::UserError } else { LogOutcome::Success }));
        }
        logs.record(add(100, 2 * 60_000, LogOutcome::Success));
        logs.record(add(300, 3 * 60_000 - 1, LogOutcome::ServerError));
        // Other functions and out-of-range rows don't count.
        logs.record(entry("todos:list", FunctionKind::Query, LogOutcome::Success, 5, base + 10));
        logs.record(entry("_system/dashboard:tables", FunctionKind::Query, LogOutcome::Success, 5, base + 10));
        logs.record(add(9, -1, LogOutcome::Success));
        logs.record(add(9, 5 * 60_000, LogOutcome::Success));
        logs.flush().await.unwrap();

        let spec =
            TimeseriesSpec { path: Some("todos:add".into()), since: base, until: base + 5 * 60_000, bucket_ms: 60_000 };
        let points = logs.timeseries(spec.clone()).await.unwrap();
        assert_eq!(points.len(), 2, "{points:?}");
        assert_eq!(
            points[0],
            TimePoint {
                t: base,
                calls: 20,
                errors: 1,
                user_errors: 1,
                server_errors: 0,
                p50_us: 10,
                p95_us: 19,
                max_us: 20
            }
        );
        assert_eq!(
            points[1],
            TimePoint {
                t: base + 2 * 60_000,
                calls: 2,
                errors: 1,
                user_errors: 0,
                server_errors: 1,
                p50_us: 100,
                p95_us: 300,
                max_us: 300
            }
        );
        // `since` is rounded down to the bucket: the same buckets from mid-minute.
        let shifted = logs.timeseries(TimeseriesSpec { since: base + 30_000, ..spec.clone() }).await.unwrap();
        assert_eq!(shifted, points);
        // Every app function (system ones excluded) when no path is given.
        let all = logs.timeseries(TimeseriesSpec { path: None, ..spec.clone() }).await.unwrap();
        assert_eq!(all[0].calls, 21);
        // One wide bucket.
        let one = logs.timeseries(TimeseriesSpec { bucket_ms: 5 * 60_000, ..spec.clone() }).await.unwrap();
        assert_eq!((one.len(), one[0].calls, one[0].errors), (1, 22, 2));
        // Bad specs.
        assert!(logs.timeseries(TimeseriesSpec { bucket_ms: 0, ..spec.clone() }).await.is_err());
        assert!(logs.timeseries(TimeseriesSpec { until: base, ..spec.clone() }).await.is_err());
        assert!(logs.timeseries(TimeseriesSpec { bucket_ms: 1, ..spec.clone() }).await.is_err(), "too many buckets");
    }
}

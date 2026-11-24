//! Mutation idempotency: `(session_id, request_id) → response`. The stored `session_id` is scoped to the caller's
//! identity ([`IdempotencyKey::scoped_to`]), so one identity can never be handed another identity's recorded
//! response.
//!
//! The TS client resends unacknowledged mutations after a reconnect with the same `(sessionId, requestId)`. The
//! runtime looks the key up *inside* the mutation's write transaction (so a concurrent duplicate waits for the writer
//! lock and then finds the first one's response), and the response is recorded in the same SQLite transaction as the
//! mutation's writes. A mutation is therefore applied at most once per key, and its recorded response is exactly what
//! was committed. A failed mutation is rolled back; its error is then recorded while the writer lock is still held,
//! so a retry gets the same error instead of a second attempt.
//!
//! ## Storage
//!
//! The responses of the mutations that commit together (a group-commit batch, `crate::db`) are written as **one
//! row** of the raw table `_mutation_responses` of `data.db`, right before the batch's `COMMIT`: the rows are
//! appended in commit order and packed ([`encode`], about 15 bytes for a success with a small value), so recording
//! costs a batch a few consecutive pages. (One row per response under a `(session_id, request_id)` primary key, the
//! layout before, wrote a B-tree page per session with a mutation in the batch; measured with `bench_transfer`,
//! 2026-11-24, that was 27-45% of the throughput over WebSockets, and half of it with a thousand sessions. Rows of
//! unpacked JSON, 100 bytes per response, were tried first: in one run throughput fell by a sixth when they began to
//! expire; with packed rows, a run in which they expire and one in which they don't are alike.)
//!
//! Lookups go through an [`Index`] in memory, kept with the writer connection (so under the writer lock): per session,
//! which row holds each request's response. It is loaded from the table when the database opens and covers the
//! responses of the batch still open too: a duplicate that joins the same batch as the original finds it. It costs
//! 20 to 40 bytes per response of the last [`RETENTION`]: measured with `index_cost` (this module's tests), the 6
//! million responses of ten minutes at 10,000 mutations a second took 126 MiB (a thousand sessions) to 226 MiB
//! (600,000 sessions) and 0.5 to 0.9 s to load.
//!
//! Rows older than [`RETENTION`] are deleted when a batch commits (a bounded number per commit, [`EXPIRE_ROWS`]).

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ClientError, ErrorKind, FunctionNotFound, InvalidArgs};
use crate::{Error, UserError};

/// How long responses are kept.
pub const RETENTION: Duration = Duration::from_secs(10 * 60);

/// Identifies one mutation request of one client session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey {
    pub session_id: String,
    pub request_id: u64,
}

impl IdempotencyKey {
    /// The key as stored: the caller's principal (`user:<id>`, `admin:<id>` or `anonymous`) is prefixed to the
    /// session id, so a recorded response is only ever replayed to the identity that ran the mutation. The runtime
    /// applies this itself; callers pass the plain key.
    pub fn scoped_to(&self, identity: &crate::auth::Identity) -> IdempotencyKey {
        IdempotencyKey {
            session_id: format!("{}|{}", identity.principal_key(), self.session_id),
            request_id: self.request_id,
        }
    }
}

/// What is stored per key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum StoredResponse {
    Success { value: Value, ts: u64 },
    Error { kind: StoredErrorKind, message: String, code: Option<String>, data: Value },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StoredErrorKind {
    User,
    InvalidArgs,
    NotFound,
    Server,
}

impl StoredResponse {
    pub fn from_error(e: &Error) -> StoredResponse {
        // Classify with the real message (dev = true): redaction happens later, per client, as for a fresh error.
        let c = ClientError::from_anyhow(e, true);
        let kind = match c.kind {
            ErrorKind::User => StoredErrorKind::User,
            ErrorKind::InvalidArgs => StoredErrorKind::InvalidArgs,
            ErrorKind::NotFound => StoredErrorKind::NotFound,
            ErrorKind::Server => StoredErrorKind::Server,
        };
        let (code, data) = match &c.data {
            Some(d) => (d["code"].as_str().map(str::to_string), d["data"].clone()),
            None => (None, Value::Null),
        };
        StoredResponse::Error { kind, message: c.message, code, data }
    }

    /// The response as the runtime returns it: the same value and ts, or an error that classifies (and redacts)
    /// exactly like the original.
    pub fn into_result(self) -> crate::Result<(Value, u64)> {
        match self {
            StoredResponse::Success { value, ts } => Ok((value, ts)),
            StoredResponse::Error { kind, message, code, data } => Err(match kind {
                StoredErrorKind::User => UserError::new(code.unwrap_or_default(), message).with_data(data).into(),
                StoredErrorKind::InvalidArgs => InvalidArgs(message).into(),
                StoredErrorKind::NotFound => FunctionNotFound(message).into(),
                StoredErrorKind::Server => anyhow::anyhow!(message),
            }),
        }
    }
}

/// A batch commit deletes at most this many expired rows more than it adds, so the first write after a long quiet
/// time doesn't hold the writer for all of them, and the table still shrinks.
pub const EXPIRE_ROWS: usize = 32;

/// The table used before: one row per response. Its rows move to [`TABLE`] when the database opens ([`Index::open`]).
const LEGACY_TABLE: &str = "_mutation_dedup";
const TABLE: &str = "_mutation_responses";

const DDL: &str = "
CREATE TABLE IF NOT EXISTS _mutation_responses (
    id INTEGER PRIMARY KEY,
    created_ms INTEGER NOT NULL,
    records BLOB NOT NULL
);
";

/// The first byte of a row's `records`: the layout of what follows ([`encode`]).
const FORMAT: u8 = 1;

/// A row takes responses up to about this size (counting each one's session id and JSON; a larger response gets a
/// row to itself), so a batch of large responses doesn't make one value too large for SQLite, or for a lookup to read.
const ROW_BYTES: usize = 1 << 20;

/// How many responses [`import`] puts in a row.
const IMPORT_ROW: usize = 256;

/// One recorded response: held in memory until its batch commits, then part of that batch's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    /// As stored: scoped to the identity ([`IdempotencyKey::scoped_to`]).
    pub session_id: String,
    pub request_id: u64,
    pub response: Response<String>,
}

/// A response as a row holds it (`T` is `String`, or `&str` borrowed from the row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Response<T> {
    /// What nearly all are: the mutation's value as JSON, and its commit ts.
    Success { value: T, ts: u64 },
    /// Anything else (an error): a [`StoredResponse`] as JSON.
    Other(T),
}

impl<T: AsRef<str>> Response<T> {
    /// Equal for equal responses, whichever way they are held: to tell whether two are the same response.
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match self {
            Response::Success { value, ts } => (0u8, ts, value.as_ref()).hash(&mut hasher),
            Response::Other(json) => (1u8, json.as_ref()).hash(&mut hasher),
        }
        hasher.finish()
    }

    fn stored(&self) -> anyhow::Result<StoredResponse> {
        Ok(match self {
            Response::Success { value, ts } => {
                StoredResponse::Success { value: serde_json::from_str(value.as_ref())?, ts: *ts }
            }
            Response::Other(json) => serde_json::from_str(json.as_ref())?,
        })
    }

    /// The bytes it takes in a row, about.
    fn len(&self) -> usize {
        match self {
            Response::Success { value, .. } => value.as_ref().len(),
            Response::Other(json) => json.as_ref().len(),
        }
    }

    fn to_owned(&self) -> Response<String> {
        match self {
            Response::Success { value, ts } => Response::Success { value: value.as_ref().into(), ts: *ts },
            Response::Other(json) => Response::Other(json.as_ref().into()),
        }
    }
}

impl Response<String> {
    /// From a [`StoredResponse`] as JSON, as the table used before held them.
    fn from_stored(json: String) -> Response<String> {
        match serde_json::from_str(&json) {
            Ok(StoredResponse::Success { value, ts }) => Response::Success { value: value.to_string(), ts },
            _ => Response::Other(json),
        }
    }
}

impl Record {
    pub(crate) fn success(key: &IdempotencyKey, value: &Value, ts: u64) -> anyhow::Result<Record> {
        Ok(Record::new(key, Response::Success { value: serde_json::to_string(value)?, ts }))
    }

    pub(crate) fn error(key: &IdempotencyKey, error: &Error) -> anyhow::Result<Record> {
        Ok(Record::new(key, Response::Other(serde_json::to_string(&StoredResponse::from_error(error))?)))
    }

    fn new(key: &IdempotencyKey, response: Response<String>) -> Record {
        Record { session_id: key.session_id.clone(), request_id: key.request_id, response }
    }

    /// The bytes it takes in a row, about ([`ROW_BYTES`]).
    fn len(&self) -> usize {
        self.session_id.len() + self.response.len()
    }
}

/// An unsigned integer in 7-bit groups, the lowest first, each byte but the last with its high bit set.
fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

/// A row's `records`. Sizes and numbers are varints ([`put_varint`]), texts a length and that many bytes of UTF-8:
///
/// * [`FORMAT`];
/// * the session ids of the row's records, each once: how many, then the texts (the mutations of one commit come from
///   few sessions, and a session id is most of what a record would otherwise take);
/// * per record: the position of its session id in that list, the request id, then `0`, the commit ts and the value's
///   JSON for a success, or `1` and a [`StoredResponse`] as JSON for anything else.
///
/// A success with a small value takes about 15 bytes, where the JSON of one with its session id took 100.
fn encode<'a>(records: impl IntoIterator<Item = &'a Record> + Clone) -> Vec<u8> {
    let mut sessions: HashMap<&str, u64> = HashMap::new();
    let mut out = vec![FORMAT];
    let mut names = Vec::new();
    for record in records.clone() {
        let next = sessions.len() as u64;
        if let std::collections::hash_map::Entry::Vacant(new) = sessions.entry(&record.session_id) {
            new.insert(next);
            put_text(&mut names, &record.session_id);
        }
    }
    put_varint(&mut out, sessions.len() as u64);
    out.append(&mut names);
    for record in records {
        put_varint(&mut out, sessions[record.session_id.as_str()]);
        put_varint(&mut out, record.request_id);
        match &record.response {
            Response::Success { value, ts } => {
                out.push(0);
                put_varint(&mut out, *ts);
                put_text(&mut out, value);
            }
            Response::Other(json) => {
                out.push(1);
                put_text(&mut out, json);
            }
        }
    }
    out
}

/// The records of a row, borrowed from it: `(session_id, request_id, response)`.
fn decode(records: &[u8]) -> anyhow::Result<Vec<(&str, u64, Response<&str>)>> {
    fn take<'a>(rest: &mut &'a [u8], n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(rest.len() >= n, "a row of {TABLE} ends in the middle of a record");
        let (head, tail) = rest.split_at(n);
        *rest = tail;
        Ok(head)
    }
    fn varint(rest: &mut &[u8]) -> anyhow::Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = take(rest, 1)?[0];
            anyhow::ensure!(shift < 63 || byte <= 1, "a row of {TABLE} holds a number that is too large");
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        anyhow::bail!("a row of {TABLE} holds a number that doesn't end")
    }
    fn text<'a>(rest: &mut &'a [u8]) -> anyhow::Result<&'a str> {
        let len = usize::try_from(varint(rest)?)?;
        Ok(std::str::from_utf8(take(rest, len)?)?)
    }
    let mut rest = records;
    let format = take(&mut rest, 1)?[0];
    anyhow::ensure!(format == FORMAT, "a row of {TABLE} has format {format}, written by a newer version");
    let count = varint(&mut rest)?;
    anyhow::ensure!(count <= rest.len() as u64, "a row of {TABLE} names more sessions than it can hold");
    let sessions = (0..count).map(|_| text(&mut rest)).collect::<anyhow::Result<Vec<&str>>>()?;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let session = usize::try_from(varint(&mut rest)?)?;
        let session_id = *sessions.get(session).context("a record's session isn't in its row's list")?;
        let request_id = varint(&mut rest)?;
        let response = match take(&mut rest, 1)?[0] {
            0 => {
                let ts = varint(&mut rest)?;
                Response::Success { value: text(&mut rest)?, ts }
            }
            1 => Response::Other(text(&mut rest)?),
            kind => anyhow::bail!("a row of {TABLE} holds a response of kind {kind}"),
        };
        out.push((session_id, request_id, response));
    }
    Ok(out)
}

/// A row's `records`. A value that isn't a blob (nothing here writes one) reads as empty, which [`decode`] refuses
/// like any other row it can't read.
fn blob(value: rusqlite::types::ValueRef<'_>) -> &[u8] {
    value.as_blob().unwrap_or_default()
}

/// The `records` of row `id`, if the table has it.
fn read_row(conn: &Connection, id: i64) -> anyhow::Result<Option<Vec<u8>>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT records FROM {TABLE} WHERE id = ?1"))?;
    Ok(stmt.query_row([id], |r| Ok(blob(r.get_ref(0)?).to_vec())).optional()?)
}

fn has_table(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |r| r.get(0),
    )?)
}

fn now_ms() -> i64 {
    crate::db::wall_clock_ms() as i64
}

/// Which row holds each recorded response; see the module docs. Belongs to the writer connection.
pub(crate) struct Index {
    /// Per session, `(request_id, row id)` sorted by request id (a client numbers its requests in order, so new ones
    /// nearly always go at the end and expired ones leave from the front).
    sessions: HashMap<Box<str>, VecDeque<(u64, i64)>>,
    /// The table's rows as `(id, created_ms)`, oldest first.
    rows: VecDeque<(i64, i64)>,
    /// The id the open batch's row gets; in `sessions`, a row id from this one on marks a response that is still in
    /// `pending`.
    next_row: i64,
    /// The responses recorded in the open batch, each with the id of the row it goes into when the batch commits:
    /// `next_row`, or the ones after it once a row is full ([`ROW_BYTES`]).
    pending: Vec<(i64, Record)>,
    /// How much the last of those rows holds so far.
    pending_bytes: usize,
    /// The table changed and this index couldn't be loaded again ([`Index::resync`]): not to be used until it is.
    stale: bool,
}

/// What [`Index::write_pending`] wrote, for [`Index::committed`] once the transaction has committed.
pub(crate) struct Written {
    /// The `created_ms` of the rows it wrote.
    now: i64,
    /// The expired rows it deleted, with their records.
    expired: Vec<(i64, Vec<u8>)>,
}

impl Index {
    /// Create the table (moving the rows of the table used before into it), drop expired rows and load the rest.
    pub(crate) fn open(conn: &Connection) -> anyhow::Result<Index> {
        conn.execute_batch(DDL)?;
        if has_table(conn, LEGACY_TABLE)? {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let moved = (|| -> anyhow::Result<()> {
                import(conn, read_legacy(conn)?)?;
                Ok(conn.execute_batch(&format!("DROP TABLE {LEGACY_TABLE}; COMMIT"))?)
            })();
            if let Err(e) = moved {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e).context("moving the recorded mutation responses to their new table");
            }
        }
        conn.execute(&format!("DELETE FROM {TABLE} WHERE created_ms < ?1"), [now_ms() - RETENTION.as_millis() as i64])?;
        Index::load(conn)
    }

    fn load(conn: &Connection) -> anyhow::Result<Index> {
        let mut index = Index {
            sessions: HashMap::new(),
            rows: VecDeque::new(),
            next_row: 1,
            pending: Vec::new(),
            pending_bytes: 0,
            stale: false,
        };
        let mut stmt = conn.prepare(&format!("SELECT id, created_ms, records FROM {TABLE} ORDER BY id"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let (id, created_ms): (i64, i64) = (row.get(0)?, row.get(1)?);
            index.next_row = id + 1;
            index.rows.push_back((id, created_ms));
            match decode(blob(row.get_ref(2)?)) {
                Ok(records) => {
                    for (session_id, request_id, _) in records {
                        index.insert(session_id, request_id, id);
                    }
                }
                // Left alone until it expires (a newer version may have written it): a retry of one of its
                // mutations runs again, which is better than a database that doesn't open.
                Err(e) => tracing::warn!("skipping the recorded mutation responses of row {id} of {TABLE}: {e:#}"),
            }
        }
        // Imported rows ([`import`]) come after newer ones: expiry takes rows from the front.
        index.rows.make_contiguous().sort_by_key(|&(_, created_ms)| created_ms);
        Ok(index)
    }

    /// After something else may have written the table through the writer connection ([`import`]): load it again
    /// if its newest row isn't the newest one this index knows. (An import's rows get ids above every row of the
    /// table, which can be ids this index used before, for rows that have expired since: comparing with `next_row`
    /// would miss those.) If that fails, the index is [stale](Index::is_stale) until a later call succeeds.
    pub(crate) fn resync(&mut self, conn: &Connection) -> anyhow::Result<()> {
        if !self.pending.is_empty() {
            return Ok(());
        }
        let reloaded = (|| -> anyhow::Result<()> {
            let sql = format!("SELECT COALESCE(MAX(id), 0) FROM {TABLE}");
            let newest: i64 = conn.prepare_cached(&sql)?.query_row([], |r| r.get(0))?;
            let known = self.rows.iter().map(|&(id, _)| id).max().unwrap_or(0);
            if self.stale || newest != known {
                *self = Index::load(conn)?;
            }
            Ok(())
        })();
        self.stale = reloaded.is_err();
        reloaded
    }

    /// The table may hold responses this index doesn't know (a reload failed): looking one up or recording one
    /// fails until [`Index::resync`] succeeds, which a write transaction tries before it begins.
    pub(crate) fn is_stale(&self) -> bool {
        self.stale
    }

    /// The table couldn't be read as it is committed: see [`Index::is_stale`].
    pub(crate) fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// A row of the table, at its place in `rows`: oldest first, also when the wall clock was set back since the
    /// last one (appended, the earlier rows would keep the ones behind them from expiring: expiry takes rows from
    /// the front).
    fn add_row(&mut self, id: i64, created_ms: i64) {
        if self.rows.back().is_none_or(|&(_, newest)| newest <= created_ms) {
            self.rows.push_back((id, created_ms));
        } else {
            let at = self.rows.partition_point(|&(_, other)| other <= created_ms);
            self.rows.insert(at, (id, created_ms));
        }
    }

    /// Responses on record, the open batch's included.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.sessions.values().map(VecDeque::len).sum()
    }

    fn insert(&mut self, session_id: &str, request_id: u64, row: i64) {
        let requests = match self.sessions.get_mut(session_id) {
            Some(requests) => requests,
            None => self.sessions.entry(session_id.into()).or_default(),
        };
        if requests.back().is_none_or(|&(last, _)| last < request_id) {
            requests.push_back((request_id, row));
            return;
        }
        match requests.binary_search_by_key(&request_id, |&(id, _)| id) {
            Ok(at) => requests[at].1 = row,
            Err(at) => requests.insert(at, (request_id, row)),
        }
    }

    /// Forget a response, if `row` is still where the index has it.
    fn remove(&mut self, session_id: &str, request_id: u64, row: i64) {
        let Some(requests) = self.sessions.get_mut(session_id) else { return };
        if let Ok(at) = requests.binary_search_by_key(&request_id, |&(id, _)| id)
            && requests[at].1 == row
        {
            requests.remove(at);
        }
        if requests.is_empty() {
            self.sessions.remove(session_id);
        } else if requests.capacity() >= 64 && requests.len() < requests.capacity() / 4 {
            requests.shrink_to(requests.len() * 2);
        }
    }

    /// The response recorded for `key`, if any: from the open batch, or read from its row.
    pub(crate) fn lookup(&self, conn: &Connection, key: &IdempotencyKey) -> anyhow::Result<Option<StoredResponse>> {
        anyhow::ensure!(!self.stale, "the recorded mutation responses could not be loaded");
        let Some(requests) = self.sessions.get(key.session_id.as_str()) else { return Ok(None) };
        // A request the session hasn't sent before has a higher id than every recorded one, nearly always.
        if requests.back().is_none_or(|&(last, _)| last < key.request_id) {
            return Ok(None);
        }
        let Ok(at) = requests.binary_search_by_key(&key.request_id, |&(id, _)| id) else { return Ok(None) };
        let row = requests[at].1;
        let is_key = |session_id: &str, request_id: u64| request_id == key.request_id && session_id == key.session_id;
        if row >= self.next_row {
            let record = self.pending.iter().find(|(_, r)| is_key(&r.session_id, r.request_id));
            let (_, record) = record.context("a recorded mutation response is missing from the open batch")?;
            return Ok(Some(record.response.stored()?));
        }
        let records = read_row(conn, row)?.with_context(|| format!("row {row} of {TABLE} is gone"))?;
        let found = decode(&records)?.into_iter().find(|&(session_id, request_id, _)| is_key(session_id, request_id));
        let (_, _, response) =
            found.with_context(|| format!("row {row} of {TABLE} lacks a response it should hold"))?;
        Ok(Some(response.stored()?))
    }

    /// Record a response in the open batch: found by [`Index::lookup`] from now on, written by
    /// [`Index::write_pending`] when the batch commits, forgotten by [`Index::rolled_back`] if it doesn't.
    pub(crate) fn add(&mut self, record: Record) {
        let bytes = record.len();
        let mut row = self.pending.last().map_or(self.next_row, |&(row, _)| row);
        if self.pending_bytes > 0 && self.pending_bytes + bytes > ROW_BYTES {
            row += 1;
            self.pending_bytes = 0;
        }
        self.pending_bytes += bytes;
        self.insert(&record.session_id, record.request_id, row);
        self.pending.push((row, record));
    }

    /// Inside the transaction that is about to commit: write the open batch's responses as one row (more if they
    /// are large, [`ROW_BYTES`]), and delete expired rows (a bounded number, [`EXPIRE_ROWS`]). The index changes with
    /// [`Index::committed`].
    pub(crate) fn write_pending(&self, conn: &Connection) -> anyhow::Result<Written> {
        self.write_pending_at(conn, now_ms())
    }

    fn write_pending_at(&self, conn: &Connection, now: i64) -> anyhow::Result<Written> {
        anyhow::ensure!(!self.stale, "the recorded mutation responses could not be loaded");
        let mut written = Written { now, expired: Vec::new() };
        let mut added = 0;
        for row in self.pending.chunk_by(|a, b| a.0 == b.0) {
            conn.prepare_cached(&format!("INSERT INTO {TABLE} (id, created_ms, records) VALUES (?1, ?2, ?3)"))?
                .execute(params![row[0].0, now, encode(row.iter().map(|(_, r)| r))])?;
            added += 1;
        }
        let cutoff = now - RETENTION.as_millis() as i64;
        // As many more as the batch added, so that batches of several rows (large responses) don't outrun it.
        let expire = EXPIRE_ROWS + added;
        for &(id, _) in self.rows.iter().take(expire).take_while(|&&(_, created_ms)| created_ms < cutoff) {
            // Its records say which responses to forget.
            let records = read_row(conn, id)?;
            conn.prepare_cached(&format!("DELETE FROM {TABLE} WHERE id = ?1"))?.execute([id])?;
            written.expired.push((id, records.unwrap_or_default()));
        }
        Ok(written)
    }

    /// The transaction [`Index::write_pending`] wrote in has committed.
    pub(crate) fn committed(&mut self, written: Written) {
        for (id, records) in written.expired {
            // The oldest rows, in order.
            if self.rows.front().is_some_and(|&(row, _)| row == id) {
                self.rows.pop_front();
            }
            match decode(&records) {
                Ok(records) => {
                    for (session_id, request_id, _) in records {
                        self.remove(session_id, request_id, id);
                    }
                }
                // It had no responses in the index ([`Index::load`] skipped it too).
                Err(e) => tracing::debug!("an expired row of {TABLE} couldn't be read: {e:#}"),
            }
        }
        for at in 0..self.pending.len() {
            let row = self.pending[at].0;
            if row >= self.next_row {
                self.add_row(row, written.now);
                self.next_row = row + 1;
            }
        }
        self.pending.clear();
        self.pending_bytes = 0;
        if self.sessions.capacity() >= 1024 && self.sessions.len() < self.sessions.capacity() / 4 {
            self.sessions.shrink_to(self.sessions.len() * 2);
        }
        if self.rows.capacity() >= 1024 && self.rows.len() < self.rows.capacity() / 4 {
            self.rows.shrink_to(self.rows.len() * 2);
        }
    }

    /// The open batch was rolled back: its responses were never recorded.
    pub(crate) fn rolled_back(&mut self) {
        for (row, record) in std::mem::take(&mut self.pending) {
            self.remove(&record.session_id, record.request_id, row);
        }
        self.pending_bytes = 0;
    }

    /// With no transaction open: commit the pending responses on their own (a failed mutation's error, when no batch
    /// is open to carry it).
    pub(crate) fn commit_alone(&mut self, conn: &Connection) -> anyhow::Result<()> {
        let written = (|| -> anyhow::Result<Written> {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let written = self.write_pending(conn)?;
            conn.execute_batch("COMMIT")?;
            Ok(written)
        })();
        match written {
            Ok(written) => {
                self.committed(written);
                Ok(())
            }
            Err(e) => {
                if !conn.is_autocommit() {
                    let _ = conn.execute_batch("ROLLBACK");
                }
                self.rolled_back();
                Err(e)
            }
        }
    }
}

/// Every response on record in a database (one written by this version or an earlier one), each with the time it was
/// recorded (its row's): what a takeover carries over to the new process's copy ([`import`]). A key whose response
/// was replaced is there twice until the earlier row expires: the later one counts.
pub(crate) fn read_all(conn: &Connection) -> anyhow::Result<Vec<(i64, Record)>> {
    let mut all = Vec::new();
    if has_table(conn, TABLE)? {
        let mut stmt = conn.prepare(&format!("SELECT created_ms, records FROM {TABLE} ORDER BY id"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let created_ms: i64 = row.get(0)?;
            // As when a database opens ([`Index::load`]): a row that can't be read is left out, not an error.
            for (session_id, request_id, response) in decode(blob(row.get_ref(1)?)).unwrap_or_default() {
                let record = Record { session_id: session_id.into(), request_id, response: response.to_owned() };
                all.push((created_ms, record));
            }
        }
    }
    // Last: where both tables exist, the earlier version wrote its own after this one's (see [`import`]).
    if has_table(conn, LEGACY_TABLE)? {
        all.extend(read_legacy(conn)?);
    }
    Ok(all)
}

fn read_legacy(conn: &Connection) -> anyhow::Result<Vec<(i64, Record)>> {
    let sql = format!("SELECT created_ms, session_id, request_id, response FROM {LEGACY_TABLE}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        let response = Response::from_stored(r.get(3)?);
        let record = Record { session_id: r.get(1)?, request_id: r.get::<_, i64>(2)? as u64, response };
        Ok((r.get(0)?, record))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Bring the table up to the responses given, inside the caller's transaction on the writer connection (through
/// `Database::with_writer`, which brings the [`Index`] up to date afterwards): the ones it lacks are added, and one
/// that differs from what the table has for its key replaces it. Expired ones are left out; the others keep the time
/// they were recorded at, to the row: a row expires with its newest response (so one can stay somewhat longer than
/// [`RETENTION`]).
///
/// What is given is the later word on its keys, for both callers. A takeover gives what the database holds at the
/// end, and its own table started as a copy of that database: where they differ, the key expired in the database
/// and its mutation ran again there. The move of the earlier table gives rows that the earlier version wrote after
/// this one last ran (this one leaves no such table behind). So no times are compared, which a wall clock set
/// back would get wrong, and which a packed row doesn't have per response anyway. The replacement goes into a new
/// row, with a higher id: that is the one the [`Index`] finds, also once the row with the old one has expired.
/// Of several responses given for one key, the last counts (as [`read_all`] returns them: the newest row last).
pub(crate) fn import(conn: &Connection, records: Vec<(i64, Record)>) -> anyhow::Result<()> {
    // Per key, the response the table has (its newest row's), as a fingerprint.
    let mut have: HashMap<(String, u64), u64> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!("SELECT records FROM {TABLE} ORDER BY id"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            for (session_id, request_id, response) in decode(blob(row.get_ref(0)?)).unwrap_or_default() {
                have.insert((session_id.to_string(), request_id), response.fingerprint());
            }
        }
    }
    let cutoff = now_ms() - RETENTION.as_millis() as i64;
    let mut given: HashMap<(String, u64), usize> = HashMap::new();
    let mut records = records.into_iter().fold(Vec::new(), |mut last: Vec<(i64, Record)>, (created_ms, record)| {
        match given.entry((record.session_id.clone(), record.request_id)) {
            std::collections::hash_map::Entry::Occupied(at) => last[*at.get()] = (created_ms, record),
            std::collections::hash_map::Entry::Vacant(at) => {
                at.insert(last.len());
                last.push((created_ms, record));
            }
        }
        last
    });
    records.retain(|(created_ms, r)| {
        *created_ms >= cutoff && have.get(&(r.session_id.clone(), r.request_id)) != Some(&r.response.fingerprint())
    });
    records.sort_by_key(|&(created_ms, _)| created_ms);
    let mut id: i64 = conn.query_row(&format!("SELECT COALESCE(MAX(id), 0) FROM {TABLE}"), [], |r| r.get(0))?;
    let mut insert = conn.prepare(&format!("INSERT INTO {TABLE} (id, created_ms, records) VALUES (?1, ?2, ?3)"))?;
    let mut rest = &records[..];
    while !rest.is_empty() {
        // Up to `IMPORT_ROW` responses and `ROW_BYTES`, and at least one.
        let mut bytes = 0;
        let fits = |(_, r): &&(i64, Record)| {
            bytes += r.len();
            bytes <= ROW_BYTES
        };
        let n = rest.iter().take(IMPORT_ROW).take_while(fits).count().max(1);
        let (row, after) = rest.split_at(n);
        rest = after;
        id += 1;
        let created_ms = row.last().expect("at least one").0;
        insert.execute(params![id, created_ms, encode(row.iter().map(|(_, r)| r))])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_round_trip_with_their_classification() {
        let cases: Vec<Error> = vec![
            UserError::new("empty", "text is empty").with_data(serde_json::json!({ "min": 1 })).into(),
            InvalidArgs("Invalid args for add: x".into()).into(),
            FunctionNotFound("Could not find public function for 'x'".into()).into(),
            anyhow::anyhow!("inner").context("outer"),
        ];
        for e in cases {
            let stored = StoredResponse::from_error(&e);
            let json = serde_json::to_string(&stored).unwrap();
            let back: StoredResponse = serde_json::from_str(&json).unwrap();
            let replayed = back.into_result().unwrap_err();
            for dev in [true, false] {
                assert_eq!(ClientError::from_anyhow(&replayed, dev), ClientError::from_anyhow(&e, dev), "{e:#}");
            }
        }
    }

    fn key(session_id: &str, request_id: u64) -> IdempotencyKey {
        IdempotencyKey { session_id: session_id.into(), request_id }
    }

    fn success(key: &IdempotencyKey, ts: u64) -> Record {
        Record::success(key, &serde_json::json!("id"), ts).unwrap()
    }

    fn stored(ts: u64) -> Option<StoredResponse> {
        Some(StoredResponse::Success { value: serde_json::json!("id"), ts })
    }

    /// Commit the pending responses as a batch does, at `now`.
    fn commit_at(index: &mut Index, conn: &Connection, now: i64) {
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let written = index.write_pending_at(conn, now).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        index.committed(written);
    }

    /// What the index holds on the heap: per session its id, a map slot and the queue's buffer; the list of rows; the
    /// open batch's responses.
    fn heap_bytes(index: &Index) -> usize {
        let slot = std::mem::size_of::<(Box<str>, VecDeque<(u64, i64)>)>() + 1;
        let sessions = index.sessions.iter().map(|(id, requests)| id.len() + requests.capacity() * 16);
        let pending = index.pending.capacity() * std::mem::size_of::<(i64, Record)>()
            + index.pending.iter().map(|(_, r)| r.session_id.capacity() + r.len()).sum::<usize>();
        index.sessions.capacity() * slot + sessions.sum::<usize>() + index.rows.capacity() * 16 + pending
    }

    fn row_count(conn: &Connection) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {TABLE}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_response_is_found_in_the_open_batch_and_after_it_commits() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        let (a, b) = (key("s", 3), key("t", 3));
        assert_eq!(index.lookup(&conn, &a).unwrap(), None);
        index.add(success(&a, 7));
        index.add(Record::error(&b, &UserError::new("c", "m").into()).unwrap());
        // Still in the open batch: nothing is written yet.
        assert_eq!(index.lookup(&conn, &a).unwrap(), stored(7));
        assert_eq!(row_count(&conn), 0);
        index.commit_alone(&conn).unwrap();
        // One row for both.
        assert_eq!(row_count(&conn), 1);
        assert_eq!(index.lookup(&conn, &a).unwrap(), stored(7));
        let replayed = index.lookup(&conn, &b).unwrap().unwrap().into_result().unwrap_err();
        assert_eq!(replayed.downcast_ref::<UserError>(), Some(&UserError::new("c", "m")));
        // Another session, or another request of the same one.
        assert_eq!(index.lookup(&conn, &key("u", 3)).unwrap(), None);
        assert_eq!(index.lookup(&conn, &key("s", 4)).unwrap(), None);
        // A database opened again loads the same.
        let reopened = Index::open(&conn).unwrap();
        assert_eq!(reopened.lookup(&conn, &a).unwrap(), stored(7));
        assert_eq!(reopened.len(), 2);
    }

    #[test]
    fn a_batch_that_rolls_back_leaves_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        index.add(success(&key("s", 1), 1));
        index.commit_alone(&conn).unwrap();
        index.add(success(&key("s", 2), 2));
        index.add(success(&key("t", 1), 2));
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        index.write_pending(&conn).unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
        index.rolled_back();
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), None);
        assert_eq!(index.lookup(&conn, &key("t", 1)).unwrap(), None);
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(1));
        assert_eq!((index.len(), row_count(&conn)), (1, 1));
        // The next batch takes the row the rolled-back one would have had.
        index.add(success(&key("s", 2), 5));
        index.commit_alone(&conn).unwrap();
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), stored(5));
        assert_eq!(Index::open(&conn).unwrap().lookup(&conn, &key("s", 2)).unwrap(), stored(5));
    }

    #[test]
    fn large_responses_of_one_batch_go_to_several_rows() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        let large = |request_id| Record::success(&key("s", request_id), &"x".repeat(ROW_BYTES * 2 / 5).into(), 1);
        for request_id in 0..5 {
            index.add(large(request_id).unwrap());
        }
        // Found while the batch is open, whichever row they will be in.
        for request_id in 0..5 {
            assert!(index.lookup(&conn, &key("s", request_id)).unwrap().is_some());
        }
        index.commit_alone(&conn).unwrap();
        // Two to a row.
        assert_eq!((index.len(), row_count(&conn)), (5, 3));
        index.add(success(&key("s", 5), 2));
        index.add(large(6).unwrap());
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        index.write_pending(&conn).unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
        index.rolled_back();
        index.add(success(&key("s", 7), 3));
        index.commit_alone(&conn).unwrap();
        assert_eq!((index.len(), row_count(&conn)), (6, 4));
        for index in [index, Index::open(&conn).unwrap()] {
            let StoredResponse::Success { value, .. } = index.lookup(&conn, &key("s", 4)).unwrap().unwrap() else {
                panic!("a success")
            };
            assert_eq!(value.as_str().unwrap().len(), ROW_BYTES * 2 / 5);
            assert_eq!(index.lookup(&conn, &key("s", 7)).unwrap(), stored(3));
            assert_eq!(index.lookup(&conn, &key("s", 5)).unwrap(), None);
            assert_eq!(index.lookup(&conn, &key("s", 6)).unwrap(), None);
        }
        // An import keeps to the same size (the small one shares the last row here).
        let other = Connection::open_in_memory().unwrap();
        Index::open(&other).unwrap();
        import(&other, read_all(&conn).unwrap()).unwrap();
        assert_eq!((Index::open(&other).unwrap().len(), row_count(&other)), (6, 3));
    }

    /// Not a test: what the index costs for ten minutes of mutations, as numbers.
    /// `cargo test --release -p rxferrix --lib index_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn index_cost() {
        // (sessions, responses of each): a few busy sessions, then many that each sent a few.
        for (sessions, each) in [(1_000u64, 6_000u64), (600_000, 10)] {
            let dir = tempfile::tempdir().unwrap();
            let conn = Connection::open(dir.path().join("t.db")).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = OFF").unwrap();
            let mut index = Index::open(&conn).unwrap();
            conn.execute_batch("BEGIN").unwrap();
            let now = now_ms();
            for request_id in 0..each {
                for session in 0..sessions {
                    let session_id = format!("user:k17c3v9q2m4x8z5b1n6d0f7h3j|{session:032x}");
                    index.add(success(&key(&session_id, request_id), request_id));
                    if index.pending.len() == 256 {
                        let written = index.write_pending_at(&conn, now).unwrap();
                        index.committed(written);
                    }
                }
            }
            let written = index.write_pending_at(&conn, now).unwrap();
            index.committed(written);
            conn.execute_batch("COMMIT").unwrap();
            let (responses, rows) = (index.len(), row_count(&conn));
            drop(index);
            let started = std::time::Instant::now();
            let loaded = Index::open(&conn).unwrap();
            let (took, bytes) = (started.elapsed(), heap_bytes(&loaded));
            assert_eq!(loaded.len(), responses);
            let file = std::fs::metadata(dir.path().join("t.db")).unwrap().len() as f64 / (1 << 20) as f64;
            println!(
                "[index_cost] {sessions} sessions x {each}: {responses} responses in {rows} rows ({file:.0} MiB on \
                 disk), loaded in {took:.2?}, {:.0} MiB in memory ({:.0} bytes per response)",
                bytes as f64 / (1 << 20) as f64,
                bytes as f64 / responses as f64
            );
        }
    }

    #[test]
    fn rows_expire_when_a_later_batch_commits() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        let retention = RETENTION.as_millis() as i64;
        // Requests recorded out of order, over two rows.
        index.add(success(&key("s", 2), 1));
        index.add(success(&key("t", 1), 1));
        commit_at(&mut index, &conn, 1_000);
        index.add(success(&key("s", 1), 2));
        commit_at(&mut index, &conn, 2_000);
        // Not yet: a commit without responses, within the retention of both.
        commit_at(&mut index, &conn, 1_000 + retention);
        assert_eq!((index.len(), row_count(&conn)), (3, 2));
        // The first row has expired, the second hasn't.
        index.add(success(&key("u", 1), 3));
        commit_at(&mut index, &conn, 1_001 + retention);
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), None);
        assert_eq!(index.lookup(&conn, &key("t", 1)).unwrap(), None);
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(2));
        assert_eq!(index.lookup(&conn, &key("u", 1)).unwrap(), stored(3));
        assert_eq!((index.len(), row_count(&conn)), (2, 2));
        // A request recorded again after its response expired is found in its new row.
        index.add(success(&key("s", 2), 9));
        commit_at(&mut index, &conn, 1_002 + retention);
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), stored(9));
    }

    #[test]
    fn a_commit_deletes_a_bounded_number_of_expired_rows() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        for request in 0..(EXPIRE_ROWS as u64 + 5) {
            index.add(success(&key("s", request), request));
            commit_at(&mut index, &conn, 1_000);
        }
        let later = 2_000 + RETENTION.as_millis() as i64;
        commit_at(&mut index, &conn, later);
        assert_eq!((index.len(), row_count(&conn)), (5, 5));
        assert_eq!(index.lookup(&conn, &key("s", EXPIRE_ROWS as u64)).unwrap(), stored(EXPIRE_ROWS as u64));
        commit_at(&mut index, &conn, later);
        assert_eq!((index.len(), row_count(&conn)), (0, 0));
        assert!(index.sessions.is_empty() && index.rows.is_empty());
    }

    /// Three hours of traffic by a clock of its own, a batch a second: sessions that stay and send all the time,
    /// sessions that send once and are gone, batches that roll back, a key recorded again. The index and the table
    /// hold what the last [`RETENTION`] holds and no more, hour after hour, and give it back when the traffic ends.
    #[test]
    fn the_index_holds_no_more_than_the_retention_holds() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        let retention = RETENTION.as_millis() as i64;
        let batches_kept = (retention / 1_000) as usize + 1;
        let (staying, passing, each) = (10u64, 10u64, 4u64);
        let per_batch = (staying * each + passing) as usize;
        let mut now = 1_000_000;
        // The most it held (responses, sessions, rows of the table, bytes), per hour.
        let mut most = Vec::new();
        for hour in 0..3u64 {
            let mut held = (0, 0, 0, 0);
            for second in 0..3_600u64 {
                let step = hour * 3_600 + second;
                now += 1_000;
                for session in 0..staying {
                    for request in 0..each {
                        index.add(success(&key(&format!("staying-{session}"), step * each + request), step));
                    }
                }
                for session in 0..passing {
                    index.add(success(&key(&format!("passing-{step}-{session}"), 1), step));
                }
                if step % 11 == 5 && step > 20 {
                    // Recorded again while its first response is still on record (as an import does).
                    index.add(success(&key("staying-0", (step - 10) * each), step));
                }
                if step % 7 == 3 {
                    index.rolled_back();
                    conn.execute_batch("BEGIN; ROLLBACK").unwrap();
                } else {
                    commit_at(&mut index, &conn, now);
                }
                held.0 = held.0.max(index.len());
                held.1 = held.1.max(index.sessions.len());
                held.2 = held.2.max(row_count(&conn) as usize);
                if step % 30 == 0 {
                    held.3 = held.3.max(heap_bytes(&index));
                }
                assert_eq!(index.rows.len(), row_count(&conn) as usize, "at {step}");
            }
            assert!(held.0 <= batches_kept * per_batch, "{held:?}");
            assert!(held.1 <= staying as usize + batches_kept * passing as usize, "{held:?}");
            assert!(held.2 <= batches_kept, "{held:?}");
            most.push(held);
        }
        // Full after the first ten minutes: the later hours hold what the first did (the buffers may be sized
        // differently, by a few percent).
        assert_eq!((most[1].0, most[1].1, most[1].2), (most[2].0, most[2].1, most[2].2), "{most:?}");
        assert!(most[2].3 <= most[1].3 + most[1].3 / 20, "{most:?}");
        // The traffic ends: commits without responses (anything that writes) clear it, and the memory goes back.
        let mut commits = 0;
        while index.len() > 0 {
            now += retention;
            commit_at(&mut index, &conn, now);
            commits += 1;
            assert!(commits <= batches_kept / EXPIRE_ROWS + 2, "{} left after {commits} commits", index.len());
        }
        assert!(index.sessions.is_empty() && index.rows.is_empty() && index.pending.is_empty());
        assert_eq!(row_count(&conn), 0);
        // (Both stop shrinking under 1024 entries. Kept at its largest, the list of rows would have 1024.)
        assert!(index.sessions.capacity() < 1024 && index.rows.capacity() < 1024, "buffers kept at their largest");
        // (What stays is a map and two buffers of a size that doesn't depend on what was held.)
        assert!(heap_bytes(&index) < 128 << 10, "{} of {} bytes kept", heap_bytes(&index), most[2].3);
    }

    #[test]
    fn a_batch_of_several_rows_expires_as_many_more() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        for request in 0..(EXPIRE_ROWS as u64 + 5) {
            index.add(success(&key("s", request), request));
            commit_at(&mut index, &conn, 1_000);
        }
        // Three responses of a row's size each: three rows.
        let large = serde_json::json!("x".repeat(ROW_BYTES));
        for request in 0..3 {
            index.add(Record::success(&key("large", request), &large, 1).unwrap());
        }
        commit_at(&mut index, &conn, 2_000 + RETENTION.as_millis() as i64);
        assert_eq!((index.len(), row_count(&conn)), (2 + 3, 2 + 3));
    }

    #[test]
    fn an_import_into_ids_used_before_is_noticed() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        index.add(success(&key("s", 1), 1));
        commit_at(&mut index, &conn, 1_000);
        commit_at(&mut index, &conn, 2_000 + RETENTION.as_millis() as i64);
        assert_eq!(row_count(&conn), 0);
        // The table is empty, so the imported row gets the id of the one that expired.
        import(&conn, vec![(now_ms(), success(&key("t", 1), 5))]).unwrap();
        index.resync(&conn).unwrap();
        assert_eq!(index.lookup(&conn, &key("t", 1)).unwrap(), stored(5));
        index.add(success(&key("u", 1), 6));
        index.commit_alone(&conn).unwrap();
        assert_eq!(index.lookup(&conn, &key("t", 1)).unwrap(), stored(5));
        assert_eq!(index.lookup(&conn, &key("u", 1)).unwrap(), stored(6));
        assert_eq!((index.len(), row_count(&conn)), (2, 2));
    }

    #[test]
    fn an_import_replaces_a_response_that_differs() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        let now = now_ms();
        for request in 1..=3 {
            index.add(success(&key("s", request), request));
        }
        commit_at(&mut index, &conn, now - 60_000);
        let carried = vec![
            // Dated before the table's row (a wall clock that was set back): the later word all the same.
            (now - 120_000, success(&key("s", 1), 7)),
            // Two for one key: the last counts.
            (now - 1_000, success(&key("s", 2), 6)),
            (now - 2_000, success(&key("s", 2), 8)),
            // What the table has: not written again.
            (now - 60_000, success(&key("s", 3), 3)),
        ];
        import(&conn, carried.clone()).unwrap();
        index.resync(&conn).unwrap();
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(7));
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), stored(8));
        assert_eq!(index.lookup(&conn, &key("s", 3)).unwrap(), stored(3));
        assert_eq!(read_all(&conn).unwrap().len(), 3 + 2);
        assert_eq!(row_count(&conn), 2);
        import(&conn, carried).unwrap();
        assert_eq!(row_count(&conn), 2);
        // The same after a restart, and once the earlier row has expired.
        let mut index = Index::open(&conn).unwrap();
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(7));
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), stored(8));
        commit_at(&mut index, &conn, now - 59_999 + RETENTION.as_millis() as i64);
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(7));
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), stored(8));
        assert_eq!(index.lookup(&conn, &key("s", 3)).unwrap(), None);
        assert_eq!((index.len(), row_count(&conn)), (2, 1));
    }

    /// A database written by an earlier version has one row per response in another table.
    #[test]
    fn the_rows_of_the_earlier_table_move_over() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _mutation_dedup (
                 session_id TEXT NOT NULL, request_id INTEGER NOT NULL, created_ms INTEGER NOT NULL,
                 response TEXT NOT NULL, PRIMARY KEY (session_id, request_id)
             ) WITHOUT ROWID;
             CREATE INDEX _mutation_dedup_by_created ON _mutation_dedup (created_ms);",
        )
        .unwrap();
        let now = now_ms();
        for (session_id, request_id, age) in [("s", 1u64, 5_000), ("s", 2, 1_000), ("old", 1, 3_600_000)] {
            conn.execute(
                "INSERT INTO _mutation_dedup VALUES (?1, ?2, ?3, ?4)",
                params![session_id, request_id as i64, now - age, serde_json::to_string(&stored(request_id)).unwrap()],
            )
            .unwrap();
        }
        let from_before = read_all(&conn).unwrap();
        assert_eq!(from_before.len(), 3);
        let index = Index::open(&conn).unwrap();
        assert!(!has_table(&conn, LEGACY_TABLE).unwrap());
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(1));
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), stored(2));
        // Expired by the time it moved.
        assert_eq!(index.lookup(&conn, &key("old", 1)).unwrap(), None);
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn an_import_adds_what_is_missing_and_the_index_follows() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        index.add(success(&key("s", 1), 1));
        index.commit_alone(&conn).unwrap();
        let now = now_ms();
        let carried: Vec<(i64, Record)> = (1..=(IMPORT_ROW as u64 + 10))
            .map(|request| (now - 1_000 + request as i64, success(&key("s", request), 100 + request)))
            .collect();
        import(&conn, carried.clone()).unwrap();
        index.resync(&conn).unwrap();
        // The one it had is replaced (the response given differs); the others arrive; over two rows.
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(101));
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), stored(102));
        assert_eq!(index.lookup(&conn, &key("s", IMPORT_ROW as u64 + 10)).unwrap(), stored(IMPORT_ROW as u64 + 110));
        assert_eq!((index.len(), row_count(&conn)), (IMPORT_ROW + 10, 3));
        // Importing the same again changes nothing, and what was read back is what went in.
        import(&conn, carried.clone()).unwrap();
        assert_eq!(row_count(&conn), 3);
        // (With the response that was replaced, which is still in its row: the later one counts.)
        let mut all = read_all(&conn).unwrap();
        all.sort_by_key(|(_, r)| r.request_id);
        assert_eq!(all.len(), 1 + IMPORT_ROW + 10);
        assert_eq!((&all[0].1, &all[1].1, &all[2].1), (&success(&key("s", 1), 1), &carried[0].1, &carried[1].1));
        // New responses keep working after the reload.
        index.add(success(&key("t", 1), 7));
        index.commit_alone(&conn).unwrap();
        assert_eq!(index.lookup(&conn, &key("t", 1)).unwrap(), stored(7));
    }

    /// The wall clock was set back (an hour here) while rows of before were on record: the rows written since still
    /// expire after [`RETENTION`], not only once the earlier ones have.
    #[test]
    fn rows_expire_after_the_clock_was_set_back() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        let (ahead, retention) = (10_000_000, RETENTION.as_millis() as i64);
        index.add(success(&key("ahead", 1), 1));
        commit_at(&mut index, &conn, ahead);
        let back = ahead - 3_600_000;
        index.add(success(&key("s", 1), 2));
        commit_at(&mut index, &conn, back);
        index.add(success(&key("s", 2), 3));
        commit_at(&mut index, &conn, back + 1_000);
        commit_at(&mut index, &conn, back + 1_001 + retention);
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), None);
        assert_eq!(index.lookup(&conn, &key("s", 2)).unwrap(), None);
        assert_eq!(index.lookup(&conn, &key("ahead", 1)).unwrap(), stored(1));
        assert_eq!((index.len(), row_count(&conn)), (1, 1));
        // The row of before goes when its own time has come, and the list is in order again for what follows.
        index.add(success(&key("s", 3), 4));
        commit_at(&mut index, &conn, ahead + 1 + retention);
        assert_eq!(index.lookup(&conn, &key("ahead", 1)).unwrap(), None);
        assert_eq!(index.lookup(&conn, &key("s", 3)).unwrap(), stored(4));
        assert_eq!((index.len(), row_count(&conn)), (1, 1));
    }

    /// What a takeover does, with a database of the earlier version: the new process's copy holds a response whose
    /// key expires in the database meanwhile and whose mutation then runs again there.
    #[test]
    fn a_takeover_carries_the_response_of_a_mutation_that_ran_again() {
        let now = now_ms();
        let legacy = |records: &[(&str, u64, i64, u64)]| {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(
                "CREATE TABLE _mutation_dedup (
                     session_id TEXT NOT NULL, request_id INTEGER NOT NULL, created_ms INTEGER NOT NULL,
                     response TEXT NOT NULL, PRIMARY KEY (session_id, request_id)) WITHOUT ROWID;",
            )
            .unwrap();
            for &(session, request, created_ms, ts) in records {
                let response = serde_json::to_string(&stored(ts).unwrap()).unwrap();
                conn.execute(
                    "INSERT INTO _mutation_dedup VALUES (?1, ?2, ?3, ?4)",
                    params![session, request as i64, created_ms, response],
                )
                .unwrap();
            }
            conn
        };
        // When the copy is made: `s 1` is about to expire, `s 2` is five minutes old, and `l 1` was recorded before
        // the wall clock was set back by half an hour. The copy packs them into one row, which carries the time of
        // the newest: one that is still ahead.
        let ahead = ("l", 1, now + 1_800_000, 9);
        let copy = legacy(&[("s", 1, now - 599_000, 1), ("s", 2, now - 300_000, 2), ahead]);
        let mut index = Index::open(&copy).unwrap();
        assert_eq!(row_count(&copy), 1);
        // Meanwhile in the database: `s 1` has expired, was sent again and ran again; `t 1` is new.
        let source = legacy(&[("s", 1, now - 1_000, 11), ("s", 2, now - 300_000, 2), ("t", 1, now - 500, 3), ahead]);
        import(&copy, read_all(&source).unwrap()).unwrap();
        index.resync(&copy).unwrap();
        assert_eq!(index.lookup(&copy, &key("s", 1)).unwrap(), stored(11));
        assert_eq!(index.lookup(&copy, &key("s", 2)).unwrap(), stored(2));
        assert_eq!(index.lookup(&copy, &key("t", 1)).unwrap(), stored(3));
        assert_eq!(index.lookup(&copy, &key("l", 1)).unwrap(), stored(9));
        // The same for a database of this version, whose rows hold what committed together.
        let source = Connection::open_in_memory().unwrap();
        let mut theirs = Index::open(&source).unwrap();
        theirs.add(success(&key("s", 1), 1));
        commit_at(&mut theirs, &source, now - 599_000);
        theirs.add(success(&key("s", 2), 2));
        commit_at(&mut theirs, &source, now - 300_000);
        let copy = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&copy).unwrap();
        import(&copy, read_all(&source).unwrap()).unwrap();
        index.resync(&copy).unwrap();
        // Its first row expires; the mutation runs again.
        commit_at(&mut theirs, &source, now - 599_000 + RETENTION.as_millis() as i64 + 1);
        assert_eq!(theirs.lookup(&source, &key("s", 1)).unwrap(), None);
        theirs.add(success(&key("s", 1), 11));
        commit_at(&mut theirs, &source, now - 500);
        import(&copy, read_all(&source).unwrap()).unwrap();
        index.resync(&copy).unwrap();
        assert_eq!(index.lookup(&copy, &key("s", 1)).unwrap(), stored(11));
        assert_eq!(index.lookup(&copy, &key("s", 2)).unwrap(), stored(2));
        let index = Index::open(&copy).unwrap();
        assert_eq!(index.lookup(&copy, &key("s", 1)).unwrap(), stored(11));
    }

    /// A database that went back to the earlier version: its table of this version's rows is still there, unknown to
    /// the earlier version, which records in its own. A mutation resent then ran again; that answer is the later one,
    /// when this version opens the database and when it takes it over.
    #[test]
    fn the_earlier_table_is_the_later_word_where_both_exist() {
        let now = now_ms();
        let both = || {
            let conn = Connection::open_in_memory().unwrap();
            let mut index = Index::open(&conn).unwrap();
            index.add(success(&key("s", 1), 1));
            index.add(success(&key("s", 2), 2));
            commit_at(&mut index, &conn, now - 60_000);
            conn.execute_batch(
                "CREATE TABLE _mutation_dedup (
                     session_id TEXT NOT NULL, request_id INTEGER NOT NULL, created_ms INTEGER NOT NULL,
                     response TEXT NOT NULL, PRIMARY KEY (session_id, request_id)) WITHOUT ROWID;",
            )
            .unwrap();
            let response = serde_json::to_string(&stored(11).unwrap()).unwrap();
            conn.execute("INSERT INTO _mutation_dedup VALUES ('s', 1, ?1, ?2)", params![now - 30_000, response])
                .unwrap();
            conn
        };
        // Opened by this version.
        let opened = both();
        let index = Index::open(&opened).unwrap();
        assert_eq!(index.lookup(&opened, &key("s", 1)).unwrap(), stored(11));
        assert_eq!(index.lookup(&opened, &key("s", 2)).unwrap(), stored(2));
        // Taken over by this version: its copy opens the same way, then gets what the database holds at the end.
        let (source, copy) = (both(), both());
        let mut index = Index::open(&copy).unwrap();
        import(&copy, read_all(&source).unwrap()).unwrap();
        index.resync(&copy).unwrap();
        assert_eq!(index.lookup(&copy, &key("s", 1)).unwrap(), stored(11));
        assert_eq!(index.lookup(&copy, &key("s", 2)).unwrap(), stored(2));
        let index = Index::open(&copy).unwrap();
        assert_eq!(index.lookup(&copy, &key("s", 1)).unwrap(), stored(11));
    }

    /// A row that can't be read stays until it expires, then goes like the others.
    #[test]
    fn a_row_that_cannot_be_read_expires() {
        let conn = Connection::open_in_memory().unwrap();
        let now = now_ms();
        Index::open(&conn).unwrap();
        // One of a format this version doesn't know, one that isn't even a blob.
        conn.execute(&format!("INSERT INTO {TABLE} VALUES (1, ?1, x'63')"), [now - 1_000]).unwrap();
        conn.execute(&format!("INSERT INTO {TABLE} VALUES (2, ?1, 'text')"), [now - 900]).unwrap();
        let mut index = Index::open(&conn).unwrap();
        index.add(success(&key("s", 1), 1));
        commit_at(&mut index, &conn, now);
        assert_eq!((index.len(), row_count(&conn)), (1, 3));
        assert_eq!(read_all(&conn).unwrap().len(), 1);
        import(&conn, vec![(now, success(&key("t", 1), 2))]).unwrap();
        index.resync(&conn).unwrap();
        assert_eq!((index.len(), row_count(&conn)), (2, 4));
        commit_at(&mut index, &conn, now + RETENTION.as_millis() as i64);
        assert_eq!((index.len(), row_count(&conn)), (2, 2));
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(1));
    }

    /// The index wasn't brought up to date after an import (reading the table failed): it says so instead of
    /// answering from what it had, and is right again once the table can be read.
    #[test]
    fn an_index_that_could_not_be_reloaded_is_not_used() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        index.add(success(&key("s", 1), 1));
        index.commit_alone(&conn).unwrap();
        import(&conn, vec![(now_ms(), success(&key("t", 1), 2))]).unwrap();
        // The table can't be read while another statement holds it exclusively... stand-in: the table is gone.
        conn.execute_batch(&format!("ALTER TABLE {TABLE} RENAME TO elsewhere")).unwrap();
        assert!(index.resync(&conn).is_err());
        conn.execute_batch(&format!("ALTER TABLE elsewhere RENAME TO {TABLE}")).unwrap();
        assert!(index.lookup(&conn, &key("t", 1)).is_err(), "answered from an index that is out of date");
        index.resync(&conn).unwrap();
        assert_eq!(index.lookup(&conn, &key("t", 1)).unwrap(), stored(2));
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(1));
    }

    #[test]
    fn a_row_holds_each_session_id_once_and_every_kind_of_response() {
        let session = |n: u64| format!("user:k17c3v9q2m4x8z5b1n6d0f7h3j|{n:032x}");
        let mut records: Vec<Record> =
            (0..400u64).map(|i| success(&key(&session(i % 4), 1_000_000 + i), 5_000_000 + i)).collect();
        records.push(Record::error(&key(&session(0), 7), &UserError::new("c", "m").into()).unwrap());
        let large =
            Record::success(&key("other", u64::MAX), &serde_json::json!({ "list": [1, "two", null] }), u64::MAX);
        records.push(large.unwrap());
        let row = encode(&records);
        // 400 successes of 4 sessions: about 15 bytes each, not the 100 their session ids alone would take.
        assert!(row.len() < 400 * 16 + 4 * 64 + 256, "{} bytes", row.len());
        let decoded = decode(&row).unwrap();
        assert_eq!(decoded.len(), records.len());
        for (record, (session_id, request_id, response)) in records.iter().zip(decoded) {
            assert_eq!((record.session_id.as_str(), record.request_id), (session_id, request_id));
            assert_eq!(record.response, response.to_owned());
            assert_eq!(record.response.stored().unwrap(), response.stored().unwrap());
        }
        // What the table used before held is the same response.
        let before = serde_json::to_string(&stored(9)).unwrap();
        assert_eq!(Response::from_stored(before).stored().unwrap(), stored(9).unwrap());
        let error = serde_json::to_string(&StoredResponse::from_error(&UserError::new("c", "m").into())).unwrap();
        assert_eq!(Response::from_stored(error.clone()), Response::Other(error));
    }

    #[test]
    fn a_row_that_cannot_be_read_is_an_error_not_a_panic() {
        let records = encode(&[success(&key("session", 1), 1), success(&key("séance", u64::MAX), 2)]);
        let decoded = decode(&records).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!((decoded[1].0, decoded[1].1), ("séance", u64::MAX));
        for cut in 1..records.len() {
            // Cut between two records it reads one; anywhere else it fails.
            if let Ok(some) = decode(&records[..cut]) {
                assert!(some.len() < 2, "cut at {cut}");
            }
        }
        assert!(decode(&[FORMAT + 1]).is_err());
        assert!(decode(&[]).is_err());
        // A database with such a row still opens, without its responses.
        let conn = Connection::open_in_memory().unwrap();
        let mut index = Index::open(&conn).unwrap();
        index.add(success(&key("s", 1), 1));
        index.commit_alone(&conn).unwrap();
        conn.execute(&format!("INSERT INTO {TABLE} VALUES (2, ?1, x'63')"), [now_ms()]).unwrap();
        let mut index = Index::open(&conn).unwrap();
        assert_eq!(index.lookup(&conn, &key("s", 1)).unwrap(), stored(1));
        index.add(success(&key("s", 2), 2));
        index.commit_alone(&conn).unwrap();
        assert_eq!(row_count(&conn), 3);
        // A takeover carries the rest over as well.
        let carried = read_all(&conn).unwrap();
        assert_eq!(carried.len(), 2);
        import(&conn, carried).unwrap();
        assert_eq!(row_count(&conn), 3);
    }
}

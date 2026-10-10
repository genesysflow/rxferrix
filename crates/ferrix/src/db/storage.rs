//! Low-level SQLite access: connection setup, DDL and the handful of statements the engine uses.
//!
//! Storage is single-version: a document row is overwritten in place and SQLite's WAL gives readers a consistent
//! snapshot for the duration of a read transaction. Every index (other than `by_id`, which uses the `documents`
//! primary key) is a set of rows in `index_entries` keyed by the memcomparable key, so every index read is a byte
//! range scan.

use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::db::catalog::IndexId;
use crate::db::readset::KeyRange;
use crate::schema::Order;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS documents (id BLOB PRIMARY KEY, table_number INTEGER NOT NULL, json TEXT NOT NULL, ts INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS index_entries (index_id INTEGER NOT NULL, key BLOB NOT NULL, doc_id BLOB NOT NULL, PRIMARY KEY (index_id, key)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS _meta (k TEXT PRIMARY KEY, v);
CREATE TABLE IF NOT EXISTS _tables (table_number INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS _indexes (index_id INTEGER PRIMARY KEY, table_number INTEGER NOT NULL, name TEXT NOT NULL, fields TEXT NOT NULL, UNIQUE(table_number, name));
INSERT OR IGNORE INTO _meta (k, v) VALUES ('ts', 0);

-- When each file's `_storage` document was deleted (maintained by the write path, also when a journal is replayed), so
-- its blob can be kept for a while (`StorageConfig::keep_deleted`): point-in-time restores to before the deletion find
-- it. Removed when the document comes back.
CREATE TABLE IF NOT EXISTS _storage_deleted (id BLOB PRIMARY KEY, deleted_at REAL NOT NULL) WITHOUT ROWID;
";

/// `_meta` keys.
pub const META_TS: &str = "ts";
/// Wall-clock ms of the latest commit; `QueryCtx::now()` returns it so queries stay deterministic per ts.
pub const META_TIME: &str = "time";
/// The latest `_creationTime` handed out, so creation times stay strictly increasing across restarts.
pub const META_LAST_CREATION_TIME: &str = "last_creation_time";

fn apply_pragmas(conn: &Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// How long opening a connection retries while the database is busy (as `busy_timeout`).
const OPEN_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `open` again while it fails with `SQLITE_BUSY`. Opening runs the auto-extensions (sqlite-vec) before a busy
/// timeout can be set, and fails at once with `SQLITE_BUSY_RECOVERY` if another connection is closing the database
/// at that moment (the last one to close checkpoints the WAL under an exclusive lock), e.g. a background thread of a
/// `Database` that was just dropped.
fn open_retrying(mut open: impl FnMut() -> rusqlite::Result<Connection>) -> rusqlite::Result<Connection> {
    let deadline = std::time::Instant::now() + OPEN_BUSY_TIMEOUT;
    loop {
        match open() {
            Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
                && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

/// Open the connection the checkpoint thread uses (`super::checkpoint`); the database exists already.
pub fn open_checkpointer(path: &Path) -> anyhow::Result<Connection> {
    let conn = open_retrying(|| {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(conn)
    })
    .with_context(|| format!("opening {} for checkpoints", path.display()))?;
    apply_pragmas(&conn)?;
    Ok(conn)
}

/// Open (creating if needed) the read-write connection and ensure the schema exists.
pub fn open_writer(path: &Path) -> anyhow::Result<Connection> {
    let conn = open_retrying(|| {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(conn)
    })
    .with_context(|| format!("opening {}", path.display()))?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    anyhow::ensure!(mode.eq_ignore_ascii_case("wal"), "could not enable WAL mode (got {mode})");
    apply_pragmas(&conn)?;
    // Group commit runs each mutation in a savepoint, whose journal (the original of every page the savepoint
    // changes) spills to a temporary file past 64 KiB by default: keep temporary files in memory.
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // Checkpoints run on a thread of their own, not inside this connection's commits (`super::checkpoint`).
    conn.pragma_update(None, "wal_autocheckpoint", 0)?;
    conn.pragma_update(None, "journal_size_limit", super::checkpoint::WAL_SIZE_LIMIT)?;
    conn.execute_batch(DDL)?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(conn)
}

/// Open a read-only connection. The writer must have created the database first.
pub fn open_reader(path: &Path) -> anyhow::Result<Connection> {
    let conn = open_retrying(|| {
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
        )
    })
    .with_context(|| format!("opening reader for {}", path.display()))?;
    apply_pragmas(&conn)?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(conn)
}

pub fn read_meta_u64(conn: &Connection, key: &str) -> anyhow::Result<u64> {
    let v: Option<i64> =
        conn.prepare_cached("SELECT v FROM _meta WHERE k = ?1")?.query_row([key], |r| r.get(0)).optional()?.flatten();
    Ok(v.unwrap_or(0) as u64)
}

pub fn read_meta_f64(conn: &Connection, key: &str) -> anyhow::Result<f64> {
    let v: Option<f64> =
        conn.prepare_cached("SELECT v FROM _meta WHERE k = ?1")?.query_row([key], |r| r.get(0)).optional()?.flatten();
    Ok(v.unwrap_or(0.0))
}

pub fn write_meta(conn: &Connection, key: &str, value: rusqlite::types::Value) -> anyhow::Result<()> {
    conn.prepare_cached("INSERT INTO _meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v")?
        .execute(params![key, value])?;
    Ok(())
}

pub fn get_document(conn: &Connection, id: &[u8; 16]) -> anyhow::Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT json FROM documents WHERE id = ?1")?
        .query_row([&id[..]], |r| r.get(0))
        .optional()?)
}

/// A statement without parameters or results (`BEGIN`, `COMMIT`), from the statement cache: `execute_batch` would
/// compile it again on every call, a measurable share of a small mutation.
pub fn exec_cached(conn: &Connection, sql: &str) -> anyhow::Result<()> {
    conn.prepare_cached(sql)?.execute([])?;
    Ok(())
}

/// Store a new document. Fails (a constraint error) if `id` exists, so a colliding id can never overwrite one.
pub fn insert_document(conn: &Connection, id: &[u8; 16], table_number: u32, json: &str, ts: u64) -> anyhow::Result<()> {
    conn.prepare_cached("INSERT INTO documents (id, table_number, json, ts) VALUES (?1, ?2, ?3, ?4)")?
        .execute(params![&id[..], table_number, json, ts as i64])
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                anyhow::anyhow!("id collision: document {} already exists", data_encoding::HEXLOWER.encode(id))
            }
            e => e.into(),
        })?;
    Ok(())
}

pub fn put_document(conn: &Connection, id: &[u8; 16], table_number: u32, json: &str, ts: u64) -> anyhow::Result<()> {
    conn.prepare_cached(
        "INSERT INTO documents (id, table_number, json, ts) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET json = excluded.json, ts = excluded.ts",
    )?
    .execute(params![&id[..], table_number, json, ts as i64])?;
    Ok(())
}

pub fn delete_document(conn: &Connection, id: &[u8; 16]) -> anyhow::Result<()> {
    conn.prepare_cached("DELETE FROM documents WHERE id = ?1")?.execute([&id[..]])?;
    Ok(())
}

pub fn insert_index_entry(conn: &Connection, index_id: IndexId, key: &[u8], doc_id: &[u8; 16]) -> anyhow::Result<()> {
    conn.prepare_cached("INSERT INTO index_entries (index_id, key, doc_id) VALUES (?1, ?2, ?3)")?
        .execute(params![index_id, key, &doc_id[..]])?;
    Ok(())
}

pub fn delete_index_entry(conn: &Connection, index_id: IndexId, key: &[u8]) -> anyhow::Result<()> {
    conn.prepare_cached("DELETE FROM index_entries WHERE index_id = ?1 AND key = ?2")?
        .execute(params![index_id, key])?;
    Ok(())
}

/// All documents of a table as `(id, json)`. Ids start with the big-endian table number, so a table is a contiguous
/// range of the `documents` primary key.
pub fn table_documents(conn: &Connection, table_number: u32) -> anyhow::Result<Vec<([u8; 16], String)>> {
    let start = table_number.to_be_bytes().to_vec();
    let end = crate::keys::prefix_successor(&start);
    let mut out = Vec::new();
    let mut stmt;
    let mut rows = match &end {
        Some(end) => {
            stmt = conn.prepare("SELECT id, json FROM documents WHERE id >= ?1 AND id < ?2 ORDER BY id")?;
            stmt.query(params![start, end])?
        }
        None => {
            stmt = conn.prepare("SELECT id, json FROM documents WHERE id >= ?1 ORDER BY id")?;
            stmt.query(params![start])?
        }
    };
    while let Some(row) = rows.next()? {
        let id: Vec<u8> = row.get(0)?;
        let id: [u8; 16] = id.try_into().map_err(|_| anyhow::anyhow!("corrupt document id"))?;
        out.push((id, row.get(1)?));
    }
    Ok(out)
}

/// The number of documents in a table (a range count over the `documents` primary key; see [`table_documents`]).
pub fn count_documents(conn: &Connection, table_number: u32) -> anyhow::Result<u64> {
    let start = table_number.to_be_bytes().to_vec();
    let n: i64 = match crate::keys::prefix_successor(&start) {
        Some(end) => conn
            .prepare_cached("SELECT COUNT(*) FROM documents WHERE id >= ?1 AND id < ?2")?
            .query_row(params![start, end], |r| r.get(0))?,
        None => conn.prepare_cached("SELECT COUNT(*) FROM documents WHERE id >= ?1")?.query_row(params![start], |r| r.get(0))?,
    };
    Ok(n as u64)
}

/// The number of entries of one index in `range` (a range count over the `index_entries` primary key; documents
/// are not read).
pub fn count_index_range(conn: &Connection, index_id: IndexId, range: &KeyRange) -> anyhow::Result<u64> {
    let n: i64 = match &range.end {
        Some(end) => conn
            .prepare_cached("SELECT COUNT(*) FROM index_entries WHERE index_id = ?1 AND key >= ?2 AND key < ?3")?
            .query_row(params![index_id, range.start, end], |r| r.get(0))?,
        None => conn
            .prepare_cached("SELECT COUNT(*) FROM index_entries WHERE index_id = ?1 AND key >= ?2")?
            .query_row(params![index_id, range.start], |r| r.get(0))?,
    };
    Ok(n as u64)
}

/// One batch of an index scan: up to `limit` `(key, json)` rows of `range` in `order`, continuing strictly after
/// (Asc) or before (Desc) `cursor` if given.
pub fn scan_index_batch(
    conn: &Connection,
    index_id: IndexId,
    range: &KeyRange,
    order: Order,
    cursor: Option<&[u8]>,
    limit: usize,
) -> anyhow::Result<Vec<(Vec<u8>, String)>> {
    // The effective bounds: the cursor replaces the bound on the side the scan has already consumed.
    let (lower, lower_inclusive, upper) = match (order, cursor) {
        (Order::Asc, Some(c)) => (c, false, range.end.as_deref()),
        (Order::Desc, Some(c)) => (range.start.as_slice(), true, Some(c)),
        (_, None) => (range.start.as_slice(), true, range.end.as_deref()),
    };
    let lower_op = if lower_inclusive { ">=" } else { ">" };
    let dir = match order {
        Order::Asc => "ASC",
        Order::Desc => "DESC",
    };
    let upper_clause = if upper.is_some() { "AND e.key < ?3" } else { "" };
    let sql = format!(
        "SELECT e.key, d.json FROM index_entries e JOIN documents d ON d.id = e.doc_id
         WHERE e.index_id = ?1 AND e.key {lower_op} ?2 {upper_clause}
         ORDER BY e.key {dir} LIMIT ?4"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = match upper {
        Some(u) => stmt.query(params![index_id, lower, u, limit as i64])?,
        // ?3 is unused but numbered parameters must all be bound.
        None => stmt.query(params![index_id, lower, rusqlite::types::Null, limit as i64])?,
    };
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?));
    }
    Ok(out)
}

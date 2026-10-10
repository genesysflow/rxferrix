//! The change journal: the document-level changes of every commit, for continuous replication and point-in-time
//! restores (`crate::ops::replicate`).
//!
//! While journaling is on ([`Journal::set_enabled`]; `serve --replicate-to` turns it on before the database serves
//! anything), every write transaction that commits documents also inserts one row into the raw SQLite table
//! `_journal`, **inside the same SQLite transaction**, right before `COMMIT`:
//!
//! ```sql
//! CREATE TABLE _journal (ts INTEGER PRIMARY KEY, time REAL NOT NULL, changes TEXT NOT NULL);
//! ```
//!
//! `ts` is the commit ts, `time` the commit's wall-clock ms (`_meta.time`), and `changes` a JSON array of
//! `[table, id, document | null]`: the final state of every document the transaction wrote (`null`: deleted), in the
//! order they were first written, user and system tables alike. A document inserted and deleted by the same
//! transaction is left out. Derived data (index entries, full-text and vector tables, HNSW files) is not journaled:
//! replaying the changes through the write path rebuilds it.
//!
//! **Exactly once.** The row commits or rolls back with the transaction's own writes and its `_meta.ts` bump, so the
//! journal holds every committed ts exactly once (ts values are consecutive) and nothing that was rolled back.
//! Whether a transaction is journaled is decided when it begins, under the writer lock, and the flag only changes
//! under that lock, so a transaction is never half journaled.
//!
//! The replicator reads rows on a reader snapshot (each row is a whole commit), ships them, and deletes them
//! ([`trim`]) under the writer lock once the upload succeeded, recording the shipped ts in `_meta` in the same
//! transaction.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context;
use rusqlite::{Connection, params};
use serde_json::{Map, Value};

use crate::id::DocId;

pub(crate) const DDL: &str = "CREATE TABLE IF NOT EXISTS _journal (ts INTEGER PRIMARY KEY, time REAL NOT NULL, changes TEXT NOT NULL);";

/// `_meta` key: the replication generation this database ships to (`crate::ops::replicate`).
pub const META_GENERATION: &str = "replication_generation";
/// `_meta` key: the newest ts whose journal row is stored in the replication target (rows up to it are trimmed).
pub const META_SHIPPED_TS: &str = "replication_shipped_ts";
/// `_meta` key: the commit time (ms) of [`META_SHIPPED_TS`].
pub const META_SHIPPED_TIME: &str = "replication_shipped_time";

/// The journaling switch and the size of the unshipped rows. Lives in the `Database`.
#[derive(Debug, Default)]
pub struct Journal {
    enabled: AtomicBool,
    /// Bytes of `changes` in `_journal` (approximate: set when replication starts, then kept up to date).
    pending_bytes: AtomicU64,
}

impl Journal {
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Only change it while holding the writer lock (`Database::with_writer`), so no transaction is in flight.
    pub(crate) fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    pub fn pending_bytes(&self) -> u64 {
        self.pending_bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn set_pending_bytes(&self, bytes: u64) {
        self.pending_bytes.store(bytes, Ordering::Relaxed);
    }

    pub(crate) fn add_pending_bytes(&self, bytes: u64) {
        self.pending_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn sub_pending_bytes(&self, bytes: u64) {
        let _ = self.pending_bytes.try_update(Ordering::Relaxed, Ordering::Relaxed, |b| Some(b.saturating_sub(bytes)));
    }
}

/// The changes a write transaction collects while journaling: the final state of each document it wrote.
#[derive(Debug, Default)]
pub(crate) struct Changes {
    /// `(table, id, existed before the transaction, final JSON or None)`, in first-write order.
    entries: Vec<(String, [u8; 16], bool, Option<String>)>,
    by_id: HashMap<[u8; 16], usize>,
}

impl Changes {
    pub(crate) fn record(&mut self, table: &str, id: &[u8; 16], existed: bool, json: Option<String>) {
        match self.by_id.get(id) {
            Some(&i) => self.entries[i].3 = json,
            None => {
                self.by_id.insert(*id, self.entries.len());
                self.entries.push((table.to_string(), *id, existed, json));
            }
        }
    }

    /// The `changes` column: `[[table, id, doc | null], ...]`, without documents created and deleted again.
    pub(crate) fn encode(&self) -> String {
        let mut out = String::with_capacity(64 + self.entries.iter().map(|e| e.3.as_ref().map_or(0, |j| j.len()) + 48).sum::<usize>());
        out.push('[');
        let mut first = true;
        for (table, id, existed, json) in &self.entries {
            if !existed && json.is_none() {
                continue;
            }
            if !first {
                out.push(',');
            }
            first = false;
            out.push('[');
            out.push_str(&Value::String(table.clone()).to_string());
            out.push_str(",\"");
            out.push_str(&DocId(*id).encode());
            out.push_str("\",");
            out.push_str(json.as_deref().unwrap_or("null"));
            out.push(']');
        }
        out.push(']');
        out
    }
}

/// One journal row.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub ts: u64,
    /// The commit's wall-clock ms.
    pub time: f64,
    /// The raw `changes` JSON (see the module docs).
    pub changes: String,
}

/// One change of a [`JournalEntry`].
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub table: String,
    pub id: DocId,
    /// The whole stored document (system fields included), or `None` for a delete.
    pub doc: Option<Map<String, Value>>,
}

/// `[table, id, document | null]` as stored.
type RawChange = (String, String, Option<Map<String, Value>>);

impl JournalEntry {
    pub fn parse_changes(&self) -> anyhow::Result<Vec<Change>> {
        let raw: Vec<RawChange> =
            serde_json::from_str(&self.changes).with_context(|| format!("journal entry ts {}: invalid changes", self.ts))?;
        raw.into_iter()
            .map(|(table, id, doc)| {
                let id = DocId::decode(&id).map_err(|e| anyhow::anyhow!("journal entry ts {}: {e}", self.ts))?;
                Ok(Change { table, id, doc })
            })
            .collect()
    }
}

pub(crate) fn insert(conn: &Connection, ts: u64, time: f64, changes: &str) -> anyhow::Result<()> {
    conn.prepare_cached("INSERT INTO _journal (ts, time, changes) VALUES (?1, ?2, ?3)")?
        .execute(params![ts as i64, time, changes])?;
    Ok(())
}

/// Up to `max_entries` rows with ts > `after`, in ts order, stopping once `max_bytes` of changes are read (at least
/// one row is returned if any exists).
pub fn read_after(conn: &Connection, after: u64, max_entries: usize, max_bytes: usize) -> anyhow::Result<Vec<JournalEntry>> {
    let mut stmt = conn.prepare_cached("SELECT ts, time, changes FROM _journal WHERE ts > ?1 ORDER BY ts LIMIT ?2")?;
    let mut rows = stmt.query(params![after as i64, max_entries as i64])?;
    let mut out = Vec::new();
    let mut bytes = 0;
    while let Some(r) = rows.next()? {
        let entry = JournalEntry { ts: r.get::<_, i64>(0)? as u64, time: r.get(1)?, changes: r.get(2)? };
        bytes += entry.changes.len();
        out.push(entry);
        if bytes >= max_bytes {
            break;
        }
    }
    Ok(out)
}

/// `(oldest ts, newest ts, rows, bytes of changes)` of the journal (`None` when empty).
pub fn stats(conn: &Connection) -> anyhow::Result<Option<(u64, u64, u64, u64)>> {
    let (min, max, n, bytes) = conn.query_row(
        "SELECT MIN(ts), MAX(ts), COUNT(*), SUM(LENGTH(changes)) FROM _journal",
        [],
        |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?)),
    )?;
    Ok(match (min, max) {
        (Some(min), Some(max)) => Some((min as u64, max as u64, n as u64, bytes.unwrap_or(0) as u64)),
        _ => None,
    })
}

/// Delete the rows up to `ts` (inclusive); returns the bytes of changes removed.
pub(crate) fn trim(conn: &Connection, ts: u64) -> anyhow::Result<u64> {
    let bytes: Option<i64> =
        conn.query_row("SELECT SUM(LENGTH(changes)) FROM _journal WHERE ts <= ?1", [ts as i64], |r| r.get(0))?;
    conn.execute("DELETE FROM _journal WHERE ts <= ?1", [ts as i64])?;
    Ok(bytes.unwrap_or(0) as u64)
}

/// Remove every replication trace from a database file that is not open elsewhere (a backup's copy, a restored
/// database): the journal rows and the generation and shipped ts in `_meta`. A restored database then starts a new
/// replication generation instead of continuing (and contradicting) the one it was copied from.
pub fn scrub(conn: &Connection) -> anyhow::Result<()> {
    let has_journal: bool =
        conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_journal')", [], |r| r.get(0))?;
    if has_journal {
        conn.execute("DELETE FROM _journal", [])?;
    }
    let has_meta: bool =
        conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_meta')", [], |r| r.get(0))?;
    if has_meta {
        conn.execute(
            "DELETE FROM _meta WHERE k IN (?1, ?2, ?3)",
            params![META_GENERATION, META_SHIPPED_TS, META_SHIPPED_TIME],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_keep_the_final_state_and_drop_transient_documents() {
        let (a, b, c) = (DocId::new_random(3), DocId::new_random(3), DocId::new_random(4));
        let mut ch = Changes::default();
        ch.record("t", &a.0, false, Some(r#"{"x":1}"#.into()));
        ch.record("t", &b.0, true, Some(r#"{"x":2}"#.into()));
        ch.record("u\"q", &c.0, false, Some(r#"{"y":1}"#.into()));
        ch.record("t", &a.0, false, Some(r#"{"x":3}"#.into()));
        ch.record("u\"q", &c.0, false, None); // created and deleted: dropped
        ch.record("t", &b.0, true, None); // existed: a delete
        let entry = JournalEntry { ts: 7, time: 1.5, changes: ch.encode() };
        let parsed = entry.parse_changes().unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!((parsed[0].table.as_str(), parsed[0].id), ("t", a));
        assert_eq!(parsed[0].doc.as_ref().unwrap()["x"], 3);
        assert_eq!((parsed[1].id, parsed[1].doc.is_none()), (b, true));
        assert_eq!(Changes::default().encode(), "[]");
    }
}

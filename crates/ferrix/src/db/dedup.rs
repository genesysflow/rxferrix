//! Mutation idempotency: `(session_id, request_id) → response`, in a raw SQLite table of `data.db`. The stored
//! `session_id` is scoped to the caller's identity ([`IdempotencyKey::scoped_to`]), so one identity can never be
//! handed another identity's recorded response.
//!
//! The TS client resends unacknowledged mutations after a reconnect with the same `(sessionId, requestId)`. The
//! runtime looks the key up *inside* the mutation's write transaction (so a concurrent duplicate waits for the writer
//! lock and then finds the first one's row), and records the response in the same transaction as the mutation's
//! writes. A mutation is therefore applied at most once per key, and its recorded response is exactly what was
//! committed. A failed mutation is rolled back; its error is then recorded while the writer lock is still held, so
//! a retry gets the same error instead of a second attempt.
//!
//! Rows older than [`RETENTION`] are deleted whenever a row is recorded (an index seek; nearly always a no-op).

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ClientError, ErrorKind, FunctionNotFound, InvalidArgs};
use crate::{Error, UserError};

/// How long responses are kept.
pub const RETENTION: Duration = Duration::from_secs(10 * 60);

const DDL: &str = "
CREATE TABLE IF NOT EXISTS _mutation_dedup (
    session_id TEXT NOT NULL,
    request_id INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    response TEXT NOT NULL,
    PRIMARY KEY (session_id, request_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS _mutation_dedup_by_created ON _mutation_dedup (created_ms);
";

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
        IdempotencyKey { session_id: format!("{}|{}", identity.principal_key(), self.session_id), request_id: self.request_id }
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

pub(crate) fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(DDL)?;
    Ok(())
}

pub(crate) fn lookup(conn: &Connection, key: &IdempotencyKey) -> anyhow::Result<Option<StoredResponse>> {
    let json: Option<String> = conn
        .prepare_cached("SELECT response FROM _mutation_dedup WHERE session_id = ?1 AND request_id = ?2")?
        .query_row(params![key.session_id, key.request_id as i64], |r| r.get(0))
        .optional()?;
    Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
}

/// Record a successful response (inside the mutation's transaction).
pub(crate) fn record_success(conn: &Connection, key: &IdempotencyKey, value: &Value, ts: u64) -> anyhow::Result<()> {
    // Same JSON as `StoredResponse::Success`, without cloning the value.
    #[derive(Serialize)]
    #[serde(tag = "status", rename_all = "camelCase")]
    enum Borrowed<'a> {
        Success { value: &'a Value, ts: u64 },
    }
    record(conn, key, &serde_json::to_string(&Borrowed::Success { value, ts })?)
}

/// Record a failure (after the rollback, in autocommit mode, still holding the writer lock).
pub(crate) fn record_error(conn: &Connection, key: &IdempotencyKey, error: &Error) -> anyhow::Result<()> {
    record(conn, key, &serde_json::to_string(&StoredResponse::from_error(error))?)
}

/// Insert a response and drop expired rows.
fn record(conn: &Connection, key: &IdempotencyKey, response_json: &str) -> anyhow::Result<()> {
    let now = crate::db::wall_clock_ms() as i64;
    conn.prepare_cached("DELETE FROM _mutation_dedup WHERE created_ms < ?1")?
        .execute([now - RETENTION.as_millis() as i64])?;
    conn.prepare_cached(
        "INSERT OR REPLACE INTO _mutation_dedup (session_id, request_id, created_ms, response) VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![key.session_id, key.request_id as i64, now, response_json])?;
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

    #[test]
    fn record_lookup_and_expiry() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let key = IdempotencyKey { session_id: "s".into(), request_id: 3 };
        assert_eq!(lookup(&conn, &key).unwrap(), None);
        record_success(&conn, &key, &serde_json::json!("id"), 7).unwrap();
        let ok = StoredResponse::Success { value: serde_json::json!("id"), ts: 7 };
        assert_eq!(lookup(&conn, &key).unwrap(), Some(ok));
        let other = IdempotencyKey { session_id: "t".into(), request_id: 3 };
        assert_eq!(lookup(&conn, &other).unwrap(), None);
        // Expired rows are removed on the next record.
        conn.execute("UPDATE _mutation_dedup SET created_ms = 0", []).unwrap();
        record_error(&conn, &other, &UserError::new("c", "m").into()).unwrap();
        assert_eq!(lookup(&conn, &key).unwrap(), None);
        let stored = lookup(&conn, &other).unwrap().unwrap();
        let replayed = stored.into_result().unwrap_err();
        assert_eq!(replayed.downcast_ref::<UserError>(), Some(&UserError::new("c", "m")));
    }
}

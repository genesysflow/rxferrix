//! While a batch is open on the writer connection, nothing commits there except the batch.
//!
//! SQLite ends a transaction by itself on some errors (a full disk, an I/O error, out of memory). The statements
//! that follow on the connection would then each commit on their own, and a `SAVEPOINT` would begin a transaction
//! that its `RELEASE` commits: a mutation that doesn't stop at the error (or the next one, continuing the batch) would
//! keep some of its writes without the rest, outside the batch, and without the batch's `_meta` values. Measured
//! with `INSERT OR ROLLBACK` standing in for the error (the tests of `crate::db`): documents and journal rows were
//! kept at a ts `_meta` never reached, while their mutations were told that the commit had failed.
//!
//! So the writer connection has two hooks, both reading a [`BatchGuard`]:
//!
//! * the rollback hook notes that the batch's transaction is gone ([`BatchGuard::lost`]) the moment SQLite ends it;
//! * the commit hook refuses every commit while a batch is open (SQLite turns it into a rollback and the statement
//!   fails), which covers statements run in autocommit mode and the `RELEASE` of a savepoint that began a
//!   transaction alike. The batch's own `COMMIT` runs after the batch is closed ([`BatchGuard::closed`]).
//!
//! A refused `RELEASE` leaves its (now empty) transaction open, so "is a transaction open" doesn't tell whether
//! the batch's still is: `lost` stays set until the next batch opens, and a batch that lost its transaction fails
//! as a whole (`WriterShared::commit_batch`).
//!
//! Checked against the bundled SQLite (2026-11-24): the commit hook runs for a statement in autocommit mode that
//! writes, for `COMMIT` and for the `RELEASE` of an outermost savepoint with writes; not for reads, for the release of
//! a nested savepoint or for a checkpoint. The rollback hook runs when SQLite ends a transaction on an error and for
//! `ROLLBACK`; not for `ROLLBACK TO`, nor for a statement that fails inside a transaction and leaves it open.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{Connection, ffi};

#[derive(Default)]
pub(crate) struct BatchGuard {
    /// A batch is open: its transaction should be, and nothing may commit.
    open: AtomicBool,
    /// The transaction of the batch that is open (or was last) ended before the batch did.
    lost: AtomicBool,
}

impl BatchGuard {
    /// A batch begins, right after its `BEGIN`.
    pub(crate) fn opened(&self) {
        self.lost.store(false, Ordering::Release);
        self.open.store(true, Ordering::Release);
    }

    /// The batch ends: what follows (its `COMMIT`, or a `ROLLBACK`) is meant.
    pub(crate) fn closed(&self) {
        self.open.store(false, Ordering::Release);
    }

    /// The batch's transaction ended before the batch did: nothing of the batch can be kept.
    pub(crate) fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    fn end_of_transaction(&self) -> bool {
        let open = self.open.load(Ordering::Acquire);
        if open {
            self.lost.store(true, Ordering::Release);
        }
        open
    }
}

/// Install the two hooks on `conn`, reading `guard` (which must outlive the connection, or [`remove`] them first).
pub(crate) fn install(conn: &Connection, guard: &Arc<BatchGuard>) {
    unsafe extern "C" fn commit(data: *mut std::ffi::c_void) -> std::ffi::c_int {
        // SAFETY: `data` is the `BatchGuard` passed below, alive as long as the hook (the caller's contract).
        let guard = unsafe { &*(data as *const BatchGuard) };
        // Non-zero: the commit becomes a rollback.
        guard.end_of_transaction() as std::ffi::c_int
    }
    unsafe extern "C" fn rollback(data: *mut std::ffi::c_void) {
        // SAFETY: as above.
        let guard = unsafe { &*(data as *const BatchGuard) };
        guard.end_of_transaction();
    }
    let data = Arc::as_ptr(guard) as *mut std::ffi::c_void;
    // SAFETY: a valid open connection; the pointer stays valid while the hooks are registered (see above).
    unsafe {
        ffi::sqlite3_commit_hook(conn.handle(), Some(commit), data);
        ffi::sqlite3_rollback_hook(conn.handle(), Some(rollback), data);
    }
}

/// Remove the hooks [`install`] set.
pub(crate) fn remove(conn: &Connection) {
    // SAFETY: a valid open connection.
    unsafe {
        ffi::sqlite3_commit_hook(conn.handle(), None, std::ptr::null_mut());
        ffi::sqlite3_rollback_hook(conn.handle(), None, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM d", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn nothing_commits_while_a_batch_is_open_and_a_lost_transaction_is_noted() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE d (x); CREATE TABLE u (k PRIMARY KEY); INSERT INTO u VALUES (1);").unwrap();
        let guard = Arc::new(BatchGuard::default());
        install(&conn, &guard);

        // A batch as usual: savepoints, a statement that fails, a savepoint rolled back, then its commit.
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        guard.opened();
        conn.execute_batch("INSERT INTO d VALUES (1); SAVEPOINT m; INSERT INTO d VALUES (2); RELEASE m;").unwrap();
        assert!(conn.execute("INSERT INTO u VALUES (1)", []).is_err());
        conn.execute_batch("SAVEPOINT m; INSERT INTO d VALUES (3); ROLLBACK TO m; RELEASE m;").unwrap();
        assert!(!guard.lost());
        guard.closed();
        conn.execute_batch("COMMIT").unwrap();
        assert!(!guard.lost());
        assert_eq!(rows(&conn), 2);

        // SQLite ends the transaction: noted at once; what follows doesn't commit.
        conn.execute_batch("BEGIN IMMEDIATE; INSERT INTO d VALUES (4);").unwrap();
        guard.opened();
        assert!(conn.execute("INSERT OR ROLLBACK INTO u VALUES (1)", []).is_err());
        assert!(guard.lost() && conn.is_autocommit());
        assert!(conn.execute("INSERT INTO d VALUES (5)", []).is_err());
        conn.execute_batch("SAVEPOINT m; INSERT INTO d VALUES (6);").unwrap();
        assert!(conn.execute_batch("RELEASE m").is_err());
        // The refused release left a transaction open (without the write): what is written in it doesn't commit
        // either.
        assert!(!conn.is_autocommit());
        conn.execute("INSERT INTO d VALUES (6)", []).unwrap();
        assert!(conn.execute_batch("COMMIT").is_err());
        // Reads still work.
        assert_eq!(rows(&conn), 2);
        guard.closed();
        if !conn.is_autocommit() {
            conn.execute_batch("ROLLBACK").unwrap();
        }
        assert!(guard.lost(), "until the next batch opens");
        assert_eq!(rows(&conn), 2);

        // A rollback that is meant (after the batch closed) is not a loss, and the next batch starts clean.
        conn.execute_batch("BEGIN IMMEDIATE; INSERT INTO d VALUES (7);").unwrap();
        guard.opened();
        assert!(!guard.lost());
        guard.closed();
        conn.execute_batch("ROLLBACK").unwrap();
        assert!(!guard.lost());
        // With no batch open, the connection commits as it likes.
        conn.execute("INSERT INTO d VALUES (8)", []).unwrap();
        assert_eq!(rows(&conn), 3);
        remove(&conn);
    }
}

//! WAL checkpoints off the commit path.
//!
//! SQLite's automatic checkpoint runs inside the commit that pushes the WAL past 1000 pages, on the writer connection,
//! and flushes the WAL and the database file to disk while holding the writer: every write waits for it. Measured on
//! an NVMe SSD (btrfs), that was most of the cost of a commit (one writer: 5.8k mutations/s with it, 7.8k without).
//! So the writer runs with `wal_autocheckpoint = 0`, and this thread checkpoints instead, on a connection of its own:
//! the writer's WAL hook nudges it after a commit that leaves [`CHECKPOINT_PAGES`] or more pages in the WAL (the
//! threshold SQLite's own automatic checkpoint uses), and it runs a `PASSIVE` checkpoint (which never blocks writers or
//! readers), at most every [`MIN_INTERVAL`]. The writer's `journal_size_limit` shrinks the WAL file
//! after it restarts, so a burst doesn't leave a large file behind.
//!
//! A checkpoint racing the writer never quite finishes (frames keep arriving while it copies), and SQLite only starts
//! the WAL over once one has: alone, the background thread would let the WAL grow without bound under constant
//! writes. So once the WAL passes [`RESTART_PAGES`] and the thread has copied what it could, it asks the writer to
//! *finish*: right before its next transaction, while no write is in progress, the writer runs a `PASSIVE` checkpoint
//! itself, which only has the few frames committed since to copy, completes, and lets that transaction restart the
//! WAL ([`finish_if_asked`]). A `PASSIVE` checkpoint that finds another one running reports it in the pragma's row
//! (`busy`), not as an error: the writer then keeps the request for its next transaction, and the thread starts no new
//! checkpoint while a request waits, so the two don't keep colliding (on a slow disk they did, and the WAL grew past
//! 200 MiB).
//!
//! Durability is unchanged: a commit is durable once its WAL frames are (on disk with `synchronous = FULL`, in the OS
//! with `NORMAL`); checkpoints only move committed frames into the database file.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ffi};

/// The least time between two checkpoints: commits in between are checkpointed together.
pub const MIN_INTERVAL: Duration = Duration::from_millis(50);

/// A commit that leaves at least this many pages in the WAL wakes the checkpoint thread.
pub const CHECKPOINT_PAGES: std::ffi::c_int = 1000;

/// Past this many pages in the WAL (64 MiB of 4 KiB pages), the writer finishes a checkpoint so the WAL restarts (see
/// the module docs). Measured on btrfs, restarting 4x as often cost 10-20% of the throughput and tripled p99 latency
/// at 64 writers: each restart writes over the start of the file again.
pub const RESTART_PAGES: std::ffi::c_int = 16 * CHECKPOINT_PAGES;

/// The WAL file is truncated to at most this size when it restarts (the writer's `journal_size_limit`).
pub const WAL_SIZE_LIMIT: i64 = 128 << 20;

#[derive(Default)]
struct State {
    /// Commits since the last checkpoint started.
    pending: bool,
    stop: bool,
}

#[derive(Default)]
pub(crate) struct Signal {
    state: Mutex<State>,
    wake: Condvar,
    /// Mirrors `state.pending`, so a commit only takes the lock when the flag goes up.
    raised: AtomicBool,
    /// Pages in the WAL after the latest commit.
    pages: std::sync::atomic::AtomicI32,
    /// The writer should finish a checkpoint before its next transaction.
    finish: AtomicBool,
}

/// Make every commit of `conn` report the WAL's size to `signal` (which must outlive the connection), nudging it past
/// [`CHECKPOINT_PAGES`]. Replaces SQLite's automatic checkpoint, which uses the same hook: set
/// `wal_autocheckpoint = 0` first.
pub(crate) fn install_hook(conn: &Connection, signal: &Arc<Signal>) {
    unsafe extern "C" fn hook(
        data: *mut std::ffi::c_void,
        _db: *mut ffi::sqlite3,
        _name: *const std::ffi::c_char,
        pages: std::ffi::c_int,
    ) -> std::ffi::c_int {
        // SAFETY: `data` is the `Signal` passed below, alive as long as the connection (the caller's contract).
        let signal = unsafe { &*(data as *const Signal) };
        signal.pages.store(pages, Ordering::Relaxed);
        if pages >= CHECKPOINT_PAGES {
            signal.nudge();
        }
        ffi::SQLITE_OK
    }
    // SAFETY: a valid open connection; the pointer stays valid while it is registered (see above).
    unsafe {
        ffi::sqlite3_wal_hook(conn.handle(), Some(hook), Arc::as_ptr(signal) as *mut std::ffi::c_void);
    }
}

/// Remove the hook [`install_hook`] set.
pub(crate) fn remove_hook(conn: &Connection) {
    // SAFETY: a valid open connection.
    unsafe {
        ffi::sqlite3_wal_hook(conn.handle(), None, std::ptr::null_mut());
    }
}

/// For the writer, before it begins a transaction: if the thread asked, run a `PASSIVE` checkpoint on the writer's
/// connection, which completes with no write in progress and so lets the next transaction restart the WAL (see the
/// module docs). Busy (the thread's own checkpoint still runs): the request stays for the next transaction.
pub(crate) fn finish_if_asked(conn: &Connection, signal: &Signal) {
    if !(signal.finish.load(Ordering::Relaxed) && signal.finish.swap(false, Ordering::AcqRel)) {
        return;
    }
    match conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get::<_, i64>(0)) {
        Ok(0) => {}
        Ok(_busy) => signal.finish.store(true, Ordering::Release),
        Err(e) => tracing::warn!("finishing a WAL checkpoint failed: {e}"),
    }
}

impl Signal {
    /// The WAL has grown: checkpoint soon.
    pub(crate) fn nudge(&self) {
        if !self.raised.swap(true, Ordering::AcqRel) {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).pending = true;
            self.wake.notify_one();
        }
    }
}

/// The checkpoint thread. Stops (and closes its connection) when dropped.
pub(crate) struct Checkpointer {
    signal: Arc<Signal>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Checkpointer {
    pub(crate) fn start(path: &Path) -> anyhow::Result<Checkpointer> {
        let conn = super::storage::open_checkpointer(path)?;
        let signal = Arc::new(Signal::default());
        let thread = std::thread::Builder::new().name("ferrix-checkpoint".into()).spawn({
            let signal = signal.clone();
            move || run(conn, &signal)
        })?;
        Ok(Checkpointer { signal, thread: Some(thread) })
    }

    pub(crate) fn signal(&self) -> &Arc<Signal> {
        &self.signal
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        self.signal.state.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.signal.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(conn: Connection, signal: &Signal) {
    let mut last = Instant::now() - MIN_INTERVAL;
    let mut last_error: Option<Instant> = None;
    loop {
        {
            let mut state = signal.state.lock().unwrap_or_else(|e| e.into_inner());
            while !state.pending && !state.stop {
                state = signal.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.stop {
                return;
            }
        }
        // Let commits that follow closely join this checkpoint.
        if let Some(wait) = MIN_INTERVAL.checked_sub(last.elapsed()) {
            std::thread::sleep(wait);
        }
        {
            let mut state = signal.state.lock().unwrap_or_else(|e| e.into_inner());
            state.pending = false;
            signal.raised.store(false, Ordering::Release);
        }
        if signal.finish.load(Ordering::Acquire) {
            // The writer hasn't finished the last one yet: a checkpoint now would only hold the lock it needs.
            continue;
        }
        last = Instant::now();
        let checkpointed = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        if checkpointed.is_ok() && signal.pages.load(Ordering::Relaxed) >= RESTART_PAGES {
            // Copied what it could; the writer finishes, and the WAL restarts.
            signal.finish.store(true, Ordering::Release);
        }
        if let Err(e) = checkpointed {
            // A checkpoint that fails is retried with the next commit; say so at most once a minute.
            if last_error.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
                tracing::warn!("WAL checkpoint failed (retried after the next commit): {e}");
                last_error = Some(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn
    }

    /// The writer's finishing checkpoint, colliding with another checkpoint, keeps the request instead of dropping it
    /// (the pragma reports busy in its row, not as an error).
    #[test]
    fn a_busy_finish_is_retried_with_the_next_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let writer = open(&path);
        writer.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);").unwrap();
        // A reader keeps a snapshot open; a TRUNCATE checkpoint waits for it, holding the checkpoint lock meanwhile.
        let reader = open(&path);
        reader.execute_batch("BEGIN; SELECT count(*) FROM t;").unwrap();
        writer.execute("INSERT INTO t VALUES (2)", []).unwrap();
        let holder = std::thread::spawn({
            let path = path.clone();
            move || {
                let conn = open(&path);
                conn.busy_timeout(Duration::from_millis(600)).unwrap();
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
            }
        });
        std::thread::sleep(Duration::from_millis(200));

        let signal = Signal::default();
        signal.finish.store(true, Ordering::Release);
        finish_if_asked(&writer, &signal);
        assert!(signal.finish.load(Ordering::Acquire), "busy: asked again for the next transaction");

        reader.execute_batch("COMMIT").unwrap();
        holder.join().unwrap();
        finish_if_asked(&writer, &signal);
        assert!(!signal.finish.load(Ordering::Acquire), "done once nothing else checkpoints");
    }
}

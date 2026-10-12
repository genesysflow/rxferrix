//! Zero-downtime deploys: `serve --takeover` starts the new version of a server next to the running one, on the same
//! data directory, and takes over without refusing a connection or losing a write. If the new version can't serve
//! the data (a document that doesn't match a changed type and no migration for it, a failing index build), it gives
//! up before touching anything and the running server never notices.
//!
//! **Control socket.** Every `serve` listens on `<dir>/ferrix.control` (a Unix socket, mode 0600) for a takeover.
//! The protocol is newline-delimited JSON:
//!
//! 1. `{"op":"prepare"}` → `{"ok":true,"ts":<ts>,"pid":<pid>,"roles":["main",…]}`: the running server turns its change
//!    journal on (`crate::db::journal`, consumer `Takeover`, from ts `ts`), so every commit from then on can be
//!    replayed elsewhere.
//! 2. The new process copies the database (`VACUUM INTO <dir>/takeover/data.db`, a consistent snapshot that doesn't
//!    block writers) and opens the copy with its own tables: catalog reconcile (new and changed indexes, search and
//!    vector indexes and aggregates are built here), the schema check, migration coverage. Any failure: it closes the
//!    socket, which the running server takes as "abandoned" (journal back off), and exits with the report.
//! 3. It replays the journal rows written since the copy into it (`Database::replay_checked`: each document must
//!    match the new types unless its table is migrating), until it has nearly caught up.
//! 4. `{"op":"handover"}` → the reply carries the listening sockets (`SCM_RIGHTS`). The running server stops
//!    accepting (the sockets stay open in the new process, so connections queue instead of being refused), closes
//!    WebSockets with code 1012 ("service restart": clients reconnect at once), lets requests finish, seals its
//!    database (no write may begin after that), and exits, releasing `<dir>/ferrix.lock`.
//! 5. The new process takes the lock, replays the last journal rows, copies what isn't journaled (the mutation dedup
//!    table, so a retried mutation still runs once; replication and analytics positions; the journal itself for
//!    replication), checks that the copy is at the old database's ts, and swaps it in: the old `data.db` moves to
//!    `<dir>/takeover/previous/`. Then it opens the app with the schema check already done and serves on the
//!    inherited sockets. A crash during the swap is finished on the next start ([`recover`]). If this last step
//!    fails, the process still serves: on the database as the old server left it.
//!
//! Under systemd the new process reports itself as the service's main process (`sd_notify`, `MAINPID=`; the unit
//! needs `NotifyAccess=all`): see README "Deploying".

#[cfg(unix)]
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::db::journal::{self, Consumer};
use crate::db::{Database, OpenOptions, storage};
use crate::ops::DirLock;
use crate::runtime::App;

/// The control socket in the data directory.
pub const CONTROL_SOCKET: &str = "ferrix.control";
/// Where the new process prepares its copy.
pub const STAGING_DIR: &str = "takeover";
/// Present in the staging directory while the copy is being swapped in.
const SWAP_MARKER: &str = "swapping";
/// How long the new process waits for the old one to exit after the handover.
const EXIT_WAIT: Duration = Duration::from_secs(60);
/// Catch up in rounds of this many journal rows; the handover happens once a round finds fewer.
const ROUND: usize = 256;
/// `_meta` keys that follow the data rather than the binary: replication and analytics positions.
const CARRIED_META: [&str; 5] = [
    journal::META_GENERATION,
    journal::META_SHIPPED_TS,
    journal::META_SHIPPED_TIME,
    journal::META_OLAP_TS,
    journal::META_OLAP_ID,
];

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Request {
    /// Is a server here? (`serve --takeover=auto`.) Answered without starting anything.
    Ping,
    Prepare,
    /// `sockets`: the new process takes the listening sockets (same network namespace). Without them (containers),
    /// the old server keeps accepting until it stops, and the new one binds its own.
    Handover {
        #[serde(default = "yes")]
        sockets: bool,
    },
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(default)]
    roles: Vec<String>,
    /// The running server's network namespace (`/proc/self/ns/net` on Linux): a new process in another one (another
    /// container) can't use its sockets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    netns: Option<String>,
    /// The running server's listening addresses, by role.
    #[serde(default)]
    addrs: Vec<(String, std::net::SocketAddr)>,
}

/// Whether a listener bound to `bound` serves `wanted` (same port; same address unless `wanted` is unspecified).
pub(crate) fn serves(bound: std::net::SocketAddr, wanted: std::net::SocketAddr) -> bool {
    bound.port() == wanted.port() && (wanted.ip().is_unspecified() || bound.ip() == wanted.ip())
}

/// This process's network namespace, where the OS says (Linux).
fn netns() -> Option<String> {
    std::fs::read_link("/proc/self/ns/net").ok().map(|p| p.to_string_lossy().into_owned())
}

/// Whether a server answers on `<dir>/ferrix.control` (`serve --takeover=auto` takes over only then). A socket file
/// left by a crashed server doesn't answer.
#[cfg(unix)]
pub async fn server_running(dir: &Path) -> bool {
    let ping = async {
        let stream = tokio::net::UnixStream::connect(dir.join(CONTROL_SOCKET)).await?;
        let mut conn = Conn::new(stream);
        conn.send(&Request::Ping, &[]).await?;
        anyhow::Ok(conn.recv::<Reply>().await?.is_some_and(|r| r.ok))
    };
    tokio::time::timeout(Duration::from_secs(5), ping).await.ok().and_then(|r| r.ok()).unwrap_or(false)
}

#[cfg(not(unix))]
pub async fn server_running(_dir: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------------------------------------------------------
// The running server's side
// ---------------------------------------------------------------------------------------------------------------------

/// The control socket of a running server. Dropping it removes the socket file.
#[cfg(unix)]
pub struct Control {
    path: PathBuf,
    handover: Arc<tokio::sync::Notify>,
    /// The new process took the listening sockets (same network namespace): stop accepting at once.
    sockets_taken: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    /// The new process's connection after the handover: it learns that this server is gone when it closes, i.e.
    /// when this is dropped (at the end of `serve`, after the database is sealed) or the process exits.
    handed_over: Arc<std::sync::Mutex<Option<Conn>>>,
}

#[cfg(unix)]
impl Control {
    /// Listen on `<dir>/ferrix.control`, offering `listeners` (role, fd) at `addrs` (role, address) to a new process.
    pub async fn start(
        app: App,
        dir: &Path,
        listeners: Vec<(String, RawFd)>,
        addrs: Vec<(String, std::net::SocketAddr)>,
    ) -> crate::Result<Control> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(CONTROL_SOCKET);
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("listening on the control socket {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let handover = Arc::new(tokio::sync::Notify::new());
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = handover.clone();
        let sockets_taken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let taken = sockets_taken.clone();
        let handed_over = Arc::new(std::sync::Mutex::new(None));
        let keep = handed_over.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (app, listeners, notify, busy) = (app.clone(), listeners.clone(), notify.clone(), busy.clone());
                let (keep, taken, addrs) = (keep.clone(), taken.clone(), addrs.clone());
                tokio::spawn(async move {
                    match serve_takeover(&app, stream, &listeners, &addrs, &notify, &taken, &busy).await {
                        Ok(Some(conn)) => *keep.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("takeover: {e:#}"),
                    }
                });
            }
        });
        Ok(Control { path, handover, sockets_taken, task, handed_over })
    }

    /// Resolves when a new process has taken over: time to stop.
    pub async fn handover_requested(&self) {
        self.handover.notified().await;
    }

    /// Whether the new process took the listening sockets (then this server stops accepting at once; otherwise, in
    /// another container, it keeps serving until it stops and the new one binds its own).
    pub fn sockets_taken(&self) -> bool {
        self.sockets_taken.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(unix)]
impl Drop for Control {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
        drop(self.handed_over.lock().unwrap_or_else(|e| e.into_inner()).take());
    }
}

/// One takeover, from the running server's side. Returns the connection after a handover (to be kept open while
/// this server stops).
#[cfg(unix)]
async fn serve_takeover(
    app: &App,
    stream: tokio::net::UnixStream,
    listeners: &[(String, RawFd)],
    addrs: &[(String, std::net::SocketAddr)],
    handover: &tokio::sync::Notify,
    sockets_taken: &std::sync::atomic::AtomicBool,
    busy: &std::sync::atomic::AtomicBool,
) -> crate::Result<Option<Conn>> {
    use std::sync::atomic::Ordering;
    let mut conn = Conn::new(stream);
    match conn.recv().await? {
        Some(Request::Ping) => {
            conn.send(&Reply { ok: true, pid: Some(std::process::id()), ..Default::default() }, &[]).await?;
            return Ok(None);
        }
        Some(Request::Prepare) => {}
        None => return Ok(None),
        Some(other) => anyhow::bail!("expected `prepare`, got {other:?}"),
    }
    // One takeover at a time, claimed by `prepare` (a probe doesn't count).
    if busy.swap(true, Ordering::SeqCst) {
        let reply = Reply { error: Some("another takeover is in progress".into()), ..Default::default() };
        conn.send(&reply, &[]).await?;
        return Ok(None);
    }
    struct Release<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let _release = Release(busy);
    let roles: Vec<String> = listeners.iter().map(|(r, _)| r.clone()).collect();
    let db = app.db();
    let was_enabled = db.journal().is_enabled();
    let ts = db
        .with_writer(|c| {
            db.journal().set_enabled(true);
            db.journal().set_consumer(Consumer::Takeover, true);
            let ts = storage::read_meta_u64(c, storage::META_TS)?;
            storage::write_meta(c, journal::META_TAKEOVER_TS, rusqlite::types::Value::Integer(ts as i64))?;
            Ok(ts)
        })
        .await?;
    tracing::info!(ts, "takeover: a new process is preparing to take over; journaling commits for it");
    let reply = Reply {
        ok: true,
        ts: Some(ts),
        pid: Some(std::process::id()),
        roles: roles.clone(),
        error: None,
        netns: netns(),
        addrs: addrs.to_vec(),
    };
    conn.send(&reply, &[]).await?;
    match conn.recv().await {
        Ok(Some(Request::Handover { sockets })) => {
            let fds: Vec<RawFd> = if sockets { listeners.iter().map(|(_, fd)| *fd).collect() } else { Vec::new() };
            let roles = if sockets { roles } else { Vec::new() };
            sockets_taken.store(sockets, std::sync::atomic::Ordering::SeqCst);
            conn.send(&Reply { ok: true, roles, ..Default::default() }, &fds).await?;
            tracing::info!(sockets, "takeover: handing over to the new process");
            handover.notify_one();
            Ok(Some(conn))
        }
        other => {
            if let Err(e) = &other {
                tracing::debug!("takeover connection: {e:#}");
            }
            let removed = db
                .with_writer(|c| {
                    db.journal().set_consumer(Consumer::Takeover, false);
                    if was_enabled || db.journal().others_active(Consumer::Takeover) {
                        return Ok(0);
                    }
                    db.journal().set_enabled(false);
                    journal::trim(c, u64::MAX)
                })
                .await?;
            db.journal().sub_pending_bytes(removed);
            tracing::warn!("takeover: the new process gave up; this server carries on");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The new process's side
// ---------------------------------------------------------------------------------------------------------------------

/// What the new process has once it took over: the data directory's lock, the listening sockets (by role), and the
/// result of the schema check its copy passed (the migrating tables).
pub struct Incoming {
    pub lock: DirLock,
    pub listeners: crate::ops::tls::Inherited,
    /// The migrating tables of the copy that was swapped in (its schema check passed), or `None` if finishing the
    /// takeover failed after the handover: the database is then as the old server left it, and opening it runs the
    /// schema check as usual.
    pub migrating: Option<Vec<String>>,
    /// The process that was serving.
    pub from_pid: u32,
}

/// Take over the server running on `dir` (see the module docs), to listen on `wanted` (role, address): its sockets
/// are taken where they serve those addresses in the same network namespace. Errors before the handover leave it
/// running.
#[cfg(unix)]
pub async fn take_over(dir: &Path, wanted: &[(String, std::net::SocketAddr)]) -> crate::Result<Incoming> {
    take_over_with(dir, wanted, crate::runtime::registered_tables()).await
}

/// [`take_over`] with the tables the new version serves (tests stand in a changed schema this way).
#[cfg(unix)]
pub async fn take_over_with(
    dir: &Path,
    wanted: &[(String, std::net::SocketAddr)],
    tables: Vec<crate::schema::TableDef>,
) -> crate::Result<Incoming> {
    recover(dir)?;
    if crate::ops::restore::pending(dir)?.is_some() {
        anyhow::bail!("a restore is staged on this data directory: restart the server normally to apply it");
    }
    let path = dir.join(CONTROL_SOCKET);
    let stream = tokio::net::UnixStream::connect(&path).await.with_context(|| {
        format!("no running server to take over ({} doesn't answer): start without --takeover", path.display())
    })?;
    let mut conn = Conn::new(stream);
    conn.send(&Request::Prepare, &[]).await?;
    let reply: Reply = conn.recv().await?.context("the running server closed the control socket")?;
    if !reply.ok {
        anyhow::bail!("the running server refused: {}", reply.error.unwrap_or_default());
    }
    let from_pid = reply.pid.unwrap_or(0);
    // In another network namespace (another container or pod), the old server's sockets lead nowhere useful: this
    // process binds its own and the orchestrator moves the traffic.
    let same_netns = match (&reply.netns, netns()) {
        (Some(theirs), Some(ours)) => *theirs == ours,
        _ => true,
    };
    // Its sockets are worth taking only if this process listens on the same addresses.
    let reuse = same_netns
        && wanted.iter().any(|(role, want)| reply.addrs.iter().any(|(r, bound)| r == role && serves(*bound, *want)));
    let started = Instant::now();
    tracing::info!(pid = from_pid, "takeover: copying the database and opening it with this version");
    let abort = |e: anyhow::Error| {
        let _ = std::fs::remove_dir_all(dir.join(STAGING_DIR));
        e.context("this version can't take over (the running server carries on, unaffected)")
    };
    let staged = Staged::create(dir, tables).await.map_err(abort)?;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        migrating = ?staged.migrating,
        "takeover: the copy opens with this version; catching up"
    );
    while staged.catch_up(dir, false).await.map_err(abort)? >= ROUND {}

    if !same_netns {
        tracing::info!("takeover: the running server is in another network namespace; listening on our own sockets");
    } else if !reuse {
        tracing::info!("takeover: listening on other addresses than the running server; binding our own sockets");
    }
    conn.send(&Request::Handover { sockets: reuse }, &[]).await?;
    let (reply, fds) = conn.recv_with_fds::<Reply>().await?;
    let reply = reply.context("the running server closed the control socket during the handover")?;
    anyhow::ensure!(reply.ok, "the handover failed: {}", reply.error.unwrap_or_default());
    let listeners: crate::ops::tls::Inherited =
        reply.roles.into_iter().zip(fds).map(|(role, fd)| (role, std::net::TcpListener::from(fd))).collect();
    let handover = Instant::now();
    // Under systemd this process becomes the service's main process now, before the old one exits (an exiting main
    // process would stop the service and take this one with it).
    sd_notify(&format!("MAINPID={}", std::process::id()));
    // The old process exits after letting requests finish; its exit closes the control socket.
    tokio::time::timeout(EXIT_WAIT, conn.closed())
        .await
        .map_err(|_| anyhow::anyhow!("the old server (pid {from_pid}) didn't exit within {EXIT_WAIT:?}"))??;
    let lock = DirLock::acquire_waiting(dir, Duration::from_secs(30)).await?;
    let copy_migrating = staged.migrating.clone();
    // The old server is gone: from here on, nothing may stop this process from serving. If finishing fails, the
    // database is as the old server left it (or the swap is completed, if it had begun), and it opens normally.
    let migrating = match staged.finish(dir).await {
        Ok(()) => {
            tracing::info!(
                pause_ms = handover.elapsed().as_millis() as u64,
                total_ms = started.elapsed().as_millis() as u64,
                "takeover: the copy is in place"
            );
            Some(copy_migrating)
        }
        Err(e) => {
            tracing::error!("takeover: finishing failed ({e:#}); serving the database as the old server left it");
            recover(dir)?;
            if !dir.join(STAGING_DIR).join(SWAP_MARKER).exists() {
                let _ = std::fs::remove_file(dir.join(STAGING_DIR).join("data.db"));
            }
            None
        }
    };
    Ok(Incoming { lock, listeners, migrating, from_pid })
}

#[cfg(not(unix))]
pub async fn take_over(_dir: &Path, _wanted: &[(String, std::net::SocketAddr)]) -> crate::Result<Incoming> {
    anyhow::bail!("`serve --takeover` needs a Unix system")
}

/// The new version's copy of the database, being caught up.
struct Staged {
    db: Option<Database>,
    dir: PathBuf,
    migrating: Vec<String>,
}

impl Staged {
    /// Copy `<dir>/data.db` and open the copy with this binary's tables (catalog reconcile, schema check).
    async fn create(dir: &Path, tables: Vec<crate::schema::TableDef>) -> crate::Result<Staged> {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let staging = dir.join(STAGING_DIR);
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(&staging)?;
            crate::ops::backup::snapshot_db(&dir.join("data.db"), &staging.join("data.db"))?;
            let db = open_staged(&staging, &tables)?;
            let migrating = db.catalog().migrating_tables();
            Ok(Staged { db: Some(db), dir: staging, migrating })
        })
        .await?
    }

    fn db(&self) -> &Database {
        self.db.as_ref().expect("open until finished")
    }

    /// Replay the journal rows of `<dir>/data.db` written after the copy's ts. One round of up to [`ROUND`] rows,
    /// or (`all`) until none is left. Returns how many were applied.
    async fn catch_up(&self, dir: &Path, all: bool) -> crate::Result<usize> {
        let source = open_source(&dir.join("data.db"))?;
        let mut applied = 0;
        loop {
            let at = self.db().snapshot().await?.ts;
            let entries = journal::read_after(&source, at, ROUND, 8 << 20)?;
            for entry in &entries {
                self.db().replay_checked(entry).await?;
            }
            applied += entries.len();
            if !all || entries.is_empty() {
                return Ok(applied);
            }
        }
    }

    /// With the old server gone: the last rows, what isn't journaled, then swap the copy in.
    async fn finish(mut self, dir: &Path) -> crate::Result<()> {
        self.catch_up(dir, true).await?;
        let source = open_source(&dir.join("data.db"))?;
        let old_ts = storage::read_meta_u64(&source, storage::META_TS)?;
        let ts = self.db().snapshot().await?.ts;
        anyhow::ensure!(ts == old_ts, "the copy is at ts {ts} but the database at ts {old_ts}");
        let carried = carried_state(&source)?;
        drop(source);
        self.db()
            .with_writer(move |c| {
                for [session, request, created, response] in &carried.dedup {
                    c.execute(
                        "INSERT OR IGNORE INTO _mutation_dedup (session_id, request_id, created_ms, response) \
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![session, request, created, response],
                    )?;
                }
                for (key, value) in &carried.meta {
                    match value {
                        Some(v) => storage::write_meta(c, key, v.clone())?,
                        None => {
                            c.execute("DELETE FROM _meta WHERE k = ?1", [key])?;
                        }
                    }
                }
                c.execute("DELETE FROM _meta WHERE k = ?1", [journal::META_TAKEOVER_TS])?;
                let last = storage::read_meta_f64(c, storage::META_LAST_CREATION_TIME)?.max(carried.last_creation_time);
                storage::write_meta(c, storage::META_LAST_CREATION_TIME, rusqlite::types::Value::Real(last))?;
                // The journal goes along for replication and the analytics copy; rows no remaining consumer needs go.
                c.execute("DELETE FROM _journal", [])?;
                for (ts, time, changes) in &carried.journal {
                    journal::insert(c, *ts, *time, changes)?;
                }
                journal::trim(c, carried.journal_floor)?;
                Ok(())
            })
            .await?;
        let staging = self.dir.clone();
        drop(self.db.take());
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || swap(&dir, &staging)).await??;
        Ok(())
    }
}

/// What the old database has that the journal doesn't carry.
struct Carried {
    /// `(session_id, request_id, created_ms, response)`, as stored.
    dedup: Vec<[rusqlite::types::Value; 4]>,
    meta: Vec<(&'static str, Option<rusqlite::types::Value>)>,
    last_creation_time: f64,
    journal: Vec<(u64, f64, String)>,
    /// Journal rows up to this ts are no consumer's any more.
    journal_floor: u64,
}

fn carried_state(source: &rusqlite::Connection) -> crate::Result<Carried> {
    use rusqlite::OptionalExtension;
    let mut dedup = Vec::new();
    let mut stmt = source.prepare("SELECT session_id, request_id, created_ms, response FROM _mutation_dedup")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        dedup.push([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?]);
    }
    let mut meta = Vec::new();
    for key in CARRIED_META {
        let v: Option<rusqlite::types::Value> =
            source.query_row("SELECT v FROM _meta WHERE k = ?1", [key], |r| r.get(0)).optional()?;
        meta.push((key, v));
    }
    let last_creation_time = storage::read_meta_f64(source, storage::META_LAST_CREATION_TIME)?;
    let mut journal_rows = Vec::new();
    let mut stmt = source.prepare("SELECT ts, time, changes FROM _journal ORDER BY ts")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        journal_rows.push((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?));
    }
    // A consumer that has a position keeps the rows after it; with none, every row was only for this takeover.
    let positions: Vec<u64> = [journal::META_SHIPPED_TS, journal::META_OLAP_TS]
        .into_iter()
        .filter_map(|k| meta.iter().find(|(key, _)| *key == k).and_then(|(_, v)| v.as_ref()))
        .filter_map(|v| match v {
            rusqlite::types::Value::Integer(i) => Some(*i as u64),
            _ => None,
        })
        .collect();
    let journal_floor = positions.into_iter().min().unwrap_or(u64::MAX);
    Ok(Carried { dedup, meta, last_creation_time, journal: journal_rows, journal_floor })
}

/// Open a copy of the database with `tables`: what a takeover does to its copy, and `check`.
fn open_staged(staging: &Path, tables: &[crate::schema::TableDef]) -> crate::Result<Database> {
    let migrations = crate::migrations::declared(tables)?;
    let options = OpenOptions { readers: 2, ..Default::default() };
    Database::open_with_migrations(staging, tables, &options, &migrations)
}

/// A read-only connection to the old database.
fn open_source(path: &Path) -> crate::Result<rusqlite::Connection> {
    crate::db::search::vector::register_extension();
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "busy_timeout", 10_000)?;
    Ok(conn)
}

/// Move the copy in place of `<dir>/data.db` (the old one goes to `<staging>/previous/`), crash-safely: the marker
/// file says a swap is under way, and [`recover`] finishes it.
fn swap(dir: &Path, staging: &Path) -> crate::Result<()> {
    // Nothing may hold the copy open: fold its WAL into the file first.
    let staged = staging.join("data.db");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let conn = rusqlite::Connection::open(&staged)?;
        let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        drop(conn);
        if busy == 0 {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the copy is still in use");
        std::thread::sleep(Duration::from_millis(20));
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(staging.join(format!("data.db{suffix}")));
    }
    std::fs::write(staging.join(SWAP_MARKER), b"")?;
    let previous = staging.join("previous");
    let _ = std::fs::remove_dir_all(&previous);
    std::fs::create_dir_all(&previous)?;
    for suffix in ["", "-wal", "-shm"] {
        let from = dir.join(format!("data.db{suffix}"));
        if from.exists() {
            std::fs::rename(&from, previous.join(format!("data.db{suffix}")))?;
        }
    }
    std::fs::rename(&staged, dir.join("data.db"))?;
    std::fs::remove_file(staging.join(SWAP_MARKER))?;
    Ok(())
}

/// Finish a swap a crash interrupted (call with the data directory locked, before opening it).
pub fn recover(dir: &Path) -> crate::Result<()> {
    let staging = dir.join(STAGING_DIR);
    if !staging.join(SWAP_MARKER).exists() {
        return Ok(());
    }
    if !dir.join("data.db").exists() && staging.join("data.db").exists() {
        std::fs::rename(staging.join("data.db"), dir.join("data.db"))?;
        tracing::warn!("finished a takeover's database swap that was interrupted");
    }
    std::fs::remove_file(staging.join(SWAP_MARKER))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// `check`: would this version open this data?
// ---------------------------------------------------------------------------------------------------------------------

/// The result of [`check`].
#[derive(Debug)]
pub struct CheckReport {
    /// The ts of the copy that was checked.
    pub ts: u64,
    /// Tables that open as migrating (documents waiting for a `#[migration]`).
    pub migrating: Vec<String>,
    /// How long opening the copy took (index builds, the schema check): roughly what a takeover's preparation costs.
    pub elapsed: Duration,
}

/// Open a copy of `<dir>/data.db` with this binary, exactly as a takeover would (also while a server runs), then
/// delete the copy. Fails with the schema check's report when this version can't serve the data.
pub async fn check(dir: &Path) -> crate::Result<CheckReport> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let scratch = dir.join(format!("check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let result = (|| {
            crate::ops::backup::snapshot_db(&dir.join("data.db"), &scratch.join("data.db"))?;
            let db = open_staged(&scratch, &crate::runtime::registered_tables())?;
            let migrating = db.catalog().migrating_tables();
            drop(db);
            let ts = storage::read_meta_u64(&open_source(&scratch.join("data.db"))?, storage::META_TS)?;
            Ok(CheckReport { ts, migrating, elapsed: started.elapsed() })
        })();
        let _ = std::fs::remove_dir_all(&scratch);
        result
    })
    .await?
}

// ---------------------------------------------------------------------------------------------------------------------
// systemd
// ---------------------------------------------------------------------------------------------------------------------

/// Tell systemd about this process (`READY=1`, `MAINPID=…`, `STOPPING=1`) when it runs under a unit with a notify
/// socket (`Type=notify`, and `NotifyAccess=all` for a takeover's new process). A no-op otherwise.
pub fn sd_notify(state: &str) {
    #[cfg(unix)]
    {
        let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return };
        let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
        let path = PathBuf::from(path);
        let bytes = path.as_os_str().as_encoded_bytes();
        let sent = if let Some(name) = bytes.strip_prefix(b"@") {
            #[cfg(target_os = "linux")]
            {
                use std::os::linux::net::SocketAddrExt;
                std::os::unix::net::SocketAddr::from_abstract_name(name)
                    .and_then(|addr| sock.send_to_addr(state.as_bytes(), &addr))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = name;
                return;
            }
        } else {
            sock.send_to(state.as_bytes(), &path)
        };
        if let Err(e) = sent {
            tracing::debug!("sd_notify: {e}");
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Framing: newline-delimited JSON, with file descriptors alongside (SCM_RIGHTS)
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(unix)]
struct Conn {
    stream: tokio::net::UnixStream,
    buf: Vec<u8>,
    fds: Vec<OwnedFd>,
}

#[cfg(unix)]
impl Conn {
    fn new(stream: tokio::net::UnixStream) -> Conn {
        Conn { stream, buf: Vec::new(), fds: Vec::new() }
    }

    async fn send(&self, msg: &impl Serialize, fds: &[RawFd]) -> crate::Result<()> {
        send(&self.stream, msg, fds).await
    }

    /// The next message, or `None` when the other side closed the socket.
    async fn recv<T: serde::de::DeserializeOwned>(&mut self) -> crate::Result<Option<T>> {
        Ok(self.recv_with_fds().await?.0)
    }

    /// The next message and the file descriptors received with it.
    async fn recv_with_fds<T: serde::de::DeserializeOwned>(&mut self) -> crate::Result<(Option<T>, Vec<OwnedFd>)> {
        use std::os::fd::AsRawFd;
        loop {
            if let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=end).collect();
                let msg = serde_json::from_slice(&line[..end]).context("a malformed control message")?;
                return Ok((Some(msg), std::mem::take(&mut self.fds)));
            }
            self.stream.readable().await?;
            let fd = self.stream.as_raw_fd();
            let received = self.stream.try_io(tokio::io::Interest::READABLE, || {
                let mut data = [0u8; 4096];
                let mut space = nix::cmsg_space!([RawFd; 8]);
                let mut iov = [std::io::IoSliceMut::new(&mut data)];
                let msg = nix::sys::socket::recvmsg::<()>(
                    fd,
                    &mut iov,
                    Some(&mut space),
                    nix::sys::socket::MsgFlags::MSG_CMSG_CLOEXEC,
                )
                .map_err(std::io::Error::from)?;
                let mut fds = Vec::new();
                for cmsg in msg.cmsgs().map_err(std::io::Error::from)? {
                    if let nix::sys::socket::ControlMessageOwned::ScmRights(received) = cmsg {
                        // SAFETY: the kernel just installed these descriptors in this process for us; nothing else
                        // owns them.
                        fds.extend(received.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }));
                    }
                }
                let n = msg.bytes;
                Ok((data[..n].to_vec(), fds))
            });
            match received {
                Ok((bytes, fds)) => {
                    self.fds.extend(fds);
                    if bytes.is_empty() {
                        return Ok((None, std::mem::take(&mut self.fds)));
                    }
                    self.buf.extend_from_slice(&bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Resolves when the other side closed the socket (its process exited).
    async fn closed(&mut self) -> crate::Result<()> {
        loop {
            if self.recv::<serde_json::Value>().await?.is_none() {
                return Ok(());
            }
        }
    }
}

#[cfg(unix)]
async fn send(stream: &tokio::net::UnixStream, msg: &impl Serialize, fds: &[RawFd]) -> crate::Result<()> {
    use std::os::fd::AsRawFd;
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(b'\n');
    let mut sent = 0;
    while sent < bytes.len() {
        stream.writable().await?;
        let fd = stream.as_raw_fd();
        let attach: &[RawFd] = if sent == 0 { fds } else { &[] };
        let n = stream.try_io(tokio::io::Interest::WRITABLE, || {
            let iov = [std::io::IoSlice::new(&bytes[sent..])];
            let cmsgs: Vec<nix::sys::socket::ControlMessage> =
                if attach.is_empty() { Vec::new() } else { vec![nix::sys::socket::ControlMessage::ScmRights(attach)] };
            nix::sys::socket::sendmsg::<()>(fd, &iov, &cmsgs, nix::sys::socket::MsgFlags::empty(), None)
                .map_err(std::io::Error::from)
        });
        match n {
            Ok(n) => sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

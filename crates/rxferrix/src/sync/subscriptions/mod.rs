//! The shared subscription manager: every distinct query runs once per invalidation, however many sockets watch it.
//!
//! One task per [`App`] owns all subscriptions. Sync workers (one per socket) forward the query-set side of the
//! protocol to it as [`Command`]s and receive the results back through an ordered per-connection [`Mailbox`]; they
//! keep everything else (versions they validate, auth, mutations, actions, pings). Queries run on runner tasks; the
//! manager itself never waits for one.
//!
//! ## Shared queries
//!
//! A shared query is identified by `(udfPath, canonical args JSON, scope)`. The scope is the part of the caller's
//! identity the query's last run read through `ctx.auth`: nothing (every caller shares it), the principal (user or
//! admin id: every socket of that user shares it, anonymous sockets share theirs), or the full identity including
//! the session id (only used by queries that read `ctx.auth.session_id()` or the raw identity). A query nobody has
//! run yet is keyed by the full identity and re-keyed after its first run. When a rerun reads more of the identity
//! than before, the subscribers that no longer match are split off to (new or existing) shared queries for their own
//! identity, which are queued at once.
//!
//! ## Versions
//!
//! A run at snapshot ts `r` gives a result that is valid at every ts from `r` up to (not including) the first commit
//! that writes a key in the run's read set: nothing it read differs in between. A shared query keeps its results as
//! *versions*, each with the last ts it is known valid at (`through`; open while no such commit is known). Older
//! versions are kept while runs that were queued at older ts are still in flight, and dropped after.
//!
//! ## Generations
//!
//! The manager keeps a ts, `self.ts` (the *frontier*): the write log has been checked up to it. A query whose newest
//! version is open is *current* (valid at the frontier); every other query is *in flight* (queued or running). When
//! commits land, a new generation starts (unless generations are held, see Liveness):
//!
//! 1. open a snapshot `S` and wait until the write log has every commit up to `S.ts`;
//! 2. close the version of every current query whose read set contains a key written in `(self.ts, S.ts]`, at the
//!    first such commit (found through the interval index, [`super::interval`]; if the log was trimmed, every query is
//!    closed at `self.ts`);
//! 3. set `self.ts = S.ts` and queue the closed queries. Runner tasks (one per snapshot at `S.ts`: `S`, plus snapshots
//!    from idle readers while their ts is still `S.ts`) take queued jobs and run them at `S.ts`.
//!
//! The queue has two lanes: queries whose runs take `SLOW_RUN_US` (5 ms) or more on average wait in the slow lane,
//! which has runners of its own, each holding a slow permit shared with one-off queries ([`crate::read_lanes`]). A new generation starts as soon as commits wait and the fast lane is empty: it waits neither for
//! queries that are still running nor for slow ones, so a slow query holds up neither the next generation nor the
//! connections that don't watch it. Commits that land while the fast lane is not empty are coalesced into one
//! generation, and queries invalidated together run at the same ts. Jobs still queued when a generation starts run at
//! the new one; a query already queued or running is not queued again.
//!
//! When a run finishes at ts `s` behind the frontier, its read set is checked against the commits in `(s, self.ts]`:
//! if one wrote a key in it, the new version is closed there and the query is queued again at the frontier.
//!
//! ## Updates
//!
//! A connection is sent an update at ts `T` only when every query of its set has a version valid at `T`, and `T` is
//! not older than its last update. The update carries, for each query whose result may have changed since it was last
//! sent, the version valid at `T` if its hash differs. The manager tries `T` = the frontier, and when a run finishes,
//! `T` = that run's ts too: so a connection watching a slow query that commits invalidate faster than it runs still gets
//! an update at each of its runs, from the versions its other queries had then.
//!
//! So the protocol's guarantees hold as they did with one worker per socket: no tearing (all results in a Transition
//! are valid at its `endVersion.ts`, across shared queries too), monotonic ts, read-your-writes (a `Cover` request is
//! answered by the first update at a ts that covers the mutation) and first results in the answering Transition (a
//! query that never ran has no version, so its connection can't be sent anything before it ran).
//!
//! ## Liveness
//!
//! A connection whose queries are invalidated together gets an update when the last of them finishes. Only runs at
//! different generations can keep it from ever having all of its queries valid at one ts: queries that commits keep
//! invalidating while they run (slow ones, or any under heavy load), each queued again when it finishes, at another
//! generation than the others. So a connection one of whose queries finished stale twice since its last update is
//! marked *starving*, and while a starving connection has a query in flight, no new generation starts: the frontier
//! stays put, every query queued meanwhile runs at it and finishes valid at it, so within at most two runs of each of
//! its queries the connection has all of them valid at the frontier and gets its update. (Other connections wait for
//! that hold too, so it is a last resort: a single slow query is served by updates at its runs' ts instead.) A hold
//! lasts at most `HOLD_MAX` (2 s); then generations go on, and those connections get no hold for `HOLD_COOLDOWN`
//! (5 s), so expensive queries a client keeps re-subscribing can't stall everyone else for long.
//!
//! ## Commands and ordering
//!
//! Commands of one connection are applied in order. A `ModifyQuerySet` whose queries all are current is answered at
//! once at `self.ts` without running anything. Otherwise the connection waits until its queries are all valid at one
//! ts (its later commands are queued behind) and is answered then; the queued commands are then applied in order. So
//! each `ModifyQuerySet` / `Authenticate` gets its own update, in order, and commit-driven updates are pushed into the
//! same mailbox: nothing can be reordered.
//!
//! ## Query cache
//!
//! A shared query whose last subscriber leaves is kept, *idle*, if it is current: it stays in the invalidation index,
//! and the first commit that writes into its read set drops it (an idle query is never run), so every idle result is
//! valid at the frontier. A new subscriber to it attaches without a run. Idle queries are dropped least recently used
//! first beyond `QueryCacheOptions::max_bytes` (estimated from their results' sizes), and after
//! `QueryCacheOptions::idle_ttl`.
//!
//! Every current query with a successful result (live or idle) is also published to a [`QueryCache`], with the
//! frontier, for HTTP queries and `ctx.run_query`, which read it without going through the manager. When they miss and
//! run the query, the same query missing again soon after ([`QueryCache`]'s doorkeeper: one-off queries don't fill the
//! cache) hands its run to the manager, which adopts it as an idle query if it is still valid at the frontier.
//!
//! A published result is removed before the frontier moves past a commit that invalidates it, so a reader that sees a
//! frontier at or past the latest commit, and the result still published, has a result valid at that commit:
//! read-your-writes holds (a caller's own commit is in the write log before its mutation returns). Results of queries
//! that read the time (`ctx.now()`) are neither kept idle nor published: they depend on more than the data.
//!
//! ## Backpressure
//!
//! The manager never waits on a connection. A mailbox holds the updates the worker has not taken yet, and the worker
//! only takes one when the socket writer has room for it (`worker::OUT_QUEUE`), so a slow client's updates wait here
//! rather than as frames queued for its socket. An update that
//! changes no version (commit-driven, or answering a `Cover`) is merged into the last queued one: the merged update
//! carries the newest result of each query, which is valid at the newest ts (a query not in the newer update did not
//! change since the older one). So a slow client gets the latest state, coalesced, with memory bounded by its query
//! set, and never stalls the manager or other clients. Updates that change a version are queued separately; a
//! client that has more than [`MAILBOX_LIMIT`] of those (or of commands waiting for its queries) unanswered is dropped
//! with a `FatalError`, which only a client flooding `ModifyQuerySet`s without reading can reach.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, mpsc, watch};

use crate::auth::identity::AuthDeps;
use crate::db::Snapshot;
use crate::db::catalog::IndexId;
use crate::error::ClientError;
use crate::runtime::{App, Identity, Metrics, QueryRun, WeakApp};
use crate::sync::interval::{Interval, IntervalIndex};
use crate::sync::protocol::{QueryId, QuerySetModification, QueryValue, StateModification};
use crate::writelog::WriteLog;

mod cache;
mod mailbox;
mod manager;
#[cfg(test)]
mod tests;

pub use cache::{CacheHit, QueryCache};
pub use mailbox::{Mailbox, Update};
use manager::Manager;

/// Identifies one connection (socket) to the manager.
pub type ConnId = u64;
type SharedId = u64;
type Hash = [u8; 32];
/// What a query run needs: path, args and the identity to run as.
type Job = (Arc<str>, Value, Identity);

/// How many version-changing updates (or commands waiting for its queries) a connection may have outstanding before
/// it is dropped as too slow.
pub const MAILBOX_LIMIT: usize = 1024;

/// A generation uses one more runner (snapshot and thread) per this many queued jobs, up to the number of readers
/// (extra snapshots are only taken from idle readers) and of runtime threads.
const PER_SNAPSHOT_MIN: usize = 4;

/// A shared query keeps at most this many versions unless a connection watching it may still need older ones.
const VERSIONS_KEPT: usize = 8;

use crate::read_lanes::SLOW_RUN_US;

/// The longest a hold for starving connections lasts (see the module docs, Liveness): every other connection waits
/// for it, and a starving connection's queries may each take up to the query timeout.
const HOLD_MAX: Duration = Duration::from_secs(2);
/// How long a connection whose hold reached [`HOLD_MAX`] gets no new one.
const HOLD_COOLDOWN: Duration = Duration::from_secs(5);
/// A query whose run timed out runs again after this, doubling with every timeout in a row ([`Manager::retries`]).
const RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between runs of a query that keeps timing out.
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Job lanes: queries known to be slow wait in a lane of their own, so they never hold up the others (see
/// `Manager::pump`).
const FAST: usize = 0;
const SLOW: usize = 1;

/// Capacity of the command channel from the workers to the manager.
const COMMAND_CAPACITY: usize = 4096;

// ---------------------------------------------------------------------------------------------------------------------
// Handle and commands
// ---------------------------------------------------------------------------------------------------------------------

/// A request from a connection's worker. Versions have already been validated by the worker.
#[derive(Debug)]
pub enum Command {
    Connect {
        conn: ConnId,
        mailbox: Arc<Mailbox>,
    },
    /// Apply these modifications (in order) and answer with an update at `query_set`.
    Modify {
        conn: ConnId,
        query_set: u64,
        modifications: Vec<QuerySetModification>,
    },
    /// The connection's identity is now `identity`, at identity version `version`.
    Authenticate {
        conn: ConnId,
        version: u64,
        identity: Identity,
    },
    /// Read-your-writes: send an update at a ts >= `ts` (even an empty one) as soon as possible.
    Cover {
        conn: ConnId,
        ts: u64,
    },
    Disconnect {
        conn: ConnId,
    },
    /// A run from outside the manager (an HTTP query, `ctx.run_query`) to keep as an idle query.
    Adopt {
        path: Arc<str>,
        args: Value,
        identity: Identity,
        run: Box<Adoptee>,
    },
}

impl Command {
    fn conn(&self) -> ConnId {
        match self {
            Command::Connect { conn, .. }
            | Command::Modify { conn, .. }
            | Command::Authenticate { conn, .. }
            | Command::Cover { conn, .. }
            | Command::Disconnect { conn } => *conn,
            Command::Adopt { .. } => unreachable!("not a connection's command"),
        }
    }
}

/// The run in a [`Command::Adopt`].
pub struct Adoptee(QueryRun);

impl std::fmt::Debug for Adoptee {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "QueryRun at ts {}", self.0.ts)
    }
}

/// A handle to the manager task of an [`App`] (`App::subscriptions`).
#[derive(Clone)]
pub struct SubscriptionManager {
    tx: mpsc::Sender<Command>,
    next_conn: Arc<AtomicU64>,
    cache: Arc<QueryCache>,
}

/// The manager is gone (the app is shutting down).
#[derive(Debug)]
pub struct ManagerClosed;

impl std::fmt::Display for ManagerClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("subscription manager stopped")
    }
}

impl SubscriptionManager {
    /// Start the manager task for `app`. It holds the app weakly and stops when the app is dropped.
    pub(crate) fn spawn(app: &App, metrics: Arc<Metrics>) -> SubscriptionManager {
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let cache = Arc::new(QueryCache::new(app.options().query_cache.max_bytes > 0));
        let manager = Manager::new(app, metrics, rx, cache.clone());
        tokio::spawn(manager.run());
        SubscriptionManager { tx, next_conn: Arc::new(AtomicU64::new(0)), cache }
    }

    /// The published results of current queries (see the module docs, Query cache).
    pub fn cache(&self) -> &QueryCache {
        &self.cache
    }

    /// A successful run of `path(args)` for `identity` outside the manager (a cache miss): offered for adoption as an
    /// idle query if the doorkeeper lets it through. Never waits: dropped if the manager is busy.
    pub(crate) fn offer(&self, path: &str, args: &Value, identity: &Identity, run: &QueryRun) {
        if !self.cache.adopt || run.result.is_err() || run.read_set.reads_clock() {
            return;
        }
        let scope = Scope::of(run.auth_deps, identity);
        let key = QueryKey { path: path.into(), args: canonical_args(args).into(), scope };
        if !self.cache.missed_again(&key) {
            return;
        }
        let run = Box::new(Adoptee(QueryRun {
            result: run.result.as_ref().map(Clone::clone).map_err(|e| anyhow::anyhow!("{e}")),
            read_set: run.read_set.clone(),
            ts: run.ts,
            log_lines: run.log_lines.clone(),
            auth_deps: run.auth_deps,
        }));
        let adopt = Command::Adopt { path: key.path, args: args.clone(), identity: identity.clone(), run };
        let _ = self.tx.try_send(adopt);
    }

    /// Register a new connection. Updates for it arrive in the returned mailbox; its doorbell rings when they do.
    pub async fn connect(&self) -> Result<(Connection, Arc<Mailbox>, mpsc::Receiver<()>), ManagerClosed> {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let (mailbox, doorbell) = Mailbox::new();
        self.tx.send(Command::Connect { conn: id, mailbox: mailbox.clone() }).await.map_err(|_| ManagerClosed)?;
        Ok((Connection { id, tx: self.tx.clone() }, mailbox, doorbell))
    }
}

/// A registered connection. Dropping it unsubscribes everything it had.
pub struct Connection {
    id: ConnId,
    tx: mpsc::Sender<Command>,
}

impl Connection {
    pub fn id(&self) -> ConnId {
        self.id
    }

    pub async fn modify(&self, query_set: u64, modifications: Vec<QuerySetModification>) -> Result<(), ManagerClosed> {
        self.send(Command::Modify { conn: self.id, query_set, modifications }).await
    }

    pub async fn authenticate(&self, version: u64, identity: Identity) -> Result<(), ManagerClosed> {
        self.send(Command::Authenticate { conn: self.id, version, identity }).await
    }

    pub async fn cover(&self, ts: u64) -> Result<(), ManagerClosed> {
        self.send(Command::Cover { conn: self.id, ts }).await
    }

    async fn send(&self, cmd: Command) -> Result<(), ManagerClosed> {
        self.tx.send(cmd).await.map_err(|_| ManagerClosed)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let cmd = Command::Disconnect { conn: self.id };
        if let Err(mpsc::error::TrySendError::Full(cmd)) = self.tx.try_send(cmd) {
            // The queue is full: deliver it from a task rather than leak the connection's subscriptions.
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let tx = self.tx.clone();
                rt.spawn(async move {
                    let _ = tx.send(cmd).await;
                });
            }
        }
    }
}

/// Counters of the subscription manager (`App::subscription_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriptionStats {
    /// Connections registered with the manager.
    pub connections: u64,
    /// Distinct shared queries.
    pub shared_queries: u64,
    /// Live subscriptions: `(connection, queryId)` pairs.
    pub subscriptions: u64,
    /// Ranges in the invalidation index.
    pub index_ranges: u64,
    /// Generations (rounds) started since start.
    pub rounds: u64,
    /// Query executions by the manager since start.
    pub reruns: u64,
    /// Query executions queued by the last generation.
    pub last_round_reruns: u64,
    /// Runs that finished after a commit had already invalidated their result (queued again), since start.
    pub stale_runs: u64,
    /// Times new generations were held for a starving connection (see `sync::subscriptions`, Liveness).
    pub holds: u64,
    /// Shared queries nobody watches, kept as a cache (see `sync::subscriptions`, Query cache).
    pub idle_queries: u64,
    /// Their estimated memory, bytes.
    pub idle_bytes: u64,
    /// Subscriptions, HTTP queries and `ctx.run_query` calls answered from the cache (a current shared query, live
    /// or idle) since start.
    pub cache_hits: u64,
    /// ... and those that waited for a run.
    pub cache_misses: u64,
    /// Idle queries dropped for the memory budget or their age (not for being invalidated), since start.
    pub cache_evictions: u64,
    /// Query runs queued or running.
    pub jobs: u64,
}

// ---------------------------------------------------------------------------------------------------------------------
// Shared queries: keys and results
// ---------------------------------------------------------------------------------------------------------------------

/// How much of the identity a shared query's result may depend on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Scope {
    Any,
    Principal { user: Option<String>, admin: Option<String> },
    Session { user: Option<String>, admin: Option<String>, session: Option<String> },
}

impl Scope {
    fn of(deps: AuthDeps, identity: &Identity) -> Scope {
        let (user, admin) = (identity.user_id.clone(), identity.admin_id.clone());
        match deps {
            AuthDeps::None => Scope::Any,
            AuthDeps::Principal => Scope::Principal { user, admin },
            AuthDeps::Session => Scope::Session { user, admin, session: identity.session_id.clone() },
        }
    }

    fn deps(&self) -> AuthDeps {
        match self {
            Scope::Any => AuthDeps::None,
            Scope::Principal { .. } => AuthDeps::Principal,
            Scope::Session { .. } => AuthDeps::Session,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueryKey {
    path: Arc<str>,
    args: Arc<str>,
    scope: Scope,
}

/// A query result as computed. Log lines are already filtered for clients (empty outside `--dev`).
struct CachedResult {
    hash: Hash,
    outcome: Result<QueryValue, ClientError>,
    log_lines: Vec<String>,
}

impl CachedResult {
    fn modification(&self, query_id: QueryId) -> StateModification {
        let log_lines = self.log_lines.clone();
        match &self.outcome {
            Ok(value) => StateModification::QueryUpdated { query_id, value: value.clone(), base: None, log_lines },
            Err(error) => StateModification::QueryFailed { query_id, error: error.clone(), log_lines },
        }
    }
}

fn canonical_args(args: &Value) -> String {
    // serde_json maps are sorted (no `preserve_order`), so equal values serialize identically.
    serde_json::to_string(args).unwrap_or_default()
}

fn hash_bytes(tag: &[u8], bytes: &[u8]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(tag);
    h.update(bytes);
    *h.finalize().as_bytes()
}

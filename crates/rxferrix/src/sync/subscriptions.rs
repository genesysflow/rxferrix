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
//! The manager never waits on a connection. A mailbox holds the updates the worker has not taken yet. An update that
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
// Updates and mailboxes
// ---------------------------------------------------------------------------------------------------------------------

/// What a connection is sent: its query set and identity versions, the ts every one of its results is valid at, and
/// the results that changed. The worker turns each into one `Transition`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub query_set: u64,
    pub identity: u64,
    pub ts: u64,
    /// Queries removed from the set (sent first).
    pub removed: Vec<QueryId>,
    /// New results by query id.
    pub changed: BTreeMap<QueryId, StateModification>,
}

impl Update {
    pub fn into_modifications(self) -> Vec<StateModification> {
        let mut mods: Vec<StateModification> =
            self.removed.into_iter().map(|query_id| StateModification::QueryRemoved { query_id }).collect();
        mods.extend(self.changed.into_values());
        mods
    }

    /// Fold a later update that changes no version into this one.
    fn absorb(&mut self, later: Update) {
        debug_assert!(later.removed.is_empty() && later.ts >= self.ts);
        debug_assert_eq!((later.query_set, later.identity), (self.query_set, self.identity));
        self.ts = later.ts;
        self.changed.extend(later.changed);
    }
}

/// The ordered, coalescing queue of updates from the manager to one connection's worker. See the module docs.
#[derive(Debug)]
pub struct Mailbox {
    state: Mutex<MailboxState>,
    doorbell: mpsc::Sender<()>,
}

#[derive(Debug, Default)]
struct MailboxState {
    queue: VecDeque<Update>,
    overflowed: bool,
}

impl Mailbox {
    /// A mailbox and the doorbell that rings when something is put in it.
    pub fn new() -> (Arc<Mailbox>, mpsc::Receiver<()>) {
        let (doorbell, rx) = mpsc::channel(1);
        (Arc::new(Mailbox { state: Mutex::default(), doorbell }), rx)
    }

    /// Queue `update`. `step` says it changes a version and must stay a Transition of its own; otherwise it is merged
    /// into the last queued update. Returns false (and marks the mailbox overflowed) when the limit is reached.
    fn push(&self, update: Update, step: bool) -> bool {
        let ok = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let len = state.queue.len();
            match state.queue.back_mut() {
                Some(last) if !step => {
                    last.absorb(update);
                    true
                }
                _ if len >= MAILBOX_LIMIT => {
                    state.overflowed = true;
                    false
                }
                _ => {
                    state.queue.push_back(update);
                    true
                }
            }
        };
        let _ = self.doorbell.try_send(());
        ok
    }

    fn overflow(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).overflowed = true;
        let _ = self.doorbell.try_send(());
    }

    /// Take every queued update, and whether the connection was dropped for being too slow.
    pub fn take(&self) -> (Vec<Update>, bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.queue.drain(..).collect(), state.overflowed)
    }

    /// Number of queued updates (for tests).
    pub fn len(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

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

/// The current successful results of shared queries, published by the manager for readers outside it (see the
/// module docs, Query cache).
pub struct QueryCache {
    entries: RwLock<HashMap<QueryKey, Published>>,
    /// The manager's frontier: every published result is valid at it.
    frontier: AtomicU64,
    /// Doorkeeper: hashes of queries that recently missed, one per slot. A query is adopted when it misses while its
    /// hash is still there (a second miss soon after the first), so one-off queries never reach the manager.
    recent_misses: Box<[AtomicU64]>,
    /// Off when the cache keeps no idle queries.
    adopt: bool,
}

const DOORKEEPER_SLOTS: usize = 8192;

struct Published {
    /// The run's ts: the result is valid from it up to the frontier.
    ts: u64,
    result: Arc<CachedResult>,
}

/// A cached query result ([`QueryCache::get`]).
pub struct CacheHit {
    pub value: QueryValue,
    /// Already filtered for clients (empty outside `--dev`).
    pub log_lines: Vec<String>,
}

impl QueryCache {
    fn new(adopt: bool) -> QueryCache {
        QueryCache {
            entries: RwLock::new(HashMap::new()),
            frontier: AtomicU64::new(0),
            recent_misses: (0..DOORKEEPER_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            adopt,
        }
    }

    /// The doorkeeper ([`QueryCache::recent_misses`]): true on the second miss of `key` in a row of its slot.
    fn missed_again(&self, key: &QueryKey) -> bool {
        use std::hash::BuildHasher;
        static SEED: std::sync::OnceLock<std::hash::RandomState> = std::sync::OnceLock::new();
        let hash = SEED.get_or_init(std::hash::RandomState::new).hash_one(key) | 1; // never 0, the empty slot
        let slot = &self.recent_misses[(hash as usize) % DOORKEEPER_SLOTS];
        if slot.load(Ordering::Relaxed) == hash {
            slot.store(0, Ordering::Relaxed);
            true
        } else {
            slot.store(hash, Ordering::Relaxed);
            false
        }
    }

    /// The result of `path(args)` for `identity`, if one is cached and valid at `latest` (the latest commit): the
    /// caller then needn't run the query.
    pub fn get(&self, path: &str, args: &Value, identity: &Identity, latest: u64) -> Option<CacheHit> {
        // The frontier first: results are unpublished before it moves past a commit that invalidates them.
        if self.frontier.load(Ordering::Acquire) < latest {
            return None;
        }
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        if entries.is_empty() {
            return None;
        }
        let (path, args): (Arc<str>, Arc<str>) = (path.into(), canonical_args(args).into());
        for deps in [AuthDeps::None, AuthDeps::Principal, AuthDeps::Session] {
            let key = QueryKey { path: path.clone(), args: args.clone(), scope: Scope::of(deps, identity) };
            if let Some(p) = entries.get(&key)
                && p.ts <= latest
                && let Ok(value) = &p.result.outcome
            {
                return Some(CacheHit { value: value.clone(), log_lines: p.result.log_lines.clone() });
            }
        }
        None
    }

    fn publish(&self, key: QueryKey, ts: u64, result: Arc<CachedResult>) {
        self.entries.write().unwrap_or_else(|e| e.into_inner()).insert(key, Published { ts, result });
    }

    fn unpublish(&self, key: &QueryKey) {
        self.entries.write().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    fn unpublish_all<'a>(&self, keys: impl IntoIterator<Item = &'a QueryKey>) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            entries.remove(key);
        }
    }
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
    /// HTTP queries and `ctx.run_query` calls answered from the cache since start.
    pub cache_hits: u64,
    /// ... and those that ran the query.
    pub cache_misses: u64,
    /// Idle queries dropped for the memory budget or their age (not for being invalidated), since start.
    pub cache_evictions: u64,
    /// Query runs queued or running.
    pub jobs: u64,
}

// ---------------------------------------------------------------------------------------------------------------------
// The manager
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

/// [`Version::through`] of a result that no known commit has invalidated: valid at the manager's ts.
const OPEN: u64 = u64::MAX;

/// One result of a shared query: computed at `ts`, valid at every ts in `ts..=through`.
struct Version {
    ts: u64,
    /// The ts right before the first commit after `ts` that wrote a key in the run's read set, or [`OPEN`].
    through: u64,
    result: Arc<CachedResult>,
}

struct SharedQuery {
    key: QueryKey,
    args: Value,
    /// The identity it runs as: one that matches `key` (that of a subscriber when it was created).
    identity: Identity,
    subscribers: BTreeSet<(ConnId, QueryId)>,
    /// Results, oldest first; empty until the first run. Older ones are kept while runs at older ts are in flight, so
    /// that a connection can still be sent an update at such a ts.
    versions: VecDeque<Version>,
    /// What the newest run read of the identity.
    deps: AuthDeps,
    /// The newest run's read set, as stored in the invalidation index.
    ranges: Vec<(IndexId, Interval)>,
    /// Queued or running, since a generation at this ts.
    in_flight: Option<u64>,
    /// Moving average of its run time, µs.
    cost_us: u64,
    /// Set while nobody subscribes to it (see the module docs, Query cache).
    idle: Option<Idle>,
    /// The newest run read the time: its result is neither kept idle nor published.
    reads_clock: bool,
}

struct Idle {
    /// Its position in [`Manager::idle`].
    seq: u64,
    since: Instant,
    /// Estimated memory, counted in [`Manager::idle_bytes`].
    bytes: usize,
}

impl SharedQuery {
    /// Is the newest result valid at the manager's ts? Every query that is not is in flight.
    fn is_current(&self) -> bool {
        self.versions.back().is_some_and(|v| v.through == OPEN)
    }

    /// The version valid at `ts` (at most the manager's ts), if one is kept.
    fn at(&self, ts: u64) -> Option<&Version> {
        self.versions.iter().rev().find(|v| v.ts <= ts && ts <= v.through)
    }

    fn job(&self) -> Job {
        (self.key.path.clone(), self.args.clone(), self.identity.clone())
    }
}

struct Sub {
    shared: SharedId,
    /// Run ts and hash of the version last put in this connection's mailbox; `None` before the first.
    sent: Option<(u64, Hash)>,
    /// Runs of its query that finished stale since the connection's last update: `(Conn::updates then, count)`.
    runs: (u64, u32),
}

struct Conn {
    mailbox: Arc<Mailbox>,
    identity: Identity,
    /// The latest query set and identity versions applied.
    query_set: u64,
    identity_version: u64,
    /// Versions and ts of the last update put in the mailbox.
    sent: (u64, u64, u64),
    subs: BTreeMap<QueryId, Sub>,
    /// Removed since the last update (were sent a result before).
    removed: Vec<QueryId>,
    /// Subscriptions whose result may differ from what was last sent.
    touched: BTreeSet<QueryId>,
    /// A mutation ts that the next update must cover.
    cover: Option<u64>,
    /// Waiting for its queries before a command can be answered; later commands queue in `deferred`.
    waiting: bool,
    deferred: VecDeque<Command>,
    /// How many times every query of the set was valid at one ts (an update was due, whether or not anything changed).
    updates: u64,
    /// A query of its set finished stale twice without an update in between (see the module docs, Liveness).
    starving: bool,
    /// After a hold for it reached [`HOLD_MAX`]: no new hold for it before then.
    no_hold_until: Option<Instant>,
}

impl Conn {
    fn new(mailbox: Arc<Mailbox>) -> Conn {
        Conn {
            mailbox,
            identity: Identity::default(),
            query_set: 0,
            identity_version: 0,
            sent: (0, 0, 0),
            subs: BTreeMap::new(),
            removed: Vec::new(),
            touched: BTreeSet::new(),
            cover: None,
            waiting: false,
            deferred: VecDeque::new(),
            updates: 0,
            starving: false,
            no_hold_until: None,
        }
    }
}

/// What runner tasks (and snapshot requests) report to the manager.
enum Event {
    /// Job `seq` ran query `id`, in `elapsed`.
    Ran { id: SharedId, seq: u64, run: QueryRun, elapsed: Duration },
    /// A runner stopped in the middle of job `seq` (it panicked; user code can't make it, its panics are caught).
    Lost { id: SharedId, seq: u64 },
    /// A runner of generation `generation` and lane `lane` stopped and released its snapshot.
    RunnerDone { generation: u64, lane: usize },
    /// A snapshot asked for when every reader was busy.
    Snapshot(anyhow::Result<Arc<Snapshot>>),
    /// A slow permit asked for when none was free.
    SlowPermit(Option<OwnedSemaphorePermit>),
}

/// Queries waiting for a runner, per lane, as `(query, job sequence number, job)`. Runners only take jobs while
/// `generation` is the generation of their snapshot: a job always runs at the newest generation.
struct JobQueue {
    generation: u64,
    lanes: [VecDeque<(SharedId, u64, Job)>; 2],
}

/// Reports the end of a runner task (and the job it was running, if it panicked).
struct RunnerGuard {
    tx: mpsc::UnboundedSender<Event>,
    generation: u64,
    lane: usize,
    job: Option<(SharedId, u64)>,
}

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        if let Some((id, seq)) = self.job.take() {
            let _ = self.tx.send(Event::Lost { id, seq });
        }
        let _ = self.tx.send(Event::RunnerDone { generation: self.generation, lane: self.lane });
    }
}

struct Manager {
    app: WeakApp,
    metrics: Arc<Metrics>,
    write_log: Arc<WriteLog>,
    dev: bool,
    rx: mpsc::Receiver<Command>,
    commits: watch::Receiver<u64>,
    events_tx: mpsc::UnboundedSender<Event>,
    events: mpsc::UnboundedReceiver<Event>,
    /// The frontier: the newest generation's ts. The write log has been checked up to it, and every current query is
    /// valid at it.
    ts: u64,
    queries: HashMap<SharedId, SharedQuery>,
    next_id: SharedId,
    by_key: HashMap<QueryKey, SharedId>,
    index: IntervalIndex<SharedId>,
    conns: HashMap<ConnId, Conn>,
    /// Connections with a `cover` ts.
    covering: BTreeSet<ConnId>,
    /// Starving connections (see the module docs, Liveness).
    starving: BTreeSet<ConnId>,
    /// New generations are held for them.
    holding: bool,
    /// Since when ([`HOLD_MAX`]).
    hold_since: Option<Instant>,
    subscriptions: u64,
    /// Commits (or a `Cover` ts) not covered by the frontier yet: a new generation is due.
    need_round: bool,
    /// Retry soon (opening a snapshot failed).
    retry: bool,
    /// Queries whose newest run timed out: when to run them again, and how many retries came before. A timed-out run
    /// read only part of what it would have, so no commit may ever invalidate its error; it is run again on a timer
    /// instead, for as long as anyone watches it.
    retries: HashMap<SharedId, (Instant, u32)>,

    // ---- execution ----
    generation: u64,
    queue: Arc<Mutex<JobQueue>>,
    /// Runner tasks of the current generation, per lane.
    runners: [usize; 2],
    /// Reader connections and runtime threads: the most fast-lane runners worth having.
    max_runners: usize,
    /// The most slow-lane runners.
    max_slow: usize,
    /// A snapshot at `ts`, kept while anything is in flight so that queued jobs can always run at `ts`.
    gen_snapshot: Option<Arc<Snapshot>>,
    /// A snapshot from `request_snapshot`.
    spare: Option<Arc<Snapshot>>,
    requesting: bool,
    /// A slow permit that arrived as an event, and whether one is being waited for ([`Manager::take_slow_permit`]).
    spare_permit: Option<OwnedSemaphorePermit>,
    requesting_permit: bool,
    /// Jobs queued or running: sequence number -> the frontier when it was queued (ascending with the sequence).
    in_flight: BTreeMap<u64, u64>,
    next_job: u64,

    // ---- query cache ----
    cache: Arc<QueryCache>,
    /// Idle queries, least recently idled first.
    idle: BTreeMap<u64, SharedId>,
    next_idle: u64,
    idle_bytes: usize,
    idle_ttl: Duration,
    idle_budget: usize,
}

fn canonical_args(args: &Value) -> String {
    // serde_json maps are sorted (no `preserve_order`), so equal values serialize identically.
    serde_json::to_string(args).unwrap_or_default()
}

/// Estimated memory of an idle query: its result (the JSON text, and the parsed value, about twice as big), its key
/// and its read set.
fn idle_bytes(q: &SharedQuery) -> usize {
    let result = q.versions.back().map_or(0, |v| match &v.result.outcome {
        Ok(value) => value.json().len() * 3,
        Err(e) => e.message.len(),
    });
    let ranges: usize = q.ranges.iter().map(|(_, interval)| interval.size_bytes() + 48).sum();
    result + q.key.path.len() + q.key.args.len() * 2 + ranges + 512
}

fn hash_bytes(tag: &[u8], bytes: &[u8]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(tag);
    h.update(bytes);
    *h.finalize().as_bytes()
}

/// Does `ranges` (sorted) contain `key` of index `index_id`?
fn touches(ranges: &[(IndexId, Interval)], index_id: IndexId, key: &[u8]) -> bool {
    let from = ranges.partition_point(|(i, _)| *i < index_id);
    ranges[from..].iter().take_while(|(i, _)| *i == index_id).any(|(_, interval)| interval.contains(key))
}

impl Manager {
    fn new(app: &App, metrics: Arc<Metrics>, rx: mpsc::Receiver<Command>, cache: Arc<QueryCache>) -> Manager {
        let (events_tx, events) = mpsc::unbounded_channel();
        cache.frontier.store(app.db().write_log().latest(), Ordering::Release);
        let threads = tokio::runtime::Handle::current().metrics().num_workers();
        Manager {
            app: app.downgrade(),
            metrics,
            write_log: app.db().write_log().clone(),
            dev: app.options().dev,
            rx,
            commits: app.db().write_log().subscribe(),
            events_tx,
            events,
            // With no queries yet, any committed ts is consistent (see `pump`).
            ts: app.db().write_log().latest(),
            queries: HashMap::new(),
            next_id: 0,
            by_key: HashMap::new(),
            index: IntervalIndex::new(),
            conns: HashMap::new(),
            covering: BTreeSet::new(),
            starving: BTreeSet::new(),
            holding: false,
            hold_since: None,
            subscriptions: 0,
            need_round: false,
            retry: false,
            retries: HashMap::new(),
            generation: 0,
            queue: Arc::new(Mutex::new(JobQueue { generation: 0, lanes: [VecDeque::new(), VecDeque::new()] })),
            runners: [0, 0],
            max_runners: app.db().readers().min(threads).max(1),
            max_slow: (app.db().readers().min(threads) / 2).max(1),
            gen_snapshot: None,
            spare: None,
            requesting: false,
            spare_permit: None,
            requesting_permit: false,
            in_flight: BTreeMap::new(),
            next_job: 0,
            cache,
            idle: BTreeMap::new(),
            next_idle: 0,
            idle_bytes: 0,
            idle_ttl: app.options().query_cache.idle_ttl,
            idle_budget: app.options().query_cache.max_bytes,
        }
    }

    async fn run(mut self) {
        loop {
            let hold_ends = self.hold_since.map(|since| since + HOLD_MAX);
            let next_retry = self.next_retry();
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => return,
                },
                // Never `None`: the manager holds a sender.
                Some(event) = self.events.recv() => self.event(event),
                changed = self.commits.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.need_round = true;
                }
                _ = tokio::time::sleep(Duration::from_millis(100)), if self.retry => self.retry = false,
                // A hold that reached its limit ends even when nothing else happens (`update_hold`).
                _ = tokio::time::sleep_until(hold_ends.unwrap_or_else(Instant::now).into()), if hold_ends.is_some() => {}
                _ = tokio::time::sleep_until(next_retry.into()), if !self.retries.is_empty() => {
                    self.retry_timed_out();
                }
                // Idle queries expire, and the write log is trimmed, even when nothing else happens.
                _ = tokio::time::sleep(Duration::from_secs(30)) => self.write_log.trim(),
            }
            self.expire_idle();
            // Take whatever else is waiting, so that one pass (and one generation) serves all of it.
            while let Ok(event) = self.events.try_recv() {
                self.event(event);
            }
            for _ in 0..COMMAND_CAPACITY {
                match self.rx.try_recv() {
                    Ok(cmd) => self.handle(cmd),
                    Err(_) => break,
                }
            }
            let Some(app) = self.app.upgrade() else { return };
            self.pump(&app).await;
            drop(app);
            self.publish_stats();
            #[cfg(debug_assertions)]
            self.check_invariants();
        }
    }

    fn publish_stats(&self) {
        self.metrics.set_subscription_counts(
            self.conns.len() as u64,
            self.queries.len() as u64,
            self.subscriptions,
            self.index.len() as u64,
        );
        self.metrics.set_idle_cache(self.idle.len() as u64, self.idle_bytes as u64);
        self.metrics.set_jobs(self.in_flight.len() as u64);
    }

    // ---- commands ----

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Connect { conn, mailbox } => {
                self.conns.insert(conn, Conn::new(mailbox));
            }
            Command::Adopt { path, args, identity, run } => self.adopt(path, args, identity, run.0),
            Command::Disconnect { conn } => self.remove_conn(conn),
            cmd => {
                let c = cmd.conn();
                // Unknown: dropped for being too slow; its worker is about to stop.
                let Some(conn) = self.conns.get_mut(&c) else { return };
                if conn.waiting {
                    if conn.deferred.len() >= MAILBOX_LIMIT {
                        self.kill(c);
                    } else {
                        conn.deferred.push_back(cmd);
                    }
                    return;
                }
                self.apply(cmd);
            }
        }
    }

    /// Apply a command of a connection that is not waiting.
    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Modify { conn: c, query_set, modifications } => {
                let Some(conn) = self.conns.get_mut(&c) else { return };
                conn.query_set = query_set;
                for m in modifications {
                    match m {
                        QuerySetModification::Add { query_id, udf_path, args } => {
                            self.subscribe(c, query_id, udf_path, args)
                        }
                        QuerySetModification::Remove { query_id } => self.unsubscribe(c, query_id),
                    }
                }
                self.settle_conn(c);
            }
            Command::Authenticate { conn: c, version, identity } => {
                let Some(conn) = self.conns.get_mut(&c) else { return };
                conn.identity = identity.clone();
                conn.identity_version = version;
                // Re-key every subscription: keep the shared query if it matches the new identity too, otherwise
                // move to (or create) the one that does.
                let subs: Vec<(QueryId, SharedId)> = conn.subs.iter().map(|(q, s)| (*q, s.shared)).collect();
                for (qid, old) in subs {
                    let q = &self.queries[&old];
                    if q.key.scope == Scope::of(q.key.scope.deps(), &identity) {
                        continue;
                    }
                    let (path, args_key, args) = (q.key.path.clone(), q.key.args.clone(), q.args.clone());
                    let target = self.find_or_create(&path, &args_key, args, &identity);
                    self.move_sub(c, qid, old, target);
                }
                self.settle_conn(c);
            }
            Command::Cover { conn: c, ts } => {
                let Some(conn) = self.conns.get_mut(&c) else { return };
                if conn.sent.2 >= ts {
                    return; // an update at >= ts is already in the mailbox
                }
                conn.cover = Some(conn.cover.map_or(ts, |p| p.max(ts)));
                self.covering.insert(c);
                if self.ts >= ts {
                    self.try_flush(c, &[self.ts]);
                } else {
                    // The commit exists, so the next generation's snapshot covers it.
                    self.need_round = true;
                }
            }
            Command::Connect { .. } | Command::Disconnect { .. } | Command::Adopt { .. } => {
                unreachable!("handled in handle()")
            }
        }
    }

    /// After a command: answer now if every query of the connection is valid at the frontier, otherwise as soon as
    /// they all are valid at one ts (later commands wait behind).
    fn settle_conn(&mut self, c: ConnId) {
        if !self.try_flush(c, &[self.ts])
            && let Some(conn) = self.conns.get_mut(&c)
        {
            conn.waiting = true;
        }
    }

    /// Subscribe `(c, qid)` (a shared query that has no result yet is queued).
    fn subscribe(&mut self, c: ConnId, qid: QueryId, path: String, args: Value) {
        if self.conns.get(&c).is_some_and(|conn| conn.subs.contains_key(&qid)) {
            // The worker rejects duplicate ids; don't leak if one gets here anyway.
            self.unsubscribe(c, qid);
        }
        let Some(conn) = self.conns.get(&c) else { return };
        let identity = conn.identity.clone();
        let path: Arc<str> = path.into();
        let args_key: Arc<str> = canonical_args(&args).into();
        let id = self.find_or_create(&path, &args_key, args, &identity);
        self.wake(id);
        self.queries.get_mut(&id).expect("just found").subscribers.insert((c, qid));
        let conn = self.conns.get_mut(&c).expect("checked");
        conn.subs.insert(qid, Sub { shared: id, sent: None, runs: (0, 0) });
        conn.touched.insert(qid);
        self.subscriptions += 1;
    }

    fn unsubscribe(&mut self, c: ConnId, qid: QueryId) {
        let Some(conn) = self.conns.get_mut(&c) else { return };
        let Some(sub) = conn.subs.remove(&qid) else { return };
        conn.touched.remove(&qid);
        if sub.sent.is_some() {
            conn.removed.push(qid);
        }
        self.detach(c, qid, sub.shared);
    }

    fn detach(&mut self, c: ConnId, qid: QueryId, id: SharedId) {
        self.subscriptions -= 1;
        let Some(q) = self.queries.get_mut(&id) else { return };
        q.subscribers.remove(&(c, qid));
        if q.subscribers.is_empty() {
            self.release(id);
        }
    }

    /// The last subscriber of `id` left: keep it idle if it can serve as a cache, else drop it.
    fn release(&mut self, id: SharedId) {
        let Some(q) = self.queries.get_mut(&id) else { return };
        let cacheable = self.idle_budget > 0
            && !q.reads_clock
            && q.is_current()
            && q.in_flight.is_none()
            && q.versions.back().is_some_and(|v| v.result.outcome.is_ok())
            && q.key.scope == Scope::of(q.deps, &q.identity);
        if !cacheable {
            self.drop_query(id);
            return;
        }
        // Only the newest version can serve anyone now.
        while q.versions.len() > 1 {
            q.versions.pop_front();
        }
        let bytes = idle_bytes(q);
        let seq = self.next_idle;
        self.next_idle += 1;
        q.idle = Some(Idle { seq, since: Instant::now(), bytes });
        self.idle.insert(seq, id);
        self.idle_bytes += bytes;
        self.evict_over_budget();
    }

    /// Keep a run from outside the manager ([`SubscriptionManager::offer`]) as an idle query, if nothing serves its
    /// key yet and its result is still valid at the frontier.
    fn adopt(&mut self, path: Arc<str>, args: Value, identity: Identity, run: QueryRun) {
        // A run past the frontier saw commits the manager hasn't checked yet; and without a budget nothing is kept.
        if run.ts > self.ts || self.idle_budget == 0 {
            return;
        }
        let key = QueryKey { path, args: canonical_args(&args).into(), scope: Scope::of(run.auth_deps, &identity) };
        if self.by_key.contains_key(&key) {
            return;
        }
        let id = self.create(key, args, identity);
        self.apply_run(id, run);
        // Idle if still current (else dropped), then published.
        self.release(id);
        self.sync_published(id);
    }

    /// `id` gets a subscriber: it is no longer idle.
    fn wake(&mut self, id: SharedId) {
        let Some(q) = self.queries.get_mut(&id) else { return };
        if let Some(idle) = q.idle.take() {
            self.idle.remove(&idle.seq);
            self.idle_bytes -= idle.bytes;
        }
    }

    fn evict_over_budget(&mut self) {
        let mut evicted = 0;
        while self.idle_bytes > self.idle_budget {
            let Some((_, id)) = self.idle.pop_first() else { break };
            self.drop_query(id);
            evicted += 1;
        }
        self.metrics.record_cache_evictions(evicted);
    }

    fn expire_idle(&mut self) {
        let mut evicted = 0;
        while let Some((_, &id)) = self.idle.first_key_value() {
            let idle = self.queries.get(&id).and_then(|q| q.idle.as_ref());
            let expired = idle.is_none_or(|i| i.since.elapsed() >= self.idle_ttl);
            if !expired {
                break;
            }
            self.idle.pop_first();
            self.drop_query(id);
            evicted += 1;
        }
        self.metrics.record_cache_evictions(evicted);
    }

    /// Forget a query. Its queued job is dropped too (a client adding and removing queries would otherwise grow the
    /// queue without bound); one that is already running finishes, and its result is ignored.
    fn drop_query(&mut self, id: SharedId) {
        self.retries.remove(&id);
        let Some(q) = self.queries.remove(&id) else { return };
        if q.in_flight.is_some() {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            for lane in &mut queue.lanes {
                if let Some(at) = lane.iter().position(|(job, ..)| *job == id) {
                    let (_, seq, _) = lane.remove(at).expect("found");
                    self.in_flight.remove(&seq);
                }
            }
        }
        if let Some(idle) = &q.idle {
            self.idle.remove(&idle.seq);
            self.idle_bytes -= idle.bytes;
        }
        if self.by_key.get(&q.key) == Some(&id) {
            self.by_key.remove(&q.key);
            self.cache.unpublish(&q.key);
        }
        for (index_id, interval) in &q.ranges {
            self.index.remove(*index_id, interval, id);
        }
    }

    /// Publish the newest result of `id` to the [`QueryCache`] if it is current and successful (and its key final),
    /// else withdraw it.
    fn sync_published(&self, id: SharedId) {
        let Some(q) = self.queries.get(&id) else { return };
        if self.by_key.get(&q.key) != Some(&id) {
            return;
        }
        match q.versions.back() {
            Some(v)
                if v.through == OPEN
                    && v.result.outcome.is_ok()
                    && !q.reads_clock
                    && q.key.scope == Scope::of(q.deps, &q.identity) =>
            {
                self.cache.publish(q.key.clone(), v.ts, v.result.clone());
            }
            _ => self.cache.unpublish(&q.key),
        }
    }

    /// Move a subscription to another shared query (current, or in flight).
    fn move_sub(&mut self, c: ConnId, qid: QueryId, from: SharedId, to: SharedId) {
        if from == to {
            return;
        }
        self.wake(to);
        if let Some(q) = self.queries.get_mut(&to) {
            q.subscribers.insert((c, qid));
        }
        if let Some(conn) = self.conns.get_mut(&c)
            && let Some(sub) = conn.subs.get_mut(&qid)
        {
            sub.shared = to;
            conn.touched.insert(qid);
        }
        if let Some(q) = self.queries.get_mut(&from) {
            q.subscribers.remove(&(c, qid));
            if q.subscribers.is_empty() {
                self.release(from);
            }
        }
    }

    /// A shared query whose result is valid for `identity`: an existing one with a matching key (preferring a current
    /// one), or a new one keyed by the full identity until its first run (queued at once).
    fn find_or_create(&mut self, path: &Arc<str>, args_key: &Arc<str>, args: Value, identity: &Identity) -> SharedId {
        let mut found = None;
        for deps in [AuthDeps::None, AuthDeps::Principal, AuthDeps::Session] {
            let key = QueryKey { path: path.clone(), args: args_key.clone(), scope: Scope::of(deps, identity) };
            if let Some(&id) = self.by_key.get(&key) {
                if self.queries[&id].is_current() {
                    return id;
                }
                found.get_or_insert(id);
            }
        }
        if let Some(id) = found {
            return id;
        }
        let key =
            QueryKey { path: path.clone(), args: args_key.clone(), scope: Scope::of(AuthDeps::Session, identity) };
        let id = self.create(key, args, identity.clone());
        self.launch([id]);
        id
    }

    fn create(&mut self, key: QueryKey, args: Value, identity: Identity) -> SharedId {
        let id = self.next_id;
        self.next_id += 1;
        self.by_key.insert(key.clone(), id);
        let q = SharedQuery {
            key,
            args,
            identity,
            subscribers: BTreeSet::new(),
            versions: VecDeque::new(),
            deps: AuthDeps::None,
            ranges: Vec::new(),
            in_flight: None,
            cost_us: 0,
            idle: None,
            reads_clock: false,
        };
        self.queries.insert(id, q);
        id
    }

    fn remove_conn(&mut self, c: ConnId) {
        let Some(conn) = self.conns.remove(&c) else { return };
        for (qid, sub) in conn.subs {
            self.detach(c, qid, sub.shared);
        }
        self.covering.remove(&c);
        self.starving.remove(&c);
    }

    /// Drop a connection that can't keep up; its worker sends `FatalError`.
    fn kill(&mut self, c: ConnId) {
        if let Some(conn) = self.conns.get(&c) {
            tracing::debug!(conn = c, "dropping a sync connection that is not keeping up");
            conn.mailbox.overflow();
        }
        self.remove_conn(c);
    }

    // ---- updates ----

    /// If every query of `c` has a version valid at one of `candidates` (tried in order; never older than what `c` was
    /// last sent), put its update at that ts in its mailbox, and resume the commands it deferred. Returns whether it
    /// did.
    fn try_flush(&mut self, c: ConnId, candidates: &[u64]) -> bool {
        let Some(conn) = self.conns.get(&c) else { return false };
        let valid_at = |ts: u64| {
            ts >= conn.sent.2
                && conn.subs.values().all(|sub| self.queries.get(&sub.shared).is_some_and(|q| q.at(ts).is_some()))
        };
        let Some(ts) = candidates.iter().copied().find(|ts| valid_at(*ts)) else { return false };
        self.flush(c, ts);
        let Some(conn) = self.conns.get_mut(&c) else { return true }; // dropped: too slow
        conn.updates += 1;
        if std::mem::take(&mut conn.starving) {
            self.starving.remove(&c);
        }
        if conn.waiting {
            self.resume(c);
        }
        true
    }

    /// Put an update for `c` at `ts` in its mailbox, if there is anything to say: changed results, removals, new
    /// versions, or a mutation ts to cover. Every query of `c` must have a version valid at `ts`.
    fn flush(&mut self, c: ConnId, ts: u64) {
        // Publish the counters before a client can see this update, so they never lag behind what it has seen.
        self.publish_stats();
        let Some(conn) = self.conns.get_mut(&c) else { return };
        let mut update = Update {
            query_set: conn.query_set,
            identity: conn.identity_version,
            ts,
            removed: std::mem::take(&mut conn.removed),
            changed: BTreeMap::new(),
        };
        let mut still_touched = Vec::new();
        for qid in std::mem::take(&mut conn.touched) {
            let Some(sub) = conn.subs.get_mut(&qid) else { continue };
            let Some(q) = self.queries.get(&sub.shared) else { continue };
            let Some(version) = q.at(ts) else {
                debug_assert!(false, "flushing a subscription without a valid version");
                continue;
            };
            let hash = version.result.hash;
            if sub.sent.map(|(_, h)| h) != Some(hash) {
                update.changed.insert(qid, version.result.modification(qid));
            }
            sub.sent = Some((version.ts, hash));
            // A later update may pick a newer version: it stays touched unless they all have this result.
            if q.versions.iter().any(|v| v.ts > version.ts && v.result.hash != hash) {
                still_touched.push(qid);
            }
        }
        conn.touched.extend(still_touched);
        let step = (update.query_set, update.identity) != (conn.sent.0, conn.sent.1);
        let covers = conn.cover.is_some_and(|m| ts >= m);
        if update.removed.is_empty() && update.changed.is_empty() && !step && !covers {
            return;
        }
        if covers {
            conn.cover = None;
            self.covering.remove(&c);
        }
        debug_assert!(ts >= conn.sent.2, "updates must not go back in time");
        conn.sent = (update.query_set, update.identity, ts);
        if !conn.mailbox.push(update, step) {
            self.kill(c);
        }
    }

    /// Answered a waiting connection: apply the commands it queued meanwhile, in order, until one has to wait again.
    fn resume(&mut self, c: ConnId) {
        let Some(conn) = self.conns.get_mut(&c) else { return };
        conn.waiting = false;
        while let Some(conn) = self.conns.get_mut(&c)
            && !conn.waiting
            && let Some(cmd) = conn.deferred.pop_front()
        {
            self.apply(cmd);
        }
    }

    // ---- execution ----

    /// Queue `ids` (those not in flight already) to run at the newest generation. Returns how many were queued.
    fn launch(&mut self, ids: impl IntoIterator<Item = SharedId>) -> u64 {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0;
        for id in ids {
            let Some(q) = self.queries.get_mut(&id) else { continue };
            if q.in_flight.is_some() {
                continue;
            }
            q.in_flight = Some(self.ts);
            let seq = self.next_job;
            self.next_job += 1;
            self.in_flight.insert(seq, self.ts);
            let lane = if q.cost_us >= SLOW_RUN_US { SLOW } else { FAST };
            queue.lanes[lane].push_back((id, seq, q.job()));
            n += 1;
        }
        drop(queue);
        // Counted when queued: results can reach clients right after, and the counter must never lag behind them.
        self.metrics.add_reruns(n);
        n
    }

    fn queued(&self, lane: usize) -> usize {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).lanes[lane].len()
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Ran { id, seq, run, elapsed } => {
                self.in_flight.remove(&seq);
                if let Some(q) = self.queries.get_mut(&id) {
                    let us = elapsed.as_micros() as u64;
                    q.cost_us = if q.cost_us == 0 { us } else { (q.cost_us * 3 + us) / 4 };
                }
                self.finish_run(id, run);
            }
            Event::Lost { id, seq } => {
                tracing::error!("a query execution task stopped early");
                self.in_flight.remove(&seq);
                if let Some(q) = self.queries.get_mut(&id) {
                    q.in_flight = None;
                    self.launch([id]);
                }
            }
            Event::RunnerDone { generation, lane } => {
                if generation == self.generation {
                    self.runners[lane] -= 1;
                }
            }
            Event::SlowPermit(permit) => {
                self.requesting_permit = false;
                self.spare_permit = permit;
            }
            Event::Snapshot(result) => {
                self.requesting = false;
                match result {
                    Ok(snapshot) => self.spare = Some(snapshot),
                    Err(e) => {
                        tracing::error!("subscription manager: opening a snapshot failed: {e:#}");
                        self.retry = true;
                    }
                }
            }
        }
    }

    /// Start a generation if one is due and allowed, and make sure queued jobs have runners.
    async fn pump(&mut self, app: &App) {
        self.update_hold();
        if self.in_flight.is_empty() {
            // Idle: don't pin a WAL snapshot.
            self.gen_snapshot = None;
        }
        let mut advanced = false;
        'pump: loop {
            // A new generation waits until the current one's fast lane is empty (or has no runner): queries invalidated
            // together then run at one ts, and commits landing meanwhile are coalesced. It waits neither for running
            // queries nor for queries known to be slow, which have a lane (and runners) of their own.
            if self.need_round && !advanced && !self.holding && (self.queued(FAST) == 0 || self.runners[FAST] == 0) {
                if self.queries.is_empty() && self.covering.is_empty() {
                    // Nothing to bring up to date: with no queries, any committed ts is consistent, so the frontier
                    // moves to the latest commit without a generation. (The query cache needs it current: a result
                    // is only adopted, and only served, at or behind the frontier.)
                    self.need_round = false;
                    let latest = *self.commits.borrow_and_update();
                    if latest > self.ts {
                        self.ts = latest;
                        self.cache.frontier.store(latest, Ordering::Release);
                    }
                } else {
                    match self.take_snapshot(app) {
                        Some(snapshot) if snapshot.ts > self.ts => {
                            self.advance(snapshot).await;
                            advanced = true;
                            continue 'pump;
                        }
                        Some(snapshot) => {
                            // Nothing committed since the frontier, as of the snapshot. A commit that landed since was
                            // just marked seen, so no notice of it will wake the loop: retry soon instead.
                            self.need_round = *self.commits.borrow_and_update() > self.ts;
                            self.retry |= self.need_round;
                            if self.gen_snapshot.is_none() && self.queued(FAST) + self.queued(SLOW) > 0 {
                                self.gen_snapshot = Some(snapshot);
                            }
                        }
                        None => {}
                    }
                }
            }
            for lane in [FAST, SLOW] {
                let queued = self.queued(lane);
                if queued == 0 {
                    continue;
                }
                let wanted = match lane {
                    FAST => self.max_runners.min(queued.div_ceil(PER_SNAPSHOT_MIN)),
                    _ => self.max_slow.min(queued),
                };
                while self.runners[lane] < wanted.max(1) {
                    // A slow-lane runner holds a slow permit while it runs.
                    let permit = match lane {
                        SLOW => match self.take_slow_permit(app) {
                            Some(p) => Some(p),
                            None => break,
                        },
                        _ => None,
                    };
                    // The fast lane's first runner uses the generation's snapshot; other runners an idle reader's.
                    let shared = if lane == FAST && self.runners[FAST] == 0 { self.gen_snapshot.clone() } else { None };
                    let may_advance = lane == FAST && self.runners[FAST] * 2 <= wanted && !advanced;
                    // Opening a snapshot that can only be newer than the frontier is wasted unless it starts the next
                    // generation.
                    let stale_frontier = self.write_log.latest() > self.ts && !may_advance;
                    let snapshot = match shared {
                        Some(s) => s,
                        None => match if stale_frontier { None } else { self.take_snapshot(app) } {
                            Some(s) if s.ts == self.ts => s,
                            Some(s) if may_advance => {
                                // Newer, and the fast lane runs at under half the runners it wants (no reader was
                                // free when the generation started, and a commit landed since: none ever will be at
                                // this ts). Move on to the newer ts: the queue runs there, on as many readers as are
                                // free, rather than on a few for the whole generation.
                                self.advance(s).await;
                                advanced = true;
                                continue 'pump;
                            }
                            other => {
                                if other.is_some() || stale_frontier {
                                    self.need_round = true; // newer: commits landed, for the next generation
                                }
                                // A slow lane without a runner shares the generation's snapshot rather than wait.
                                match &self.gen_snapshot {
                                    Some(s) if lane == SLOW && self.runners[SLOW] == 0 => s.clone(),
                                    _ => break,
                                }
                            }
                        },
                    };
                    if self.gen_snapshot.is_none() {
                        self.gen_snapshot = Some(snapshot.clone());
                    }
                    self.spawn_runner(app, snapshot, lane, permit);
                }
            }
            break;
        }
        self.spare = None;
        self.spare_permit = None;
    }

    /// A spare slow permit, else a free one. If none is free, `None`, and one is requested (it arrives as an event).
    fn take_slow_permit(&mut self, app: &App) -> Option<OwnedSemaphorePermit> {
        if let Some(p) = self.spare_permit.take().or_else(|| app.read_lanes().try_slow_permit()) {
            return Some(p);
        }
        if !self.requesting_permit {
            self.requesting_permit = true;
            let (app, tx) = (app.downgrade(), self.events_tx.clone());
            tokio::spawn(async move {
                let Some(app) = app.upgrade() else { return };
                let permit = app.read_lanes().slow_permit().await;
                let _ = tx.send(Event::SlowPermit(permit));
            });
        }
        None
    }

    /// A spare snapshot if it is not behind the frontier, else an idle reader's. If every reader is busy, `None`, and
    /// one is requested (it arrives as an event).
    fn take_snapshot(&mut self, app: &App) -> Option<Arc<Snapshot>> {
        if let Some(s) = self.spare.take()
            && s.ts >= self.ts
        {
            return Some(s);
        }
        match app.db().try_snapshot() {
            Some(Ok(s)) => Some(s),
            Some(Err(e)) => {
                tracing::error!("subscription manager: opening a snapshot failed: {e:#}");
                self.retry = true;
                None
            }
            None => {
                if !self.requesting {
                    self.requesting = true;
                    let (app, tx) = (app.downgrade(), self.events_tx.clone());
                    tokio::spawn(async move {
                        let Some(app) = app.upgrade() else { return };
                        let _ = tx.send(Event::Snapshot(app.db().snapshot().await));
                    });
                }
                None
            }
        }
    }

    fn spawn_runner(&mut self, app: &App, snapshot: Arc<Snapshot>, lane: usize, permit: Option<OwnedSemaphorePermit>) {
        debug_assert_eq!(snapshot.ts, self.ts);
        self.runners[lane] += 1;
        let (generation, queue, tx, app) =
            (self.generation, self.queue.clone(), self.events_tx.clone(), app.downgrade());
        tokio::spawn(async move {
            let mut guard = RunnerGuard { tx: tx.clone(), generation, lane, job: None };
            loop {
                let next = {
                    let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                    if queue.generation == generation { queue.lanes[lane].pop_front() } else { None }
                };
                let Some((id, seq, (path, args, identity))) = next else { break };
                guard.job = Some((id, seq));
                let Some(app) = app.upgrade() else { break };
                let started = std::time::Instant::now();
                let run = app.query_in_snapshot(&snapshot, &path, args, &identity, crate::guard::Origin::Client).await;
                let elapsed = started.elapsed();
                drop(app);
                guard.job = None;
                if tx.send(Event::Ran { id, seq, run, elapsed }).is_err() {
                    break;
                }
                // Queries rarely suspend; yield so this task neither hogs a thread the manager (or another runner)
                // needs nor keeps results from being applied as they come.
                tokio::task::yield_now().await;
            }
            // Release the reader (and the slow permit) before reporting the end.
            drop(snapshot);
            drop(permit);
            drop(guard);
        });
    }

    /// Start a generation at `snapshot` (newer than the frontier): close the versions of every query whose read set
    /// contains a key written in `(frontier, snapshot.ts]`, move the frontier, and queue those queries.
    async fn advance(&mut self, snapshot: Arc<Snapshot>) {
        let started = std::time::Instant::now();
        let s = snapshot.ts;
        debug_assert!(s > self.ts);
        // The writer appends to the log right after COMMIT; the snapshot may have seen a commit whose entry isn't
        // appended yet.
        self.write_log.wait_for(s).await;
        let frontier = self.ts;
        let mut stale = Vec::new();
        // Idle queries a commit invalidated: dropped, not run (see the module docs, Query cache).
        let mut dropped = Vec::new();
        match self.write_log.entries_between(frontier, s) {
            // Trimmed: we can't tell what changed, so everything did (all we know is that it was valid at the frontier).
            None => {
                for (id, q) in &mut self.queries {
                    if q.idle.is_some() {
                        dropped.push(*id);
                    } else if q.is_current() {
                        q.versions.back_mut().expect("current").through = frontier;
                        stale.push(*id);
                    }
                }
            }
            Some(entries) => {
                // In ts order, so the first hit of a query is the commit that ends its version.
                let mut hits = Vec::new();
                for entry in &entries {
                    for (index_id, key) in &entry.writes {
                        self.index.stab(*index_id, key, |id| hits.push((id, entry.ts)));
                    }
                }
                for (id, ts) in hits {
                    if let Some(q) = self.queries.get_mut(&id)
                        && q.is_current()
                    {
                        if q.idle.is_some() {
                            dropped.push(id);
                            continue;
                        }
                        q.versions.back_mut().expect("current").through = ts - 1;
                        stale.push(id);
                    }
                }
            }
        }
        // Withdraw the invalidated results before the frontier moves past the commits that invalidated them.
        self.cache.unpublish_all(stale.iter().filter_map(|id| self.queries.get(id)).map(|q| &q.key));
        for id in dropped {
            self.drop_query(id);
        }
        // From here on, jobs run at `s`. Every query that is still current is valid at `s` (nothing it read changed).
        self.ts = s;
        self.cache.frontier.store(s, Ordering::Release);
        self.generation += 1;
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).generation = self.generation;
        self.runners = [0, 0];
        self.gen_snapshot = Some(snapshot);
        let invalidated = stale.len();
        // In id order rather than in the order commits hit them: queries subscribed together have neighbouring ids, so
        // a connection's queries run close together and it gets its update early in the generation, not at its end.
        stale.sort_unstable();
        let queued = self.launch(stale);
        self.metrics.record_round(queued);
        self.need_round = *self.commits.borrow_and_update() > s;
        // A connection waiting for a mutation's ts may have everything valid at `s` now.
        for c in self.covering.iter().copied().collect::<Vec<_>>() {
            self.try_flush(c, &[s]);
        }
        tracing::debug!(
            ts = s,
            invalidated,
            queued = self.queued(FAST) + self.queued(SLOW),
            in_flight = self.in_flight.len(),
            shared_queries = self.queries.len(),
            elapsed_us = started.elapsed().as_micros() as u64,
            "subscription generation"
        );
    }

    /// A run of `id` finished: record its result as the newest version, re-key it, and try to send their update to the
    /// connections it serves (at the frontier, or at the run's ts). A result that is stale already is queued again.
    fn finish_run(&mut self, id: SharedId, run: QueryRun) {
        let Some(q) = self.queries.get(&id) else { return }; // dropped meanwhile
        if q.in_flight.is_none() {
            debug_assert!(false, "a result for a query that is not in flight");
            return;
        }
        let s = run.ts;
        let mut subscribers: BTreeMap<ConnId, Vec<QueryId>> = BTreeMap::new();
        for &(c, qid) in &q.subscribers {
            subscribers.entry(c).or_default().push(qid);
        }
        let timed_out = run.result.as_ref().is_err_and(|e| e.chain().any(|c| c.is::<crate::error::QueryTimeout>()));
        if timed_out {
            let attempt = self.retries.get(&id).map_or(0, |&(_, n)| n + 1);
            let wait = (RETRY_FIRST * 2u32.saturating_pow(attempt.min(16))).min(RETRY_MAX);
            self.retries.insert(id, (Instant::now() + wait, attempt));
        } else {
            self.retries.remove(&id);
        }
        self.apply_run(id, run);
        let stale = self.queries.get(&id).is_some_and(|q| !q.is_current());
        let mut split = Vec::new();
        self.settle_key(id, &mut split);
        self.sync_published(id);
        self.launch(split);
        for (c, qids) in subscribers {
            if self.try_flush(c, &[self.ts, s]) || !stale {
                continue;
            }
            // Not every query of `c` is valid at one ts, and this one is stale already: count, to notice misalignment.
            let Some(conn) = self.conns.get_mut(&c) else { continue };
            let updates = conn.updates;
            for qid in qids {
                let Some(sub) = conn.subs.get_mut(&qid) else { continue };
                if sub.runs.0 != updates {
                    sub.runs = (updates, 0);
                }
                sub.runs.1 += 1;
                if sub.runs.1 >= 2 && !conn.starving {
                    conn.starving = true;
                    self.starving.insert(c);
                }
            }
        }
        if let Some(q) = self.queries.get(&id)
            && !q.is_current()
            && q.in_flight.is_none()
        {
            self.metrics.record_stale_run();
            self.launch([id]);
        }
    }

    /// When the next timed-out query is due to run again ([`Manager::retries`]).
    fn next_retry(&self) -> Instant {
        self.retries.values().map(|&(at, _)| at).min().unwrap_or_else(|| Instant::now() + RETRY_MAX)
    }

    /// Run again the timed-out queries that are due and still watched; forget those nobody watches.
    fn retry_timed_out(&mut self) {
        let now = Instant::now();
        let due: Vec<SharedId> = self.retries.iter().filter(|(_, (at, _))| *at <= now).map(|(id, _)| *id).collect();
        let mut launch = Vec::new();
        for id in due {
            match self.queries.get(&id) {
                Some(q) if !q.subscribers.is_empty() => {
                    if q.in_flight.is_none() {
                        launch.push(id);
                    }
                    // Not due again until its run reports back (`finish_run` reschedules it, or forgets it).
                    if let Some(entry) = self.retries.get_mut(&id) {
                        entry.0 = now + RETRY_MAX * 60;
                    }
                }
                _ => {
                    self.retries.remove(&id);
                }
            }
        }
        self.launch(launch);
    }

    /// Hold new generations while a starving connection has a query in flight (see the module docs, Liveness), for at
    /// most [`HOLD_MAX`] at a time: then those connections get no hold for [`HOLD_COOLDOWN`], and generations go on.
    fn update_hold(&mut self) {
        let now = Instant::now();
        let conns = &self.conns;
        let queries = &self.queries;
        let holding_for: Vec<ConnId> = self
            .starving
            .iter()
            .filter(|c| {
                conns.get(c).is_some_and(|conn| {
                    conn.no_hold_until.is_none_or(|t| t <= now)
                        && conn.subs.values().any(|sub| queries.get(&sub.shared).is_some_and(|q| q.in_flight.is_some()))
                })
            })
            .copied()
            .collect();
        let mut holding = !holding_for.is_empty();
        if holding && now.duration_since(*self.hold_since.get_or_insert(now)) >= HOLD_MAX {
            // Every other connection has waited long enough for these.
            for c in holding_for {
                if let Some(conn) = self.conns.get_mut(&c) {
                    conn.no_hold_until = Some(now + HOLD_COOLDOWN);
                }
            }
            holding = false;
        }
        if !holding {
            self.hold_since = None;
        }
        if holding && !self.holding {
            self.metrics.record_hold();
        }
        self.holding = holding;
    }

    /// Record a run of `id` as its newest version. Its validity ends at the first commit after the run's ts (up to the
    /// frontier) that wrote a key in its read set.
    fn apply_run(&mut self, id: SharedId, run: QueryRun) {
        let frontier = self.ts;
        let dev = self.dev;
        let Some(q) = self.queries.get_mut(&id) else { return };
        let log_lines = crate::log::for_client(dev, run.log_lines);
        // Serialized here, once: the hash and every subscriber's frame use this text.
        let outcome = run.result.map(QueryValue::new).map_err(|e| ClientError::from_anyhow(&e, dev));
        // Log lines don't count towards the hash: a rerun with the same result sends nothing.
        let hash = match &outcome {
            Ok(value) => hash_bytes(b"ok", value.json().as_bytes()),
            Err(e) => {
                let error = serde_json::to_vec(&serde_json::json!([e.message, e.data])).unwrap_or_default();
                hash_bytes(b"err", &error)
            }
        };
        let ranges: BTreeSet<(IndexId, Interval)> =
            run.read_set.ranges().map(|(index_id, range)| (index_id, Interval::from_range(range))).collect();
        let ranges: Vec<(IndexId, Interval)> = ranges.into_iter().collect();
        let through = if run.ts >= frontier {
            OPEN
        } else {
            match self.write_log.entries_between(run.ts, frontier) {
                // Trimmed: all we know is that it is valid at its own ts.
                None => run.ts,
                Some(entries) => entries
                    .iter()
                    .find(|e| e.writes.iter().any(|(index_id, key)| touches(&ranges, *index_id, key)))
                    .map_or(OPEN, |e| e.ts - 1),
            }
        };
        if q.versions.back().is_some_and(|v| v.ts == run.ts && v.result.outcome.is_err()) {
            // A retry of a run that failed (timed out) at the same ts, with nothing committed since: it replaces it.
            q.versions.pop_back();
        }
        debug_assert!(q.versions.back().is_none_or(|v| v.ts < run.ts), "versions in ts order");
        let changed = q.versions.back().is_none_or(|v| v.result.hash != hash);
        q.versions.push_back(Version {
            ts: run.ts,
            through,
            result: Arc::new(CachedResult { hash, outcome, log_lines }),
        });
        q.deps = run.auth_deps;
        q.reads_clock = run.read_set.reads_clock();
        q.in_flight = None;
        if ranges != q.ranges {
            for (index_id, interval) in &q.ranges {
                self.index.remove(*index_id, interval, id);
            }
            for (index_id, interval) in &ranges {
                self.index.insert(*index_id, interval.clone(), id);
            }
            q.ranges = ranges;
        }
        if changed {
            for &(c, qid) in &q.subscribers {
                if let Some(conn) = self.conns.get_mut(&c) {
                    conn.touched.insert(qid);
                }
            }
        }
        self.trim_versions(id);
    }

    /// Drop the versions of `id` no update can use any more: those that end before the oldest job in flight was
    /// queued. If many are left (a long run is in flight), keep only those a connection watching `id` may still need.
    fn trim_versions(&mut self, id: SharedId) {
        let floor = self.in_flight.values().next().copied().unwrap_or(self.ts);
        let Some(q) = self.queries.get(&id) else { return };
        let floor = if q.versions.len() > VERSIONS_KEPT {
            q.subscribers
                .iter()
                .filter_map(|(c, _)| self.conns.get(c))
                .flat_map(|conn| conn.subs.values())
                .filter_map(|sub| self.queries.get(&sub.shared)?.in_flight)
                .min()
                .unwrap_or(self.ts)
                .max(floor)
        } else {
            floor
        };
        let q = self.queries.get_mut(&id).expect("checked");
        while q.versions.len() > 1 && q.versions[0].through < floor {
            q.versions.pop_front();
        }
    }

    /// After a run of `id`: give it the key its auth reads call for, and move the subscribers whose identity doesn't
    /// match that key to shared queries of their own (new ones are pushed to `split`, to be queued).
    fn settle_key(&mut self, id: SharedId, split: &mut Vec<SharedId>) {
        let Some(q) = self.queries.get(&id) else { return };
        let deps = q.deps;
        let own = QueryKey { path: q.key.path.clone(), args: q.key.args.clone(), scope: Scope::of(deps, &q.identity) };
        let mut movers: HashMap<QueryKey, Vec<(ConnId, QueryId)>> = HashMap::new();
        for &(c, qid) in &q.subscribers {
            let scope = Scope::of(deps, &self.conns[&c].identity);
            if scope != own.scope {
                let key = QueryKey { scope, ..own.clone() };
                movers.entry(key).or_default().push((c, qid));
            }
        }
        let args = q.args.clone();
        for (key, subs) in movers {
            let target = match self.by_key.get(&key) {
                Some(&t) => t,
                None => {
                    let identity = self.conns[&subs[0].0].identity.clone();
                    let t = self.create(key, args.clone(), identity);
                    split.push(t);
                    t
                }
            };
            for (c, qid) in subs {
                self.move_sub(c, qid, id, target);
            }
        }
        let Some(q) = self.queries.get_mut(&id) else { return };
        if q.key != own {
            match self.by_key.get(&own) {
                Some(&other) if other != id => {
                    // Another query already serves this key: merge into it.
                    let subs: Vec<_> = q.subscribers.iter().copied().collect();
                    for (c, qid) in subs {
                        self.move_sub(c, qid, id, other);
                    }
                }
                _ => {
                    let old = std::mem::replace(&mut q.key, own.clone());
                    if self.by_key.get(&old) == Some(&id) {
                        self.by_key.remove(&old);
                        self.cache.unpublish(&old);
                    }
                    self.by_key.insert(own, id);
                }
            }
        }
    }

    /// Cross-check the bookkeeping (debug builds; skipped for large states).
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        for lane in [FAST, SLOW] {
            let queued = self.queued(lane);
            let waiting_for_permit = lane == SLOW && self.requesting_permit;
            assert!(
                queued == 0 || self.runners[lane] > 0 || self.requesting || self.retry || waiting_for_permit,
                "{queued} jobs queued in lane {lane} without a runner"
            );
        }
        if self.subscriptions > 5_000 {
            return;
        }
        let mut subs = 0u64;
        let mut ranges = 0usize;
        let mut in_flight = 0;
        let mut idle_bytes = 0usize;
        for (id, q) in &self.queries {
            assert_eq!(self.by_key.get(&q.key), Some(id), "by_key out of sync");
            assert_eq!(q.subscribers.is_empty(), q.idle.is_some(), "without subscribers if and only if idle");
            assert!(q.is_current() || q.in_flight.is_some(), "stale query not in flight");
            if let Some(idle) = &q.idle {
                assert!(q.is_current() && q.in_flight.is_none(), "an idle query must be current and not in flight");
                assert_eq!(self.idle.get(&idle.seq), Some(id), "idle query not in the LRU");
                idle_bytes += idle.bytes;
            }
            for (i, v) in q.versions.iter().enumerate() {
                assert!(v.ts <= v.through && v.ts <= self.ts, "version past the frontier");
                assert!(i + 1 == q.versions.len() || (v.through != OPEN && v.through < q.versions[i + 1].ts));
            }
            for (c, qid) in &q.subscribers {
                assert_eq!(self.conns[c].subs[qid].shared, *id, "subscriber points elsewhere");
                assert_eq!(
                    q.key.scope,
                    Scope::of(q.key.scope.deps(), &self.conns[c].identity),
                    "subscriber/key mismatch"
                );
            }
            in_flight += usize::from(q.in_flight.is_some());
            ranges += q.ranges.len();
        }
        assert!(in_flight <= self.in_flight.len(), "query in flight without a job");
        for (c, conn) in &self.conns {
            for (qid, sub) in &conn.subs {
                assert!(self.queries[&sub.shared].subscribers.contains(&(*c, *qid)));
                subs += 1;
            }
        }
        assert_eq!(self.by_key.len(), self.queries.len());
        assert_eq!(idle_bytes, self.idle_bytes, "idle bytes out of sync");
        assert_eq!(self.idle.len(), self.queries.values().filter(|q| q.idle.is_some()).count());
        assert_eq!(subs, self.subscriptions);
        assert_eq!(ranges, self.index.len());
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn update(query_set: u64, ts: u64, removed: Vec<QueryId>, changed: &[(QueryId, i64)]) -> Update {
        let changed = changed
            .iter()
            .map(|(q, v)| {
                (
                    *q,
                    StateModification::QueryUpdated {
                        query_id: *q,
                        value: json!(v).into(),
                        base: None,
                        log_lines: vec![],
                    },
                )
            })
            .collect();
        Update { query_set, identity: 0, ts, removed, changed }
    }

    #[test]
    fn the_cache_answers_only_at_or_past_its_frontier() {
        let cache = QueryCache::new(true);
        let identity = Identity::default();
        let key = QueryKey { path: "q:f".into(), args: canonical_args(&json!({})).into(), scope: Scope::Any };
        let result = |n: i64| {
            let value = QueryValue::new(json!(n));
            Arc::new(CachedResult { hash: [0; 32], outcome: Ok(value), log_lines: vec![] })
        };
        cache.publish(key.clone(), 5, result(1));
        cache.frontier.store(7, Ordering::Release);
        let get = |latest| cache.get("q:f", &json!({}), &identity, latest).map(|h| h.value.value().clone());
        assert_eq!(get(7), Some(json!(1)), "valid from its run (5) through the frontier (7)");
        assert_eq!(get(6), Some(json!(1)));
        assert_eq!(get(8), None, "a commit past the frontier may have invalidated it");
        assert_eq!(get(4), None, "computed after the ts asked for");
        cache.unpublish(&key);
        assert_eq!(get(7), None);
        // Another identity's scope doesn't match a session-scoped entry.
        let mine = Identity { session_id: Some("s1".into()), ..Default::default() };
        let session_key = QueryKey { scope: Scope::of(AuthDeps::Session, &mine), ..key.clone() };
        cache.publish(session_key, 5, result(2));
        assert_eq!(cache.get("q:f", &json!({}), &mine, 7).map(|h| h.value.value().clone()), Some(json!(2)));
        assert!(cache.get("q:f", &json!({}), &Identity::default(), 7).is_none());
    }

    #[test]
    fn mailbox_merges_updates_without_version_steps() {
        let (mailbox, mut doorbell) = Mailbox::new();
        assert!(mailbox.push(update(1, 5, vec![3], &[(1, 10)]), true));
        assert!(mailbox.push(update(1, 6, vec![], &[(1, 11), (2, 20)]), false));
        assert!(mailbox.push(update(1, 8, vec![], &[(2, 21)]), false));
        assert!(mailbox.push(update(2, 8, vec![1], &[]), true));
        assert!(mailbox.push(update(2, 9, vec![], &[(2, 22)]), false));
        assert!(doorbell.try_recv().is_ok());
        let (updates, overflowed) = mailbox.take();
        assert!(!overflowed);
        assert_eq!(updates, vec![update(1, 8, vec![3], &[(1, 11), (2, 21)]), update(2, 9, vec![1], &[(2, 22)])]);
        assert_eq!(
            updates[0].clone().into_modifications(),
            vec![
                StateModification::QueryRemoved { query_id: 3 },
                StateModification::QueryUpdated { query_id: 1, value: json!(11).into(), base: None, log_lines: vec![] },
                StateModification::QueryUpdated { query_id: 2, value: json!(21).into(), base: None, log_lines: vec![] },
            ]
        );
        assert!(mailbox.is_empty());
    }

    #[test]
    fn mailbox_overflows_on_too_many_version_steps() {
        let (mailbox, _doorbell) = Mailbox::new();
        for v in 0..MAILBOX_LIMIT as u64 {
            assert!(mailbox.push(update(v + 1, 1, vec![], &[]), true));
        }
        // Commit-driven updates still merge...
        assert!(mailbox.push(update(MAILBOX_LIMIT as u64, 2, vec![], &[(1, 1)]), false));
        // ...but one more step is too many.
        assert!(!mailbox.push(update(MAILBOX_LIMIT as u64 + 1, 2, vec![], &[]), true));
        let (updates, overflowed) = mailbox.take();
        assert!(overflowed);
        assert_eq!(updates.len(), MAILBOX_LIMIT);
    }

    #[test]
    fn scopes() {
        let anon = Identity::default();
        let alice = Identity { user_id: Some("a".into()), admin_id: None, session_id: Some("s1".into()) };
        let alice2 = Identity { session_id: Some("s2".into()), ..alice.clone() };
        assert_eq!(Scope::of(AuthDeps::None, &anon), Scope::of(AuthDeps::None, &alice));
        assert_ne!(Scope::of(AuthDeps::Principal, &anon), Scope::of(AuthDeps::Principal, &alice));
        assert_eq!(Scope::of(AuthDeps::Principal, &alice), Scope::of(AuthDeps::Principal, &alice2));
        assert_ne!(Scope::of(AuthDeps::Session, &alice), Scope::of(AuthDeps::Session, &alice2));
        for deps in [AuthDeps::None, AuthDeps::Principal, AuthDeps::Session] {
            assert_eq!(Scope::of(deps, &alice).deps(), deps);
        }
    }
}

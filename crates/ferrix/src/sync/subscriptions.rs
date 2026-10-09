//! The shared subscription manager: every distinct query runs once per invalidation, however many sockets watch it.
//!
//! One task per [`App`] owns all subscriptions. Sync workers (one per socket) forward the query-set side of the
//! protocol to it as [`Command`]s and receive the results back through an ordered per-connection [`Mailbox`]; they
//! keep everything else (versions they validate, auth, mutations, actions, pings).
//!
//! ## Shared queries
//!
//! A shared query is identified by `(udfPath, canonical args JSON, scope)`. The scope is the part of the caller's
//! identity the query's last run read through `ctx.auth`: nothing (every caller shares it), the principal (user or
//! admin id: every socket of that user shares it, anonymous sockets share theirs), or the full identity including
//! the session id (only used by queries that read `ctx.auth.session_id()` or the raw identity). A query nobody has
//! run yet is keyed by the full identity and re-keyed after its first run. When a rerun reads more of the identity
//! than before, the subscribers that no longer match are split off to (new or existing) shared queries for their own
//! identity, which run in the same round.
//!
//! ## Rounds
//!
//! The manager keeps one ts, `self.ts`, at which every shared query that has a result (and is not being rerun) is
//! valid. A round:
//!
//! 1. opens a snapshot `S` (plus, for a big round, a few more from idle readers, kept only if they are at the same
//!    ts; if a commit lands meanwhile it starts over at the newer ts) and waits until the write log has every commit up
//!    to `S.ts`;
//! 2. finds the queries whose read sets contain a key written in `(self.ts, S.ts]` through the interval index
//!    ([`super::interval`]; if the log was trimmed, every query), plus the queries that never ran;
//! 3. sets `self.ts = S.ts` and marks those queries running: every other query is valid at `S.ts`, since nothing it
//!    read changed;
//! 4. reruns the running ones at `S.ts`, spread over the snapshots and threads (and, if a rerun splits a query, runs
//!    the split-off queries at `S.ts` too);
//! 5. as soon as none of a connection's queries is running any more, sends it ONE update at `S.ts` with the queries
//!    of its set whose result hash differs from what it was last sent.
//!
//! Every result in an update is valid at its ts, and `self.ts` only grows, so the protocol's guarantees hold as they
//! did with one worker per socket: no tearing (all results in a Transition are valid at its `endVersion.ts`, across
//! shared queries too), monotonic ts, read-your-writes (a `Cover` request is answered by the first update at a ts
//! that covers the mutation), and first results in the answering Transition.
//!
//! ## Commands and ordering
//!
//! Commands of one connection are applied in order. A `ModifyQuerySet` whose new queries all have a result is
//! answered at once at `self.ts` without running anything. Otherwise the connection waits for the next round (its
//! later commands are queued behind) and is answered right after it, at the round's ts; the queued commands are then
//! applied in order. So each `ModifyQuerySet` / `Authenticate` gets its own update, in order, and commit-driven
//! updates are pushed into the same mailbox: nothing can be reordered.
//!
//! ## Backpressure
//!
//! The manager never waits on a connection. A mailbox holds the updates the worker has not taken yet. An update that
//! changes no version (commit-driven, or answering a `Cover`) is merged into the last queued one: the merged update
//! carries the newest result of each query, which is valid at the newest ts (a query not in the newer update did not
//! change since the older one). So a slow client gets the latest state, coalesced, with memory bounded by its query
//! set, and never stalls the manager or other clients. Updates that change a version are queued separately; a
//! client that has more than [`MAILBOX_LIMIT`] of those (or of commands waiting for a round) unanswered is dropped
//! with a `FatalError`, which only a client flooding `ModifyQuerySet`s without reading can reach.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::auth::identity::AuthDeps;
use crate::db::Snapshot;
use crate::db::catalog::IndexId;
use crate::error::ClientError;
use crate::runtime::{App, Identity, Metrics, QueryRun, WeakApp};
use crate::sync::interval::{Interval, IntervalIndex};
use crate::sync::protocol::{QueryId, QuerySetModification, StateModification};

/// Identifies one connection (socket) to the manager.
pub type ConnId = u64;
type SharedId = u64;
type Hash = [u8; 32];
/// What a query run needs: path, args and the identity to run as.
type Job = (Arc<str>, Value, Identity);

/// How many version-changing updates (or commands waiting for a round) a connection may have outstanding before it
/// is dropped as too slow.
pub const MAILBOX_LIMIT: usize = 1024;

/// A round uses one more snapshot (and thread) per this many queries to rerun, up to the number of
/// readers (extra snapshots are only taken from idle readers) and of runtime threads.
const PER_SNAPSHOT_MIN: usize = 4;

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
    Connect { conn: ConnId, mailbox: Arc<Mailbox> },
    /// Apply these modifications (in order) and answer with an update at `query_set`.
    Modify { conn: ConnId, query_set: u64, modifications: Vec<QuerySetModification> },
    /// The connection's identity is now `identity`, at identity version `version`.
    Authenticate { conn: ConnId, version: u64, identity: Identity },
    /// Read-your-writes: send an update at a ts >= `ts` (even an empty one) as soon as possible.
    Cover { conn: ConnId, ts: u64 },
    Disconnect { conn: ConnId },
}

impl Command {
    fn conn(&self) -> ConnId {
        match self {
            Command::Connect { conn, .. }
            | Command::Modify { conn, .. }
            | Command::Authenticate { conn, .. }
            | Command::Cover { conn, .. }
            | Command::Disconnect { conn } => *conn,
        }
    }
}

/// A handle to the manager task of an [`App`] (`App::subscriptions`).
#[derive(Clone)]
pub struct SubscriptionManager {
    tx: mpsc::Sender<Command>,
    next_conn: Arc<AtomicU64>,
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
        let manager = Manager {
            app: app.downgrade(),
            metrics,
            rx,
            commits: app.db().write_log().subscribe(),
            ts: 0,
            queries: HashMap::new(),
            next_id: 0,
            by_key: HashMap::new(),
            index: IntervalIndex::new(),
            conns: HashMap::new(),
            pending: BTreeSet::new(),
            waiting: BTreeSet::new(),
            covering: BTreeSet::new(),
            flush_set: BTreeSet::new(),
            subscriptions: 0,
            need_round: false,
        };
        tokio::spawn(manager.run());
        SubscriptionManager { tx, next_conn: Arc::new(AtomicU64::new(0)) }
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
    /// Rounds run since start.
    pub rounds: u64,
    /// Query executions by the manager since start.
    pub reruns: u64,
    /// Query executions in the last round.
    pub last_round_reruns: u64,
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

/// A query result as last computed. Log lines are already filtered for clients (empty outside `--dev`).
struct CachedResult {
    hash: Hash,
    outcome: Result<Value, ClientError>,
    log_lines: Vec<String>,
}

impl CachedResult {
    fn modification(&self, query_id: QueryId) -> StateModification {
        let log_lines = self.log_lines.clone();
        match &self.outcome {
            Ok(value) => StateModification::QueryUpdated { query_id, value: value.clone(), log_lines },
            Err(error) => StateModification::QueryFailed { query_id, error: error.clone(), log_lines },
        }
    }
}

struct SharedQuery {
    key: QueryKey,
    args: Value,
    /// The identity it runs as: one that matches `key` (that of a subscriber when it was created).
    identity: Identity,
    subscribers: BTreeSet<(ConnId, QueryId)>,
    /// `None` until the first run. Otherwise valid at the manager's ts (between rounds).
    result: Option<Arc<CachedResult>>,
    /// What the last run read of the identity.
    deps: AuthDeps,
    /// The read set's ranges, as stored in the invalidation index.
    ranges: Vec<(IndexId, Interval)>,
    /// Being rerun in the current round (its result is not valid at the manager's ts yet).
    running: bool,
}

struct Sub {
    shared: SharedId,
    /// Hash of the result last put in this connection's mailbox; `None` before the first.
    sent_hash: Option<Hash>,
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
    /// Waiting for a round to compute new queries; later commands queue in `deferred`.
    waiting: bool,
    deferred: VecDeque<Command>,
}

struct Manager {
    app: WeakApp,
    metrics: Arc<Metrics>,
    rx: mpsc::Receiver<Command>,
    commits: watch::Receiver<u64>,
    /// Every shared query with a result is valid at this ts.
    ts: u64,
    queries: HashMap<SharedId, SharedQuery>,
    next_id: SharedId,
    by_key: HashMap<QueryKey, SharedId>,
    index: IntervalIndex<SharedId>,
    conns: HashMap<ConnId, Conn>,
    /// Shared queries that never ran.
    pending: BTreeSet<SharedId>,
    /// Connections waiting for a round.
    waiting: BTreeSet<ConnId>,
    /// Connections with a `cover` ts.
    covering: BTreeSet<ConnId>,
    /// Connections touched by a round.
    flush_set: BTreeSet<ConnId>,
    subscriptions: u64,
    need_round: bool,
}

fn canonical_args(args: &Value) -> String {
    // serde_json maps are sorted (no `preserve_order`), so equal values serialize identically.
    serde_json::to_string(args).unwrap_or_default()
}

fn hash_result(tag: &[u8], value: &Value) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(tag);
    h.update(&serde_json::to_vec(value).unwrap_or_default());
    *h.finalize().as_bytes()
}

impl Manager {
    async fn run(mut self) {
        loop {
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => return,
                },
                changed = self.commits.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.need_round = true;
                }
                // Retry after a failed round.
                _ = tokio::time::sleep(Duration::from_millis(100)), if self.need_round => {}
            }
            // Take whatever else is queued, so one round serves all of it.
            for _ in 0..COMMAND_CAPACITY {
                match self.rx.try_recv() {
                    Ok(cmd) => self.handle(cmd),
                    Err(_) => break,
                }
            }
            if self.need_round {
                let Some(app) = self.app.upgrade() else { return };
                // A round's answers can queue commands that need another round.
                while self.need_round {
                    if let Err(e) = self.round(&app).await {
                        tracing::error!("subscription round failed: {e:#}");
                        self.need_round = true;
                        break;
                    }
                }
            }
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
    }

    // ---- commands ----

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Connect { conn, mailbox } => {
                self.conns.insert(
                    conn,
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
                    },
                );
            }
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
                let mut wait = false;
                for m in modifications {
                    match m {
                        QuerySetModification::Add { query_id, udf_path, args } => {
                            wait |= self.subscribe(c, query_id, udf_path, args);
                        }
                        QuerySetModification::Remove { query_id } => self.unsubscribe(c, query_id),
                    }
                }
                self.settle_conn(c, wait);
            }
            Command::Authenticate { conn: c, version, identity } => {
                let Some(conn) = self.conns.get_mut(&c) else { return };
                conn.identity = identity.clone();
                conn.identity_version = version;
                // Re-key every subscription: keep the shared query if it matches the new identity too, otherwise
                // move to (or create) the one that does.
                let subs: Vec<(QueryId, SharedId)> = conn.subs.iter().map(|(q, s)| (*q, s.shared)).collect();
                let mut wait = false;
                for (qid, old) in subs {
                    let q = &self.queries[&old];
                    if q.key.scope == Scope::of(q.key.scope.deps(), &identity) {
                        wait |= q.result.is_none();
                        continue;
                    }
                    let (path, args_key, args) = (q.key.path.clone(), q.key.args.clone(), q.args.clone());
                    let target = self.find_or_create(&path, &args_key, args, &identity);
                    self.move_sub(c, qid, old, target);
                    wait |= self.queries[&target].result.is_none();
                }
                self.settle_conn(c, wait);
            }
            Command::Cover { conn: c, ts } => {
                let Some(conn) = self.conns.get_mut(&c) else { return };
                if conn.sent.2 >= ts {
                    return; // an update at >= ts is already in the mailbox
                }
                conn.cover = Some(conn.cover.map_or(ts, |p| p.max(ts)));
                if self.ts >= ts {
                    self.flush(c);
                } else {
                    // The commit exists, so the next round's snapshot covers it.
                    self.covering.insert(c);
                    self.need_round = true;
                }
            }
            Command::Connect { .. } | Command::Disconnect { .. } => unreachable!("handled in handle()"),
        }
    }

    /// After a command: answer now if every result is at hand, otherwise wait for the next round.
    fn settle_conn(&mut self, c: ConnId, wait: bool) {
        if wait {
            if let Some(conn) = self.conns.get_mut(&c) {
                conn.waiting = true;
                self.waiting.insert(c);
                self.need_round = true;
            }
        } else {
            self.flush(c);
        }
    }

    /// Subscribe `(c, qid)`. Returns true when the shared query has no result yet (a round must run it).
    fn subscribe(&mut self, c: ConnId, qid: QueryId, path: String, args: Value) -> bool {
        if self.conns.get(&c).is_some_and(|conn| conn.subs.contains_key(&qid)) {
            // The worker rejects duplicate ids; don't leak if one gets here anyway.
            self.unsubscribe(c, qid);
        }
        let Some(conn) = self.conns.get(&c) else { return false };
        let identity = conn.identity.clone();
        let path: Arc<str> = path.into();
        let args_key: Arc<str> = canonical_args(&args).into();
        let id = self.find_or_create(&path, &args_key, args, &identity);
        let q = self.queries.get_mut(&id).expect("just found");
        q.subscribers.insert((c, qid));
        let needs_run = q.result.is_none();
        let conn = self.conns.get_mut(&c).expect("checked");
        conn.subs.insert(qid, Sub { shared: id, sent_hash: None });
        conn.touched.insert(qid);
        self.subscriptions += 1;
        needs_run
    }

    fn unsubscribe(&mut self, c: ConnId, qid: QueryId) {
        let Some(conn) = self.conns.get_mut(&c) else { return };
        let Some(sub) = conn.subs.remove(&qid) else { return };
        conn.touched.remove(&qid);
        if sub.sent_hash.is_some() {
            conn.removed.push(qid);
        }
        self.detach(c, qid, sub.shared);
    }

    fn detach(&mut self, c: ConnId, qid: QueryId, id: SharedId) {
        self.subscriptions -= 1;
        let Some(q) = self.queries.get_mut(&id) else { return };
        q.subscribers.remove(&(c, qid));
        if q.subscribers.is_empty() {
            self.drop_query(id);
        }
    }

    fn drop_query(&mut self, id: SharedId) {
        let Some(q) = self.queries.remove(&id) else { return };
        if self.by_key.get(&q.key) == Some(&id) {
            self.by_key.remove(&q.key);
        }
        for (index_id, interval) in &q.ranges {
            self.index.remove(*index_id, interval, id);
        }
        self.pending.remove(&id);
    }

    /// Move a subscription to another shared query (which must have a result valid for its identity, or get one in
    /// the current round).
    fn move_sub(&mut self, c: ConnId, qid: QueryId, from: SharedId, to: SharedId) {
        if from == to {
            return;
        }
        if let Some(q) = self.queries.get_mut(&to) {
            q.subscribers.insert((c, qid));
        }
        if let Some(conn) = self.conns.get_mut(&c)
            && let Some(sub) = conn.subs.get_mut(&qid)
        {
            sub.shared = to;
            conn.touched.insert(qid);
            self.flush_set.insert(c);
        }
        if let Some(q) = self.queries.get_mut(&from) {
            q.subscribers.remove(&(c, qid));
            if q.subscribers.is_empty() {
                self.drop_query(from);
            }
        }
    }

    /// A shared query whose result is valid for `identity`: an existing one with a matching key (preferring one that
    /// has a result), or a new one keyed by the full identity until its first run (added to `pending`).
    fn find_or_create(&mut self, path: &Arc<str>, args_key: &Arc<str>, args: Value, identity: &Identity) -> SharedId {
        let mut found = None;
        for deps in [AuthDeps::None, AuthDeps::Principal, AuthDeps::Session] {
            let key = QueryKey { path: path.clone(), args: args_key.clone(), scope: Scope::of(deps, identity) };
            if let Some(&id) = self.by_key.get(&key) {
                if self.queries[&id].result.is_some() {
                    return id;
                }
                found.get_or_insert(id);
            }
        }
        if let Some(id) = found {
            return id;
        }
        let key = QueryKey { path: path.clone(), args: args_key.clone(), scope: Scope::of(AuthDeps::Session, identity) };
        let id = self.create(key, args, identity.clone());
        self.pending.insert(id);
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
            result: None,
            deps: AuthDeps::None,
            ranges: Vec::new(),
            running: false,
        };
        self.queries.insert(id, q);
        id
    }

    fn remove_conn(&mut self, c: ConnId) {
        let Some(conn) = self.conns.remove(&c) else { return };
        for (qid, sub) in conn.subs {
            self.detach(c, qid, sub.shared);
        }
        self.waiting.remove(&c);
        self.covering.remove(&c);
        self.flush_set.remove(&c);
    }

    /// Drop a connection that can't keep up; its worker sends `FatalError`.
    fn kill(&mut self, c: ConnId) {
        if let Some(conn) = self.conns.get(&c) {
            tracing::debug!(conn = c, "dropping a sync connection that is not keeping up");
            conn.mailbox.overflow();
        }
        self.remove_conn(c);
    }

    /// Put an update for `c` in its mailbox at the manager's ts, if there is anything to say: changed results,
    /// removals, new versions, or a mutation ts to cover. Every result of `c` must be valid at `self.ts`.
    fn flush(&mut self, c: ConnId) {
        // Publish the counters before a client can see this update, so they never lag behind what it has seen.
        self.publish_stats();
        let ts = self.ts;
        let Some(conn) = self.conns.get_mut(&c) else { return };
        let mut update = Update {
            query_set: conn.query_set,
            identity: conn.identity_version,
            ts,
            removed: std::mem::take(&mut conn.removed),
            changed: BTreeMap::new(),
        };
        for qid in std::mem::take(&mut conn.touched) {
            let Some(sub) = conn.subs.get_mut(&qid) else { continue };
            let Some(result) = self.queries.get(&sub.shared).and_then(|q| q.result.as_ref()) else {
                debug_assert!(false, "flushing a subscription without a result");
                continue;
            };
            if sub.sent_hash != Some(result.hash) {
                sub.sent_hash = Some(result.hash);
                update.changed.insert(qid, result.modification(qid));
            }
        }
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

    // ---- rounds ----

    async fn round(&mut self, app: &App) -> anyhow::Result<()> {
        self.need_round = false;
        let started = std::time::Instant::now();
        let db = app.db();
        // Commits from here on trigger another round.
        self.commits.borrow_and_update();
        if self.queries.is_empty() && self.waiting.is_empty() && self.covering.is_empty() {
            // Nothing to bring up to date. (`self.ts` stays: with no queries, any committed ts is consistent.)
            return Ok(());
        }
        let mut snapshots = vec![db.snapshot().await?];
        let threads = tokio::runtime::Handle::current().metrics().num_workers();
        let mut restarts = 0;
        let (s, mut batch, fresh, invalidated) = loop {
            let s = snapshots[0].ts;
            // The writer appends to the log right after COMMIT; the snapshot may have seen a commit whose entry isn't
            // appended yet.
            db.write_log().wait_for(s).await;
            let (batch, fresh, invalidated) = self.to_run(db, s);
            // Spread big batches over several snapshots at the same ts (each is one SQLite connection, used from one
            // thread at a time). Extra snapshots are only taken from idle readers, never waited for.
            let parallel = db.readers().min(threads).min(batch.len() / PER_SNAPSHOT_MIN).max(1);
            let mut newer = None;
            while snapshots.len() < parallel {
                match db.try_snapshot() {
                    Some(Ok(extra)) if extra.ts == s => snapshots.push(extra),
                    Some(Ok(extra)) if extra.ts > s => {
                        newer = Some(extra);
                        break;
                    }
                    _ => break, // all readers busy, or failed: run with what we have
                }
            }
            match newer {
                // A commit landed while opening them: start over at the newer ts rather than run serially.
                Some(extra) if restarts < 3 => {
                    restarts += 1;
                    snapshots = vec![extra];
                }
                _ => break (s, batch, fresh, invalidated),
            }
        };
        self.pending.clear();
        // From here on results are computed at S. Every query that is not running is valid at S already (nothing it
        // read changed), so a connection none of whose queries is running can be sent its update at S right away.
        debug_assert!(s >= self.ts);
        self.ts = s;
        for id in &batch {
            if let Some(q) = self.queries.get_mut(id) {
                q.running = true;
            }
        }
        let mut reruns = 0u64;
        while !batch.is_empty() {
            reruns += batch.len() as u64;
            self.metrics.add_reruns(batch.len() as u64);
            // Re-keying after a run may split subscribers off to new queries; those run in S too, in `next`.
            let mut next = Vec::new();
            self.run_batch(app, &snapshots, &batch, &mut next).await;
            batch = next;
        }
        let parallelism = snapshots.len();
        drop(snapshots);

        // Fan out: one update per affected connection, all at S.
        let mut conns = std::mem::take(&mut self.flush_set);
        conns.append(&mut self.waiting);
        conns.extend(self.covering.iter().copied());
        let flushed = conns.len();
        for c in conns {
            self.flush(c);
            self.resume(c);
        }
        self.metrics.record_round(reruns);
        tracing::debug!(
            ts = s,
            invalidated,
            new = fresh,
            reruns,
            connections = flushed,
            snapshots = parallelism,
            shared_queries = self.queries.len(),
            elapsed_us = started.elapsed().as_micros() as u64,
            "subscription round"
        );
        Ok(())
    }

    /// The queries a round at `s` must run: those that never ran, and those whose read set contains a key written in
    /// `(self.ts, s]` (all of them if the log no longer covers that span). Also returns how many of each.
    fn to_run(&self, db: &crate::db::Database, s: u64) -> (Vec<SharedId>, usize, usize) {
        let mut to_run: BTreeSet<SharedId> = self.pending.clone();
        let fresh = to_run.len();
        if s > self.ts {
            match db.write_log().entries_between(self.ts, s) {
                // Trimmed: we can't tell what changed, so everything did.
                None => to_run.extend(self.queries.keys().copied()),
                Some(entries) => {
                    for entry in &entries {
                        for (index_id, key) in &entry.writes {
                            self.index.stab(*index_id, key, |id| {
                                to_run.insert(id);
                            });
                        }
                    }
                }
            }
        }
        let invalidated = to_run.len() - fresh;
        (to_run.into_iter().filter(|id| self.queries.contains_key(id)).collect(), fresh, invalidated)
    }

    /// Run `batch` in `snapshots` (all at `self.ts`) and apply each result as it arrives (see [`Self::finish_run`]).
    /// With several snapshots, one task per snapshot takes queries from a shared queue, so they run on several threads.
    /// Split-off queries that must still run in this round are added to `next`.
    async fn run_batch(&mut self, app: &App, snapshots: &[Arc<Snapshot>], batch: &[SharedId], next: &mut Vec<SharedId>) {
        let job = |q: &SharedQuery| (q.key.path.clone(), q.args.clone(), q.identity.clone());
        if snapshots.len() == 1 || batch.len() < 2 {
            for id in batch {
                let Some((path, args, identity)) = self.queries.get(id).map(job) else { continue };
                let run = app.query_in_snapshot(&snapshots[0], &path, args, &identity, false).await;
                self.finish_run(app, *id, run, next);
                tokio::task::yield_now().await;
            }
            return;
        }
        let queue: VecDeque<(SharedId, Job)> =
            batch.iter().filter_map(|id| Some((*id, job(self.queries.get(id)?)))).collect();
        let mut unfinished: BTreeSet<SharedId> = queue.iter().map(|(id, _)| *id).collect();
        let queue = Arc::new(Mutex::new(queue));
        let (tx, mut rx) = mpsc::unbounded_channel();
        for snapshot in snapshots {
            let (app, snapshot, queue, tx) = (app.clone(), snapshot.clone(), queue.clone(), tx.clone());
            tokio::spawn(async move {
                loop {
                    let next = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                    let Some((id, (path, args, identity))) = next else { break };
                    let run = app.query_in_snapshot(&snapshot, &path, args, &identity, false).await;
                    if tx.send((id, run)).is_err() {
                        break;
                    }
                    // Queries rarely suspend; yield so this task neither hogs a thread the manager (or another
                    // runner) needs nor keeps results from being applied as they come.
                    tokio::task::yield_now().await;
                }
            });
        }
        drop(tx);
        while let Some((id, run)) = rx.recv().await {
            unfinished.remove(&id);
            self.finish_run(app, id, run, next);
        }
        // Only if a task died (user code panics are caught inside, so this is unexpected): run the rest here.
        for id in unfinished {
            tracing::error!("a query execution task stopped early");
            let Some((path, args, identity)) = self.queries.get(&id).map(job) else { continue };
            let run = app.query_in_snapshot(&snapshots[0], &path, args, &identity, false).await;
            self.finish_run(app, id, run, next);
        }
    }

    /// Apply a run of `id` at `self.ts`, re-key it, and send their update to the connections it served that now have
    /// nothing left running (so a connection doesn't wait for the rest of a big round).
    fn finish_run(&mut self, app: &App, id: SharedId, run: QueryRun, next: &mut Vec<SharedId>) {
        let conns: BTreeSet<ConnId> =
            self.queries.get(&id).map(|q| q.subscribers.iter().map(|(c, _)| *c).collect()).unwrap_or_default();
        self.apply_run(app, id, run);
        self.settle_key(id, next);
        for c in conns {
            let complete = self.conns.get(&c).is_some_and(|conn| {
                conn.subs.values().all(|sub| self.queries.get(&sub.shared).is_some_and(|q| !q.running))
            });
            if complete {
                self.flush(c);
            }
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

    fn apply_run(&mut self, app: &App, id: SharedId, run: QueryRun) {
        let dev = app.options().dev;
        let Some(q) = self.queries.get_mut(&id) else { return };
        let log_lines = crate::log::for_client(dev, run.log_lines);
        let outcome = run.result.map_err(|e| ClientError::from_anyhow(&e, dev));
        // Log lines don't count towards the hash: a rerun with the same result sends nothing.
        let hash = match &outcome {
            Ok(value) => hash_result(b"ok", value),
            Err(e) => hash_result(b"err", &serde_json::json!([e.message, e.data])),
        };
        let changed = q.result.as_ref().is_none_or(|r| r.hash != hash);
        q.result = Some(Arc::new(CachedResult { hash, outcome, log_lines }));
        q.deps = run.auth_deps;
        q.running = false;

        let ranges: BTreeSet<(IndexId, Interval)> =
            run.read_set.ranges().map(|(index_id, range)| (index_id, Interval::from_range(range))).collect();
        let ranges: Vec<(IndexId, Interval)> = ranges.into_iter().collect();
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
                    self.flush_set.insert(c);
                }
            }
        }
    }

    /// After a run of `id`: give it the key its auth reads call for, and move the subscribers whose identity doesn't
    /// match that key to shared queries of their own (new ones are pushed to `next`, to run in the same snapshot).
    fn settle_key(&mut self, id: SharedId, next: &mut Vec<SharedId>) {
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
                    self.queries.get_mut(&t).expect("just created").running = true;
                    next.push(t);
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
                    // Another query already serves this key (it is valid at S too): merge into it.
                    let subs: Vec<_> = q.subscribers.iter().copied().collect();
                    for (c, qid) in subs {
                        self.move_sub(c, qid, id, other);
                    }
                }
                _ => {
                    let old = std::mem::replace(&mut q.key, own.clone());
                    if self.by_key.get(&old) == Some(&id) {
                        self.by_key.remove(&old);
                    }
                    self.by_key.insert(own, id);
                }
            }
        }
    }

    /// Cross-check the bookkeeping (debug builds; skipped for large states).
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        if self.subscriptions > 5_000 {
            return;
        }
        let mut subs = 0u64;
        let mut ranges = 0usize;
        for (id, q) in &self.queries {
            assert_eq!(self.by_key.get(&q.key), Some(id), "by_key out of sync");
            assert!(!q.subscribers.is_empty(), "shared query without subscribers");
            assert!(q.result.is_some() || self.pending.contains(id), "never-run query not pending");
            assert!(!q.running, "running outside a round");
            for (c, qid) in &q.subscribers {
                assert_eq!(self.conns[c].subs[qid].shared, *id, "subscriber points elsewhere");
                assert_eq!(q.key.scope, Scope::of(q.key.scope.deps(), &self.conns[c].identity), "subscriber/key mismatch");
            }
            ranges += q.ranges.len();
        }
        for (c, conn) in &self.conns {
            for (qid, sub) in &conn.subs {
                assert!(self.queries[&sub.shared].subscribers.contains(&(*c, *qid)));
                subs += 1;
            }
        }
        assert_eq!(self.by_key.len(), self.queries.len());
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
            .map(|(q, v)| (*q, StateModification::QueryUpdated { query_id: *q, value: json!(v), log_lines: vec![] }))
            .collect();
        Update { query_set, identity: 0, ts, removed, changed }
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
                StateModification::QueryUpdated { query_id: 1, value: json!(11), log_lines: vec![] },
                StateModification::QueryUpdated { query_id: 2, value: json!(21), log_lines: vec![] },
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

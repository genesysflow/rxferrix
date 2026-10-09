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
//! version is open is *current* (valid at the frontier); every other query is *in flight* (queued or running), or,
//! rarely, *parked* (see Liveness). When commits land, a new generation starts:
//!
//! 1. open a snapshot `S` and wait until the write log has every commit up to `S.ts`;
//! 2. close the version of every current query whose read set contains a key written in `(self.ts, S.ts]`, at the
//!    first such commit (found through the interval index, [`super::interval`]; if the log was trimmed, every query is
//!    closed at `self.ts`);
//! 3. set `self.ts = S.ts` and queue the closed queries. Runner tasks (one per snapshot at `S.ts`: `S`, plus snapshots
//!    from idle readers while their ts is still `S.ts`) take queued jobs and run them at `S.ts`.
//!
//! A new generation starts as soon as commits wait and the queue is empty: it does not wait for queries that are still
//! running, so a slow query holds up neither the next generation nor the connections that don't watch it. Commits that
//! land while the queue is not empty are coalesced into one generation, and queries invalidated together run at the
//! same ts. Jobs still queued when a generation starts run at the new one.
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
//! different generations can keep it from ever having all of its queries valid at one ts: two slow queries that commits
//! keep invalidating while they run, each re-queued when it finishes at a later generation than the other. So a
//! connection one of whose queries finished twice since its last update is marked *starving*. Its queries that finish
//! stale are then parked instead of queued again while another of its queries is in flight; when none is, they are
//! queued together, run at one generation, and the connection gets its update there.
//!
//! ## Commands and ordering
//!
//! Commands of one connection are applied in order. A `ModifyQuerySet` whose queries all are current is answered at
//! once at `self.ts` without running anything. Otherwise the connection waits until its queries are all valid at one
//! ts (its later commands are queued behind) and is answered then; the queued commands are then applied in order. So
//! each `ModifyQuerySet` / `Authenticate` gets its own update, in order, and commit-driven updates are pushed into the
//! same mailbox: nothing can be reordered.
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

/// A query whose runs take this long (moving average, µs) is queued in the slow lane.
const SLOW_RUN_US: u64 = 5_000;

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
        let manager = Manager::new(app, metrics, rx);
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
    /// Generations (rounds) started since start.
    pub rounds: u64,
    /// Query executions by the manager since start.
    pub reruns: u64,
    /// Query executions queued by the last generation.
    pub last_round_reruns: u64,
    /// Runs that finished after a commit had already invalidated their result (queued again), since start.
    pub stale_runs: u64,
    /// Of those, the ones held back for a starving connection (see `sync::subscriptions`).
    pub parked: u64,
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
    /// Stale, but held back until a starving subscriber has nothing else in flight (see the module docs).
    parked: bool,
    /// Moving average of its run time, µs.
    cost_us: u64,
}

impl SharedQuery {
    /// Is the newest result valid at the manager's ts? Every query that is not is in flight or parked.
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
    /// Runs of its query finished since the connection's last update: `(Conn::updates then, count)`.
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
    /// A query of its set finished twice without an update in between (see the module docs).
    starving: bool,
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
    /// Parked queries.
    parked: BTreeSet<SharedId>,
    subscriptions: u64,
    /// Commits (or a `Cover` ts) not covered by the frontier yet: a new generation is due.
    need_round: bool,
    /// Retry soon (opening a snapshot failed).
    retry: bool,

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
    /// Jobs queued or running: sequence number -> the frontier when it was queued (ascending with the sequence).
    in_flight: BTreeMap<u64, u64>,
    next_job: u64,
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

/// Does `ranges` (sorted) contain `key` of index `index_id`?
fn touches(ranges: &[(IndexId, Interval)], index_id: IndexId, key: &[u8]) -> bool {
    let from = ranges.partition_point(|(i, _)| *i < index_id);
    ranges[from..].iter().take_while(|(i, _)| *i == index_id).any(|(_, interval)| interval.contains(key))
}

impl Manager {
    fn new(app: &App, metrics: Arc<Metrics>, rx: mpsc::Receiver<Command>) -> Manager {
        let (events_tx, events) = mpsc::unbounded_channel();
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
            ts: 0,
            queries: HashMap::new(),
            next_id: 0,
            by_key: HashMap::new(),
            index: IntervalIndex::new(),
            conns: HashMap::new(),
            covering: BTreeSet::new(),
            parked: BTreeSet::new(),
            subscriptions: 0,
            need_round: false,
            retry: false,
            generation: 0,
            queue: Arc::new(Mutex::new(JobQueue { generation: 0, lanes: [VecDeque::new(), VecDeque::new()] })),
            runners: [0, 0],
            max_runners: app.db().readers().min(threads).max(1),
            max_slow: (app.db().readers().min(threads) / 2).max(1),
            gen_snapshot: None,
            spare: None,
            requesting: false,
            in_flight: BTreeMap::new(),
            next_job: 0,
        }
    }

    async fn run(mut self) {
        loop {
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
            }
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
    }

    // ---- commands ----

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Connect { conn, mailbox } => {
                self.conns.insert(conn, Conn::new(mailbox));
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
                for m in modifications {
                    match m {
                        QuerySetModification::Add { query_id, udf_path, args } => self.subscribe(c, query_id, udf_path, args),
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
            Command::Connect { .. } | Command::Disconnect { .. } => unreachable!("handled in handle()"),
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
            self.drop_query(id);
        }
    }

    /// Forget a query. A job of it that is in flight still runs; its result is ignored.
    fn drop_query(&mut self, id: SharedId) {
        let Some(q) = self.queries.remove(&id) else { return };
        if self.by_key.get(&q.key) == Some(&id) {
            self.by_key.remove(&q.key);
        }
        for (index_id, interval) in &q.ranges {
            self.index.remove(*index_id, interval, id);
        }
        self.parked.remove(&id);
    }

    /// Move a subscription to another shared query (current, or in flight).
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
        }
        if let Some(q) = self.queries.get_mut(&from) {
            q.subscribers.remove(&(c, qid));
            if q.subscribers.is_empty() {
                self.drop_query(from);
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
        let key = QueryKey { path: path.clone(), args: args_key.clone(), scope: Scope::of(AuthDeps::Session, identity) };
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
            parked: false,
            cost_us: 0,
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
        conn.starving = false;
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
            if std::mem::take(&mut q.parked) {
                self.parked.remove(&id);
            }
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
        if !self.parked.is_empty() {
            self.unpark();
        }
        if self.in_flight.is_empty() {
            // Idle: don't pin a WAL snapshot.
            self.gen_snapshot = None;
        }
        let mut advanced = false;
        'pump: loop {
            // A new generation waits until the current one's fast lane is empty (or has no runner): queries invalidated
            // together then run at one ts, and commits landing meanwhile are coalesced. It waits neither for running
            // queries nor for queries known to be slow, which have a lane (and runners) of their own.
            if self.need_round && !advanced && (self.queued(FAST) == 0 || self.runners[FAST] == 0) {
                if self.queries.is_empty() && self.covering.is_empty() {
                    // Nothing to bring up to date. (`self.ts` stays: with no queries, any committed ts is consistent.)
                    self.need_round = false;
                    self.commits.borrow_and_update();
                } else {
                    match self.take_snapshot(app) {
                        Some(snapshot) if snapshot.ts > self.ts => {
                            self.advance(snapshot).await;
                            advanced = true;
                            continue 'pump;
                        }
                        Some(snapshot) => {
                            // Nothing committed since the frontier.
                            self.need_round = *self.commits.borrow_and_update() > self.ts;
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
                    // The fast lane's first runner uses the generation's snapshot; other runners an idle reader's.
                    let shared = if lane == FAST && self.runners[FAST] == 0 { self.gen_snapshot.clone() } else { None };
                    let snapshot = match shared {
                        Some(s) => s,
                        None => match self.take_snapshot(app) {
                            Some(s) if s.ts == self.ts => s,
                            Some(s) if lane == FAST && self.runners[FAST] == 0 && !advanced => {
                                // Newer, and nothing runs at the frontier: move on to it (the queue runs there).
                                self.advance(s).await;
                                advanced = true;
                                continue 'pump;
                            }
                            other => {
                                if other.is_some() {
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
                    self.spawn_runner(app, snapshot, lane);
                }
            }
            break;
        }
        self.spare = None;
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

    fn spawn_runner(&mut self, app: &App, snapshot: Arc<Snapshot>, lane: usize) {
        debug_assert_eq!(snapshot.ts, self.ts);
        self.runners[lane] += 1;
        let (generation, queue, tx, app) = (self.generation, self.queue.clone(), self.events_tx.clone(), app.downgrade());
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
                let run = app.query_in_snapshot(&snapshot, &path, args, &identity, false).await;
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
            // Release the reader before reporting the end.
            drop(snapshot);
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
        match self.write_log.entries_between(frontier, s) {
            // Trimmed: we can't tell what changed, so everything did (all we know is that it was valid at the frontier).
            None => {
                for (id, q) in &mut self.queries {
                    if q.is_current() {
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
                        q.versions.back_mut().expect("current").through = ts - 1;
                        stale.push(id);
                    }
                }
            }
        }
        // From here on, jobs run at `s`. Every query that is still current is valid at `s` (nothing it read changed).
        self.ts = s;
        self.generation += 1;
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).generation = self.generation;
        self.runners = [0, 0];
        self.gen_snapshot = Some(snapshot);
        let invalidated = stale.len();
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
    /// connections it serves (at the frontier, or at the run's ts). A result that is stale already is queued again
    /// (or parked).
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
        self.apply_run(id, run);
        let mut split = Vec::new();
        self.settle_key(id, &mut split);
        self.launch(split);
        for (c, qids) in subscribers {
            if self.try_flush(c, &[self.ts, s]) {
                continue;
            }
            // Not every query of `c` is valid at one ts yet. Count the runs, to notice misaligned ones.
            let Some(conn) = self.conns.get_mut(&c) else { continue };
            let updates = conn.updates;
            for qid in qids {
                let Some(sub) = conn.subs.get_mut(&qid) else { continue };
                if sub.runs.0 != updates {
                    sub.runs = (updates, 0);
                }
                sub.runs.1 += 1;
                conn.starving |= sub.runs.1 >= 2;
            }
        }
        if let Some(q) = self.queries.get(&id)
            && !q.is_current()
            && q.in_flight.is_none()
        {
            let park = self.held_back(id);
            self.metrics.record_stale_run(park);
            if park {
                self.queries.get_mut(&id).expect("checked").parked = true;
                self.parked.insert(id);
            } else {
                self.launch([id]);
            }
        }
    }

    /// Should stale `id` wait (parked) rather than run now: does a starving subscriber still have a query in flight?
    fn held_back(&self, id: SharedId) -> bool {
        let Some(q) = self.queries.get(&id) else { return false };
        q.subscribers.iter().any(|(c, _)| {
            self.conns.get(c).is_some_and(|conn| {
                conn.starving
                    && conn.subs.values().any(|sub| self.queries.get(&sub.shared).is_some_and(|o| o.in_flight.is_some()))
            })
        })
    }

    /// Queue the parked queries no starving connection holds back any more, together (so they run at one ts).
    fn unpark(&mut self) {
        let ready: Vec<SharedId> = self.parked.iter().copied().filter(|id| !self.held_back(*id)).collect();
        self.launch(ready);
    }

    /// Record a run of `id` as its newest version. Its validity ends at the first commit after the run's ts (up to the
    /// frontier) that wrote a key in its read set.
    fn apply_run(&mut self, id: SharedId, run: QueryRun) {
        let frontier = self.ts;
        let dev = self.dev;
        let Some(q) = self.queries.get_mut(&id) else { return };
        let log_lines = crate::log::for_client(dev, run.log_lines);
        let outcome = run.result.map_err(|e| ClientError::from_anyhow(&e, dev));
        // Log lines don't count towards the hash: a rerun with the same result sends nothing.
        let hash = match &outcome {
            Ok(value) => hash_result(b"ok", value),
            Err(e) => hash_result(b"err", &serde_json::json!([e.message, e.data])),
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
        debug_assert!(q.versions.back().is_none_or(|v| v.ts < run.ts), "versions in ts order");
        let changed = q.versions.back().is_none_or(|v| v.result.hash != hash);
        q.versions.push_back(Version { ts: run.ts, through, result: Arc::new(CachedResult { hash, outcome, log_lines }) });
        q.deps = run.auth_deps;
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
            assert!(
                queued == 0 || self.runners[lane] > 0 || self.requesting || self.retry,
                "{queued} jobs queued in lane {lane} without a runner"
            );
        }
        if self.subscriptions > 5_000 {
            return;
        }
        let mut subs = 0u64;
        let mut ranges = 0usize;
        let mut in_flight = 0;
        for (id, q) in &self.queries {
            assert_eq!(self.by_key.get(&q.key), Some(id), "by_key out of sync");
            assert!(!q.subscribers.is_empty(), "shared query without subscribers");
            assert!(q.is_current() || q.in_flight.is_some() || q.parked, "stale query neither in flight nor parked");
            assert!(!(q.parked && q.in_flight.is_some()), "parked and in flight");
            assert_eq!(q.parked, self.parked.contains(id), "parked set out of sync");
            for (i, v) in q.versions.iter().enumerate() {
                assert!(v.ts <= v.through && v.ts <= self.ts, "version past the frontier");
                assert!(i + 1 == q.versions.len() || (v.through != OPEN && v.through < q.versions[i + 1].ts));
            }
            for (c, qid) in &q.subscribers {
                assert_eq!(self.conns[c].subs[qid].shared, *id, "subscriber points elsewhere");
                assert_eq!(q.key.scope, Scope::of(q.key.scope.deps(), &self.conns[c].identity), "subscriber/key mismatch");
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

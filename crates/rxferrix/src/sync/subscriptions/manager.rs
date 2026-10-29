//! The manager task: its state, main loop and command dispatch. The rest of its methods are in the submodules.

mod conns;
mod generations;
mod queries;
mod runs;

use super::*;
use runs::Prepared;

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
    /// The sequence number of that job.
    job: Option<u64>,
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
    /// The subscription that kept the last [`Manager::try_flush`] from flushing: the next one starts there.
    blocker: QueryId,
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
            blocker: 0,
            no_hold_until: None,
        }
    }
}

/// What runner tasks (and snapshot requests) report to the manager.
enum Event {
    /// Job `seq` ran query `id`, in `elapsed`.
    Ran { id: SharedId, seq: u64, run: Prepared, elapsed: Duration },
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
    /// Jobs of dropped queries, by sequence number: runners skip them when they come up (finding them in a lane to
    /// remove them would cost the queue's length per dropped query). A job already running when its query was dropped
    /// is taken off when its result comes in.
    cancelled: std::collections::HashSet<u64>,
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

pub(super) struct Manager {
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

/// Does `ranges` (sorted) contain `key` of index `index_id`?
fn touches(ranges: &[(IndexId, Interval)], index_id: IndexId, key: &[u8]) -> bool {
    let from = ranges.partition_point(|(i, _)| *i < index_id);
    ranges[from..].iter().take_while(|(i, _)| *i == index_id).any(|(_, interval)| interval.contains(key))
}

impl Manager {
    pub(super) fn new(
        app: &App,
        metrics: Arc<Metrics>,
        rx: mpsc::Receiver<Command>,
        cache: Arc<QueryCache>,
    ) -> Manager {
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
            queue: Arc::new(Mutex::new(JobQueue {
                generation: 0,
                lanes: [VecDeque::new(), VecDeque::new()],
                cancelled: Default::default(),
            })),
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

    pub(super) async fn run(mut self) {
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

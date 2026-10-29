//! Running queries: the job lanes, runners and snapshots, and recording finished runs.

use super::*;

/// A finished run, ready to be recorded: its result serialized and hashed, its read set as sorted intervals. Built in
/// the runner task (`Prepared::new`), so the manager's single task doesn't spend O(result size) per run.
pub(super) struct Prepared {
    ts: u64,
    result: Arc<CachedResult>,
    ranges: Vec<(IndexId, Interval)>,
    auth_deps: AuthDeps,
    reads_clock: bool,
    /// It hit the query timeout ([`Manager::retries`]).
    timed_out: bool,
}

impl Prepared {
    pub(super) fn new(run: QueryRun, dev: bool) -> Prepared {
        let timed_out = run.result.as_ref().is_err_and(|e| e.chain().any(|c| c.is::<crate::error::QueryTimeout>()));
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
        Prepared {
            ts: run.ts,
            result: Arc::new(CachedResult { hash, outcome, log_lines }),
            ranges: ranges.into_iter().collect(),
            auth_deps: run.auth_deps,
            reads_clock: run.read_set.reads_clock(),
            timed_out,
        }
    }
}

impl Manager {
    /// Queue `ids` (those not in flight already) to run at the newest generation. Returns how many were queued.
    pub(super) fn launch(&mut self, ids: impl IntoIterator<Item = SharedId>) -> u64 {
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
            q.job = Some(seq);
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

    pub(super) fn queued(&self, lane: usize) -> usize {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).lanes[lane].len()
    }

    pub(super) fn event(&mut self, event: Event) {
        match event {
            Event::Ran { id, seq, run, elapsed } => {
                self.in_flight.remove(&seq);
                match self.queries.get_mut(&id) {
                    Some(q) => {
                        let us = elapsed.as_micros() as u64;
                        q.cost_us = if q.cost_us == 0 { us } else { (q.cost_us * 3 + us) / 4 };
                        self.finish_run(id, run);
                    }
                    // Dropped while it ran: its job was marked cancelled, and is done now.
                    None => {
                        self.queue.lock().unwrap_or_else(|e| e.into_inner()).cancelled.remove(&seq);
                    }
                }
            }
            Event::Lost { id, seq } => {
                tracing::error!("a query execution task stopped early");
                self.in_flight.remove(&seq);
                match self.queries.get_mut(&id) {
                    Some(q) => {
                        (q.in_flight, q.job) = (None, None);
                        self.launch([id]);
                    }
                    None => {
                        self.queue.lock().unwrap_or_else(|e| e.into_inner()).cancelled.remove(&seq);
                    }
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
    pub(super) async fn pump(&mut self, app: &App) {
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
    pub(super) fn take_slow_permit(&mut self, app: &App) -> Option<OwnedSemaphorePermit> {
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
    pub(super) fn take_snapshot(&mut self, app: &App) -> Option<Arc<Snapshot>> {
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

    pub(super) fn spawn_runner(
        &mut self,
        app: &App,
        snapshot: Arc<Snapshot>,
        lane: usize,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        debug_assert_eq!(snapshot.ts, self.ts);
        self.runners[lane] += 1;
        let (generation, queue, tx, app, dev) =
            (self.generation, self.queue.clone(), self.events_tx.clone(), app.downgrade(), self.dev);
        tokio::spawn(async move {
            let mut guard = RunnerGuard { tx: tx.clone(), generation, lane, job: None };
            loop {
                let next = {
                    let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if queue.generation != generation {
                            break None;
                        }
                        match queue.lanes[lane].pop_front() {
                            Some((_, seq, _)) if queue.cancelled.remove(&seq) => continue,
                            next => break next,
                        }
                    }
                };
                let Some((id, seq, (path, args, identity))) = next else { break };
                guard.job = Some((id, seq));
                let Some(app) = app.upgrade() else { break };
                let started = std::time::Instant::now();
                let run = app.query_in_snapshot(&snapshot, &path, args, &identity, crate::guard::Origin::Client).await;
                let elapsed = started.elapsed();
                drop(app);
                // Serialized, hashed and its read set sorted here, not on the manager's one task.
                let run = Prepared::new(run, dev);
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

    /// A run of `id` finished: record its result as the newest version, re-key it, and try to send their update to the
    /// connections it serves (at the frontier, or at the run's ts). A result that is stale already is queued again.
    pub(super) fn finish_run(&mut self, id: SharedId, run: Prepared) {
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
        if run.timed_out {
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
    pub(super) fn next_retry(&self) -> Instant {
        self.retries.values().map(|&(at, _)| at).min().unwrap_or_else(|| Instant::now() + RETRY_MAX)
    }

    /// Run again the timed-out queries that are due and still watched; forget those nobody watches.
    pub(super) fn retry_timed_out(&mut self) {
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

    /// Record a run of `id` as its newest version. Its validity ends at the first commit after the run's ts (up to the
    /// frontier) that wrote a key in its read set.
    pub(super) fn apply_run(&mut self, id: SharedId, run: Prepared) {
        let frontier = self.ts;
        let Some(q) = self.queries.get_mut(&id) else { return };
        let Prepared { ts: _, result, ranges, auth_deps, reads_clock, timed_out: _ } = run;
        let hash = result.hash;
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
        q.versions.push_back(Version { ts: run.ts, through, result });
        q.deps = auth_deps;
        q.reads_clock = reads_clock;
        (q.in_flight, q.job) = (None, None);
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
    pub(super) fn trim_versions(&mut self, id: SharedId) {
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
}

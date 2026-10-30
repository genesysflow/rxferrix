//! Shared queries: subscribing, finding or creating them, idle queries and their eviction.

use super::*;

impl Manager {
    /// Subscribe `(c, qid)` (a shared query that has no result yet is queued).
    pub(super) fn subscribe(&mut self, c: ConnId, qid: QueryId, path: String, args: Value) {
        if self.conns.get(&c).is_some_and(|conn| conn.subs.contains_key(&qid)) {
            // The worker rejects duplicate ids; don't leak if one gets here anyway.
            self.unsubscribe(c, qid);
        }
        let Some(conn) = self.conns.get(&c) else { return };
        let identity = conn.identity.clone();
        let path: Arc<str> = path.into();
        let args_key: Arc<str> = canonical_args(&args).into();
        let id = self.find_or_create(&path, &args_key, args, &identity);
        // A hit when it is answered without a run: from a current query, live or idle.
        self.metrics.record_cache_lookup(self.queries[&id].is_current());
        self.wake(id);
        self.queries.get_mut(&id).expect("just found").subscribers.insert((c, qid));
        let conn = self.conns.get_mut(&c).expect("checked");
        conn.subs.insert(qid, Sub { shared: id, sent: None, runs: (0, 0) });
        conn.touched.insert(qid);
        self.subscriptions += 1;
    }

    pub(super) fn unsubscribe(&mut self, c: ConnId, qid: QueryId) {
        let Some(conn) = self.conns.get_mut(&c) else { return };
        let Some(sub) = conn.subs.remove(&qid) else { return };
        conn.touched.remove(&qid);
        if sub.sent.is_some() {
            conn.removed.push(qid);
        }
        self.detach(c, qid, sub.shared);
    }

    pub(super) fn detach(&mut self, c: ConnId, qid: QueryId, id: SharedId) {
        self.subscriptions -= 1;
        let Some(q) = self.queries.get_mut(&id) else { return };
        q.subscribers.remove(&(c, qid));
        if q.subscribers.is_empty() {
            self.release(id);
        }
    }

    /// The last subscriber of `id` left: keep it idle if it can serve as a cache, else drop it.
    pub(super) fn release(&mut self, id: SharedId) {
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
    pub(super) fn adopt(&mut self, path: Arc<str>, args: Value, identity: Identity, run: QueryRun) {
        // A run past the frontier saw commits the manager hasn't checked yet; and without a budget nothing is kept.
        if run.ts > self.ts || self.idle_budget == 0 {
            return;
        }
        let key = QueryKey { path, args: canonical_args(&args).into(), scope: Scope::of(run.auth_deps, &identity) };
        if self.by_key.contains_key(&key) {
            return;
        }
        let id = self.create(key, args, identity);
        self.apply_run(id, Prepared::new(run, self.dev));
        // Idle if still current (else dropped), then published.
        self.release(id);
        self.sync_published(id);
    }

    /// `id` gets a subscriber: it is no longer idle.
    pub(super) fn wake(&mut self, id: SharedId) {
        let Some(q) = self.queries.get_mut(&id) else { return };
        if let Some(idle) = q.idle.take() {
            self.idle.remove(&idle.seq);
            self.idle_bytes -= idle.bytes;
        }
    }

    pub(super) fn evict_over_budget(&mut self) {
        let mut evicted = 0;
        while self.idle_bytes > self.idle_budget {
            let Some((_, id)) = self.idle.pop_first() else { break };
            self.drop_query(id);
            evicted += 1;
        }
        self.metrics.record_cache_evictions(evicted);
    }

    pub(super) fn expire_idle(&mut self) {
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
    pub(super) fn drop_query(&mut self, id: SharedId) {
        self.retries.remove(&id);
        let Some(q) = self.queries.remove(&id) else { return };
        if let Some(seq) = q.job {
            self.in_flight.remove(&seq);
            self.queue.lock().unwrap_or_else(|e| e.into_inner()).cancelled.insert(seq);
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
    pub(super) fn sync_published(&self, id: SharedId) {
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
    pub(super) fn move_sub(&mut self, c: ConnId, qid: QueryId, from: SharedId, to: SharedId) {
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
    pub(super) fn find_or_create(
        &mut self,
        path: &Arc<str>,
        args_key: &Arc<str>,
        args: Value,
        identity: &Identity,
    ) -> SharedId {
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

    pub(super) fn create(&mut self, key: QueryKey, args: Value, identity: Identity) -> SharedId {
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
            job: None,
            cost_us: 0,
            idle: None,
            reads_clock: false,
        };
        self.queries.insert(id, q);
        id
    }
}

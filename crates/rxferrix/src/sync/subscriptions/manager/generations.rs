//! Generations: advancing the frontier over new commits, holds for starving connections, re-keying.

use super::*;

impl Manager {
    /// Start a generation at `snapshot` (newer than the frontier): close the versions of every query whose read set
    /// contains a key written in `(frontier, snapshot.ts]`, move the frontier, and queue those queries.
    pub(super) async fn advance(&mut self, snapshot: Arc<Snapshot>) {
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
                // In ts order, so the first hit of a query is the commit that ends its version. One hit per query, and
                // none for queries no longer current: a burst of writes over a range thousands of queries read would
                // otherwise collect writes × queries entries.
                let mut hits: HashMap<SharedId, u64> = HashMap::new();
                let queries = &self.queries;
                for entry in &entries {
                    for (index_id, key) in &entry.writes {
                        self.index.stab(*index_id, key, |id| {
                            if !hits.contains_key(&id) && queries.get(&id).is_some_and(SharedQuery::is_current) {
                                hits.insert(id, entry.ts);
                            }
                        });
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

    /// Hold new generations while a starving connection has a query in flight (see the module docs, Liveness), for at
    /// most [`HOLD_MAX`] at a time: then those connections get no hold for [`HOLD_COOLDOWN`], and generations go on.
    pub(super) fn update_hold(&mut self) {
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

    /// After a run of `id`: give it the key its auth reads call for, and move the subscribers whose identity doesn't
    /// match that key to shared queries of their own (new ones are pushed to `split`, to be queued).
    pub(super) fn settle_key(&mut self, id: SharedId, split: &mut Vec<SharedId>) {
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
}

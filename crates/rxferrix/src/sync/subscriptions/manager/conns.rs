//! Connections: flushing their updates in order, slow and removed connections.

use super::*;

impl Manager {
    /// After a command: answer now if every query of the connection is valid at the frontier, otherwise as soon as
    /// they all are valid at one ts (later commands wait behind).
    pub(super) fn settle_conn(&mut self, c: ConnId) {
        if !self.try_flush(c, &[self.ts])
            && let Some(conn) = self.conns.get_mut(&c)
        {
            conn.waiting = true;
        }
    }

    pub(super) fn remove_conn(&mut self, c: ConnId) {
        let Some(conn) = self.conns.remove(&c) else { return };
        for (qid, sub) in conn.subs {
            self.detach(c, qid, sub.shared);
        }
        self.covering.remove(&c);
        self.starving.remove(&c);
    }

    /// Drop a connection that can't keep up; its worker sends `FatalError`.
    pub(super) fn kill(&mut self, c: ConnId) {
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
    ///
    /// Called for every subscriber of each query that finishes, so a connection whose queries finish one by one is
    /// checked once per query. Each check starts at the subscription that failed the previous one ([`Conn::blocker`])
    /// and goes round from there: until it is valid, a check costs one lookup, and the subscriptions it passes once it
    /// is aren't passed again before the end (finished queries stay valid at the frontier until the next generation).
    /// Checking from the first subscription every time instead cost a connection with k queries invalidated together
    /// k²/2 lookups per generation, on the manager's one task.
    pub(super) fn try_flush(&mut self, c: ConnId, candidates: &[u64]) -> bool {
        let Some(conn) = self.conns.get_mut(&c) else { return false };
        let queries = &self.queries;
        let mut valid = None;
        for &ts in candidates {
            if ts < conn.sent.2 {
                continue;
            }
            let from = conn.blocker;
            let blocked = conn
                .subs
                .range(from..)
                .chain(conn.subs.range(..from))
                .find(|(_, sub)| !queries.get(&sub.shared).is_some_and(|q| q.at(ts).is_some()));
            match blocked {
                Some((&qid, _)) => conn.blocker = qid,
                None => {
                    valid = Some(ts);
                    break;
                }
            }
        }
        let Some(ts) = valid else { return false };
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
    pub(super) fn flush(&mut self, c: ConnId, ts: u64) {
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
    pub(super) fn resume(&mut self, c: ConnId) {
        let Some(conn) = self.conns.get_mut(&c) else { return };
        conn.waiting = false;
        while let Some(conn) = self.conns.get_mut(&c)
            && !conn.waiting
            && let Some(cmd) = conn.deferred.pop_front()
        {
            self.apply(cmd);
        }
    }
}

//! The slow lane of reads: a query function whose runs take [`SLOW_RUN_US`] or more on average must hold a slow
//! permit before it takes a reader, and there are about half as many permits as readers ([`slow_permits`]), so slow
//! queries can never hold every reader and keep fast ones waiting. One-off queries (HTTP queries, `ctx.run_query`,
//! the dashboard) take one in `App::call_query`; the subscription manager's slow-lane runners take one each
//! (`sync::subscriptions`, lanes), from the same permits.
//!
//! Measured with `bench_readers` (integration tests, 2026-10-15), fast queries running next to slow ones (tens of ms):
//! with as many slow ones as readers, fast queries went from 2.4k/s at p50 4 ms to 57k/s at p50 0.3 ms (4 cores, 4
//! readers), and from 667/s to 30k/s (2 cores, 2 readers). With the default 1.5 readers per core, fast queries ran
//! 45-95% faster and slow ones half as fast, as they get half the readers.
//!
//! Costs are kept per function, not per arguments: the map stays as small as the set of registered queries. A
//! function runs in the fast lane until its first run is timed. Only the run is timed, not the wait for a reader, so a
//! fast query waiting behind slow ones doesn't end up in the slow lane itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A query whose runs take this long on average (moving average, µs) waits in the slow lane.
pub(crate) const SLOW_RUN_US: u64 = 5_000;

/// How many slow queries may run at once with `readers` readers: half of them, but at least 2 (with one, a slow query
/// waiting behind another on a busy CPU barely ran), and from 2 readers on, at least one reader left for the others.
pub(crate) fn slow_permits(readers: usize) -> usize {
    (readers / 2).max(2).min(readers.saturating_sub(1)).max(1)
}

pub(crate) struct ReadLanes {
    /// Per registered query path: a moving average of its run time (µs).
    costs: Mutex<HashMap<String, u64>>,
    slow: Arc<Semaphore>,
}

impl ReadLanes {
    pub(crate) fn new(readers: usize) -> ReadLanes {
        ReadLanes { costs: Mutex::new(HashMap::new()), slow: Arc::new(Semaphore::new(slow_permits(readers))) }
    }

    /// Wait for a slow permit if `path` is known to be slow (`None` for a fast one). Hold it for the whole run.
    pub(crate) async fn enter(&self, path: &str) -> Option<OwnedSemaphorePermit> {
        let cost = self.costs.lock().unwrap_or_else(|e| e.into_inner()).get(path).copied().unwrap_or(0);
        if cost < SLOW_RUN_US {
            return None;
        }
        self.slow_permit().await
    }

    /// A slow permit, if one is free.
    pub(crate) fn try_slow_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.slow.clone().try_acquire_owned().ok()
    }

    /// Wait for a slow permit (in turn: the semaphore is fair). `None` only if it was closed, which it never is.
    pub(crate) async fn slow_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.slow.clone().acquire_owned().await.ok()
    }

    /// Record a run of the registered query `path` that took `took`.
    pub(crate) fn record(&self, path: &str, took: Duration) {
        let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        let mut costs = self.costs.lock().unwrap_or_else(|e| e.into_inner());
        match costs.get_mut(path) {
            // Weighted towards recent runs, so a query that got slow (or fast) changes lanes within a few runs.
            Some(cost) => *cost = (*cost * 3 + us) / 4,
            None => {
                costs.insert(path.to_string(), us);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_the_readers_but_at_least_two_and_one_left() {
        let permits: Vec<usize> = [1, 2, 3, 4, 6, 12, 24].map(slow_permits).to_vec();
        assert_eq!(permits, [1, 1, 2, 2, 3, 6, 12]);
    }

    #[tokio::test]
    async fn slow_queries_share_half_the_readers() {
        let lanes = ReadLanes::new(6);
        assert!(lanes.enter("q:fast").await.is_none(), "unknown: fast lane");
        lanes.record("q:fast", Duration::from_micros(200));
        lanes.record("q:slow", Duration::from_millis(20));
        assert!(lanes.enter("q:fast").await.is_none());
        let held: Vec<_> = futures::future::join_all((0..3).map(|_| lanes.enter("q:slow"))).await;
        assert!(held.iter().all(Option::is_some));
        let fourth = tokio::time::timeout(Duration::from_millis(20), lanes.enter("q:slow")).await;
        assert!(fourth.is_err(), "a fourth slow query waits for one of the 3 permits");
        drop(held);
        assert!(lanes.enter("q:slow").await.is_some());
        // Faster runs move it back to the fast lane.
        for _ in 0..12 {
            lanes.record("q:slow", Duration::from_micros(100));
        }
        assert!(lanes.enter("q:slow").await.is_none());
    }
}

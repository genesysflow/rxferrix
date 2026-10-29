//! The query cache: current results of shared queries, published for readers outside the manager.

use super::*;

/// The current successful results of shared queries, published by the manager for readers outside it (see the
/// module docs, Query cache).
pub struct QueryCache {
    entries: RwLock<HashMap<QueryKey, Published>>,
    /// The manager's frontier: every published result is valid at it.
    pub(super) frontier: AtomicU64,
    /// Doorkeeper: hashes of queries that recently missed, one per slot. A query is adopted when it misses while its
    /// hash is still there (a second miss soon after the first), so one-off queries never reach the manager.
    recent_misses: Box<[AtomicU64]>,
    /// Off when the cache keeps no idle queries.
    pub(super) adopt: bool,
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
    pub(super) fn new(adopt: bool) -> QueryCache {
        QueryCache {
            entries: RwLock::new(HashMap::new()),
            frontier: AtomicU64::new(0),
            recent_misses: (0..DOORKEEPER_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            adopt,
        }
    }

    /// The doorkeeper ([`QueryCache::recent_misses`]): true on the second miss of `key` in a row of its slot.
    pub(super) fn missed_again(&self, key: &QueryKey) -> bool {
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

    pub(super) fn publish(&self, key: QueryKey, ts: u64, result: Arc<CachedResult>) {
        self.entries.write().unwrap_or_else(|e| e.into_inner()).insert(key, Published { ts, result });
    }

    pub(super) fn unpublish(&self, key: &QueryKey) {
        self.entries.write().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    pub(super) fn unpublish_all<'a>(&self, keys: impl IntoIterator<Item = &'a QueryKey>) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            entries.remove(key);
        }
    }
}

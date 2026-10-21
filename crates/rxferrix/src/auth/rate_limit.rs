//! In-memory token buckets keyed by (endpoint group, client key). Single process, so no shared store is needed.
//!
//! The buckets are spread over [`SHARDS`] maps, each behind its own mutex, so requests from different clients rarely
//! wait on each other: a check is one short critical section, and with a `Copy` key (the `rateLimits` setting's
//! `(rule, IP)`) it allocates nothing once the client's bucket exists.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Size and refill rate of one kind of bucket.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub burst: u32,
    pub per_minute: u32,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
    /// When the bucket will have refilled completely: from then on it carries no information and can be dropped.
    full_at: Instant,
}

struct Shard<K> {
    buckets: HashMap<K, Bucket>,
    /// Prune when the shard grows past this; doubled after a prune that keeps most buckets, so a flood of active
    /// clients costs amortized O(1) per check instead of a scan per check.
    prune_above: usize,
}

const SHARDS: usize = 32;
/// Buckets per shard before full ones are dropped (so about 16k in all).
const PRUNE_ABOVE: usize = 512;

/// Buckets for every key. Full buckets are dropped periodically so memory stays bounded by the number of recently
/// active clients.
pub struct RateLimiter<K = (&'static str, String)> {
    shards: Box<[Mutex<Shard<K>>]>,
    hasher: RandomState,
}

impl<K> Default for RateLimiter<K> {
    fn default() -> Self {
        let shards = (0..SHARDS).map(|_| Mutex::new(Shard { buckets: HashMap::new(), prune_above: PRUNE_ABOVE }));
        RateLimiter { shards: shards.collect(), hasher: RandomState::new() }
    }
}

impl RateLimiter {
    /// Take one token. `Err(retry_after)` when the bucket is empty.
    pub fn check(&self, group: &'static str, key: &str, rule: Rule) -> Result<(), Duration> {
        self.check_at(group, key, rule, Instant::now())
    }

    fn check_at(&self, group: &'static str, key: &str, rule: Rule, now: Instant) -> Result<(), Duration> {
        let per_sec = f64::from(rule.per_minute.max(1)) / 60.0;
        self.take_at((group, key.to_string()), rule.burst, per_sec, now)
    }
}

impl<K: Hash + Eq> RateLimiter<K> {
    /// Take one token from `key`'s bucket, which holds up to `burst` tokens and refills at `per_sec`.
    /// `Err(retry_after)` when it is empty.
    pub(crate) fn take(&self, key: K, burst: u32, per_sec: f64) -> Result<(), Duration> {
        self.take_at(key, burst, per_sec, Instant::now())
    }

    fn take_at(&self, key: K, burst: u32, per_sec: f64, now: Instant) -> Result<(), Duration> {
        let burst = f64::from(burst.max(1));
        let shard = &self.shards[self.hasher.hash_one(&key) as usize % SHARDS];
        let mut shard = shard.lock().unwrap_or_else(|e| e.into_inner());
        if shard.buckets.len() > shard.prune_above {
            shard.buckets.retain(|_, b| b.full_at > now);
            shard.prune_above = (shard.buckets.len() * 2).max(PRUNE_ABOVE);
        }
        let bucket = shard.buckets.entry(key).or_insert(Bucket { tokens: burst, updated: now, full_at: now });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * per_sec).min(burst);
        bucket.updated = now;
        let result = if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64(((1.0 - bucket.tokens) / per_sec).max(1.0)))
        };
        let refill = Duration::try_from_secs_f64((burst - bucket.tokens) / per_sec).unwrap_or(Duration::MAX);
        bucket.full_at = now.checked_add(refill).unwrap_or(now + Duration::from_secs(365 * 24 * 3600));
        result
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().buckets.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_refills() {
        let rl = RateLimiter::default();
        let rule = Rule { burst: 3, per_minute: 60 };
        let t0 = Instant::now();
        for _ in 0..3 {
            rl.check_at("g", "ip", rule, t0).unwrap();
        }
        assert!(rl.check_at("g", "ip", rule, t0).is_err());
        // Separate keys and groups have separate buckets.
        rl.check_at("g", "other", rule, t0).unwrap();
        rl.check_at("h", "ip", rule, t0).unwrap();
        // One token per second.
        rl.check_at("g", "ip", rule, t0 + Duration::from_millis(1100)).unwrap();
        assert!(rl.check_at("g", "ip", rule, t0 + Duration::from_millis(1200)).is_err());
    }

    #[test]
    fn fractional_rates_and_retry_after() {
        let rl = RateLimiter::<(u64, u32)>::default();
        // 2 requests per 3 seconds.
        let t0 = Instant::now();
        rl.take_at((1, 7), 2, 2.0 / 3.0, t0).unwrap();
        rl.take_at((1, 7), 2, 2.0 / 3.0, t0).unwrap();
        let retry = rl.take_at((1, 7), 2, 2.0 / 3.0, t0).unwrap_err();
        assert!((retry.as_secs_f64() - 1.5).abs() < 1e-6, "{retry:?}");
        rl.take_at((1, 7), 2, 2.0 / 3.0, t0 + Duration::from_millis(1501)).unwrap();
    }

    #[test]
    fn full_buckets_are_pruned_without_rescanning_on_every_check() {
        let rl = RateLimiter::<u32>::default();
        let t0 = Instant::now();
        let n = (SHARDS * PRUNE_ABOVE * 3) as u32;
        // Every bucket stays active (1 token per 100 s): pruning can't drop any, and backs off instead.
        for k in 0..n {
            rl.take_at(k, 10, 0.01, t0).unwrap();
        }
        assert_eq!(rl.len(), n as usize);
        // Once they have refilled, the next prune of each shard drops them.
        let later = t0 + Duration::from_secs(1000);
        for k in n..2 * n {
            rl.take_at(k, 10, 0.01, later).unwrap();
        }
        assert!(rl.len() < (2 * n) as usize, "{}", rl.len());
    }
}

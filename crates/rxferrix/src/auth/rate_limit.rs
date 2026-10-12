//! In-memory token buckets keyed by (endpoint group, client key). Single process, so no shared store is needed.

use std::collections::HashMap;
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
}

/// Buckets for every (group, key). Full buckets are dropped periodically so memory stays bounded by the number of
/// recently active clients.
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<(&'static str, String), Bucket>>,
}

const PRUNE_ABOVE: usize = 10_000;

impl RateLimiter {
    /// Take one token. `Err(retry_after)` when the bucket is empty.
    pub fn check(&self, group: &'static str, key: &str, rule: Rule) -> Result<(), Duration> {
        self.check_at(group, key, rule, Instant::now())
    }

    fn check_at(&self, group: &'static str, key: &str, rule: Rule, now: Instant) -> Result<(), Duration> {
        let burst = f64::from(rule.burst.max(1));
        let per_sec = f64::from(rule.per_minute.max(1)) / 60.0;
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() > PRUNE_ABOVE {
            // A bucket that would have refilled completely carries no information.
            buckets.retain(|_, b| now.duration_since(b.updated).as_secs_f64() * per_sec < burst);
        }
        let bucket = buckets.entry((group, key.to_string())).or_insert(Bucket { tokens: burst, updated: now });
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * per_sec).min(burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64(((1.0 - bucket.tokens) / per_sec).max(1.0)))
        }
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
}

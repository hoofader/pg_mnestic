// SPDX-License-Identifier: MIT

//! Per-key token-bucket rate limiting, in-process. Each API key (identified by its stored
//! SHA-256 digest) gets its own bucket, so one tenant's traffic can't starve another's. The
//! state is per-process: with multiple replicas each counts independently, so the effective
//! limit is about N times the configured rate (see DEPLOYMENT.md). The check runs after the
//! key is authenticated, so only valid keys ever allocate a bucket and an unauthenticated
//! flood can't grow the map.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Default sustained rate when `MNESTIC_RATE_LIMIT_PER_MIN` is unset.
const DEFAULT_PER_MINUTE: u32 = 600;

pub struct RateLimiter {
    /// Bucket size (the burst a key can spend at once). Zero disables limiting.
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<Vec<u8>, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// Allow `per_minute` requests sustained, bursting up to `per_minute`. Zero disables it.
    pub fn per_minute(per_minute: u32) -> Arc<Self> {
        Arc::new(Self {
            capacity: per_minute as f64,
            refill_per_sec: per_minute as f64 / 60.0,
            buckets: Mutex::new(HashMap::new()),
        })
    }

    /// A limiter that never rejects, for tests. The env opt-out
    /// (`MNESTIC_RATE_LIMIT_PER_MIN=0`) produces an equivalent limiter.
    pub fn disabled() -> Arc<Self> {
        Self::per_minute(0)
    }

    /// Configure from `MNESTIC_RATE_LIMIT_PER_MIN` (default 600; 0 disables). A non-numeric
    /// value falls back to the default rather than failing startup.
    pub fn from_env() -> Arc<Self> {
        let per_min = std::env::var("MNESTIC_RATE_LIMIT_PER_MIN")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_PER_MINUTE);
        Self::per_minute(per_min)
    }

    /// Consume one token for `key`. Returns false when the key is over its limit. A disabled
    /// limiter always allows.
    pub fn allow(&self, key: &[u8]) -> bool {
        if self.capacity <= 0.0 {
            return true;
        }
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap();
        // Fast path: an existing key refills in place, so the common hit does not allocate a
        // copy of the key (entry() would, since it takes an owned key).
        if let Some(bucket) = buckets.get_mut(key) {
            let elapsed = now.duration_since(bucket.last).as_secs_f64();
            bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            bucket.last = now;
            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                return true;
            }
            return false;
        }
        // First request for this key: start at a full bucket and spend one.
        buckets.insert(
            key.to_vec(),
            Bucket {
                tokens: self.capacity - 1.0,
                last: now,
            },
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_capacity_then_rejects() {
        let limiter = RateLimiter::per_minute(3);
        let key = b"k";
        // Burst of 3 succeeds; the 4th (with no time to refill) is rejected.
        assert!(limiter.allow(key));
        assert!(limiter.allow(key));
        assert!(limiter.allow(key));
        assert!(!limiter.allow(key), "over capacity");
        // A different key has its own budget.
        assert!(limiter.allow(b"other"));
    }

    #[test]
    fn disabled_always_allows() {
        let limiter = RateLimiter::disabled();
        for _ in 0..1000 {
            assert!(limiter.allow(b"k"));
        }
    }
}

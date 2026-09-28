//! Token bucket rate limiter for the Headroom proxy.
//!
//! Rate limits requests and token usage per API key or IP address.
//! Uses a classic token bucket algorithm with time-based refill.
//!
//! A limit of 0 means unlimited for that dimension. A request larger than
//! the whole bucket is admitted once the bucket is full and charged in full,
//! so the balance goes negative and later requests wait it off (upstream
//! `79681226`): otherwise its wait could never come true and the caller
//! would be refused forever.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::Instant;

use lru::LruCache;

// ─── Constants ───────────────────────────────────────────────────────────

/// Maximum rate limiter keys (prevents DoS via spoofed API keys). At the cap
/// the least recently used key is evicted when a new one arrives.
pub const MAX_RATE_LIMITER_BUCKETS: usize = 1000;

// ─── Types ───────────────────────────────────────────────────────────────

/// State of a single token bucket.
#[derive(Debug, Clone)]
struct BucketState {
    tokens: f64,
    last_update: Instant,
}

impl BucketState {
    fn new(initial_tokens: f64) -> Self {
        Self {
            tokens: initial_tokens,
            last_update: Instant::now(),
        }
    }
}

/// Both buckets of one key, so they share one LRU lifecycle.
#[derive(Default)]
struct KeyBuckets {
    requests: Option<BucketState>,
    tokens: Option<BucketState>,
}

/// Rate limiter configuration and state.
pub struct TokenBucketRateLimiter {
    requests_per_minute: f64,
    tokens_per_minute: f64,
    buckets: Mutex<LruCache<String, KeyBuckets>>,
}

/// Result of a rate limit check.
#[derive(Debug, Clone)]
pub struct RateLimitResult {
    pub allowed: bool,
    pub wait_seconds: f64,
}

/// Rate limiter statistics.
#[derive(Debug, Clone)]
pub struct RateLimiterStats {
    pub requests_per_minute: f64,
    pub tokens_per_minute: f64,
    pub active_keys: usize,
}

const ALLOWED: RateLimitResult = RateLimitResult {
    allowed: true,
    wait_seconds: 0.0,
};

impl TokenBucketRateLimiter {
    /// Create a new rate limiter with the given limits (0 = unlimited).
    pub fn new(requests_per_minute: u32, tokens_per_minute: u32) -> Self {
        Self {
            requests_per_minute: requests_per_minute as f64,
            tokens_per_minute: tokens_per_minute as f64,
            buckets: Mutex::new(LruCache::new(
                NonZeroUsize::new(MAX_RATE_LIMITER_BUCKETS).expect("nonzero cap"),
            )),
        }
    }

    /// Whether a token count is ever checked (a TPM limit is set).
    pub fn limits_tokens(&self) -> bool {
        self.tokens_per_minute > 0.0
    }

    /// Refill the bucket for elapsed time, then take `requested` from it.
    fn consume(state: &mut BucketState, requested: f64, rate_per_minute: f64) -> RateLimitResult {
        let now = Instant::now();
        let elapsed = now.duration_since(state.last_update).as_secs_f64();
        state.tokens = rate_per_minute.min(state.tokens + elapsed * (rate_per_minute / 60.0));
        state.last_update = now;

        let required = requested.min(rate_per_minute);
        if state.tokens >= required {
            state.tokens -= requested;
            ALLOWED
        } else {
            RateLimitResult {
                allowed: false,
                wait_seconds: (required - state.tokens) * (60.0 / rate_per_minute),
            }
        }
    }

    /// Check if a request is allowed.
    pub fn check_request(&self, key: &str) -> RateLimitResult {
        let rate = self.requests_per_minute;
        if rate <= 0.0 {
            return ALLOWED;
        }
        let mut buckets = self.buckets.lock().unwrap();
        let entry = buckets.get_or_insert_mut(key.to_string(), KeyBuckets::default);
        let state = entry.requests.get_or_insert_with(|| BucketState::new(rate));
        Self::consume(state, 1.0, rate)
    }

    /// Check if token usage is allowed.
    pub fn check_tokens(&self, key: &str, token_count: u32) -> RateLimitResult {
        let rate = self.tokens_per_minute;
        if rate <= 0.0 {
            return ALLOWED;
        }
        let mut buckets = self.buckets.lock().unwrap();
        let entry = buckets.get_or_insert_mut(key.to_string(), KeyBuckets::default);
        let state = entry.tokens.get_or_insert_with(|| BucketState::new(rate));
        Self::consume(state, token_count as f64, rate)
    }

    /// Get rate limiter statistics.
    pub fn stats(&self) -> RateLimiterStats {
        let buckets = self.buckets.lock().unwrap();
        RateLimiterStats {
            requests_per_minute: self.requests_per_minute,
            tokens_per_minute: self.tokens_per_minute,
            active_keys: buckets.len(),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rate_limiter_has_correct_limits() {
        let limiter = TokenBucketRateLimiter::new(60, 100000);
        let stats = limiter.stats();
        assert_eq!(stats.requests_per_minute, 60.0);
        assert_eq!(stats.tokens_per_minute, 100000.0);
        assert_eq!(stats.active_keys, 0);
    }

    #[test]
    fn check_request_allows_first_request() {
        let limiter = TokenBucketRateLimiter::new(60, 100000);
        let result = limiter.check_request("test_key");
        assert!(result.allowed);
        assert_eq!(result.wait_seconds, 0.0);
    }

    #[test]
    fn check_request_allows_up_to_limit() {
        let limiter = TokenBucketRateLimiter::new(10, 100000); // 10 req/min
        for _ in 0..10 {
            let result = limiter.check_request("test_key");
            assert!(result.allowed);
        }
        // 11th request should be denied
        let result = limiter.check_request("test_key");
        assert!(!result.allowed);
        assert!(result.wait_seconds > 0.0);
    }

    #[test]
    fn check_request_different_keys_independent() {
        let limiter = TokenBucketRateLimiter::new(1, 100000); // 1 req/min
        let result1 = limiter.check_request("key1");
        assert!(result1.allowed);
        let result2 = limiter.check_request("key2");
        assert!(result2.allowed); // Different key, still has full bucket
    }

    #[test]
    fn check_tokens_allows_within_limit() {
        let limiter = TokenBucketRateLimiter::new(60, 1000);
        let result = limiter.check_tokens("test_key", 500);
        assert!(result.allowed);
        assert_eq!(result.wait_seconds, 0.0);
    }

    #[test]
    fn oversized_request_is_admitted_on_a_full_bucket_then_waits_it_off() {
        let limiter = TokenBucketRateLimiter::new(60, 100);
        // Larger than the whole bucket: refusing it would be forever.
        assert!(limiter.check_tokens("test_key", 200).allowed);
        // Charged in full, so the balance is -100 and the next small request
        // waits for the bucket to climb back to 1.
        let next = limiter.check_tokens("test_key", 1);
        assert!(!next.allowed);
        assert!(next.wait_seconds > 60.0, "{}", next.wait_seconds);
    }

    #[test]
    fn oversized_request_waits_for_a_full_bucket_not_forever() {
        let limiter = TokenBucketRateLimiter::new(60, 100);
        assert!(limiter.check_tokens("test_key", 50).allowed);
        let r = limiter.check_tokens("test_key", 500);
        assert!(!r.allowed);
        // Needs the bucket full (100), has ~50: about 30s at 100/min.
        assert!(
            r.wait_seconds > 29.0 && r.wait_seconds <= 30.0,
            "{}",
            r.wait_seconds
        );
    }

    #[test]
    fn zero_limits_are_unlimited() {
        let limiter = TokenBucketRateLimiter::new(0, 0);
        for _ in 0..1000 {
            assert!(limiter.check_request("k").allowed);
            assert!(limiter.check_tokens("k", u32::MAX).allowed);
        }
        assert!(!limiter.limits_tokens());
        assert_eq!(limiter.stats().active_keys, 0);
    }

    #[test]
    fn keys_are_bounded_and_the_least_recent_is_evicted() {
        let limiter = TokenBucketRateLimiter::new(1, 0);
        assert!(limiter.check_request("oldest").allowed);
        assert!(!limiter.check_request("oldest").allowed);
        for i in 0..MAX_RATE_LIMITER_BUCKETS {
            limiter.check_request(&format!("key_{i}"));
        }
        assert_eq!(limiter.stats().active_keys, MAX_RATE_LIMITER_BUCKETS);
        // "oldest" was evicted, so it comes back with a fresh bucket.
        assert!(limiter.check_request("oldest").allowed);
    }

    #[test]
    fn check_tokens_multiple_partial_uses() {
        let limiter = TokenBucketRateLimiter::new(60, 100);
        let r1 = limiter.check_tokens("test_key", 60);
        assert!(r1.allowed);
        let r2 = limiter.check_tokens("test_key", 60);
        assert!(!r2.allowed); // Only 40 left
    }

    #[test]
    fn stats_tracks_active_keys() {
        let limiter = TokenBucketRateLimiter::new(60, 100000);
        limiter.check_request("key1");
        limiter.check_request("key2");
        limiter.check_request("key3");
        let stats = limiter.stats();
        assert_eq!(stats.active_keys, 3);
    }

    #[test]
    fn refill_caps_at_rate() {
        let limiter = TokenBucketRateLimiter::new(10, 100000);
        // Use all tokens
        for _ in 0..10 {
            limiter.check_request("test_key");
        }
        // Wait a bit (in practice, time passes between calls)
        // The bucket should refill but not exceed the rate
        let result = limiter.check_request("test_key");
        // After immediate check, might still be denied or allowed depending on timing
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    fn concurrent_access_safe() {
        use std::sync::Arc;
        use std::thread;

        let limiter = Arc::new(TokenBucketRateLimiter::new(100, 100000));
        let mut handles = vec![];

        for i in 0..10 {
            let limiter = Arc::clone(&limiter);
            handles.push(thread::spawn(move || {
                for _ in 0..10 {
                    let _ = limiter.check_request(&format!("key_{}", i));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let stats = limiter.stats();
        assert!(stats.active_keys <= 10);
    }
}

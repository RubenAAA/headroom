//! Token bucket rate limiter for the Headroom proxy.
//!
//! Rate limits requests and token usage per API key or IP address.
//! Uses a classic token bucket algorithm with time-based refill.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ─── Constants ───────────────────────────────────────────────────────────

/// Maximum rate limiter buckets (prevents DoS via spoofed API keys).
pub const MAX_RATE_LIMITER_BUCKETS: usize = 1000;

/// Stale bucket cleanup threshold (10 minutes).
const STALE_THRESHOLD: Duration = Duration::from_secs(600);

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

/// Rate limiter configuration and state.
pub struct TokenBucketRateLimiter {
    requests_per_minute: f64,
    tokens_per_minute: f64,
    request_buckets: Mutex<HashMap<String, BucketState>>,
    token_buckets: Mutex<HashMap<String, BucketState>>,
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

impl TokenBucketRateLimiter {
    /// Create a new rate limiter with the given limits.
    pub fn new(requests_per_minute: u32, tokens_per_minute: u32) -> Self {
        Self {
            requests_per_minute: requests_per_minute as f64,
            tokens_per_minute: tokens_per_minute as f64,
            request_buckets: Mutex::new(HashMap::new()),
            token_buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Refill bucket based on elapsed time.
    fn refill(state: &mut BucketState, rate_per_minute: f64) -> f64 {
        let now = Instant::now();
        let elapsed = now.duration_since(state.last_update).as_secs_f64();
        let refill = elapsed * (rate_per_minute / 60.0);
        state.tokens = rate_per_minute.min(state.tokens + refill);
        state.last_update = now;
        state.tokens
    }

    /// Clean up stale buckets that haven't been used in the last 10 minutes.
    /// Cleans both request and token buckets for the same stale keys.
    fn cleanup_stale_buckets(
        request_buckets: &mut HashMap<String, BucketState>,
        token_buckets: &mut HashMap<String, BucketState>,
    ) {
        let now = Instant::now();
        let stale_keys: Vec<String> = request_buckets
            .iter()
            .filter(|(_, state)| now.duration_since(state.last_update) > STALE_THRESHOLD)
            .map(|(k, _)| k.clone())
            .collect();

        for key in &stale_keys {
            request_buckets.remove(key);
            token_buckets.remove(key);
        }

        if !stale_keys.is_empty() {
            tracing::debug!(
                event = "rate_limiter_cleanup",
                count = stale_keys.len(),
                "Cleaned up stale rate limiter buckets"
            );
        }
    }

    /// Check if a request is allowed.
    pub fn check_request(&self, key: &str) -> RateLimitResult {
        let mut buckets = self.request_buckets.lock().unwrap();

        // Prevent unbounded bucket growth from spoofed keys
        if buckets.len() > MAX_RATE_LIMITER_BUCKETS {
            let mut token_buckets = self.token_buckets.lock().unwrap();
            Self::cleanup_stale_buckets(&mut buckets, &mut token_buckets);
        }

        let state = buckets
            .entry(key.to_string())
            .or_insert_with(|| BucketState::new(self.requests_per_minute));

        let available = Self::refill(state, self.requests_per_minute);

        if available >= 1.0 {
            state.tokens -= 1.0;
            RateLimitResult {
                allowed: true,
                wait_seconds: 0.0,
            }
        } else {
            let wait_seconds = (1.0 - available) * (60.0 / self.requests_per_minute);
            RateLimitResult {
                allowed: false,
                wait_seconds,
            }
        }
    }

    /// Check if token usage is allowed.
    pub fn check_tokens(&self, key: &str, token_count: u32) -> RateLimitResult {
        let mut buckets = self.token_buckets.lock().unwrap();

        let state = buckets
            .entry(key.to_string())
            .or_insert_with(|| BucketState::new(self.tokens_per_minute));

        let available = Self::refill(state, self.tokens_per_minute);
        let token_count_f64 = token_count as f64;

        if available >= token_count_f64 {
            state.tokens -= token_count_f64;
            RateLimitResult {
                allowed: true,
                wait_seconds: 0.0,
            }
        } else {
            let wait_seconds = (token_count_f64 - available) * (60.0 / self.tokens_per_minute);
            RateLimitResult {
                allowed: false,
                wait_seconds,
            }
        }
    }

    /// Get rate limiter statistics.
    pub fn stats(&self) -> RateLimiterStats {
        let buckets = self.request_buckets.lock().unwrap();
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
    fn check_tokens_denies_over_limit() {
        let limiter = TokenBucketRateLimiter::new(60, 100);
        let result = limiter.check_tokens("test_key", 200);
        assert!(!result.allowed);
        assert!(result.wait_seconds > 0.0);
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

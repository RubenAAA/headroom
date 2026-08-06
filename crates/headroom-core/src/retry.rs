//! Retry/backoff helpers (Rust port of `jitter_delay_ms` / `retry_after_ms` /
//! `RETRYABLE_OVERLOAD_STATUSES` in `headroom/proxy/helpers.py` ~L919-960).

use chrono::{DateTime, Utc};

/// Transient upstream statuses worth retrying with backoff: 429 (rate limit)
/// and 529 (Anthropic `overloaded_error`). Mirrors Python's frozenset.
pub const RETRYABLE_OVERLOAD_STATUSES: [u16; 2] = [429, 529];

/// `true` when `status` is a retryable overload/rate-limit status.
pub fn is_retryable_overload(status: u16) -> bool {
    RETRYABLE_OVERLOAD_STATUSES.contains(&status)
}

/// Exponential backoff with 50-150% jitter:
/// `min(base_ms * 2**attempt, max_ms) * (0.5 + jitter)`.
///
/// `jitter` is the caller-supplied `random()` value in `[0.0, 1.0)`; use
/// [`jitter_delay_ms`] for the production path (thread RNG).
pub fn jitter_delay_ms_with(base_ms: i64, max_ms: i64, attempt: u32, jitter: f64) -> f64 {
    let scaled = (base_ms as f64) * 2f64.powi(attempt as i32);
    let capped = scaled.min(max_ms as f64);
    capped * (0.5 + jitter)
}

/// Exponential backoff with 50-150% jitter using a fresh random draw.
pub fn jitter_delay_ms(base_ms: i64, max_ms: i64, attempt: u32) -> f64 {
    // Tiny xorshift-based draw so the core crate needs no `rand` dep; seeded
    // from the process clock. Randomness quality is irrelevant here — it only
    // spreads retries to avoid thundering herds.
    let jitter = pseudo_random_unit();
    jitter_delay_ms_with(base_ms, max_ms, attempt, jitter)
}

/// A pseudo-random `f64` in `[0.0, 1.0)` seeded from the wall clock.
fn pseudo_random_unit() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // xorshift64 mix so successive calls within the same nanosecond diverge.
    let mut x = nanos.wrapping_mul(2685821657736338717).wrapping_add(1);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    ((x >> 11) as f64) / ((1u64 << 53) as f64)
}

/// Parse an HTTP `Retry-After` header into a millisecond delay, capped at
/// `max_ms`. Accepts numeric seconds or an HTTP-date; `None` when absent or
/// unparseable (caller falls back to exponential backoff). Fails open.
pub fn retry_after_ms(header_value: &str, max_ms: i64) -> Option<f64> {
    let value = header_value.trim();
    if value.is_empty() {
        return None;
    }
    let seconds = match value.parse::<f64>() {
        Ok(s) => s,
        Err(_) => {
            // Try an HTTP-date (RFC 2822 / IMF-fixdate).
            let retry_at = DateTime::parse_from_rfc2822(value).ok()?;
            let now = Utc::now();
            (retry_at.with_timezone(&Utc) - now).num_milliseconds() as f64 / 1000.0
        }
    };
    Some((seconds.max(0.0) * 1000.0).min(max_ms as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_overload(429));
        assert!(is_retryable_overload(529));
        assert!(!is_retryable_overload(500));
        assert!(!is_retryable_overload(200));
    }

    #[test]
    fn jitter_bounds() {
        // With jitter=0.0 → 0.5x capped; jitter→1.0 → 1.5x capped.
        assert_eq!(jitter_delay_ms_with(100, 10_000, 0, 0.0), 50.0);
        assert_eq!(jitter_delay_ms_with(100, 10_000, 0, 1.0), 150.0);
    }

    #[test]
    fn jitter_exponential_growth() {
        // base=100, attempt=3 → 100*8 = 800, *0.5 = 400.
        assert_eq!(jitter_delay_ms_with(100, 10_000, 3, 0.0), 400.0);
    }

    #[test]
    fn jitter_caps_at_max() {
        // base=100, attempt=10 → 102400 capped to 1000, *0.5 = 500.
        assert_eq!(jitter_delay_ms_with(100, 1000, 10, 0.0), 500.0);
    }

    #[test]
    fn jitter_delay_stays_in_range() {
        for _ in 0..100 {
            let d = jitter_delay_ms(100, 10_000, 1);
            // base*2^1 = 200 → [100, 300)
            assert!((100.0..300.0).contains(&d), "delay {d} out of range");
        }
    }

    #[test]
    fn retry_after_numeric_seconds() {
        assert_eq!(retry_after_ms("2", 10_000), Some(2000.0));
        assert_eq!(retry_after_ms("0.5", 10_000), Some(500.0));
    }

    #[test]
    fn retry_after_caps_at_max() {
        assert_eq!(retry_after_ms("100", 5000), Some(5000.0));
    }

    #[test]
    fn retry_after_negative_clamped_to_zero() {
        assert_eq!(retry_after_ms("-5", 10_000), Some(0.0));
    }

    #[test]
    fn retry_after_absent_or_garbage() {
        assert!(retry_after_ms("", 10_000).is_none());
        assert!(retry_after_ms("   ", 10_000).is_none());
        assert!(retry_after_ms("not-a-date", 10_000).is_none());
    }

    #[test]
    fn retry_after_http_date() {
        // A date far in the past → clamped to 0.
        let past = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(retry_after_ms(past, 10_000), Some(0.0));
    }
}

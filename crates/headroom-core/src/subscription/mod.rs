//! Anthropic subscription-window tracking (Rust port of
//! `headroom/subscription/`).
//!
//! Modules:
//! * [`models`] — serde types mirroring the Anthropic OAuth usage API and the
//!   persisted tracker state (same JSON field names as Python for cross-compat).
//! * [`base`] — [`base::QuotaTracker`] trait + registry.
//! * [`client`] — OAuth token resolution + the [`client::SubscriptionFetcher`]
//!   trait (the HTTP-backed impl + async poll loop live in `headroom-proxy`).
//! * [`session_tracking`] — Claude transcript JSONL parsing for per-window
//!   token breakdowns.
//! * [`tracker`] — pure tracker state machine: reconciliation, surge/cache-miss
//!   detection, headroom-contribution accounting, persistence, and post-reset
//!   render synthesis.

pub mod base;
pub mod client;
pub mod models;
pub mod session_tracking;
pub mod tracker;

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};

/// Serialises tests that mutate process-global environment variables.
///
/// `CLAUDE_CONFIG_DIR` is read by both [`client`] and [`session_tracking`], and
/// `cargo test` runs their test modules on threads of a single process. Giving
/// each test its own tempdir makes the *value* unique but not the *variable*:
/// without this lock, one module's `remove_var` lands between another's
/// `set_var` and the read it was setting up, and the reader falls back to the
/// real `~/.claude`. Every test touching these variables must hold this guard
/// for as long as it needs the value it set.
#[cfg(test)]
pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    // A test that panics while holding the guard poisons the lock. Recovering
    // the inner value keeps that one failure from cascading into every other
    // env-dependent test, which would bury the real one.
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Current UTC time.
pub(crate) fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// Render a UTC datetime as `...Z` with second precision, mirroring Python's
/// `_to_utc_iso` (microseconds dropped, `+00:00` → `Z`).
pub(crate) fn to_utc_iso(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Parse an ISO-8601 / RFC3339 timestamp to UTC. `Z` accepted; naive strings
/// assumed UTC. Mirrors Python's `_parse_timestamp`.
pub(crate) fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    if value.is_empty() {
        return None;
    }
    let normalized = value.replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&normalized, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

/// Round to `ndigits` decimals with round-half-to-even (Python's `round`).
pub(crate) fn round_half_even(value: f64, ndigits: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let factor = 10f64.powi(ndigits);
    (value * factor).round_ties_even() / factor
}

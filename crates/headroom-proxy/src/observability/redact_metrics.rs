//! Restore-miss metric for `--redact-sensitive`.
//!
//! A miss means a placeholder reached the client that no session could resolve.
//! It is emitted as an unresolved marker rather than a raw token (see
//! `crate::redact`), so it can no longer be mistaken for a path — but it is
//! still a defect every time, and a counter is what makes it countable.
//! Same `OnceLock` shape as `ctx_metrics`.

use std::sync::OnceLock;

use prometheus::{IntCounter, Registry};

const METRIC_RESTORE_MISSES_TOTAL: &str = "proxy_redact_restore_misses_total";
const METRIC_RESTORE_MISSES_TOTAL_HELP: &str =
    "Placeholders that reached the client unresolved, by any session's map.";

fn restore_misses_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_RESTORE_MISSES_TOTAL,
            METRIC_RESTORE_MISSES_TOTAL_HELP,
        )
        .expect("proxy_redact_restore_misses_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_redact_restore_misses_total registers exactly once");
        c
    })
}

/// Record one placeholder the process could not restore.
pub fn observe_restore_miss() {
    restore_misses_counter(super::prometheus::registry()).inc();
}

/// Misses so far. Used by tests.
pub fn restore_misses_get(registry: &Registry) -> u64 {
    restore_misses_counter(registry).get()
}

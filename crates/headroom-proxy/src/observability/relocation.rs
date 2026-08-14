//! Relocation conservation metric.
//!
//! Prometheus surface for the one thing the relocation pass in
//! [`crate::cache_stabilization::prefix_replay`] must never do: forward fewer
//! `<system-reminder>` spans than the client sent. Kept here rather than at the
//! call site, per the module-doc rule that all instrumentation lives under
//! `observability`.
//!
//! The log line beside it names the turn; this is what an alert can fire on,
//! because reminder loss has so far only ever been noticed by the model
//! behaving oddly several turns later.
//!
//! # Cardinality
//!
//! No labels. One counter, incremented by the number of spans lost.

use std::sync::OnceLock;

use prometheus::{IntCounter, Registry};

use super::metric_names::{
    METRIC_PROXY_CACHE_REMINDER_SPANS_LOST_TOTAL,
    METRIC_PROXY_CACHE_REMINDER_SPANS_LOST_TOTAL_HELP,
};

fn spans_lost_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let counter = IntCounter::new(
            METRIC_PROXY_CACHE_REMINDER_SPANS_LOST_TOTAL,
            METRIC_PROXY_CACHE_REMINDER_SPANS_LOST_TOTAL_HELP,
        )
        .expect("proxy_cache_reminder_spans_lost_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_cache_reminder_spans_lost_total registers exactly once");
        counter
    })
}

/// Record reminder spans a relocation pass failed to conserve.
pub fn observe_reminder_spans_lost(lost: u64) {
    spans_lost_counter(super::prometheus::registry()).inc_by(lost);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_spans_accumulate() {
        let registry = crate::observability::prometheus::registry();
        let before = spans_lost_counter(registry).get();
        observe_reminder_spans_lost(3);
        // Other tests in this binary drive the shared global counter
        // concurrently, so assert a lower bound, not equality.
        assert!(spans_lost_counter(registry).get() >= before + 3);
    }
}

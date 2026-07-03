//! CTX-7 re-cache watchdog metrics.
//!
//! Prometheus surface for the events classified by
//! [`crate::cache_stabilization::usage_observer`]. Kept here (not in
//! the observer) per the module-doc rule: all instrumentation lives
//! under `observability`, never sprinkled across handlers.
//!
//! # Cardinality
//!
//! `reason` is bounded to a fixed vocabulary derived from the PR-E6
//! drift dimensions: `system`, `tools`, `early_messages`, `multi`
//! (more than one axis drifted), and `unknown` (drift detector saw
//! stable bytes — likely a message edit/branch below its window).

use std::sync::OnceLock;

use prometheus::{IntCounter, IntCounterVec, Opts, Registry};

use super::metric_names::{
    LABEL_REASON, METRIC_PROXY_CACHE_RECACHE_EVENTS_TOTAL,
    METRIC_PROXY_CACHE_RECACHE_EVENTS_TOTAL_HELP,
    METRIC_PROXY_CACHE_RECACHE_WASTED_TOKENS_TOTAL,
    METRIC_PROXY_CACHE_RECACHE_WASTED_TOKENS_TOTAL_HELP,
};

fn events_counter(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                METRIC_PROXY_CACHE_RECACHE_EVENTS_TOTAL,
                METRIC_PROXY_CACHE_RECACHE_EVENTS_TOTAL_HELP,
            ),
            &[LABEL_REASON],
        )
        .expect("proxy_cache_recache_events_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_cache_recache_events_total registers exactly once");
        counter
    })
}

fn wasted_tokens_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let counter = IntCounter::new(
            METRIC_PROXY_CACHE_RECACHE_WASTED_TOKENS_TOTAL,
            METRIC_PROXY_CACHE_RECACHE_WASTED_TOKENS_TOTAL_HELP,
        )
        .expect("proxy_cache_recache_wasted_tokens_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_cache_recache_wasted_tokens_total registers exactly once");
        counter
    })
}

/// Map PR-E6 drift dims (comma-joined, e.g. `"tools"` or
/// `"system,tools"`) to the bounded label vocabulary.
fn reason_label(drift_dims: Option<&str>) -> &'static str {
    match drift_dims {
        Some("system") => "system",
        Some("tools") => "tools",
        Some("early_messages") => "early_messages",
        Some(s) if s.contains(',') => "multi",
        _ => "unknown",
    }
}

/// Record one re-cache event. Called by the usage observer off the
/// client byte path.
pub fn observe_recache_event(drift_dims: Option<&str>, wasted_tokens: u64) {
    let registry = super::prometheus::registry();
    events_counter(registry)
        .with_label_values(&[reason_label(drift_dims)])
        .inc();
    wasted_tokens_counter(registry).inc_by(wasted_tokens);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_label_vocabulary_is_bounded() {
        assert_eq!(reason_label(Some("tools")), "tools");
        assert_eq!(reason_label(Some("system")), "system");
        assert_eq!(reason_label(Some("early_messages")), "early_messages");
        assert_eq!(reason_label(Some("system,tools")), "multi");
        assert_eq!(reason_label(Some("weird_future_dim")), "unknown");
        assert_eq!(reason_label(None), "unknown");
    }

    #[test]
    fn counters_accumulate() {
        let registry = crate::observability::prometheus::registry();
        let before = wasted_tokens_counter(registry).get();
        observe_recache_event(Some("tools"), 1234);
        assert_eq!(wasted_tokens_counter(registry).get(), before + 1234);
        assert!(
            events_counter(registry)
                .with_label_values(&["tools"])
                .get()
                >= 1
        );
    }
}

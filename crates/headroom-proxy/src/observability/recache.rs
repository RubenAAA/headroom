//! CTX-7 re-cache watchdog metrics.
//!
//! Prometheus surface for the events classified by
//! [`crate::cache_stabilization::usage_observer`]. Kept here (not in
//! the observer) per the module-doc rule: all instrumentation lives
//! under `observability`, never sprinkled across handlers.
//!
//! # Cardinality
//!
//! `reason` is bounded to a fixed vocabulary derived from direct
//! attribution evidence: structural drift dimensions, causal replay-skip
//! reasons, cache-timing races, and `unknown` when no such evidence
//! exists.

use std::sync::OnceLock;

use prometheus::{IntCounter, IntCounterVec, Opts, Registry};

use super::metric_names::{
    LABEL_REASON, METRIC_PROXY_CACHE_RECACHE_EVENTS_TOTAL,
    METRIC_PROXY_CACHE_RECACHE_EVENTS_TOTAL_HELP, METRIC_PROXY_CACHE_RECACHE_WASTED_TOKENS_TOTAL,
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

/// Map an evidence-only attribution reason to a bounded label vocabulary.
fn reason_label(attribution_reason: Option<&str>) -> &'static str {
    match attribution_reason {
        Some("system") => "system",
        Some("tools") => "tools",
        Some("early_messages") => "early_messages",
        Some("inbound_tail_replaced") => "inbound_tail_replaced",
        Some("unexplained_after_replay") => "unexplained_after_replay",
        Some("aftershock_of_diverged_prefix") => "aftershock_of_diverged_prefix",
        Some("concurrent_turn_in_flight") => "concurrent_turn_in_flight",
        Some("prefix_content_diverged") => "prefix_content_diverged",
        Some("forwarded_count_mismatch") => "forwarded_count_mismatch",
        Some("shorter_than_stored_prefix") => "shorter_than_stored_prefix",
        Some("optimized_shorter_than_prefix") => "optimized_shorter_than_prefix",
        Some("reminder_inside_prefix") => "reminder_inside_prefix",
        Some(s) if s.contains(',') => "multi",
        // A non-empty structural dimension added in the future is still
        // evidence, but must not create an unbounded label value.
        Some(_) => "structural_drift",
        None => "unknown",
    }
}

fn observe_wasted_tokens(counter: &IntCounter, wasted_tokens: Option<u64>) {
    if let Some(wasted_tokens) = wasted_tokens {
        counter.inc_by(wasted_tokens);
    }
}

/// Record one re-cache event. Called by the usage observer off the
/// client byte path.
pub fn observe_recache_event(attribution_reason: Option<&str>, wasted_tokens: Option<u64>) {
    let registry = super::prometheus::registry();
    events_counter(registry)
        .with_label_values(&[reason_label(attribution_reason)])
        .inc();
    observe_wasted_tokens(wasted_tokens_counter(registry), wasted_tokens);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_label_vocabulary_is_bounded() {
        assert_eq!(reason_label(Some("tools")), "tools");
        assert_eq!(reason_label(Some("system")), "system");
        assert_eq!(reason_label(Some("early_messages")), "early_messages");
        assert_eq!(
            reason_label(Some("inbound_tail_replaced")),
            "inbound_tail_replaced"
        );
        assert_eq!(
            reason_label(Some("unexplained_after_replay")),
            "unexplained_after_replay"
        );
        assert_eq!(reason_label(Some("system,tools")), "multi");
        assert_eq!(
            reason_label(Some("prefix_content_diverged")),
            "prefix_content_diverged"
        );
        assert_eq!(
            reason_label(Some("reminder_inside_prefix")),
            "reminder_inside_prefix"
        );
        // A timing race is not structural drift, and must not land in that
        // bucket on the dashboard.
        assert_eq!(
            reason_label(Some("concurrent_turn_in_flight")),
            "concurrent_turn_in_flight"
        );
        assert_eq!(reason_label(Some("weird_future_dim")), "structural_drift");
        assert_eq!(reason_label(None), "unknown");
    }

    #[test]
    fn counters_accumulate() {
        let registry = crate::observability::prometheus::registry();
        let before = wasted_tokens_counter(registry).get();
        observe_recache_event(Some("tools"), Some(1234));
        // Other tests in this binary also drive the shared global
        // counter concurrently, so assert a lower bound, not equality.
        assert!(wasted_tokens_counter(registry).get() >= before + 1234);
        assert!(events_counter(registry).with_label_values(&["tools"]).get() >= 1);
    }

    #[test]
    fn branch_cache_build_does_not_increment_prometheus_waste() {
        let counter = IntCounter::new("branch_build_waste_test", "test counter").unwrap();
        observe_wasted_tokens(&counter, None);
        assert_eq!(counter.get(), 0);

        observe_wasted_tokens(&counter, Some(123));
        assert_eq!(
            counter.get(),
            123,
            "charged events still increment normally"
        );
    }
}

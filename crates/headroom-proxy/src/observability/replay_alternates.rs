//! Branch-prefix eviction metric.
//!
//! Prometheus surface for the one thing the alternates list in
//! [`crate::cache_stabilization::prefix_replay`] does silently: drop a stored
//! branch prefix when the count or message budget is full. Kept here rather
//! than at the call site, per the module-doc rule that all instrumentation
//! lives under `observability`.
//!
//! An evicted stream busts on its next turn, and nothing else in the proxy
//! says so — the turn simply reports a miss with no stored prefix to blame.
//! Whether the caps are worth raising is a question only this counter answers,
//! since the budget is spent by conversation depth and the deepest sessions
//! hold the fewest branches.
//!
//! # Cardinality
//!
//! No labels. One counter, incremented by the number of prefixes dropped.

use std::sync::OnceLock;

use prometheus::{IntCounter, Registry};

use super::metric_names::{
    METRIC_PROXY_CACHE_REPLAY_ALTERNATES_EVICTED_TOTAL,
    METRIC_PROXY_CACHE_REPLAY_ALTERNATES_EVICTED_TOTAL_HELP,
};

fn evicted_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let counter = IntCounter::new(
            METRIC_PROXY_CACHE_REPLAY_ALTERNATES_EVICTED_TOTAL,
            METRIC_PROXY_CACHE_REPLAY_ALTERNATES_EVICTED_TOTAL_HELP,
        )
        .expect("proxy_cache_replay_alternates_evicted_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_cache_replay_alternates_evicted_total registers exactly once");
        counter
    })
}

/// Record branch prefixes dropped because the alternates budget was full.
pub fn observe_alternates_evicted(evicted: u64) {
    evicted_counter(super::prometheus::registry()).inc_by(evicted);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evictions_accumulate() {
        let registry = crate::observability::prometheus::registry();
        let before = evicted_counter(registry).get();
        observe_alternates_evicted(2);
        // Other tests in this binary drive the shared global counter
        // concurrently, so assert a lower bound, not equality.
        assert!(evicted_counter(registry).get() >= before + 2);
    }
}

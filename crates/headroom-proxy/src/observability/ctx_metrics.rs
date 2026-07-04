//! CTX-5/6 Prometheus metrics for offload, recall, and search activity.
//!
//! Follows the `recache.rs` pattern: `OnceLock`-backed counters, `observe_*`
//! functions for call sites, `*_get` functions for the `/ctx/stats` endpoint.

use std::sync::OnceLock;

use prometheus::{IntCounter, Registry};

use super::metric_names::*;

// ── Counters ──

fn offloaded_bytes_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_OFFLOADED_BYTES_TOTAL,
            METRIC_CTX_OFFLOADED_BYTES_TOTAL_HELP,
        )
        .expect("ctx_offloaded_bytes_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_offloaded_bytes_total registers exactly once");
        c
    })
}

fn offloaded_blocks_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_OFFLOADED_BLOCKS_TOTAL,
            METRIC_CTX_OFFLOADED_BLOCKS_TOTAL_HELP,
        )
        .expect("ctx_offloaded_blocks_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_offloaded_blocks_total registers exactly once");
        c
    })
}

fn recall_injections_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_RECALL_INJECTIONS_TOTAL,
            METRIC_CTX_RECALL_INJECTIONS_TOTAL_HELP,
        )
        .expect("ctx_recall_injections_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_recall_injections_total registers exactly once");
        c
    })
}

fn search_queries_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_SEARCH_QUERIES_TOTAL,
            METRIC_CTX_SEARCH_QUERIES_TOTAL_HELP,
        )
        .expect("ctx_search_queries_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_search_queries_total registers exactly once");
        c
    })
}

// ── Emit helpers (called by CTX-3/4/5 code paths) ──

/// Record bytes offloaded on the request path. Called from `ctx_offload.rs`.
pub fn observe_offloaded(bytes: u64) {
    let reg = super::prometheus::registry();
    offloaded_bytes_counter(reg).inc_by(bytes);
    offloaded_blocks_counter(reg).inc();
}

/// Record a recall injection. Called from `ctx/inject.rs`.
pub fn observe_recall_injection() {
    recall_injections_counter(super::prometheus::registry()).inc();
}

/// Record a search query served. Called from `ctx/endpoints.rs`.
pub fn observe_search_query() {
    search_queries_counter(super::prometheus::registry()).inc();
}

// ── Getter helpers (called by /ctx/stats endpoint) ──

pub fn offloaded_bytes_get(registry: &Registry) -> u64 {
    offloaded_bytes_counter(registry).get()
}

pub fn offloaded_blocks_get(registry: &Registry) -> u64 {
    offloaded_blocks_counter(registry).get()
}

pub fn recall_injections_get(registry: &Registry) -> u64 {
    recall_injections_counter(registry).get()
}

pub fn search_queries_get(registry: &Registry) -> u64 {
    search_queries_counter(registry).get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let reg = super::super::prometheus::registry();
        let before_bytes = offloaded_bytes_get(reg);
        let before_blocks = offloaded_blocks_get(reg);
        observe_offloaded(1234);
        assert_eq!(offloaded_bytes_get(reg), before_bytes + 1234);
        assert_eq!(offloaded_blocks_get(reg), before_blocks + 1);

        let before_inj = recall_injections_get(reg);
        observe_recall_injection();
        assert_eq!(recall_injections_get(reg), before_inj + 1);

        let before_q = search_queries_get(reg);
        observe_search_query();
        assert_eq!(search_queries_get(reg), before_q + 1);
    }
}

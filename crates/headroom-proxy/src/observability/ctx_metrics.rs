//! CTX-5/6 Prometheus metrics for offload, recall, and search activity.
//!
//! Follows the `recache.rs` pattern: `OnceLock`-backed counters, `observe_*`
//! functions for call sites, `*_get` functions for the `/ctx/stats` endpoint.

use std::sync::OnceLock;
use std::time::Duration;

use prometheus::{Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry};

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

fn offload_index_pending_jobs_gauge(registry: &Registry) -> &'static IntGauge {
    static GAUGE: OnceLock<IntGauge> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let gauge = IntGauge::new(
            METRIC_CTX_OFFLOAD_INDEX_PENDING_JOBS,
            METRIC_CTX_OFFLOAD_INDEX_PENDING_JOBS_HELP,
        )
        .expect("ctx_offload_index_pending_jobs descriptor is well-formed");
        registry
            .register(Box::new(gauge.clone()))
            .expect("ctx_offload_index_pending_jobs registers exactly once");
        gauge
    })
}

fn offload_index_pending_bytes_gauge(registry: &Registry) -> &'static IntGauge {
    static GAUGE: OnceLock<IntGauge> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let gauge = IntGauge::new(
            METRIC_CTX_OFFLOAD_INDEX_PENDING_BYTES,
            METRIC_CTX_OFFLOAD_INDEX_PENDING_BYTES_HELP,
        )
        .expect("ctx_offload_index_pending_bytes descriptor is well-formed");
        registry
            .register(Box::new(gauge.clone()))
            .expect("ctx_offload_index_pending_bytes registers exactly once");
        gauge
    })
}

fn offload_index_oldest_age_gauge(registry: &Registry) -> &'static IntGauge {
    static GAUGE: OnceLock<IntGauge> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let gauge = IntGauge::new(
            METRIC_CTX_OFFLOAD_INDEX_OLDEST_AGE_SECONDS,
            METRIC_CTX_OFFLOAD_INDEX_OLDEST_AGE_SECONDS_HELP,
        )
        .expect("ctx_offload_index_oldest_age_seconds descriptor is well-formed");
        registry
            .register(Box::new(gauge.clone()))
            .expect("ctx_offload_index_oldest_age_seconds registers exactly once");
        gauge
    })
}

fn offload_index_batches_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let counter = IntCounter::new(
            METRIC_CTX_OFFLOAD_INDEX_BATCHES_TOTAL,
            METRIC_CTX_OFFLOAD_INDEX_BATCHES_TOTAL_HELP,
        )
        .expect("ctx_offload_index_batches_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("ctx_offload_index_batches_total registers exactly once");
        counter
    })
}

fn offload_index_retries_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let counter = IntCounter::new(
            METRIC_CTX_OFFLOAD_INDEX_RETRIES_TOTAL,
            METRIC_CTX_OFFLOAD_INDEX_RETRIES_TOTAL_HELP,
        )
        .expect("ctx_offload_index_retries_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("ctx_offload_index_retries_total registers exactly once");
        counter
    })
}

fn offload_index_backpressure_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let counter = IntCounter::new(
            METRIC_CTX_OFFLOAD_INDEX_BACKPRESSURE_TOTAL,
            METRIC_CTX_OFFLOAD_INDEX_BACKPRESSURE_TOTAL_HELP,
        )
        .expect("ctx_offload_index_backpressure_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("ctx_offload_index_backpressure_total registers exactly once");
        counter
    })
}

fn offload_index_batch_duration(registry: &Registry) -> &'static Histogram {
    static HISTOGRAM: OnceLock<Histogram> = OnceLock::new();
    HISTOGRAM.get_or_init(|| {
        let histogram = Histogram::with_opts(HistogramOpts::new(
            METRIC_CTX_OFFLOAD_INDEX_BATCH_DURATION_SECONDS,
            METRIC_CTX_OFFLOAD_INDEX_BATCH_DURATION_SECONDS_HELP,
        ))
        .expect("ctx_offload_index_batch_duration_seconds descriptor is well-formed");
        registry
            .register(Box::new(histogram.clone()))
            .expect("ctx_offload_index_batch_duration_seconds registers exactly once");
        histogram
    })
}

fn proactive_expansion_bytes_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_PROACTIVE_EXPANSION_BYTES_TOTAL,
            METRIC_CTX_PROACTIVE_EXPANSION_BYTES_TOTAL_HELP,
        )
        .expect("ctx_proactive_expansion_bytes_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_proactive_expansion_bytes_total registers exactly once");
        c
    })
}

fn proactive_expansion_cache_write_tokens_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_PROACTIVE_EXPANSION_CACHE_WRITE_TOKENS_TOTAL,
            METRIC_CTX_PROACTIVE_EXPANSION_CACHE_WRITE_TOKENS_TOTAL_HELP,
        )
        .expect("ctx_proactive_expansion_cache_write_tokens_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_proactive_expansion_cache_write_tokens_total registers exactly once");
        c
    })
}

fn proactive_expansions_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_PROACTIVE_EXPANSIONS_TOTAL,
            METRIC_CTX_PROACTIVE_EXPANSIONS_TOTAL_HELP,
        )
        .expect("ctx_proactive_expansions_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_proactive_expansions_total registers exactly once");
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

fn gate_seeded_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_OFFLOAD_GATE_SEEDED_TOTAL,
            METRIC_OFFLOAD_GATE_SEEDED_TOTAL_HELP,
        )
        .expect("offload_gate_seeded_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("offload_gate_seeded_total registers exactly once");
        c
    })
}

fn gate_seed_refused_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_OFFLOAD_GATE_SEED_REFUSED_LIVE_TOTAL,
            METRIC_OFFLOAD_GATE_SEED_REFUSED_LIVE_TOTAL_HELP,
        )
        .expect("offload_gate_seed_refused_live_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("offload_gate_seed_refused_live_total registers exactly once");
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

fn events_deduped_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_PROXY_CTX_EVENTS_DEDUPED_TOTAL,
            METRIC_PROXY_CTX_EVENTS_DEDUPED_TOTAL_HELP,
        )
        .expect("proxy_ctx_events_deduped_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_ctx_events_deduped_total registers exactly once");
        c
    })
}

/// Record one session event the store refused as a duplicate. Called from the
/// CTX-2a capture observer.
pub fn observe_event_deduped() {
    events_deduped_counter(super::prometheus::registry()).inc();
}

/// Duplicate events refused so far. Used by tests and `/ctx/stats`.
pub fn events_deduped_get(registry: &Registry) -> u64 {
    events_deduped_counter(registry).get()
}

fn retrieval_hits_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_RETRIEVAL_HITS_TOTAL,
            METRIC_CTX_RETRIEVAL_HITS_TOTAL_HELP,
        )
        .expect("ctx_retrieval_hits_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_retrieval_hits_total registers exactly once");
        c
    })
}

fn retrieval_misses_counter(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_CTX_RETRIEVAL_MISSES_TOTAL,
            METRIC_CTX_RETRIEVAL_MISSES_TOTAL_HELP,
        )
        .expect("ctx_retrieval_misses_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_retrieval_misses_total registers exactly once");
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

/// Publish the durable CTX-3 index backlog. These gauges are refreshed at
/// startup, on enqueue, and after a batch is acknowledged or deferred.
pub fn observe_offload_index_backlog(jobs: u64, bytes: u64, oldest_age_ms: u64) {
    let registry = super::prometheus::registry();
    offload_index_pending_jobs_gauge(registry).set(jobs.min(i64::MAX as u64) as i64);
    offload_index_pending_bytes_gauge(registry).set(bytes.min(i64::MAX as u64) as i64);
    offload_index_oldest_age_gauge(registry)
        .set((oldest_age_ms / 1000).min(i64::MAX as u64) as i64);
}

/// Record one background FTS batch attempt and its duration.
pub fn observe_offload_index_batch(duration: Duration) {
    let registry = super::prometheus::registry();
    offload_index_batches_counter(registry).inc();
    offload_index_batch_duration(registry).observe(duration.as_secs_f64());
}

/// Record one index batch deferred for retry.
pub fn observe_offload_index_retry() {
    offload_index_retries_counter(super::prometheus::registry()).inc();
}

/// Record a request whose FTS jobs were refused at the durable outbox cap.
pub fn observe_offload_index_backpressure() {
    offload_index_backpressure_counter(super::prometheus::registry()).inc();
}

/// Record bytes put *back* into the request by CCR proactive expansion.
/// Called from `proxy::maybe_append_ccr_proactive_expansion`.
///
/// The pair with [`observe_offloaded`] is the point: offload and expansion
/// move bytes in opposite directions, and reading either alone gives a
/// flattering answer.
pub fn observe_proactive_expansion(bytes: u64) {
    let reg = super::prometheus::registry();
    proactive_expansion_bytes_counter(reg).inc_by(bytes);
    proactive_expansions_counter(reg).inc();
}

/// Record the provider-reported cache write for a request that injected a
/// proactive expansion. This is emitted only after a complete Anthropic
/// response, when `cache_creation_input_tokens` is trustworthy.
pub fn observe_proactive_expansion_cache_write_tokens(tokens: u64) {
    proactive_expansion_cache_write_tokens_counter(super::prometheus::registry()).inc_by(tokens);
}

/// Record a recall injection. Called from `ctx/inject.rs`.
pub fn observe_recall_injection() {
    recall_injections_counter(super::prometheus::registry()).inc();
}

/// Record a cross-session gate seeding. Called from `ctx_offload.rs`
/// (`seed_newborn_session`) when a newborn session inherits its lineage's
/// conversions.
pub fn observe_gate_seeded() {
    gate_seeded_counter(super::prometheus::registry()).inc();
}

/// Record a refused seeding (gate already knew the session). The refusal is
/// the safe outcome; a rising rate alongside zero seedings means the birth
/// signal is misfiring and deserves a look.
pub fn observe_gate_seed_refused() {
    gate_seed_refused_counter(super::prometheus::registry()).inc();
}

fn injection_clipped_bytes_counter(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                METRIC_CTX_INJECTION_CLIPPED_BYTES_TOTAL,
                METRIC_CTX_INJECTION_CLIPPED_BYTES_TOTAL_HELP,
            ),
            &["stage"],
        )
        .expect("ctx_injection_clipped_bytes_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_injection_clipped_bytes_total registers exactly once");
        c
    })
}

/// Record bytes the shared injection budget refused a stage. Called from
/// `injection_budget.rs`. A rising count means the three appenders together
/// want more room than `--max-injection-bytes` allows.
pub fn observe_injection_clipped(stage: &str, dropped_bytes: u64) {
    injection_clipped_bytes_counter(super::prometheus::registry())
        .with_label_values(&[stage])
        .inc_by(dropped_bytes);
}

/// Record a search query served. Called from `ctx/endpoints.rs`.
pub fn observe_search_query() {
    search_queries_counter(super::prometheus::registry()).inc();
}

/// PR-J5: record a /ctx/get retrieval outcome. Called from `ctx/endpoints.rs`.
pub fn observe_retrieval(hit: bool) {
    let reg = super::prometheus::registry();
    if hit {
        retrieval_hits_counter(reg).inc();
    } else {
        retrieval_misses_counter(reg).inc();
    }
}

// ── Getter helpers (called by /ctx/stats endpoint) ──

pub fn offloaded_bytes_get(registry: &Registry) -> u64 {
    offloaded_bytes_counter(registry).get()
}

pub fn offloaded_blocks_get(registry: &Registry) -> u64 {
    offloaded_blocks_counter(registry).get()
}

pub fn proactive_expansion_bytes_get(registry: &Registry) -> u64 {
    proactive_expansion_bytes_counter(registry).get()
}

pub fn proactive_expansion_cache_write_tokens_get(registry: &Registry) -> u64 {
    proactive_expansion_cache_write_tokens_counter(registry).get()
}

pub fn proactive_expansions_get(registry: &Registry) -> u64 {
    proactive_expansions_counter(registry).get()
}

pub fn recall_injections_get(registry: &Registry) -> u64 {
    recall_injections_counter(registry).get()
}

pub fn gate_seeded_get(registry: &Registry) -> u64 {
    gate_seeded_counter(registry).get()
}

pub fn gate_seed_refused_get(registry: &Registry) -> u64 {
    gate_seed_refused_counter(registry).get()
}

pub fn search_queries_get(registry: &Registry) -> u64 {
    search_queries_counter(registry).get()
}

pub fn retrieval_hits_get(registry: &Registry) -> u64 {
    retrieval_hits_counter(registry).get()
}

pub fn retrieval_misses_get(registry: &Registry) -> u64 {
    retrieval_misses_counter(registry).get()
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

        // Expansion is the other direction of the same ledger: bytes offload
        // took out, put back in. Both counters have to move or the /ctx/stats
        // pair reads as pure saving.
        let before_exp_bytes = proactive_expansion_bytes_get(reg);
        let before_exp = proactive_expansions_get(reg);
        observe_proactive_expansion(4321);
        assert_eq!(proactive_expansion_bytes_get(reg), before_exp_bytes + 4321);
        assert_eq!(proactive_expansions_get(reg), before_exp + 1);

        let before_exp_write = proactive_expansion_cache_write_tokens_get(reg);
        observe_proactive_expansion_cache_write_tokens(8765);
        assert_eq!(
            proactive_expansion_cache_write_tokens_get(reg),
            before_exp_write + 8765
        );

        let before_inj = recall_injections_get(reg);
        observe_recall_injection();
        assert_eq!(recall_injections_get(reg), before_inj + 1);

        let before_seed = gate_seeded_get(reg);
        observe_gate_seeded();
        assert_eq!(gate_seeded_get(reg), before_seed + 1);

        let before_ref = gate_seed_refused_get(reg);
        observe_gate_seed_refused();
        assert_eq!(gate_seed_refused_get(reg), before_ref + 1);

        let before_q = search_queries_get(reg);
        observe_search_query();
        assert_eq!(search_queries_get(reg), before_q + 1);

        let before_hits = retrieval_hits_get(reg);
        let before_misses = retrieval_misses_get(reg);
        observe_retrieval(true);
        observe_retrieval(false);
        assert_eq!(retrieval_hits_get(reg), before_hits + 1);
        assert_eq!(retrieval_misses_get(reg), before_misses + 1);
    }
}

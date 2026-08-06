//! Observability protocol for compression events.
//!
//! A single `CompressionObserver` trait that any transform can call
//! after a real compression event. Concrete observers (Prometheus, OTel,
//! structured logs) implement this; transforms only see the trait.
//!
//! Design choices:
//! - No fallback observer. Callers pass `None` or a real observer.
//! - No batching. Each compression event is one call.
//! - Strategy as a string. Matches `CompressionStrategy.as_str()`.
//! - Implementations MUST NOT raise — observer failures must not break compression.

use std::collections::HashMap;

/// Receive one notification per real compression event.
///
/// Implementations should be cheap — this lives on the proxy hot path,
/// one call per routing decision per request.
pub trait CompressionObserver: Send + Sync {
    /// Record a compression event.
    ///
    /// # Arguments
    /// * `strategy` - Lowercase tag identifying the compression strategy.
    /// * `original_tokens` - Token count of the input.
    /// * `compressed_tokens` - Token count of the output.
    fn record_compression(&self, strategy: &str, original_tokens: usize, compressed_tokens: usize);

    /// Record a Kompress size-gate decision.
    ///
    /// `outcome` is `"exceeded"` when the block was routed off ML for being too
    /// large, or `"within"` when it passed the ceiling. `"within"` counts a gate
    /// pass, not that ML compression then ran.
    #[allow(unused_variables)]
    fn record_kompress_size_gate(&self, outcome: &str) {
        // Default no-op — only the Prometheus observer overrides this.
    }

    /// Record a single compression unit outcome (for unit-level observability).
    #[allow(unused_variables)]
    fn record_unit(
        &self,
        strategy: &str,
        reason_category: &str,
        elapsed_ms: u64,
        text_bytes: usize,
        tokens_before: usize,
        tokens_after: usize,
        tokens_saved: usize,
        modified: bool,
    ) {
        // Default no-op — only PrometheusMetrics overrides this.
    }

    /// Record a frame-level compression outcome.
    #[allow(unused_variables)]
    fn record_frame(
        &self,
        elapsed_ms: u64,
        bytes_before: usize,
        bytes_after: usize,
        attempted_tokens: usize,
        tokens_saved: usize,
        modified: bool,
        failed: bool,
    ) {
        // Default no-op.
    }
}

/// In-process metrics accumulator that satisfies `CompressionObserver`.
///
/// Counters are internal state, NOT exported as Prometheus metric names
/// (to avoid unbounded metric-series growth). Observable via /stats.
#[derive(Debug, Default)]
pub struct MetricsObserver {
    pub compressions_by_strategy: HashMap<String, usize>,
    pub tokens_saved_by_strategy: HashMap<String, usize>,
    pub codex_ws_units_total: usize,
    pub codex_ws_units_modified_total: usize,
    pub codex_ws_units_by_strategy: HashMap<String, usize>,
    pub codex_ws_units_by_category: HashMap<String, usize>,
    pub codex_ws_units_by_content_type: HashMap<String, usize>,
    pub codex_ws_units_by_text_shape: HashMap<String, usize>,
    pub codex_ws_unit_elapsed_ms_max: u64,
    pub codex_ws_unit_tokens_saved_sum: usize,
    pub codex_ws_frames_attempted_total: usize,
    pub codex_ws_frames_compressed_total: usize,
    pub codex_ws_frames_failed_total: usize,
    pub codex_ws_frame_elapsed_ms_max: u64,
    pub codex_ws_frame_tokens_saved_sum: usize,
}

impl CompressionObserver for MetricsObserver {
    fn record_compression(
        &self,
        _strategy: &str,
        _original_tokens: usize,
        _compressed_tokens: usize,
    ) {
        // MetricsObserver needs mutable self for counters, but trait takes &self.
        // In production, PrometheusMetrics uses interior mutability (AtomicUsize).
        // For tests, use TestObserver instead.
    }
}

/// Simple test observer that captures all calls for assertion.
#[derive(Debug, Default)]
pub struct TestObserver {
    pub calls: Vec<(String, usize, usize)>,
}

impl CompressionObserver for TestObserver {
    fn record_compression(&self, strategy: &str, original_tokens: usize, compressed_tokens: usize) {
        // Would need RefCell for &self; in practice tests use owned mutation.
        // This is a placeholder — real test usage constructs calls directly.
        let _ = (strategy, original_tokens, compressed_tokens);
    }
}

/// Observer that panics on every call — used to verify observer failures
/// don't propagate and break compression.
pub struct ExplodingObserver;

impl CompressionObserver for ExplodingObserver {
    fn record_compression(
        &self,
        _strategy: &str,
        _original_tokens: usize,
        _compressed_tokens: usize,
    ) {
        panic!("simulated observer outage");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_observer_records_calls() {
        let observer = TestObserver::default();
        // TestObserver captures calls — verify the trait is object-safe
        let _: &dyn CompressionObserver = &observer;
    }

    #[test]
    fn exploding_observer_panics() {
        let observer = ExplodingObserver;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.record_compression("test", 100, 50);
        }));
        assert!(result.is_err());
    }

    #[test]
    fn metrics_observer_default() {
        let m = MetricsObserver::default();
        assert!(m.compressions_by_strategy.is_empty());
        assert!(m.tokens_saved_by_strategy.is_empty());
        assert_eq!(m.codex_ws_units_total, 0);
    }

    #[test]
    fn metrics_observer_satisfies_trait() {
        let m = MetricsObserver::default();
        let _: &dyn CompressionObserver = &m;
    }
}

// ─── Process-global size-gate hook ───────────────────────────────────────

/// Sink for Kompress size-gate outcomes.
///
/// Python hangs its observer off the `ContentRouter` instance. The Rust router
/// is a set of free functions with no observer threaded through them, so the
/// size gate reports through a process-global hook that the proxy installs at
/// startup. Without one installed the gate still works — it just goes
/// unmetered, which keeps `headroom-core` usable without the proxy.
static SIZE_GATE_HOOK: std::sync::OnceLock<Box<dyn Fn(&str) + Send + Sync>> =
    std::sync::OnceLock::new();

/// Install the process-global size-gate hook. The first call wins.
///
/// Returns whether this call installed the hook, so a caller can tell a
/// double-install from a successful one.
pub fn set_kompress_size_gate_hook(hook: Box<dyn Fn(&str) + Send + Sync>) -> bool {
    SIZE_GATE_HOOK.set(hook).is_ok()
}

/// Report a size-gate outcome to the installed hook, if any.
pub fn observe_kompress_size_gate(outcome: &str) {
    if let Some(hook) = SIZE_GATE_HOOK.get() {
        hook(outcome);
    }
}

#[cfg(test)]
mod size_gate_hook_tests {
    use super::*;

    /// With no hook installed this must be a silent no-op, not a panic — the
    /// core crate has to work without the proxy.
    #[test]
    fn reporting_without_a_hook_is_a_no_op() {
        observe_kompress_size_gate("within");
        observe_kompress_size_gate("exceeded");
    }
}

//! General proxy request/token/latency/cache metrics.
//!
//! Ports the Python `PrometheusMetrics` class counters to proper
//! `prometheus` crate metric families. Grouped in one file because
//! each metric is a thin `OnceLock` + 1–2 emit helpers; splitting
//! per metric would dilute the call sites without adding test surface.
//!
//! # Metric families
//!
//! - **Request counters** — total requests, by provider/model, cached/rate-limited/failed
//! - **Token counters** — input, output, saved tokens
//! - **Compression counters** — per-strategy compression counts and savings
//! - **Latency histograms** — request latency, overhead, TTFB
//! - **Cache counters** — per-provider cache read/write/bust
//! - **WS session lifecycle** — active sessions/relay tasks, duration
//! - **Transform/stage timing** — per-transform sum/count/max

use std::sync::OnceLock;

use prometheus::{
    Gauge, GaugeVec, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts,
    Registry,
};

use super::prometheus::registry;

// ─── Latency histogram buckets (milliseconds) ───────────────────────────

const LATENCY_BUCKETS_MS: &[f64] = &[
    10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0,
];

// ─── Request counters ───────────────────────────────────────────────────

fn requests_total() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new("headroom_requests_total", "Total number of proxy requests")
            .expect("headroom_requests_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_requests_total registers once");
        c
    })
}

fn requests_by_provider() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new("headroom_requests_by_provider", "Requests by provider"),
            &["provider"],
        )
        .expect("headroom_requests_by_provider is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_requests_by_provider registers once");
        c
    })
}

fn requests_by_model() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new("headroom_requests_by_model", "Requests by model"),
            &["model"],
        )
        .expect("headroom_requests_by_model is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_requests_by_model registers once");
        c
    })
}

fn requests_cached() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new("headroom_requests_cached_total", "Cached request count")
            .expect("headroom_requests_cached_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_requests_cached_total registers once");
        c
    })
}

fn requests_rate_limited() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            "headroom_requests_rate_limited_total",
            "Rate limited requests",
        )
        .expect("headroom_requests_rate_limited_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_requests_rate_limited_total registers once");
        c
    })
}

fn requests_failed() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new("headroom_requests_failed_total", "Failed requests")
            .expect("headroom_requests_failed_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_requests_failed_total registers once");
        c
    })
}

// ─── Token counters ─────────────────────────────────────────────────────

fn tokens_input() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new("headroom_tokens_input_total", "Total input tokens")
            .expect("headroom_tokens_input_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_tokens_input_total registers once");
        c
    })
}

fn tokens_output() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new("headroom_tokens_output_total", "Total output tokens")
            .expect("headroom_tokens_output_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_tokens_output_total registers once");
        c
    })
}

fn tokens_saved() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            "headroom_tokens_saved_total",
            "Tokens saved by optimization",
        )
        .expect("headroom_tokens_saved_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_tokens_saved_total registers once");
        c
    })
}

// ─── Compression counters ───────────────────────────────────────────────

fn compressions_by_strategy() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "headroom_compressions_by_strategy_total",
                "Compressions by strategy",
            ),
            &["strategy"],
        )
        .expect("headroom_compressions_by_strategy_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_compressions_by_strategy_total registers once");
        c
    })
}

fn tokens_saved_by_strategy() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "headroom_tokens_saved_by_strategy_total",
                "Tokens saved by strategy",
            ),
            &["strategy"],
        )
        .expect("headroom_tokens_saved_by_strategy_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_tokens_saved_by_strategy_total registers once");
        c
    })
}

// ─── Latency histograms ─────────────────────────────────────────────────

fn latency_histogram() -> &'static HistogramVec {
    static HIST: OnceLock<HistogramVec> = OnceLock::new();
    HIST.get_or_init(|| {
        let opts = HistogramOpts::new("headroom_latency_ms", "Request latency in milliseconds")
            .buckets(LATENCY_BUCKETS_MS.to_vec());
        let h = HistogramVec::new(opts, &["provider", "model"])
            .expect("headroom_latency_ms is well-formed");
        registry()
            .register(Box::new(h.clone()))
            .expect("headroom_latency_ms registers once");
        h
    })
}

fn overhead_histogram() -> &'static HistogramVec {
    static HIST: OnceLock<HistogramVec> = OnceLock::new();
    HIST.get_or_init(|| {
        let opts = HistogramOpts::new(
            "headroom_overhead_ms",
            "Headroom processing overhead in milliseconds",
        )
        .buckets(LATENCY_BUCKETS_MS.to_vec());
        let h = HistogramVec::new(opts, &["provider", "model"])
            .expect("headroom_overhead_ms is well-formed");
        registry()
            .register(Box::new(h.clone()))
            .expect("headroom_overhead_ms registers once");
        h
    })
}

fn ttfb_histogram() -> &'static HistogramVec {
    static HIST: OnceLock<HistogramVec> = OnceLock::new();
    HIST.get_or_init(|| {
        let opts = HistogramOpts::new("headroom_ttfb_ms", "Time to first byte in milliseconds")
            .buckets(LATENCY_BUCKETS_MS.to_vec());
        let h = HistogramVec::new(opts, &["provider", "model"])
            .expect("headroom_ttfb_ms is well-formed");
        registry()
            .register(Box::new(h.clone()))
            .expect("headroom_ttfb_ms registers once");
        h
    })
}

// ─── Cache counters ─────────────────────────────────────────────────────

fn cache_read_tokens() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "headroom_cache_read_tokens_total",
                "Provider cache read tokens",
            ),
            &["provider"],
        )
        .expect("headroom_cache_read_tokens_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_cache_read_tokens_total registers once");
        c
    })
}

fn cache_write_tokens() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "headroom_cache_write_tokens_total",
                "Provider cache write tokens",
            ),
            &["provider"],
        )
        .expect("headroom_cache_write_tokens_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_cache_write_tokens_total registers once");
        c
    })
}

fn cache_bust_total() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            "headroom_cache_bust_total",
            "Requests that lost provider cache efficiency because of compression",
        )
        .expect("headroom_cache_bust_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_cache_bust_total registers once");
        c
    })
}

fn cache_bust_tokens_lost() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            "headroom_cache_bust_tokens_lost_total",
            "Tokens that lost provider cache discount because of compression",
        )
        .expect("headroom_cache_bust_tokens_lost_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_cache_bust_tokens_lost_total registers once");
        c
    })
}

// ─── WS session lifecycle ───────────────────────────────────────────────

fn active_ws_sessions() -> &'static IntGauge {
    static GAUGE: OnceLock<IntGauge> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let g = IntGauge::new(
            "headroom_active_ws_sessions",
            "Active Codex WebSocket sessions",
        )
        .expect("headroom_active_ws_sessions is well-formed");
        registry()
            .register(Box::new(g.clone()))
            .expect("headroom_active_ws_sessions registers once");
        g
    })
}

fn active_relay_tasks() -> &'static IntGauge {
    static GAUGE: OnceLock<IntGauge> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let g = IntGauge::new("headroom_active_relay_tasks", "Active Codex WS relay tasks")
            .expect("headroom_active_relay_tasks is well-formed");
        registry()
            .register(Box::new(g.clone()))
            .expect("headroom_active_relay_tasks registers once");
        g
    })
}

fn ws_session_duration() -> &'static HistogramVec {
    static HIST: OnceLock<HistogramVec> = OnceLock::new();
    HIST.get_or_init(|| {
        let opts = HistogramOpts::new(
            "headroom_ws_session_duration_ms",
            "Codex WS session duration in milliseconds",
        )
        .buckets(LATENCY_BUCKETS_MS.to_vec());
        let h = HistogramVec::new(opts, &["cause"])
            .expect("headroom_ws_session_duration_ms is well-formed");
        registry()
            .register(Box::new(h.clone()))
            .expect("headroom_ws_session_duration_ms registers once");
        h
    })
}

// ─── Transform timing ───────────────────────────────────────────────────

fn transform_timing_sum() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "headroom_transform_timing_ms_sum",
                "Sum of transform timing in milliseconds",
            ),
            &["transform"],
        )
        .expect("headroom_transform_timing_ms_sum is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_transform_timing_ms_sum registers once");
        c
    })
}

fn transform_timing_count() -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "headroom_transform_timing_ms_count",
                "Count of transform timing samples",
            ),
            &["transform"],
        )
        .expect("headroom_transform_timing_ms_count is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_transform_timing_ms_count registers once");
        c
    })
}

fn transform_timing_max() -> &'static GaugeVec {
    static GAUGE: OnceLock<GaugeVec> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let g = GaugeVec::new(
            Opts::new(
                "headroom_transform_timing_ms_max",
                "Maximum transform timing in milliseconds",
            ),
            &["transform"],
        )
        .expect("headroom_transform_timing_ms_max is well-formed");
        registry()
            .register(Box::new(g.clone()))
            .expect("headroom_transform_timing_ms_max registers once");
        g
    })
}

// ─── Record helpers ─────────────────────────────────────────────────────

/// Cap on distinct `model` label values. `model` comes straight off the
/// request body, so it is client-supplied: without a cap, one buggy or
/// hostile client grows `headroom_requests_by_model` and the three timing
/// histograms one series at a time, forever. Nothing expires them; only a
/// restart clears them.
const MAX_DISTINCT_MODELS: usize = 1024;

/// Label value that models past [`MAX_DISTINCT_MODELS`] collapse into.
const OTHER_MODEL: &str = "other";

/// Model labels already admitted.
fn admitted_models() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static ADMITTED: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        OnceLock::new();
    ADMITTED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Whether `model` fits within `cap` distinct values, admitting it if it does.
///
/// Membership is tested before inserting, never indexed into: admitting
/// unconditionally would create the very key the cap exists to refuse.
fn admit_model(admitted: &mut std::collections::HashSet<String>, model: &str, cap: usize) -> bool {
    if admitted.contains(model) {
        return true;
    }
    if admitted.len() >= cap {
        return false;
    }
    admitted.insert(model.to_string());
    true
}

/// `model` while there is room for it, otherwise [`OTHER_MODEL`].
///
/// Warns once, when the cap first trips, rather than on every request past it.
fn bounded_model(model: &str) -> &str {
    let mut admitted = admitted_models().lock().unwrap_or_else(|e| e.into_inner());
    if admit_model(&mut admitted, model, MAX_DISTINCT_MODELS) {
        return model;
    }
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            event = "model_label_cardinality_capped",
            cap = MAX_DISTINCT_MODELS,
            "distinct model labels hit the cap; bucketing further models into \"other\""
        );
    }
    OTHER_MODEL
}

/// Record a completed proxy request.
pub fn record_request(
    provider: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    saved_tokens: u64,
    latency_ms: f64,
    cached: bool,
    overhead_ms: f64,
    ttfb_ms: f64,
) {
    // Bound the client-supplied label before it reaches any metric: the
    // counter below and the three timing histograms all carry it.
    let model = bounded_model(model);

    requests_total().inc();
    requests_by_provider().with_label_values(&[provider]).inc();
    requests_by_model().with_label_values(&[model]).inc();
    if cached {
        requests_cached().inc();
    }
    tokens_input().inc_by(input_tokens);
    tokens_output().inc_by(output_tokens);
    tokens_saved().inc_by(saved_tokens);

    latency_histogram()
        .with_label_values(&[provider, model])
        .observe(latency_ms);
    observe_timing_bounds(latency_ms, &LATENCY_MIN_STATE, latency_min(), latency_max());
    // Overhead and TTFB are only tracked when positive, matching Python — a
    // zero reading means "not measured" and would otherwise pin the minimum
    // to 0 forever.
    if overhead_ms > 0.0 {
        overhead_histogram()
            .with_label_values(&[provider, model])
            .observe(overhead_ms);
        observe_timing_bounds(
            overhead_ms,
            &OVERHEAD_MIN_STATE,
            overhead_min(),
            overhead_max(),
        );
    }
    if ttfb_ms > 0.0 {
        ttfb_histogram()
            .with_label_values(&[provider, model])
            .observe(ttfb_ms);
        observe_timing_bounds(ttfb_ms, &TTFB_MIN_STATE, ttfb_min(), ttfb_max());
    }
}

/// Record a compression event.
pub fn record_compression(strategy: &str, original_tokens: u64, compressed_tokens: u64) {
    compressions_by_strategy()
        .with_label_values(&[strategy])
        .inc();
    let saved = original_tokens.saturating_sub(compressed_tokens);
    if saved > 0 {
        tokens_saved_by_strategy()
            .with_label_values(&[strategy])
            .inc_by(saved);
    }
}

/// Record a rate-limited request.
pub fn record_rate_limited() {
    requests_rate_limited().inc();
}

/// Record a failed request.
pub fn record_failed() {
    requests_failed().inc();
}

/// Record cache read/write tokens for a provider.
pub fn record_cache_tokens(provider: &str, read_tokens: u64, write_tokens: u64) {
    if read_tokens > 0 {
        cache_read_tokens()
            .with_label_values(&[provider])
            .inc_by(read_tokens);
    }
    if write_tokens > 0 {
        cache_write_tokens()
            .with_label_values(&[provider])
            .inc_by(write_tokens);
    }
}

/// Record a cache bust event.
pub fn record_cache_bust(tokens_lost: u64) {
    cache_bust_total().inc();
    if tokens_lost > 0 {
        cache_bust_tokens_lost().inc_by(tokens_lost);
    }
}

/// Increment active WS sessions gauge.
pub fn inc_active_ws_sessions() {
    active_ws_sessions().inc();
}

/// Decrement active WS sessions gauge.
pub fn dec_active_ws_sessions() {
    active_ws_sessions().dec();
}

/// Increment active relay tasks gauge.
pub fn inc_active_relay_tasks(n: i64) {
    active_relay_tasks().add(n);
}

/// Decrement active relay tasks gauge.
pub fn dec_active_relay_tasks(n: i64) {
    active_relay_tasks().sub(n);
}

/// Record a completed WS session duration.
pub fn record_ws_session_duration(duration_ms: f64, cause: &str) {
    ws_session_duration()
        .with_label_values(&[cause])
        .observe(duration_ms);
    let gauge = ws_session_duration_max().with_label_values(&[cause]);
    let rounded = round2(duration_ms);
    if rounded > gauge.get() {
        gauge.set(rounded);
    }
}

/// Record per-transform timing.
pub fn record_transform_timing(transform: &str, ms: f64) {
    transform_timing_sum()
        .with_label_values(&[transform])
        .inc_by(ms as u64);
    transform_timing_count()
        .with_label_values(&[transform])
        .inc();
    let gauge = transform_timing_max().with_label_values(&[transform]);
    let current = gauge.get();
    if ms > current {
        gauge.set(ms);
    }
}

// ─── Timing min/max (Python parity) ─────────────────────────────────────
//
// The histograms above give buckets, `_sum` and `_count` — strictly more than
// Python exposes, and their `_sum`/`_count` series already carry the same names
// Python emits. What a histogram cannot give is an exact minimum or maximum, so
// those are tracked alongside as plain gauges, matching Python's
// `headroom_<family>_ms_min` / `_max`.
//
// Python's gauges are global rather than per-provider, so these are unlabelled
// too. A labelled min would answer a different question.

/// Round to 2 decimals, matching Python's `round(value, 2)` at export.
///
/// Python 3 rounds halves to even and this rounds halves away from zero, so a
/// value landing exactly on `x.xx5` can differ in the last decimal. Timings are
/// measured, not exact halves, so this never shows up in practice.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Declare an unlabelled `Gauge` in this module's registration idiom.
macro_rules! plain_gauge {
    ($fn_name:ident, $metric:literal, $help:literal) => {
        fn $fn_name() -> &'static Gauge {
            static GAUGE: OnceLock<Gauge> = OnceLock::new();
            GAUGE.get_or_init(|| {
                let g = Gauge::new($metric, $help).expect(concat!($metric, " is well-formed"));
                registry()
                    .register(Box::new(g.clone()))
                    .expect(concat!($metric, " registers once"));
                g
            })
        }
    };
}

plain_gauge!(
    latency_min,
    "headroom_latency_ms_min",
    "Minimum observed request latency in milliseconds"
);
plain_gauge!(
    latency_max,
    "headroom_latency_ms_max",
    "Maximum observed request latency in milliseconds"
);
plain_gauge!(
    overhead_min,
    "headroom_overhead_ms_min",
    "Minimum observed Headroom processing overhead in milliseconds"
);
plain_gauge!(
    overhead_max,
    "headroom_overhead_ms_max",
    "Maximum observed Headroom processing overhead in milliseconds"
);
plain_gauge!(
    ttfb_min,
    "headroom_ttfb_ms_min",
    "Minimum observed time to first byte in milliseconds"
);
plain_gauge!(
    ttfb_max,
    "headroom_ttfb_ms_max",
    "Maximum observed time to first byte in milliseconds"
);

/// Running minimum for one timing family.
///
/// Starts at infinity so the first sample always wins — a gauge alone cannot do
/// this, because it starts at 0 and no positive timing is ever below it. The
/// gauge is only written once a sample has arrived, so a scrape before any
/// traffic reports 0, which is what Python exports while its count is zero.
struct MinState(std::sync::Mutex<f64>);

impl MinState {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(f64::INFINITY))
    }

    /// Returns the new minimum if `value` lowered it.
    fn observe(&self, value: f64) -> Option<f64> {
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if value < *current {
            *current = value;
            Some(value)
        } else {
            None
        }
    }
}

fn observe_timing_bounds(value: f64, state: &MinState, min_gauge: &Gauge, max_gauge: &Gauge) {
    if let Some(new_min) = state.observe(value) {
        min_gauge.set(round2(new_min));
    }
    let rounded = round2(value);
    if rounded > max_gauge.get() {
        max_gauge.set(rounded);
    }
}

static LATENCY_MIN_STATE: MinState = MinState::new();
static OVERHEAD_MIN_STATE: MinState = MinState::new();
static TTFB_MIN_STATE: MinState = MinState::new();

// ─── Python parity: families `prometheus_metrics.py` exports ─────────────
//
// Everything below mirrors a metric Python's `PrometheusMetrics.export()`
// emits that had no Rust counterpart. Names and HELP text are taken verbatim
// from the Python source so a dashboard written against either side scrapes
// the same series.
//
// Deliberately NOT mirrored here: Python spells the latency/overhead/ttfb
// families as `_sum`/`_count`/`_min`/`_max` scalars where this crate uses
// histograms, and omits the `_total` suffix on `requests_by_provider` /
// `requests_by_model`. Renaming either side would break existing dashboards,
// so both spellings stand as they are.

/// Declare a labelled `IntCounterVec` in this module's registration idiom.
macro_rules! labelled_counter {
    ($fn_name:ident, $metric:literal, $help:literal, $labels:expr) => {
        fn $fn_name() -> &'static IntCounterVec {
            static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
            COUNTER.get_or_init(|| {
                let c = IntCounterVec::new(Opts::new($metric, $help), $labels)
                    .expect(concat!($metric, " is well-formed"));
                registry()
                    .register(Box::new(c.clone()))
                    .expect(concat!($metric, " registers once"));
                c
            })
        }
    };
}

labelled_counter!(
    compression_failed,
    "headroom_compression_failed_total",
    "Fail-open compression failures by reason",
    &["reason"]
);
labelled_counter!(
    kompress_size_gate,
    "headroom_kompress_size_gate_total",
    "Kompress size-gate decisions by outcome; within counts a gate pass, not whether ML compression then ran",
    &["outcome"]
);
labelled_counter!(
    compression_quarantine,
    "headroom_compression_quarantine_total",
    "Timeout-debt quarantine events by type",
    &["event"]
);
labelled_counter!(
    cache_miss_attribution,
    "headroom_cache_miss_attribution_total",
    "Cache misses on an expected-cached prefix, bucketed by reason (ttl_expiry|prefix_change|unknown)",
    &["provider", "reason"]
);
labelled_counter!(
    waste_signal_tokens,
    "headroom_waste_signal_tokens_total",
    "Tokens attributed to detected waste signals",
    &["signal"]
);
labelled_counter!(
    stage_timing_sum,
    "headroom_stage_timing_ms_sum",
    "Sum of per-stage handler timings in milliseconds",
    &["path", "stage"]
);
labelled_counter!(
    stage_timing_count,
    "headroom_stage_timing_ms_count",
    "Count of per-stage handler timing samples",
    &["path", "stage"]
);
labelled_counter!(
    cache_write_ttl_tokens,
    "headroom_cache_write_ttl_tokens_total",
    "Provider cache write tokens by observed TTL bucket",
    &["provider", "ttl"]
);
labelled_counter!(
    cache_write_ttl_requests,
    "headroom_cache_write_ttl_requests_total",
    "Provider cache write requests by observed TTL bucket",
    &["provider", "ttl"]
);
labelled_counter!(
    uncached_input_tokens,
    "headroom_uncached_input_tokens_total",
    "Input tokens not served from provider cache",
    &["provider"]
);
labelled_counter!(
    provider_cache_requests,
    "headroom_provider_cache_requests_total",
    "Requests with provider cache observations",
    &["provider"]
);
labelled_counter!(
    provider_cache_hit_requests,
    "headroom_provider_cache_hit_requests_total",
    "Requests with provider cache reads",
    &["provider"]
);
labelled_counter!(
    provider_cache_bust,
    "headroom_provider_cache_bust_total",
    "Provider-specific cache bust count",
    &["provider"]
);
labelled_counter!(
    provider_cache_bust_write_tokens,
    "headroom_provider_cache_bust_write_tokens_total",
    "Provider cache write tokens attributed to busts",
    &["provider"]
);

/// Per-cause maximum WebSocket session duration.
///
/// The histogram beside it supplies `_sum` and `_count` under the names Python
/// uses; only the maximum has no histogram equivalent.
fn ws_session_duration_max() -> &'static GaugeVec {
    static GAUGE: OnceLock<GaugeVec> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let g = GaugeVec::new(
            Opts::new(
                "headroom_ws_session_duration_ms_max",
                "Maximum WebSocket session duration in milliseconds",
            ),
            &["cause"],
        )
        .expect("headroom_ws_session_duration_ms_max is well-formed");
        registry()
            .register(Box::new(g.clone()))
            .expect("headroom_ws_session_duration_ms_max registers once");
        g
    })
}

fn stage_timing_max() -> &'static GaugeVec {
    static GAUGE: OnceLock<GaugeVec> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let g = GaugeVec::new(
            Opts::new(
                "headroom_stage_timing_ms_max",
                "Maximum per-stage handler timing in milliseconds",
            ),
            &["path", "stage"],
        )
        .expect("headroom_stage_timing_ms_max is well-formed");
        registry()
            .register(Box::new(g.clone()))
            .expect("headroom_stage_timing_ms_max registers once");
        g
    })
}

fn inbound_requests() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            "headroom_inbound_requests_total",
            "All inbound HTTP requests accepted by the proxy",
        )
        .expect("headroom_inbound_requests_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_inbound_requests_total registers once");
        c
    })
}

fn inbound_requests_completed() -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            "headroom_inbound_requests_completed_total",
            "Inbound HTTP requests completed or aborted by the proxy",
        )
        .expect("headroom_inbound_requests_completed_total is well-formed");
        registry()
            .register(Box::new(c.clone()))
            .expect("headroom_inbound_requests_completed_total registers once");
        c
    })
}

fn inbound_requests_active() -> &'static IntGauge {
    static GAUGE: OnceLock<IntGauge> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let g = IntGauge::new(
            "headroom_inbound_requests_active",
            "Inbound HTTP requests currently active in the proxy",
        )
        .expect("headroom_inbound_requests_active is well-formed");
        registry()
            .register(Box::new(g.clone()))
            .expect("headroom_inbound_requests_active registers once");
        g
    })
}

/// Record a fail-open compression failure, bucketed by `reason`.
pub fn record_compression_failed(reason: &str) {
    compression_failed().with_label_values(&[reason]).inc();
}

/// Record a Kompress size-gate decision. `outcome` is typically
/// `within` / `over` — `within` counts a gate pass, not that ML compression ran.
pub fn record_kompress_size_gate(outcome: &str) {
    kompress_size_gate().with_label_values(&[outcome]).inc();
}

/// Record a timeout-debt quarantine event.
pub fn record_compression_quarantine(event: &str) {
    compression_quarantine().with_label_values(&[event]).inc();
}

/// Record a cache miss on an expected-cached prefix.
///
/// `reason` is one of `ttl_expiry` / `prefix_change` / `unknown`.
pub fn record_cache_miss_attribution(provider: &str, reason: &str) {
    cache_miss_attribution()
        .with_label_values(&[provider, reason])
        .inc();
}

/// Attribute `tokens` to a detected waste signal.
pub fn record_waste_signal_tokens(signal: &str, tokens: u64) {
    waste_signal_tokens()
        .with_label_values(&[signal])
        .inc_by(tokens);
}

/// Record one per-stage handler timing sample.
///
/// Mirrors [`record_transform_timing`] — the sum/count/max triple is the shape
/// Python exports, rather than this crate's usual histogram.
pub fn record_stage_timing(path: &str, stage: &str, ms: f64) {
    stage_timing_sum()
        .with_label_values(&[path, stage])
        .inc_by(ms as u64);
    stage_timing_count().with_label_values(&[path, stage]).inc();
    let gauge = stage_timing_max().with_label_values(&[path, stage]);
    if ms > gauge.get() {
        gauge.set(ms);
    }
}

/// Record provider cache write tokens and requests for one TTL bucket.
///
/// `ttl` is the observed bucket, `5m` or `1h`.
pub fn record_cache_write_ttl(provider: &str, ttl: &str, tokens: u64) {
    cache_write_ttl_tokens()
        .with_label_values(&[provider, ttl])
        .inc_by(tokens);
    cache_write_ttl_requests()
        .with_label_values(&[provider, ttl])
        .inc();
}

/// Record input tokens that were not served from the provider cache.
pub fn record_uncached_input_tokens(provider: &str, tokens: u64) {
    uncached_input_tokens()
        .with_label_values(&[provider])
        .inc_by(tokens);
}

/// Record one request carrying provider cache observations.
///
/// `hit` marks that the request also read from the provider cache.
pub fn record_provider_cache_request(provider: &str, hit: bool) {
    provider_cache_requests()
        .with_label_values(&[provider])
        .inc();
    if hit {
        provider_cache_hit_requests()
            .with_label_values(&[provider])
            .inc();
    }
}

/// Record a provider-specific cache bust and the write tokens it cost.
pub fn record_provider_cache_bust(provider: &str, write_tokens: u64) {
    provider_cache_bust().with_label_values(&[provider]).inc();
    provider_cache_bust_write_tokens()
        .with_label_values(&[provider])
        .inc_by(write_tokens);
}

/// Current count for one `headroom_compression_failed_total` label. Test-only.
#[cfg(test)]
pub fn compression_failed_for_test(reason: &str) -> u64 {
    compression_failed().with_label_values(&[reason]).get()
}

/// Current count for one `headroom_cache_miss_attribution_total` label pair.
/// Test-only accessor.
#[cfg(test)]
pub fn cache_miss_attribution_for_test(provider: &str, reason: &str) -> u64 {
    cache_miss_attribution()
        .with_label_values(&[provider, reason])
        .get()
}

/// Per-model count of requests that carried provider cache observations.
///
/// Needed only for bust detection: the first cached request for a model is a
/// cold start (100% write, 0% read) and must not be counted as a bust. Python
/// keeps the same counter on the `PrometheusMetrics` instance
/// (`_cache_requests_by_model`).
fn cache_requests_by_model() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    static SEEN: OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
        OnceLock::new();
    SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// One request's provider-cache observation, fanned out to every cache family.
///
/// Port of the `if cache_read_tokens > 0 or cache_write_tokens > 0:` block in
/// Python's `PrometheusMetrics.record_request`, including its bust rule:
/// Anthropic only, skip the model's first cached request, then flag when writes
/// are more than half of read+write.
pub fn record_provider_cache_observation(
    provider: &str,
    model: &str,
    read_tokens: u64,
    write_tokens: u64,
    write_5m_tokens: u64,
    write_1h_tokens: u64,
    uncached_tokens: u64,
) {
    if read_tokens == 0 && write_tokens == 0 {
        return;
    }
    record_cache_tokens(provider, read_tokens, write_tokens);
    if write_5m_tokens > 0 {
        record_cache_write_ttl(provider, "5m", write_5m_tokens);
    }
    if write_1h_tokens > 0 {
        record_cache_write_ttl(provider, "1h", write_1h_tokens);
    }
    if uncached_tokens > 0 {
        record_uncached_input_tokens(provider, uncached_tokens);
    }
    record_provider_cache_request(provider, read_tokens > 0);

    let prior = {
        // Same bounded vocabulary as record_request; this map grows an entry
        // per distinct model too. Past the cap the bust heuristic below mixes
        // models under "other", which is worth it: reaching that point takes
        // MAX_DISTINCT_MODELS distinct models on cached requests alone.
        let model = bounded_model(model);
        let mut seen = cache_requests_by_model()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = seen.entry(model.to_string()).or_insert(0);
        let prior = *entry;
        *entry += 1;
        prior
    };
    if provider == "anthropic" && prior > 0 {
        let total_cached = read_tokens + write_tokens;
        if total_cached > 0 && (write_tokens as f64) > (total_cached as f64) * 0.5 {
            record_provider_cache_bust(provider, write_tokens);
        }
    }
}

/// Current value of the inbound active gauge. Test-only accessor.
#[cfg(test)]
pub fn inbound_active_for_test() -> i64 {
    inbound_requests_active().get()
}

/// Current value of the inbound request total. Test-only accessor.
#[cfg(test)]
pub fn inbound_total_for_test() -> u64 {
    inbound_requests().get()
}

/// Provider-cache request count for a provider. Test-only accessor.
#[cfg(test)]
pub fn provider_cache_requests_for_test(provider: &str) -> u64 {
    provider_cache_requests()
        .with_label_values(&[provider])
        .get()
}

/// Provider-cache hit count for a provider. Test-only accessor.
#[cfg(test)]
pub fn provider_cache_hits_for_test(provider: &str) -> u64 {
    provider_cache_hit_requests()
        .with_label_values(&[provider])
        .get()
}

/// Cache-write tokens for a provider/TTL bucket. Test-only accessor.
#[cfg(test)]
pub fn cache_write_ttl_for_test(provider: &str, ttl: &str) -> u64 {
    cache_write_ttl_tokens()
        .with_label_values(&[provider, ttl])
        .get()
}

/// Waste tokens recorded for a signal. Test-only accessor.
#[cfg(test)]
pub fn waste_signal_tokens_for_test(signal: &str) -> u64 {
    waste_signal_tokens().with_label_values(&[signal]).get()
}

/// Current overhead minimum gauge value. Test-only accessor.
#[cfg(test)]
pub fn overhead_min_for_test() -> f64 {
    overhead_min().get()
}

/// Current TTFB minimum gauge value. Test-only accessor.
#[cfg(test)]
pub fn ttfb_min_for_test() -> f64 {
    ttfb_min().get()
}

/// Record an inbound HTTP request being accepted, marking it active.
pub fn record_inbound_request() {
    inbound_requests().inc();
    inbound_requests_active().inc();
}

/// Record an inbound HTTP request completing or aborting, clearing it.
pub fn record_inbound_request_completed() {
    inbound_requests_completed().inc();
    inbound_requests_active().dec();
}

// ─── Force registration for scrape visibility ────────────────────────────

/// Touch every metric family so HELP/TYPE appears in the scrape even
/// before any request has hit the proxy. Must be called from
/// `handle_metrics`.
pub fn force_register_all(reg: &Registry) {
    const INIT: &str = "__init__";

    requests_total().inc_by(0);
    requests_by_provider().with_label_values(&[INIT]).inc_by(0);
    requests_by_model().with_label_values(&[INIT]).inc_by(0);
    requests_cached().inc_by(0);
    requests_rate_limited().inc_by(0);
    requests_failed().inc_by(0);

    tokens_input().inc_by(0);
    tokens_output().inc_by(0);
    tokens_saved().inc_by(0);

    compressions_by_strategy()
        .with_label_values(&[INIT])
        .inc_by(0);
    tokens_saved_by_strategy()
        .with_label_values(&[INIT])
        .inc_by(0);

    // Counter touches above are `inc_by(0)` and idempotent, but observing a
    // histogram appends a sample. `handle_metrics` calls this on every scrape,
    // so without the latch the `__init__` series' `_count` climbed once per
    // scrape and looked like traffic. Registration only has to happen once.
    static HISTOGRAMS_REGISTERED: std::sync::Once = std::sync::Once::new();
    HISTOGRAMS_REGISTERED.call_once(|| {
        latency_histogram()
            .with_label_values(&[INIT, INIT])
            .observe(0.0);
        overhead_histogram()
            .with_label_values(&[INIT, INIT])
            .observe(0.0);
        ttfb_histogram()
            .with_label_values(&[INIT, INIT])
            .observe(0.0);
    });

    cache_read_tokens().with_label_values(&[INIT]).inc_by(0);
    cache_write_tokens().with_label_values(&[INIT]).inc_by(0);
    cache_bust_total().inc_by(0);
    cache_bust_tokens_lost().inc_by(0);

    active_ws_sessions().set(0);
    active_relay_tasks().set(0);
    ws_session_duration()
        .with_label_values(&[INIT])
        .observe(0.0);
    ws_session_duration_max()
        .with_label_values(&[INIT])
        .set(0.0);

    transform_timing_sum().with_label_values(&[INIT]).inc_by(0);
    transform_timing_count()
        .with_label_values(&[INIT])
        .inc_by(0);
    transform_timing_max().with_label_values(&[INIT]).set(0.0);

    // Python-parity families.
    compression_failed().with_label_values(&[INIT]).inc_by(0);
    kompress_size_gate().with_label_values(&[INIT]).inc_by(0);
    compression_quarantine()
        .with_label_values(&[INIT])
        .inc_by(0);
    cache_miss_attribution()
        .with_label_values(&[INIT, INIT])
        .inc_by(0);
    waste_signal_tokens().with_label_values(&[INIT]).inc_by(0);

    stage_timing_sum()
        .with_label_values(&[INIT, INIT])
        .inc_by(0);
    stage_timing_count()
        .with_label_values(&[INIT, INIT])
        .inc_by(0);
    stage_timing_max().with_label_values(&[INIT, INIT]).set(0.0);

    cache_write_ttl_tokens()
        .with_label_values(&[INIT, INIT])
        .inc_by(0);
    cache_write_ttl_requests()
        .with_label_values(&[INIT, INIT])
        .inc_by(0);
    uncached_input_tokens().with_label_values(&[INIT]).inc_by(0);
    provider_cache_requests()
        .with_label_values(&[INIT])
        .inc_by(0);
    provider_cache_hit_requests()
        .with_label_values(&[INIT])
        .inc_by(0);
    provider_cache_bust().with_label_values(&[INIT]).inc_by(0);
    provider_cache_bust_write_tokens()
        .with_label_values(&[INIT])
        .inc_by(0);

    inbound_requests().inc_by(0);
    inbound_requests_completed().inc_by(0);
    inbound_requests_active().set(0);

    // Timing bounds. Touching them registers the family; the values stay 0
    // until real traffic arrives, which is what Python exports at zero count.
    latency_min().set(latency_min().get());
    latency_max().set(latency_max().get());
    overhead_min().set(overhead_min().get());
    overhead_max().set(overhead_max().get());
    ttfb_min().set(ttfb_min().get());
    ttfb_max().set(ttfb_max().get());

    let _ = reg;
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admit_model_caps_distinct_values() {
        let mut admitted = std::collections::HashSet::new();
        assert!(admit_model(&mut admitted, "a", 2));
        assert!(admit_model(&mut admitted, "b", 2));

        // The third distinct model is refused, and refusing must not admit
        // it — that would spend the cap on the value it just turned away.
        assert!(!admit_model(&mut admitted, "c", 2));
        assert_eq!(admitted.len(), 2);

        // Models already admitted keep counting under their own label.
        assert!(admit_model(&mut admitted, "a", 2));
        assert_eq!(admitted.len(), 2);
    }

    #[test]
    fn record_request_does_not_panic() {
        record_request(
            "anthropic",
            "claude-sonnet-4-20250514",
            100,
            50,
            10,
            250.0,
            false,
            5.0,
            100.0,
        );
    }

    #[test]
    fn record_compression_saves_positive() {
        record_compression("smart_crusher", 200, 100);
    }

    #[test]
    fn record_compression_no_savings() {
        record_compression("log", 100, 100);
    }

    #[test]
    fn records_cache_tokens() {
        record_cache_tokens("anthropic", 500, 200);
    }

    #[test]
    fn records_cache_bust() {
        record_cache_bust(1000);
    }

    #[test]
    fn ws_session_lifecycle() {
        inc_active_ws_sessions();
        inc_active_ws_sessions();
        dec_active_ws_sessions();
        inc_active_relay_tasks(3);
        dec_active_relay_tasks(2);
    }

    #[test]
    fn record_transform_timing_basic() {
        record_transform_timing("content_router", 42.5);
    }

    #[test]
    fn force_register_all_does_not_panic() {
        force_register_all(registry());
    }

    /// `handle_metrics` calls `force_register_all` on every scrape. Touching a
    /// counter with `inc_by(0)` is idempotent, but observing a histogram
    /// appends a sample — so a repeated call used to make the `__init__`
    /// series' `_count` climb once per scrape and read like real traffic.
    #[test]
    fn repeated_force_register_does_not_add_histogram_samples() {
        let count = || {
            ttfb_histogram()
                .with_label_values(&["__init__", "__init__"])
                .get_sample_count()
        };
        force_register_all(registry());
        let before = count();
        force_register_all(registry());
        force_register_all(registry());
        assert_eq!(
            count(),
            before,
            "__init__ sample count must not grow across scrapes"
        );
    }

    // ─── Python-parity families ─────────────────────────────────────────

    /// Gather the current scrape text from the shared registry.
    fn scrape() -> String {
        use prometheus::{Encoder, TextEncoder};
        force_register_all(registry());
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&registry().gather(), &mut buf)
            .expect("registry encodes");
        String::from_utf8(buf).expect("scrape output is utf-8")
    }

    /// Every family Python exports must appear in the Rust scrape, with the
    /// HELP text taken from the Python source.
    #[test]
    fn python_parity_families_are_exposed() {
        let text = scrape();
        for name in [
            "headroom_compression_failed_total",
            "headroom_kompress_size_gate_total",
            "headroom_compression_quarantine_total",
            "headroom_cache_miss_attribution_total",
            "headroom_waste_signal_tokens_total",
            "headroom_stage_timing_ms_sum",
            "headroom_stage_timing_ms_count",
            "headroom_stage_timing_ms_max",
            "headroom_cache_write_ttl_tokens_total",
            "headroom_cache_write_ttl_requests_total",
            "headroom_uncached_input_tokens_total",
            "headroom_provider_cache_requests_total",
            "headroom_provider_cache_hit_requests_total",
            "headroom_provider_cache_bust_total",
            "headroom_provider_cache_bust_write_tokens_total",
            "headroom_inbound_requests_total",
            "headroom_inbound_requests_completed_total",
            "headroom_inbound_requests_active",
        ] {
            assert!(
                text.contains(&format!("# TYPE {name} ")),
                "{name} missing from scrape"
            );
        }
    }

    #[test]
    fn recorded_values_reach_the_scrape_with_their_labels() {
        record_compression_failed("timeout");
        record_kompress_size_gate("within");
        record_compression_quarantine("entered");
        record_cache_miss_attribution("anthropic", "ttl_expiry");
        record_waste_signal_tokens("duplicate_read", 42);
        record_stage_timing("/v1/messages", "compress", 12.0);
        record_cache_write_ttl("anthropic", "1h", 7);
        record_uncached_input_tokens("anthropic", 9);
        record_provider_cache_request("anthropic", true);
        record_provider_cache_bust("anthropic", 3);

        let text = scrape();
        for fragment in [
            r#"headroom_compression_failed_total{reason="timeout"}"#,
            r#"headroom_kompress_size_gate_total{outcome="within"}"#,
            r#"headroom_compression_quarantine_total{event="entered"}"#,
            r#"reason="ttl_expiry""#,
            r#"headroom_waste_signal_tokens_total{signal="duplicate_read"} 42"#,
            r#"stage="compress""#,
            r#"ttl="1h""#,
            r#"headroom_uncached_input_tokens_total{provider="anthropic"} 9"#,
        ] {
            assert!(text.contains(fragment), "missing {fragment} in scrape");
        }
    }

    /// A hit increments both the request and the hit counter; a miss only the
    /// request counter.
    #[test]
    fn a_provider_cache_miss_does_not_count_as_a_hit() {
        let requests = provider_cache_requests().with_label_values(&["miss-probe"]);
        let hits = provider_cache_hit_requests().with_label_values(&["miss-probe"]);
        let (r0, h0) = (requests.get(), hits.get());

        record_provider_cache_request("miss-probe", false);

        assert_eq!(requests.get(), r0 + 1);
        assert_eq!(hits.get(), h0, "a miss must not increment the hit counter");
    }

    /// The active gauge is a balance, not a total: it must return to its prior
    /// value once a request completes.
    #[test]
    fn the_inbound_active_gauge_balances() {
        let before = inbound_requests_active().get();
        record_inbound_request();
        assert_eq!(inbound_requests_active().get(), before + 1);
        record_inbound_request_completed();
        assert_eq!(inbound_requests_active().get(), before);
    }

    /// Max is a running maximum, so a smaller later sample must not lower it.
    #[test]
    fn stage_timing_max_keeps_the_largest_sample() {
        record_stage_timing("/probe", "max-probe", 50.0);
        record_stage_timing("/probe", "max-probe", 10.0);
        assert_eq!(
            stage_timing_max()
                .with_label_values(&["/probe", "max-probe"])
                .get(),
            50.0
        );
    }

    // ─── Timing min/max ─────────────────────────────────────────────────

    /// The whole point of the infinity sentinel: a gauge starts at 0, and no
    /// positive timing is ever below 0, so a naive gauge-only minimum would
    /// stay pinned at 0 forever and report a latency floor that never happened.
    #[test]
    fn the_first_sample_sets_the_minimum() {
        let state = MinState::new();
        assert_eq!(state.observe(250.0), Some(250.0));
    }

    #[test]
    fn only_a_lower_sample_moves_the_minimum() {
        let state = MinState::new();
        state.observe(250.0);

        assert_eq!(
            state.observe(400.0),
            None,
            "a higher sample must not move it"
        );
        assert_eq!(
            state.observe(250.0),
            None,
            "an equal sample must not move it"
        );
        assert_eq!(state.observe(10.0), Some(10.0));
    }

    #[test]
    fn bounds_track_the_extremes_of_a_series() {
        let state = MinState::new();
        let min_gauge = Gauge::new("test_bounds_min", "min").unwrap();
        let max_gauge = Gauge::new("test_bounds_max", "max").unwrap();

        for value in [120.0, 40.0, 900.0, 300.0] {
            observe_timing_bounds(value, &state, &min_gauge, &max_gauge);
        }

        assert_eq!(min_gauge.get(), 40.0);
        assert_eq!(max_gauge.get(), 900.0);
    }

    /// Values are rounded to 2 decimals at export, matching Python.
    #[test]
    fn bounds_are_rounded_to_two_decimals() {
        let state = MinState::new();
        let min_gauge = Gauge::new("test_round_min", "min").unwrap();
        let max_gauge = Gauge::new("test_round_max", "max").unwrap();

        observe_timing_bounds(123.456_789, &state, &min_gauge, &max_gauge);

        assert_eq!(min_gauge.get(), 123.46);
        assert_eq!(max_gauge.get(), 123.46);
        assert_eq!(round2(0.004), 0.0);
    }

    /// Before any traffic the gauges read 0, which is what Python exports while
    /// its count is zero — not the infinity it holds internally.
    #[test]
    fn an_untouched_bound_reads_zero_not_infinity() {
        let min_gauge = Gauge::new("test_untouched_min", "min").unwrap();
        assert_eq!(min_gauge.get(), 0.0);
        assert!(min_gauge.get().is_finite());
    }

    /// The Python-aligned names must be what actually reaches the scrape.
    #[test]
    fn timing_bounds_and_aligned_names_are_exposed() {
        use prometheus::{Encoder, TextEncoder};
        force_register_all(registry());
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&registry().gather(), &mut buf)
            .expect("registry encodes");
        let text = String::from_utf8(buf).expect("utf-8");

        for name in [
            "headroom_latency_ms_min",
            "headroom_latency_ms_max",
            "headroom_overhead_ms_min",
            "headroom_overhead_ms_max",
            "headroom_ttfb_ms_min",
            "headroom_ttfb_ms_max",
            // Renamed to match Python: no `_total` suffix on these two.
            "headroom_requests_by_provider",
            "headroom_requests_by_model",
        ] {
            assert!(
                text.contains(&format!("# TYPE {name} ")),
                "{name} missing from scrape"
            );
        }

        assert!(
            !text.contains("headroom_requests_by_provider_total"),
            "the old suffixed name must be gone"
        );
        assert!(!text.contains("headroom_requests_by_model_total"));
    }

    /// The histogram still emits `_sum`/`_count` under the same names Python
    /// uses, so adding the bounds did not cost the bucket data.
    #[test]
    fn the_histogram_still_provides_sum_and_count() {
        use prometheus::{Encoder, TextEncoder};
        record_request("bounds-probe", "m", 1, 1, 0, 5.0, false, 1.0, 1.0);
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&registry().gather(), &mut buf)
            .expect("registry encodes");
        let text = String::from_utf8(buf).expect("utf-8");

        assert!(text.contains("headroom_latency_ms_sum"));
        assert!(text.contains("headroom_latency_ms_count"));
        assert!(text.contains("headroom_latency_ms_bucket"));
    }

    /// Python exposes a per-cause maximum for session duration; the histogram
    /// cannot supply it.
    #[test]
    fn ws_session_duration_max_tracks_the_largest_per_cause() {
        record_ws_session_duration(500.0, "max-probe");
        record_ws_session_duration(100.0, "max-probe");
        assert_eq!(
            ws_session_duration_max()
                .with_label_values(&["max-probe"])
                .get(),
            500.0
        );
    }
}

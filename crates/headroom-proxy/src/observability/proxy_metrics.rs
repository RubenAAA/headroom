//! Phase G PR-G3 proxy-side observability metrics: rate-limit gauges,
//! the passthrough-bytes-modified alarm counter, service-tier +
//! response-status counters, and the image-base64 redaction counter.
//!
//! Grouped in one file because each metric is a thin
//! `OnceLock` + 1–2 emit helpers; splitting per metric would dilute
//! the call sites without adding test surface. Heavier metrics with
//! their own validation logic (`cache_hit_rate`, `compression_ratio`)
//! live in their own modules.

use std::sync::OnceLock;

use prometheus::{Gauge, GaugeVec, IntCounterVec, IntGaugeVec, Opts, Registry};

use super::metric_names::{
    LABEL_PATH, LABEL_PROVIDER, LABEL_REASON, LABEL_STATUS, LABEL_TIER, LABEL_WINDOW,
    METRIC_PROXY_PASSTHROUGH_BYTES_MODIFIED_TOTAL,
    METRIC_PROXY_PASSTHROUGH_BYTES_MODIFIED_TOTAL_HELP,
    METRIC_PROXY_RATELIMIT_UNIFIED_FALLBACK_PERCENTAGE,
    METRIC_PROXY_RATELIMIT_UNIFIED_FALLBACK_PERCENTAGE_HELP,
    METRIC_PROXY_RATELIMIT_UNIFIED_RESET_SECONDS,
    METRIC_PROXY_RATELIMIT_UNIFIED_RESET_SECONDS_HELP, METRIC_PROXY_RATELIMIT_UNIFIED_THROTTLED,
    METRIC_PROXY_RATELIMIT_UNIFIED_THROTTLED_HELP, METRIC_PROXY_RATELIMIT_UNIFIED_UTILIZATION,
    METRIC_PROXY_RATELIMIT_UNIFIED_UTILIZATION_HELP,
    METRIC_PROXY_RATE_LIMIT_REMAINING_INPUT_TOKENS,
    METRIC_PROXY_RATE_LIMIT_REMAINING_INPUT_TOKENS_HELP,
    METRIC_PROXY_RATE_LIMIT_REMAINING_OUTPUT_TOKENS,
    METRIC_PROXY_RATE_LIMIT_REMAINING_OUTPUT_TOKENS_HELP,
    METRIC_PROXY_RATE_LIMIT_REMAINING_REQUESTS, METRIC_PROXY_RATE_LIMIT_REMAINING_REQUESTS_HELP,
    METRIC_PROXY_RATE_LIMIT_REMAINING_TOKENS, METRIC_PROXY_RATE_LIMIT_REMAINING_TOKENS_HELP,
    METRIC_PROXY_RESPONSE_STATUS_COUNT_TOTAL, METRIC_PROXY_RESPONSE_STATUS_COUNT_TOTAL_HELP,
    METRIC_PROXY_SERVICE_TIER_COUNT_TOTAL, METRIC_PROXY_SERVICE_TIER_COUNT_TOTAL_HELP,
    METRIC_PROXY_STREAM_INCOMPLETE_TOTAL, METRIC_PROXY_STREAM_INCOMPLETE_TOTAL_HELP,
    METRIC_PROXY_UPSTREAM_RETRIES_EXHAUSTED_TOTAL,
    METRIC_PROXY_UPSTREAM_RETRIES_EXHAUSTED_TOTAL_HELP, METRIC_PROXY_UPSTREAM_RETRIES_TOTAL,
    METRIC_PROXY_UPSTREAM_RETRIES_TOTAL_HELP,
};

// ---------- proxy_upstream_retries_total{path,reason} ----------

/// Both label values come from the code, never from request input: `path` is
/// one of two forward paths and `reason` is one of the four
/// [`retry_reason`] constants.
pub fn upstream_retries_counter(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_UPSTREAM_RETRIES_TOTAL,
            METRIC_PROXY_UPSTREAM_RETRIES_TOTAL_HELP,
        );
        let counter = IntCounterVec::new(opts, &[LABEL_PATH, LABEL_REASON])
            .expect("proxy_upstream_retries_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_upstream_retries_total registers exactly once");
        counter
    })
}

/// The bounded `reason` label values.
pub mod retry_reason {
    pub const STATUS_429: &str = "status_429";
    pub const STATUS_529: &str = "status_529";
    pub const STATUS_5XX: &str = "status_5xx";
    pub const TRANSPORT: &str = "transport";
    /// A 200 response whose SSE body opened with an error event. The HTTP
    /// status says success; the body disagrees.
    pub const IN_BAND_SSE: &str = "in_band_sse";

    /// Bucket an HTTP status into a reason. 529 is Anthropic's "overloaded"
    /// and gets its own bucket because it behaves differently from a generic
    /// 5xx: it means the upstream is busy, not broken.
    pub fn from_status(status: u16) -> &'static str {
        match status {
            429 => STATUS_429,
            529 => STATUS_529,
            _ => STATUS_5XX,
        }
    }
}

/// Count one re-send. Call it at the point the retry is decided, next to the
/// `sleep`, so the counter and the backoff cannot drift apart.
pub fn record_upstream_retry(path: &str, reason: &str) {
    upstream_retries_counter(super::prometheus::registry())
        .with_label_values(&[path, reason])
        .inc();
}

// ---------- proxy_upstream_retries_exhausted_total{path,reason} ----------

pub fn upstream_retries_exhausted_counter(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_UPSTREAM_RETRIES_EXHAUSTED_TOTAL,
            METRIC_PROXY_UPSTREAM_RETRIES_EXHAUSTED_TOTAL_HELP,
        );
        let counter = IntCounterVec::new(opts, &[LABEL_PATH, LABEL_REASON])
            .expect("proxy_upstream_retries_exhausted_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_upstream_retries_exhausted_total registers exactly once");
        counter
    })
}

/// Count one turn the retry loop could not save. Same label vocabulary as
/// [`record_upstream_retry`], so the two divide.
pub fn record_upstream_retry_exhausted(path: &str, reason: &str) {
    upstream_retries_exhausted_counter(super::prometheus::registry())
        .with_label_values(&[path, reason])
        .inc();
}

// ---------- proxy_stream_incomplete_total{provider} ----------

pub fn stream_incomplete_counter(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_STREAM_INCOMPLETE_TOTAL,
            METRIC_PROXY_STREAM_INCOMPLETE_TOTAL_HELP,
        );
        let counter = IntCounterVec::new(opts, &[LABEL_PROVIDER])
            .expect("proxy_stream_incomplete_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_stream_incomplete_total registers exactly once");
        counter
    })
}

/// Count a stream that ended before its terminal event.
pub fn record_stream_incomplete(provider: &str) {
    stream_incomplete_counter(super::prometheus::registry())
        .with_label_values(&[provider])
        .inc();
}

// ---------- proxy_passthrough_bytes_modified_total{path} ----------

/// Counter (not gauge) so the metric obeys Prometheus' `_total`
/// convention while still meeting the spec's "must stay 0" alarm
/// requirement: dashboards alert on `rate(...[5m]) > 0`. A counter
/// stays at 0 forever until something actually modifies passthrough
/// bytes — which is the alarmable event.
pub fn passthrough_bytes_modified_counter(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_PASSTHROUGH_BYTES_MODIFIED_TOTAL,
            METRIC_PROXY_PASSTHROUGH_BYTES_MODIFIED_TOTAL_HELP,
        );
        let counter = IntCounterVec::new(opts, &[LABEL_PATH])
            .expect("proxy_passthrough_bytes_modified_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_passthrough_bytes_modified_total registers exactly once");
        counter
    })
}

/// Add `bytes` modified on a path that was supposed to be byte-equal
/// passthrough. The increment value is the byte delta — operators
/// then `rate(...)` to see "bytes/sec of policy violation".
pub fn record_passthrough_bytes_modified(path: &str, bytes: u64, request_id: &str) {
    passthrough_bytes_modified_counter(super::prometheus::registry())
        .with_label_values(&[path])
        .inc_by(bytes);
    tracing::warn!(
        event = "passthrough_bytes_modified",
        metric = METRIC_PROXY_PASSTHROUGH_BYTES_MODIFIED_TOTAL,
        path = %path,
        bytes = bytes,
        request_id = %request_id,
        "passthrough path modified bytes; this is the cache-safety alarm condition"
    );
}

// ---------- proxy_rate_limit_remaining_* gauges ----------

pub fn rate_limit_remaining_requests_gauge(registry: &Registry) -> &'static IntGaugeVec {
    static GAUGE: OnceLock<IntGaugeVec> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_RATE_LIMIT_REMAINING_REQUESTS,
            METRIC_PROXY_RATE_LIMIT_REMAINING_REQUESTS_HELP,
        );
        let gauge = IntGaugeVec::new(opts, &[LABEL_PROVIDER])
            .expect("proxy_rate_limit_remaining_requests descriptor is well-formed");
        registry
            .register(Box::new(gauge.clone()))
            .expect("proxy_rate_limit_remaining_requests registers exactly once");
        gauge
    })
}

pub fn rate_limit_remaining_tokens_gauge(registry: &Registry) -> &'static IntGaugeVec {
    static GAUGE: OnceLock<IntGaugeVec> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_RATE_LIMIT_REMAINING_TOKENS,
            METRIC_PROXY_RATE_LIMIT_REMAINING_TOKENS_HELP,
        );
        let gauge = IntGaugeVec::new(opts, &[LABEL_PROVIDER])
            .expect("proxy_rate_limit_remaining_tokens descriptor is well-formed");
        registry
            .register(Box::new(gauge.clone()))
            .expect("proxy_rate_limit_remaining_tokens registers exactly once");
        gauge
    })
}

pub fn rate_limit_remaining_input_tokens_gauge(registry: &Registry) -> &'static IntGaugeVec {
    static GAUGE: OnceLock<IntGaugeVec> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_RATE_LIMIT_REMAINING_INPUT_TOKENS,
            METRIC_PROXY_RATE_LIMIT_REMAINING_INPUT_TOKENS_HELP,
        );
        let gauge = IntGaugeVec::new(opts, &[LABEL_PROVIDER])
            .expect("proxy_rate_limit_remaining_input_tokens descriptor is well-formed");
        registry
            .register(Box::new(gauge.clone()))
            .expect("proxy_rate_limit_remaining_input_tokens registers exactly once");
        gauge
    })
}

pub fn rate_limit_remaining_output_tokens_gauge(registry: &Registry) -> &'static IntGaugeVec {
    static GAUGE: OnceLock<IntGaugeVec> = OnceLock::new();
    GAUGE.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_RATE_LIMIT_REMAINING_OUTPUT_TOKENS,
            METRIC_PROXY_RATE_LIMIT_REMAINING_OUTPUT_TOKENS_HELP,
        );
        let gauge = IntGaugeVec::new(opts, &[LABEL_PROVIDER])
            .expect("proxy_rate_limit_remaining_output_tokens descriptor is well-formed");
        registry
            .register(Box::new(gauge.clone()))
            .expect("proxy_rate_limit_remaining_output_tokens registers exactly once");
        gauge
    })
}

/// Snapshot of upstream rate-limit headers extracted from one
/// response. None-fields are headers the upstream did not include
/// (per realignment build-constraint "no silent fallbacks": we do not
/// fabricate a value, we just don't emit on that gauge).
#[derive(Debug, Default, Clone, Copy)]
pub struct RateLimitSnapshot {
    pub remaining_requests: Option<i64>,
    pub remaining_tokens: Option<i64>,
    pub remaining_input_tokens: Option<i64>,
    pub remaining_output_tokens: Option<i64>,
}

/// Extract a `RateLimitSnapshot` from a HeaderMap. Accepts both
/// Anthropic (`anthropic-ratelimit-*`) and OpenAI (`x-ratelimit-*`)
/// header families. Missing headers stay `None`.
///
/// `Retry-After` is intentionally NOT parsed here — that header is
/// upstream-bounded by 429s, not steady-state telemetry, and lives in
/// the existing structured 429 log line.
pub fn extract_rate_limit_snapshot(headers: &http::HeaderMap) -> RateLimitSnapshot {
    let parse_i64 = |name: &str| -> Option<i64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<i64>().ok())
    };
    RateLimitSnapshot {
        remaining_requests: parse_i64("anthropic-ratelimit-requests-remaining")
            .or_else(|| parse_i64("x-ratelimit-remaining-requests")),
        remaining_tokens: parse_i64("anthropic-ratelimit-tokens-remaining")
            .or_else(|| parse_i64("x-ratelimit-remaining-tokens")),
        remaining_input_tokens: parse_i64("anthropic-ratelimit-input-tokens-remaining"),
        remaining_output_tokens: parse_i64("anthropic-ratelimit-output-tokens-remaining"),
    }
}

/// Set all four gauges that the snapshot populates.
pub fn record_rate_limit_snapshot(
    provider: &'static str,
    snapshot: &RateLimitSnapshot,
    request_id: &str,
) {
    let registry = super::prometheus::registry();
    if let Some(v) = snapshot.remaining_requests {
        rate_limit_remaining_requests_gauge(registry)
            .with_label_values(&[provider])
            .set(v);
    }
    if let Some(v) = snapshot.remaining_tokens {
        rate_limit_remaining_tokens_gauge(registry)
            .with_label_values(&[provider])
            .set(v);
    }
    if let Some(v) = snapshot.remaining_input_tokens {
        rate_limit_remaining_input_tokens_gauge(registry)
            .with_label_values(&[provider])
            .set(v);
    }
    if let Some(v) = snapshot.remaining_output_tokens {
        rate_limit_remaining_output_tokens_gauge(registry)
            .with_label_values(&[provider])
            .set(v);
    }
    tracing::debug!(
        event = "metric_recorded",
        metric = "proxy_rate_limit_remaining_*",
        provider = provider,
        request_id = %request_id,
        remaining_requests = ?snapshot.remaining_requests,
        remaining_tokens = ?snapshot.remaining_tokens,
        remaining_input_tokens = ?snapshot.remaining_input_tokens,
        remaining_output_tokens = ?snapshot.remaining_output_tokens,
        "recorded proxy_rate_limit_remaining_* gauges"
    );
}

// ---------- proxy_ratelimit_unified_* (subscription/OAuth) ----------
//
// API-key traffic returns `anthropic-ratelimit-{requests,tokens,...}-
// remaining` (parsed by `extract_rate_limit_snapshot` above).
// Subscription / OAuth traffic returns a DIFFERENT header family the
// API-key parser never matches, which is why the `*-remaining` gauges
// stay empty on a Claude-subscription deployment:
//
//   anthropic-ratelimit-unified-5h-utilization: 0.2
//   anthropic-ratelimit-unified-5h-status:      allowed
//   anthropic-ratelimit-unified-5h-reset:       1781973000
//   anthropic-ratelimit-unified-7d-utilization: 0.06
//   anthropic-ratelimit-unified-7d_sonnet-utilization: 0.01   (per-model window)
//   anthropic-ratelimit-unified-status:         allowed       (top-level)
//   anthropic-ratelimit-unified-reset:          1781973000
//   anthropic-ratelimit-unified-overage-status: rejected
//   anthropic-ratelimit-unified-fallback-percentage: 0.5
//   anthropic-ratelimit-unified-representative-claim: five_hour
//
// `utilization` is the fraction [0,1] of the window consumed, so
// `1 - utilization` is the remaining subscription headroom — the
// number an operator actually wants on a subscription plan.

const UNIFIED_PREFIX: &str = "anthropic-ratelimit-unified-";

/// One window's worth of unified rate-limit data. `window` is the
/// dynamic key Anthropic emits (`5h`, `7d`, `7d_sonnet`, …).
#[derive(Debug, Clone, PartialEq)]
pub struct UnifiedWindow {
    pub window: String,
    pub utilization: Option<f64>,
    pub status: Option<String>,
    pub reset: Option<i64>,
}

/// Parsed `anthropic-ratelimit-unified-*` family. `windows` holds the
/// per-window rows; the remaining fields are the top-level keys.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnifiedRateLimitSnapshot {
    pub windows: Vec<UnifiedWindow>,
    pub overall_status: Option<String>,
    pub overall_reset: Option<i64>,
    pub overage_status: Option<String>,
    pub overage_disabled_reason: Option<String>,
    pub fallback_percentage: Option<f64>,
    pub representative_claim: Option<String>,
}

/// Extract the unified (subscription/OAuth) rate-limit family. Returns
/// an empty snapshot when no `anthropic-ratelimit-unified-*` headers
/// are present (API-key traffic). Window rows are sorted by window key
/// for deterministic output.
pub fn extract_unified_rate_limit(headers: &http::HeaderMap) -> UnifiedRateLimitSnapshot {
    use std::collections::BTreeMap;

    let mut snap = UnifiedRateLimitSnapshot::default();
    // BTreeMap → window rows come out sorted by key, deterministically.
    let mut windows: BTreeMap<String, UnifiedWindow> = BTreeMap::new();

    for (name, value) in headers.iter() {
        let rest = match name.as_str().strip_prefix(UNIFIED_PREFIX) {
            Some(r) => r,
            None => continue,
        };
        let val = match value.to_str() {
            Ok(v) => v.trim(),
            // Non-UTF8 header value — per "no silent fallback" we skip
            // rather than guess; absence is the signal.
            Err(_) => continue,
        };
        match rest {
            // Top-level keys first, so multi-dash keys like
            // "overage-status" aren't mis-read as window "overage".
            "status" => snap.overall_status = Some(val.to_string()),
            "reset" => snap.overall_reset = val.parse::<i64>().ok(),
            "overage-status" => snap.overage_status = Some(val.to_string()),
            "overage-disabled-reason" => snap.overage_disabled_reason = Some(val.to_string()),
            "fallback-percentage" => snap.fallback_percentage = val.parse::<f64>().ok(),
            "representative-claim" => snap.representative_claim = Some(val.to_string()),
            _ => {
                // Window-scoped: "<window>-<field>", field ∈
                // {utilization,status,reset}. Window keys use '_'
                // (e.g. 7d_sonnet) so rsplit on '-' is unambiguous.
                // Only recognised fields create a row — an unknown
                // suffix must NOT spawn a junk all-None window.
                let (window, field) = match rest.rsplit_once('-') {
                    Some((w, f)) if matches!(f, "utilization" | "status" | "reset") => (w, f),
                    _ => continue,
                };
                let entry = windows
                    .entry(window.to_string())
                    .or_insert_with(|| UnifiedWindow {
                        window: window.to_string(),
                        utilization: None,
                        status: None,
                        reset: None,
                    });
                match field {
                    "utilization" => entry.utilization = val.parse::<f64>().ok(),
                    "status" => entry.status = Some(val.to_string()),
                    "reset" => entry.reset = val.parse::<i64>().ok(),
                    _ => unreachable!("field guarded by matches! above"),
                }
            }
        }
    }

    snap.windows = windows.into_values().collect();
    snap
}

/// Window key used for the top-level (cross-window) unified fields.
pub const UNIFIED_OVERALL_WINDOW: &str = "overall";

/// The non-throttled status: every other value flips `throttled` to 1.
const UNIFIED_STATUS_ALLOWED: &str = "allowed";

pub fn unified_utilization_gauge(registry: &Registry) -> &'static GaugeVec {
    static G: OnceLock<GaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_RATELIMIT_UNIFIED_UTILIZATION,
            METRIC_PROXY_RATELIMIT_UNIFIED_UTILIZATION_HELP,
        );
        let g = GaugeVec::new(opts, &[LABEL_WINDOW])
            .expect("proxy_ratelimit_unified_utilization descriptor is well-formed");
        registry
            .register(Box::new(g.clone()))
            .expect("proxy_ratelimit_unified_utilization registers exactly once");
        g
    })
}

pub fn unified_reset_gauge(registry: &Registry) -> &'static IntGaugeVec {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_RATELIMIT_UNIFIED_RESET_SECONDS,
            METRIC_PROXY_RATELIMIT_UNIFIED_RESET_SECONDS_HELP,
        );
        let g = IntGaugeVec::new(opts, &[LABEL_WINDOW])
            .expect("proxy_ratelimit_unified_reset_seconds descriptor is well-formed");
        registry
            .register(Box::new(g.clone()))
            .expect("proxy_ratelimit_unified_reset_seconds registers exactly once");
        g
    })
}

pub fn unified_throttled_gauge(registry: &Registry) -> &'static IntGaugeVec {
    static G: OnceLock<IntGaugeVec> = OnceLock::new();
    G.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_RATELIMIT_UNIFIED_THROTTLED,
            METRIC_PROXY_RATELIMIT_UNIFIED_THROTTLED_HELP,
        );
        let g = IntGaugeVec::new(opts, &[LABEL_WINDOW])
            .expect("proxy_ratelimit_unified_throttled descriptor is well-formed");
        registry
            .register(Box::new(g.clone()))
            .expect("proxy_ratelimit_unified_throttled registers exactly once");
        g
    })
}

pub fn unified_fallback_percentage_gauge(registry: &Registry) -> &'static Gauge {
    static G: OnceLock<Gauge> = OnceLock::new();
    G.get_or_init(|| {
        let g = Gauge::new(
            METRIC_PROXY_RATELIMIT_UNIFIED_FALLBACK_PERCENTAGE,
            METRIC_PROXY_RATELIMIT_UNIFIED_FALLBACK_PERCENTAGE_HELP,
        )
        .expect("proxy_ratelimit_unified_fallback_percentage descriptor is well-formed");
        registry
            .register(Box::new(g.clone()))
            .expect("proxy_ratelimit_unified_fallback_percentage registers exactly once");
        g
    })
}

/// Map a status string to the `throttled` gauge value: 0 when the
/// upstream says `allowed`, 1 for any throttling/blocking state.
fn throttled_value(status: &str) -> i64 {
    if status == UNIFIED_STATUS_ALLOWED {
        0
    } else {
        1
    }
}

/// Record a parsed unified snapshot into the subscription rate-limit
/// gauges. Numeric signals (utilization, reset, fallback) become
/// gauges; the per-window status string collapses to a boolean
/// `throttled` gauge (1 when status != "allowed") with the full
/// string preserved on the structured log line. Overage/claim strings
/// are log-only — they don't map cleanly to a numeric series.
pub fn record_unified_rate_limit(snapshot: &UnifiedRateLimitSnapshot, request_id: &str) {
    let registry = super::prometheus::registry();

    for w in &snapshot.windows {
        if let Some(u) = w.utilization {
            unified_utilization_gauge(registry)
                .with_label_values(&[&w.window])
                .set(u);
        }
        if let Some(r) = w.reset {
            unified_reset_gauge(registry)
                .with_label_values(&[&w.window])
                .set(r);
        }
        if let Some(status) = &w.status {
            unified_throttled_gauge(registry)
                .with_label_values(&[&w.window])
                .set(throttled_value(status));
        }
    }

    // Top-level (cross-window) signals under the `overall` window.
    if let Some(status) = &snapshot.overall_status {
        unified_throttled_gauge(registry)
            .with_label_values(&[UNIFIED_OVERALL_WINDOW])
            .set(throttled_value(status));
    }
    if let Some(r) = snapshot.overall_reset {
        unified_reset_gauge(registry)
            .with_label_values(&[UNIFIED_OVERALL_WINDOW])
            .set(r);
    }
    if let Some(p) = snapshot.fallback_percentage {
        unified_fallback_percentage_gauge(registry).set(p);
    }

    tracing::debug!(
        event = "metric_recorded",
        metric = "proxy_ratelimit_unified_*",
        request_id = %request_id,
        windows = snapshot.windows.len(),
        overall_status = snapshot.overall_status.as_deref().unwrap_or(""),
        overage_status = snapshot.overage_status.as_deref().unwrap_or(""),
        overage_disabled_reason = snapshot.overage_disabled_reason.as_deref().unwrap_or(""),
        representative_claim = snapshot.representative_claim.as_deref().unwrap_or(""),
        "recorded proxy_ratelimit_unified_* subscription gauges"
    );
}

// ---------- proxy_service_tier_count_total{tier} ----------

pub fn service_tier_counter(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_SERVICE_TIER_COUNT_TOTAL,
            METRIC_PROXY_SERVICE_TIER_COUNT_TOTAL_HELP,
        );
        let counter = IntCounterVec::new(opts, &[LABEL_TIER])
            .expect("proxy_service_tier_count_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_service_tier_count_total registers exactly once");
        counter
    })
}

pub fn record_service_tier(tier: &str, request_id: &str) {
    service_tier_counter(super::prometheus::registry())
        .with_label_values(&[tier])
        .inc();
    tracing::debug!(
        event = "metric_recorded",
        metric = METRIC_PROXY_SERVICE_TIER_COUNT_TOTAL,
        tier = %tier,
        request_id = %request_id,
        "incremented proxy_service_tier_count_total"
    );
}

// ---------- proxy_response_status_count_total{status} ----------

pub fn response_status_counter(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let opts = Opts::new(
            METRIC_PROXY_RESPONSE_STATUS_COUNT_TOTAL,
            METRIC_PROXY_RESPONSE_STATUS_COUNT_TOTAL_HELP,
        );
        let counter = IntCounterVec::new(opts, &[LABEL_STATUS])
            .expect("proxy_response_status_count_total descriptor is well-formed");
        registry
            .register(Box::new(counter.clone()))
            .expect("proxy_response_status_count_total registers exactly once");
        counter
    })
}

/// Record a Responses terminal status. `reason` is the
/// `incomplete_details.reason` field on `incomplete` responses (or
/// any other side-channel info worth pairing with the metric). It is
/// emitted in the structured log alongside the counter increment but
/// is NOT used as a label (would blow up cardinality).
pub fn record_response_status(status: &str, reason: Option<&str>, request_id: &str) {
    response_status_counter(super::prometheus::registry())
        .with_label_values(&[status])
        .inc();
    // Optional-3: aligned with the peer `record_*` helpers in this
    // module which all use `debug!` for the metric-correlation log
    // line. INFO was inconsistent and produced extra log volume
    // during normal Responses traffic.
    tracing::debug!(
        event = "metric_recorded",
        metric = METRIC_PROXY_RESPONSE_STATUS_COUNT_TOTAL,
        status = %status,
        reason = reason.unwrap_or(""),
        request_id = %request_id,
        "incremented proxy_response_status_count_total"
    );
}

// Phase G PR-G3 remediation (C3 + C4): the image-redacted counter
// and the wrap_rtk_invocations counter were originally registered
// here but neither had a production emit site that crossed the
// Python/Rust boundary. The image-redacted counter moved Python-side
// (`headroom.proxy.request_logger::redactions_total`) and the Python
// proxy's `/metrics` exporter surfaces it; the RTK counter is gone
// entirely along with the rtk integration itself — see
// `docs/observability.md` for the placement decision. Keeping a
// dead Rust counter would (a) violate the "no dead metrics
// registered" review finding and (b) mislead Phase H canary
// dashboards into expecting two scrape sources for what is really
// one Python-side counter.

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};

    #[test]
    fn extract_rate_limit_snapshot_anthropic() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-requests-remaining",
            HeaderValue::from_static("499"),
        );
        h.insert(
            "anthropic-ratelimit-tokens-remaining",
            HeaderValue::from_static("99000"),
        );
        h.insert(
            "anthropic-ratelimit-input-tokens-remaining",
            HeaderValue::from_static("80000"),
        );
        h.insert(
            "anthropic-ratelimit-output-tokens-remaining",
            HeaderValue::from_static("16000"),
        );
        let snap = extract_rate_limit_snapshot(&h);
        assert_eq!(snap.remaining_requests, Some(499));
        assert_eq!(snap.remaining_tokens, Some(99000));
        assert_eq!(snap.remaining_input_tokens, Some(80000));
        assert_eq!(snap.remaining_output_tokens, Some(16000));
    }

    #[test]
    fn extract_rate_limit_snapshot_openai() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("1000"),
        );
        h.insert(
            "x-ratelimit-remaining-tokens",
            HeaderValue::from_static("250000"),
        );
        let snap = extract_rate_limit_snapshot(&h);
        assert_eq!(snap.remaining_requests, Some(1000));
        assert_eq!(snap.remaining_tokens, Some(250000));
        // OpenAI does not split input/output buckets.
        assert_eq!(snap.remaining_input_tokens, None);
        assert_eq!(snap.remaining_output_tokens, None);
    }

    #[test]
    fn extract_rate_limit_snapshot_no_headers() {
        let h = HeaderMap::new();
        let snap = extract_rate_limit_snapshot(&h);
        assert_eq!(snap.remaining_requests, None);
        assert_eq!(snap.remaining_tokens, None);
        assert_eq!(snap.remaining_input_tokens, None);
        assert_eq!(snap.remaining_output_tokens, None);
    }

    #[test]
    fn extract_rate_limit_snapshot_unparseable_value_is_none() {
        let mut h = HeaderMap::new();
        // Junk value — must not panic; must surface as None per
        // "no silent fallback" (the absence itself is the signal).
        h.insert(
            "anthropic-ratelimit-requests-remaining",
            HeaderValue::from_static("not-a-number"),
        );
        let snap = extract_rate_limit_snapshot(&h);
        assert_eq!(snap.remaining_requests, None);
    }

    fn window<'a>(snap: &'a UnifiedRateLimitSnapshot, name: &str) -> &'a UnifiedWindow {
        snap.windows
            .iter()
            .find(|w| w.window == name)
            .unwrap_or_else(|| panic!("window {name} missing from {:?}", snap.windows))
    }

    #[test]
    fn extract_unified_parses_subscription_headers() {
        // Real header shape captured from a live Claude-subscription
        // (OAuth) response through the proxy.
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-reset", "1781973000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.06"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-reset", "1782414000"),
            ("anthropic-ratelimit-unified-7d_sonnet-utilization", "0.01"),
            ("anthropic-ratelimit-unified-7d_sonnet-status", "allowed"),
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-reset", "1781973000"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "org_level_disabled",
            ),
            ("anthropic-ratelimit-unified-fallback-percentage", "0.5"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour",
            ),
        ] {
            h.insert(k, HeaderValue::from_str(v).unwrap());
        }
        let snap = extract_unified_rate_limit(&h);

        // Per-window rows (incl. the dynamic per-model 7d_sonnet window).
        assert_eq!(snap.windows.len(), 3, "windows: {:?}", snap.windows);
        let w5h = window(&snap, "5h");
        assert_eq!(w5h.utilization, Some(0.2));
        assert_eq!(w5h.status.as_deref(), Some("allowed"));
        assert_eq!(w5h.reset, Some(1781973000));
        assert_eq!(window(&snap, "7d").utilization, Some(0.06));
        assert_eq!(window(&snap, "7d_sonnet").utilization, Some(0.01));

        // Top-level keys must NOT be mis-parsed as windows.
        assert_eq!(snap.overall_status.as_deref(), Some("allowed"));
        assert_eq!(snap.overall_reset, Some(1781973000));
        assert_eq!(snap.overage_status.as_deref(), Some("rejected"));
        assert_eq!(
            snap.overage_disabled_reason.as_deref(),
            Some("org_level_disabled")
        );
        assert_eq!(snap.fallback_percentage, Some(0.5));
        assert_eq!(snap.representative_claim.as_deref(), Some("five_hour"));
    }

    #[test]
    fn extract_unified_empty_on_api_key_traffic() {
        // API-key responses carry the *-remaining family, never unified-*.
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-requests-remaining",
            HeaderValue::from_static("499"),
        );
        let snap = extract_unified_rate_limit(&h);
        assert_eq!(snap, UnifiedRateLimitSnapshot::default());
    }

    fn scrape_registry() -> String {
        use prometheus::Encoder;
        let mf = super::super::prometheus::registry().gather();
        let mut buf = Vec::new();
        prometheus::TextEncoder::new()
            .encode(&mf, &mut buf)
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn record_unified_emits_subscription_gauges() {
        // Unique window suffixes so this test's series don't collide
        // with other tests sharing the global registry.
        let snap = UnifiedRateLimitSnapshot {
            windows: vec![
                UnifiedWindow {
                    window: "5h_rectest".into(),
                    utilization: Some(0.2),
                    status: Some("allowed".into()),
                    reset: Some(1781973000),
                },
                UnifiedWindow {
                    window: "7d_rectest".into(),
                    utilization: Some(0.06),
                    status: Some("rejected".into()),
                    reset: Some(1782414000),
                },
            ],
            overall_status: Some("allowed".into()),
            overall_reset: Some(1781973000),
            overage_status: Some("rejected".into()),
            overage_disabled_reason: Some("org_level_disabled".into()),
            fallback_percentage: Some(0.5),
            representative_claim: Some("five_hour".into()),
        };
        record_unified_rate_limit(&snap, "req-rectest");
        let body = scrape_registry();

        // utilization (the headroom number) per window
        assert!(
            body.contains(r#"proxy_ratelimit_unified_utilization{window="5h_rectest"} 0.2"#),
            "missing 5h utilization:\n{body}"
        );
        // throttled: allowed -> 0, rejected -> 1
        assert!(
            body.contains(r#"proxy_ratelimit_unified_throttled{window="5h_rectest"} 0"#),
            "5h should not be throttled:\n{body}"
        );
        assert!(
            body.contains(r#"proxy_ratelimit_unified_throttled{window="7d_rectest"} 1"#),
            "7d rejected should be throttled:\n{body}"
        );
        // overall window carries top-level status + reset
        assert!(body.contains(r#"proxy_ratelimit_unified_throttled{window="overall"} 0"#));
        // reset epoch per window
        assert!(body
            .contains(r#"proxy_ratelimit_unified_reset_seconds{window="5h_rectest"} 1781973000"#));
        // top-level fallback percentage (no window label)
        assert!(
            body.contains("proxy_ratelimit_unified_fallback_percentage 0.5"),
            "missing fallback percentage:\n{body}"
        );
    }

    #[test]
    fn extract_unified_unparseable_utilization_is_none_but_window_kept() {
        // A junk utilization must not panic and must not drop the
        // window (its status/reset may still be useful).
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            HeaderValue::from_static("not-a-float"),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-status",
            HeaderValue::from_static("allowed"),
        );
        let snap = extract_unified_rate_limit(&h);
        let w = window(&snap, "5h");
        assert_eq!(w.utilization, None);
        assert_eq!(w.status.as_deref(), Some("allowed"));
    }
}

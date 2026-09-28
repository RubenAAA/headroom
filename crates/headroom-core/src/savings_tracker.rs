//! Durable proxy savings + display-session tracking (Rust port of
//! `headroom/proxy/savings_tracker.py`, SCHEMA_VERSION = 4).
//!
//! Persists cumulative compression savings, a canonical display-session window
//! (60-min inactivity rollover), per-project stats (capped at 50), and a bounded
//! cumulative-checkpoint history (5000 points / 365 days) to a JSON file via an
//! atomic temp-file+fsync+rename write. [`SavingsTracker::history_response`]
//! derives hourly/daily/weekly/monthly rollups on demand.
//!
//! Deviation from Python: cost pricing uses the vendored [`crate::pricing`]
//! table (no `litellm`), falling back to the blended per-token rate for unpriced
//! models — same shape as Python's litellm-absent path. Persisted `projects`
//! use a `BTreeMap` (deterministic key order) rather than Python's insertion
//! order; byte-identical file parity is not required (each impl reads its own).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const SCHEMA_VERSION: i64 = 4;
pub const DEFAULT_MAX_HISTORY_POINTS: usize = 5000;
pub const DEFAULT_MAX_PROJECTS: usize = 50;
pub const PROJECT_NAME_MAX_LENGTH: usize = 128;
pub const DEFAULT_MAX_HISTORY_AGE_DAYS: i64 = 365;
pub const DEFAULT_MAX_RESPONSE_HISTORY_POINTS: usize = 500;
pub const DEFAULT_DISPLAY_SESSION_INACTIVITY_MINUTES: i64 = 60;
pub const DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN: f64 = 3.0 / 1_000_000.0;
/// Blended OUTPUT price ($/token) for models the pricing table doesn't cover.
/// Higher than the input fallback because generated tokens cost more.
pub const DEFAULT_FALLBACK_OUTPUT_COST_PER_TOKEN: f64 = 15.0 / 1_000_000.0;

const PROVIDER_UNKNOWN: &str = "unknown";
const MODEL_UNKNOWN: &str = "unknown";

// ── small helpers ──

fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// ISO-8601 UTC, seconds precision, `Z` suffix (mirrors `_to_utc_iso`).
fn to_utc_iso(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Parse an ISO timestamp (accepting `Z`), assuming UTC when naive.
fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    if value.is_empty() {
        return None;
    }
    let normalized = value.replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&normalized, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

fn round_n(value: f64, ndigits: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let f = 10f64.powi(ndigits);
    (value * f).round_ties_even() / f
}

fn coerce_int(value: i64) -> i64 {
    value.max(0)
}

fn coerce_float(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.max(0.0)
}

fn coerce_signed_float(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

fn normalize_provider(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => PROVIDER_UNKNOWN.to_string(),
    }
}

fn normalize_model(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => MODEL_UNKNOWN.to_string(),
    }
}

fn is_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    !c.is_control() && !c.is_whitespace()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Normalize a client-supplied project name; `None` when unusable.
pub fn sanitize_project_name(value: Option<&str>) -> Option<String> {
    let value = value?;
    let decoded = percent_decode(value);
    let cleaned: String = decoded.chars().filter(|c| is_printable(*c)).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.chars().take(PROJECT_NAME_MAX_LENGTH).collect())
}

// ── pricing (via vendored table) ──

fn estimate_compression_savings_usd(model: &str, tokens_saved: i64) -> f64 {
    if tokens_saved <= 0 {
        return 0.0;
    }
    // Distinguish "price unknown" (model not in the table -> fall back) from a
    // model that is legitimately FREE (rate 0.0). Filtering on `> 0.0` treated a
    // real zero as unavailable and billed the fallback rate, inventing savings
    // for a model that costs nothing.
    let rate = crate::pricing::lookup(model)
        .map(|p| p.input_cost_per_token)
        .unwrap_or(DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN);
    tokens_saved as f64 * rate
}

/// Estimate output-shaping savings in USD from saved *output* tokens.
///
/// Mirrors [`estimate_compression_savings_usd`] but prices at the model's
/// OUTPUT rate: the shaper reduces generated tokens, not input. Carries the
/// same zero-price-versus-unknown-price distinction.
fn estimate_output_savings_usd(model: &str, tokens_saved: i64) -> f64 {
    if tokens_saved <= 0 {
        return 0.0;
    }
    let rate = crate::pricing::lookup(model)
        .map(|p| p.output_cost_per_token)
        .unwrap_or(DEFAULT_FALLBACK_OUTPUT_COST_PER_TOKEN);
    tokens_saved as f64 * rate
}

/// Resolve a request's cache writes into `(token_count, cost_usd)` across the
/// two published TTL rates.
///
/// Anthropic bills a 5-minute write at 1.25x input and a 1-hour write at 2.0x,
/// and reports which of the two each request used in
/// `usage.cache_creation.ephemeral_{5m,1h}_input_tokens`. Where that split is
/// present the price is measured, not assumed from the configured TTL — which
/// matters because `--force-1h-cache-ttl` makes every write the 2.0x kind while
/// the 5m rate is what a single-rate table would charge.
///
/// Any total the reported split does not cover — including every token on a
/// route that reports no split at all — is priced at the 5m rate. That is the
/// cheaper of the two, so an unreported write understates cost rather than
/// inventing a premium the provider may never have charged.
fn cache_write_tokens_and_cost(
    pricing: &crate::pricing::ModelPricing,
    fallback_5m_rate: f64,
    cache_write_tokens: i64,
    cache_write_5m_tokens: i64,
    cache_write_1h_tokens: i64,
) -> (f64, f64) {
    let rate_5m = pricing
        .cache_write_cost_per_token
        .unwrap_or(fallback_5m_rate);
    let rate_1h = pricing.cache_write_1h_cost_per_token.unwrap_or(rate_5m);
    let total = coerce_int(cache_write_tokens);
    let w5m = coerce_int(cache_write_5m_tokens);
    let w1h = coerce_int(cache_write_1h_tokens);
    // A split that overshoots the reported total wins: it is the more specific
    // measurement. `residual` therefore floors at zero rather than going
    // negative and refunding tokens the provider did bill.
    let residual = (total - w5m - w1h).max(0);
    let at_5m = (w5m + residual) as f64;
    let at_1h = w1h as f64;
    (at_5m + at_1h, at_5m * rate_5m + at_1h * rate_1h)
}

fn estimate_input_cost_usd(
    model: &str,
    input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cache_write_5m_tokens: i64,
    cache_write_1h_tokens: i64,
    uncached_input_tokens: i64,
) -> f64 {
    let total_input = coerce_int(input_tokens);
    let cr = coerce_int(cache_read_tokens);
    let cw_total = coerce_int(cache_write_tokens);
    let unc = coerce_int(uncached_input_tokens);
    // Effective write count: a reported split that overshoots the total wins,
    // matching how `cache_write_tokens_and_cost` prices it.
    let cw = cw_total.max(coerce_int(cache_write_5m_tokens) + coerce_int(cache_write_1h_tokens));
    let use_breakdown = (cr + cw + unc) > 0;
    let chargeable = if use_breakdown {
        cr + cw + unc
    } else {
        total_input
    };
    if chargeable <= 0 {
        return 0.0;
    }
    // Same zero-price-versus-unknown-price distinction as
    // `estimate_compression_savings_usd`: a model IN the table is priced at
    // its own rate even when that rate is 0.0 — only a failed lookup falls
    // back. Filtering on `> 0.0` here once billed free models at $3/M.
    match crate::pricing::lookup(model) {
        None => chargeable as f64 * DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN,
        Some(p) => {
            let input_rate = p.input_cost_per_token;
            if use_breakdown {
                let cr_rate = p.cache_read_cost_per_token.unwrap_or(input_rate);
                let (_, cw_cost) = cache_write_tokens_and_cost(
                    p,
                    input_rate,
                    cw_total,
                    cache_write_5m_tokens,
                    cache_write_1h_tokens,
                );
                cr as f64 * cr_rate + cw_cost + unc as f64 * input_rate
            } else {
                total_input as f64 * input_rate
            }
        }
    }
}

/// Net provider-cache savings against pricing every observed input token at
/// the model's fresh-input rate.
///
/// Cache reads contribute their discount while cache writes contribute their
/// premium (and can make the result negative). Writes are priced per TTL from
/// the provider's own `ephemeral_{5m,1h}` split — see
/// [`cache_write_tokens_and_cost`]. Unknown models fail open: with no published
/// rates there is no defensible cache counterfactual to book.
fn estimate_cache_savings_usd(
    model: &str,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cache_write_5m_tokens: i64,
    cache_write_1h_tokens: i64,
    uncached_input_tokens: i64,
) -> f64 {
    let Some(pricing) = crate::pricing::lookup(model) else {
        return 0.0;
    };
    let fresh_rate = pricing.input_cost_per_token;
    let read_rate = pricing.cache_read_cost_per_token.unwrap_or(fresh_rate);
    let (write, write_cost) = cache_write_tokens_and_cost(
        pricing,
        fresh_rate,
        cache_write_tokens,
        cache_write_5m_tokens,
        cache_write_1h_tokens,
    );
    let read = coerce_int(cache_read_tokens) as f64;
    let uncached = coerce_int(uncached_input_tokens) as f64;
    let all_fresh = (read + write + uncached) * fresh_rate;
    let actual = read * read_rate + write_cost + uncached * fresh_rate;
    all_fresh - actual
}

/// Savings as a share of what the same work would have cost without the
/// proxy.
///
/// The baseline is the client on its own, NOT an all-fresh uncached request.
/// Claude Code sends its own `cache_control` markers and gets cache reads at
/// 0.1x whether or not this proxy exists, so counting the whole cache discount
/// as a saving credits us with Anthropic's cache. It made this figure read
/// 87% on a session whose measured saving was under 1%: $1,095 of "cache
/// savings" against $164 of actual spend.
///
/// What is left is what the proxy actually changed about the request -- the
/// tokens it removed before forwarding. Actual spend is already cache-priced
/// and the compression saving is priced on the basis it would have been billed
/// at, so the two add to the baseline exactly once.
///
/// Still missing, and it would raise this number honestly: whatever cache
/// stabilisation is worth against the client's own breakpoint placement. Not
/// folded in here because it is not measured on this path yet.
///
/// The model-offload counterfactual IS folded in (third parameter): when the
/// router serves a turn on a cheaper model than the client asked for, the
/// bill the original model never saw is a real reduction of what could have
/// been consumed — e.g. Opus asked, Spark served at $0. `offload_savings_usd`
/// is `max(0, would_have_cost − did_cost)` on identical token counts, so it
/// adds to the baseline exactly once, like compression.
fn cost_savings_percent(
    actual_input_cost_usd: f64,
    compression_savings_usd: f64,
    offload_savings_usd: f64,
) -> f64 {
    let actual = coerce_float(actual_input_cost_usd);
    let compression = coerce_float(compression_savings_usd);
    let offload = coerce_float(offload_savings_usd).max(0.0);
    let net_savings = compression + offload;
    let counterfactual = actual + net_savings;
    if counterfactual > 0.0 {
        round_n(net_savings / counterfactual * 100.0, 2)
    } else {
        0.0
    }
}

// ── persisted state ──

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Lifetime {
    requests: i64,
    tokens_saved: i64,
    compression_savings_usd: f64,
    total_input_tokens: i64,
    total_input_cost_usd: f64,
    /// Output-shaping savings. `#[serde(default)]` so a state file written
    /// before these fields existed still loads instead of being discarded.
    #[serde(default)]
    output_tokens_saved: i64,
    #[serde(default)]
    output_savings_usd: f64,
    /// Emitted output priced at the output rate (upstream `c766bdb2`): the
    /// output half of the bill, so a share-of-bill rate whose numerator holds
    /// output savings has a denominator that holds output spend.
    #[serde(default)]
    total_output_cost_usd: f64,
    /// Bill the requested model never saw because the router served the turn
    /// on a cheaper one (`would_have_cost − did_cost`, clamped ≥ 0).
    /// `#[serde(default)]` so older state files still load.
    #[serde(default)]
    offload_savings_usd: f64,
    /// Lifetime cumulative cache reads and their savings (see `d1258055`:
    /// history points in cache mode). `#[serde(default)]` so older files load.
    #[serde(default)]
    cache_read_tokens: i64,
    #[serde(default)]
    cache_savings_usd: f64,
}

impl Default for Lifetime {
    fn default() -> Self {
        Self {
            requests: 0,
            tokens_saved: 0,
            compression_savings_usd: 0.0,
            total_input_tokens: 0,
            total_input_cost_usd: 0.0,
            output_tokens_saved: 0,
            output_savings_usd: 0.0,
            total_output_cost_usd: 0.0,
            offload_savings_usd: 0.0,
            cache_read_tokens: 0,
            cache_savings_usd: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DisplaySession {
    requests: i64,
    tokens_saved: i64,
    compression_savings_usd: f64,
    /// What Anthropic's prompt cache was worth on this session, net of
    /// cache-write premiums.
    ///
    /// Reported, not claimed: the client would have got most of this on its
    /// own, so it is not part of `savings_percent`. Kept because it is the
    /// right denominator for asking whether the cache is working at all.
    #[serde(default)]
    cache_savings_usd: f64,
    /// Router offload savings booked on this session (see `Lifetime`).
    /// Part of `savings_percent`; cache savings stay reported-only.
    #[serde(default)]
    offload_savings_usd: f64,
    total_input_tokens: i64,
    total_input_cost_usd: f64,
    savings_percent: f64,
    started_at: Option<String>,
    last_activity_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectEntry {
    requests: i64,
    tokens_saved: i64,
    compression_savings_usd: f64,
    total_input_tokens: i64,
    total_input_cost_usd: f64,
    last_activity_at: Option<String>,
}

/// Failed upstream work is deliberately not part of [`Lifetime`]. A request
/// that never completed must not improve the successful savings rate, but the
/// attempts and tokens it exposed to the provider still need durable books.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FailedWork {
    requests: i64,
    upstream_attempts: i64,
    /// One forwarded request body per failed client turn.
    forwarded_tokens: i64,
    /// The same body multiplied by the number of upstream transmissions.
    forwarded_tokens_at_risk: i64,
    /// Actual usage only when the provider supplied a usage block.
    provider_reported_input_tokens: i64,
    provider_reported_output_tokens: i64,
    provider_usage_observed_requests: i64,
    by_status: BTreeMap<String, i64>,
    last_activity_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    timestamp: String,
    provider: String,
    model: String,
    total_tokens_saved: i64,
    compression_savings_usd: f64,
    /// Lifetime cumulative cache reads/savings at this point. `#[serde(default)]`
    /// so history written before cache-mode history gating existed still loads.
    #[serde(default)]
    cache_read_tokens: i64,
    #[serde(default)]
    cache_savings_usd: f64,
    total_input_tokens: i64,
    total_input_cost_usd: f64,
    #[serde(default)]
    output_tokens_saved: i64,
    #[serde(default)]
    output_savings_usd: f64,
    #[serde(default)]
    total_output_cost_usd: f64,
}

#[derive(Debug, Clone, Default)]
struct State {
    lifetime: Lifetime,
    failed_work: FailedWork,
    display_session: DisplaySession,
    history: Vec<HistoryEntry>,
    /// `history` already rendered as JSON, one entry per element.
    ///
    /// Rendering the whole history was the bulk of a save, and a request saves
    /// several times, so this is kept in step by `push_history` and
    /// `trim_history` instead of being rebuilt each time. It only mirrors
    /// pushes and removals, which is all `history` ever sees. Editing an
    /// entry in place would leave this stale, so don't.
    history_rendered: Vec<Value>,
    projects: BTreeMap<String, ProjectEntry>,
    /// Durable cache-behaviour counters. The tracker owns loading and saving
    /// these, per `persistent_metrics`'s own contract; without a home here
    /// they were a finished port with no call site, and nothing about cache
    /// busts survived a proxy restart.
    metrics: crate::persistent_metrics::PersistentMetricsState,
}

/// Rebuild the rendered mirror when it has fallen out of step with `history`.
///
/// Two cases reach this: a state just loaded from disk, where the mirror
/// starts empty, and a history replaced wholesale. Both are one-off, so the
/// rebuild cost lands once rather than on every save.
fn sync_history_rendered(st: &mut State) {
    if st.history_rendered.len() != st.history.len() {
        st.history_rendered = st.history.iter().map(history_entry_value).collect();
    }
}

/// Persist bounded proxy compression savings history.
pub struct SavingsTracker {
    path: PathBuf,
    max_history_points: usize,
    max_history_age_days: i64,
    max_response_history_points: usize,
    display_session_inactivity_minutes: i64,
    stateless: bool,
    state: Mutex<State>,
    /// When the state file was last written, and whether it has fallen behind
    /// the state in memory.
    ///
    /// Nine `record_*` methods each ended with a `save`, and a save serialises
    /// the whole file — 1.48 MB in production — then fsyncs it, all while
    /// holding `state`. Several of those fire per request, so the proxy could
    /// not clear more than about 45 saves a second in total and requests piled
    /// up behind the lock, blocking their tokio worker threads rather than
    /// yielding them. Writes are now coalesced: the state is still updated on
    /// every record, but it reaches disk at most once per
    /// [`MIN_SAVE_INTERVAL`], plus a final write when the tracker drops.
    last_write: Mutex<Option<Instant>>,
    dirty: AtomicBool,
    /// Set by [`Self::begin_handoff`] when this process stops accepting work.
    ///
    /// A restart starts the next proxy while this one drains its open turns,
    /// for as long as the graceful-shutdown timeout allows. Both load the file
    /// once and rewrite it whole, so whichever wrote last erased the other's
    /// records: the turns that finished during the drain went missing from the
    /// totals. Once set, every `record_*` call is appended to the handoff
    /// journal instead, and the next proxy applies it with
    /// [`Self::absorb_handoff`].
    handed_off: AtomicBool,
}

fn upstream_rate_limit_source() -> String {
    "upstream".to_string()
}

/// One `record_*` call made after [`SavingsTracker::begin_handoff`], with
/// owned copies of its arguments. Timestamps are fixed when the call is made,
/// so a replayed turn lands in the display session and history where it
/// happened, not where it was applied.
///
/// Externally tagged on purpose: an internally tagged enum buffers its fields,
/// and with serde_json's `arbitrary_precision` on in this workspace a buffered
/// number no longer reads back as `f64`, so every `request` line failed.
///
/// A field added later needs no `#[serde(default)]` when it is an `Option`:
/// serde reads a missing `Option` as `None`, so older journal lines still
/// load. Any other type needs one, like `source` on `RateLimited`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HandoffOp {
    CompressionSavings {
        model: String,
        tokens_saved: i64,
        provider: Option<String>,
        total_input_tokens: Option<i64>,
        total_input_cost_usd: Option<f64>,
        timestamp: DateTime<Utc>,
    },
    Request {
        model: String,
        input_tokens: i64,
        tokens_saved: i64,
        tool_schema_saved: i64,
        compression_savings_cost_usd: Option<f64>,
        tool_schema_savings_cost_usd: Option<f64>,
        provider: Option<String>,
        project: Option<String>,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
        uncached_input_tokens: i64,
        total_input_tokens: Option<i64>,
        total_input_cost_usd: Option<f64>,
        timestamp: DateTime<Utc>,
        output_tokens_saved: i64,
        output_tokens: i64,
        attempted_input_tokens: i64,
        cache_write_5m_tokens: i64,
        cache_write_1h_tokens: i64,
        cached: bool,
        stack: Option<String>,
        waste_signals: Option<Vec<(String, i64)>>,
        offload_savings_usd: f64,
    },
    RateLimited {
        provider: Option<String>,
        /// Absent in ops written before the source split, when only upstream
        /// 429s reached this path.
        #[serde(default = "upstream_rate_limit_source")]
        source: String,
    },
    FailedWork {
        provider: Option<String>,
        status_code: i64,
        upstream_attempts: i64,
        forwarded_tokens: i64,
        provider_input_tokens: Option<i64>,
        provider_output_tokens: Option<i64>,
        timestamp: DateTime<Utc>,
    },
    CacheBust {
        tokens_lost: i64,
    },
    CacheMiss {
        provider: Option<String>,
        reason: Option<String>,
    },
    ProxyOverhead {
        before_bytes: i64,
        after_bytes: i64,
    },
    UnbookedTurn {
        partial_input_tokens: i64,
        partial_output_tokens: i64,
    },
    Tools {
        definitions: Vec<(String, i64)>,
        calls: Vec<(String, i64)>,
    },
    WireFootprint {
        bytes_in: i64,
        bytes_out: i64,
        input_tokens: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
    },
}

/// Floor on how often the state file is rewritten. A crash can cost the
/// statistics recorded since the last write; nothing else reads this file
/// mid-run.
const MIN_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

/// Optional inputs to [`SavingsTracker::record_request`]. Neutral defaults
/// mirror the Python keyword arguments.
#[derive(Default)]
pub struct RequestRecord<'a> {
    pub model: &'a str,
    pub input_tokens: i64,
    pub tokens_saved: i64,
    /// Tool-schema tokens that never entered context (deferral/hook shrink),
    /// additive to `tokens_saved`. Carried separately so accumulators fold a
    /// headline without losing the split; never the compaction delta (already
    /// inside `tokens_saved`).
    pub tool_schema_saved: i64,
    /// Request-scoped pricing counterfactual for `tokens_saved`. `None` keeps
    /// the legacy fresh-input estimate for callers without cache placement.
    pub compression_savings_cost_usd: Option<f64>,
    /// Request-scoped pricing counterfactual for `tool_schema_saved`, priced
    /// read-first. `None` keeps the fresh-input estimate.
    pub tool_schema_savings_cost_usd: Option<f64>,
    pub provider: Option<&'a str>,
    pub project: Option<&'a str>,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub uncached_input_tokens: i64,
    pub total_input_tokens: Option<i64>,
    pub total_input_cost_usd: Option<f64>,
    pub timestamp: Option<DateTime<Utc>>,
    /// Output tokens the shaper avoided generating, from
    /// [`crate::output_savings::SavingsRecorder::estimate_request_savings`].
    /// Priced at the model's OUTPUT rate, and kept separate from
    /// `tokens_saved` (an input-side figure) so the two never mix.
    pub output_tokens_saved: i64,

    // ── Fields below feed the durable lifetime metrics only ──────────────
    // `RequestOutcome` has carried these all along; the tracker used to drop
    // them on the floor, which is why the persisted blob could say nothing
    // about cache behaviour.
    /// Output tokens the model actually generated.
    pub output_tokens: i64,
    /// Input tokens before compression — the denominator for "did compression
    /// do anything".
    pub attempted_input_tokens: i64,
    /// Cache writes billed at the 5-minute TTL.
    pub cache_write_5m_tokens: i64,
    /// Cache writes billed at the 1-hour TTL.
    pub cache_write_1h_tokens: i64,
    /// Whether this request read anything from the prefix cache.
    pub cached: bool,
    /// Calling stack label (`claude-code`, `codex`, …).
    pub stack: Option<&'a str>,
    /// Waste-signal token counts, keyed by signal name.
    pub waste_signals: Option<Vec<(String, i64)>>,
    /// Bill the requested model never saw: the router served this turn on a
    /// cheaper model. Priced as `would_have_cost − did_cost` on identical
    /// token counts (clamped ≥ 0 by the caller), so routing to a free model
    /// instead of a paying one counts as the saving it is.
    pub offload_savings_usd: f64,
}

/// Inputs to [`SavingsTracker::record_failed_work`]. Provider usage stays
/// optional so a forwarded-token estimate can never masquerade as billing
/// reported by the upstream.
#[derive(Default)]
pub struct FailedWorkRecord {
    pub provider: Option<String>,
    pub status_code: i64,
    pub upstream_attempts: i64,
    pub forwarded_tokens: i64,
    pub provider_input_tokens: Option<i64>,
    pub provider_output_tokens: Option<i64>,
    pub timestamp: Option<DateTime<Utc>>,
}

impl SavingsTracker {
    /// Construct a tracker with the standard defaults.
    pub fn new(path: Option<PathBuf>, stateless: bool) -> Self {
        Self::with_options(
            path,
            DEFAULT_MAX_HISTORY_POINTS,
            DEFAULT_MAX_HISTORY_AGE_DAYS,
            DEFAULT_MAX_RESPONSE_HISTORY_POINTS,
            DEFAULT_DISPLAY_SESSION_INACTIVITY_MINUTES,
            stateless,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_options(
        path: Option<PathBuf>,
        max_history_points: usize,
        max_history_age_days: i64,
        max_response_history_points: usize,
        display_session_inactivity_minutes: i64,
        stateless: bool,
    ) -> Self {
        let path = path.unwrap_or_else(|| crate::paths::savings_path(None));
        let tracker = Self {
            path,
            max_history_points,
            max_history_age_days,
            max_response_history_points: max_response_history_points.max(1),
            display_session_inactivity_minutes: display_session_inactivity_minutes.max(1),
            stateless,
            state: Mutex::new(State::default()),
            last_write: Mutex::new(None),
            dirty: AtomicBool::new(false),
            handed_off: AtomicBool::new(false),
        };
        let loaded = tracker.load_state();
        *tracker.state.lock().unwrap() = loaded;
        tracker
    }

    pub fn storage_path(&self) -> &Path {
        &self.path
    }

    fn is_display_session_expired(
        &self,
        last_activity: DateTime<Utc>,
        reference: DateTime<Utc>,
    ) -> bool {
        reference - last_activity > Duration::minutes(self.display_session_inactivity_minutes)
    }

    /// Persist a cumulative savings checkpoint (lifetime + history only).
    /// Returns `false` when `tokens_saved <= 0`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_compression_savings(
        &self,
        model: &str,
        tokens_saved: i64,
        provider: Option<&str>,
        total_input_tokens: Option<i64>,
        total_input_cost_usd: Option<f64>,
        timestamp: Option<DateTime<Utc>>,
    ) -> bool {
        let delta_tokens = coerce_int(tokens_saved);
        if delta_tokens <= 0 {
            return false;
        }
        let ts = timestamp.unwrap_or_else(utc_now);
        let delta_usd = estimate_compression_savings_usd(model, delta_tokens);

        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::CompressionSavings {
            model: model.to_string(),
            tokens_saved,
            provider: provider.map(str::to_string),
            total_input_tokens,
            total_input_cost_usd,
            timestamp: ts,
        }) {
            return true;
        }
        st.lifetime.tokens_saved += delta_tokens;
        st.lifetime.compression_savings_usd =
            round_n(st.lifetime.compression_savings_usd + delta_usd, 6);
        let cur_tokens = st.lifetime.total_input_tokens;
        st.lifetime.total_input_tokens =
            cur_tokens.max(coerce_int(total_input_tokens.unwrap_or(cur_tokens)));
        let cur_cost = st.lifetime.total_input_cost_usd;
        st.lifetime.total_input_cost_usd = round_n(
            cur_cost.max(coerce_float(total_input_cost_usd.unwrap_or(cur_cost))),
            6,
        );

        let entry = HistoryEntry {
            timestamp: to_utc_iso(ts),
            provider: normalize_provider(provider),
            model: normalize_model(Some(model)),
            total_tokens_saved: st.lifetime.tokens_saved,
            compression_savings_usd: st.lifetime.compression_savings_usd,
            cache_read_tokens: st.lifetime.cache_read_tokens,
            cache_savings_usd: st.lifetime.cache_savings_usd,
            total_input_tokens: st.lifetime.total_input_tokens,
            total_input_cost_usd: st.lifetime.total_input_cost_usd,
            output_tokens_saved: st.lifetime.output_tokens_saved,
            output_savings_usd: st.lifetime.output_savings_usd,
            total_output_cost_usd: st.lifetime.total_output_cost_usd,
        };
        Self::push_history(&mut st, entry);
        self.trim_history(&mut st, ts);
        self.save(&mut st);
        true
    }

    /// Persist a canonical display-session update for every request.
    pub fn record_request(&self, rec: &RequestRecord) -> bool {
        let ts = rec.timestamp.unwrap_or_else(utc_now);
        let delta_tokens_saved = coerce_int(rec.tokens_saved);
        // Headline leg: tool-schema tokens never entered context. Folded
        // into token totals and priced by the caller read-first against the
        // turn's cache mix, or at list without one; excluded from cache-mix
        // tier buckets downstream, which price only billed tiers.
        let delta_tool_schema_saved = coerce_int(rec.tool_schema_saved);
        let delta_headline_saved = delta_tokens_saved.saturating_add(delta_tool_schema_saved);
        let delta_input_tokens = coerce_int(rec.input_tokens);
        let delta_savings_usd = rec
            .compression_savings_cost_usd
            .map(|cost| cost.max(0.0))
            .unwrap_or_else(|| estimate_compression_savings_usd(rec.model, delta_tokens_saved))
            + rec
                .tool_schema_savings_cost_usd
                .map(|cost| cost.max(0.0))
                .unwrap_or_else(|| {
                    estimate_compression_savings_usd(rec.model, delta_tool_schema_saved)
                });
        // Output-shaping savings, priced at the OUTPUT rate and accumulated
        // separately — folding them into `tokens_saved` would mix an
        // output-side count into an input-side figure and misprice both.
        let delta_output_tokens_saved = coerce_int(rec.output_tokens_saved).max(0);
        let delta_output_savings_usd =
            estimate_output_savings_usd(rec.model, delta_output_tokens_saved);
        // Emitted output, disjoint from `output_tokens_saved`, priced from the
        // same table so spend and savings agree even for an unpriced model.
        let delta_output_cost_usd =
            estimate_output_savings_usd(rec.model, coerce_int(rec.output_tokens).max(0));
        let delta_input_cost_usd = estimate_input_cost_usd(
            rec.model,
            delta_input_tokens,
            rec.cache_read_tokens,
            rec.cache_write_tokens,
            rec.cache_write_5m_tokens,
            rec.cache_write_1h_tokens,
            rec.uncached_input_tokens,
        );
        let delta_cache_savings_usd = estimate_cache_savings_usd(
            rec.model,
            rec.cache_read_tokens,
            rec.cache_write_tokens,
            rec.cache_write_5m_tokens,
            rec.cache_write_1h_tokens,
            rec.uncached_input_tokens,
        );
        let delta_offload_savings_usd = coerce_float(rec.offload_savings_usd).max(0.0);

        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::Request {
            model: rec.model.to_string(),
            input_tokens: rec.input_tokens,
            tokens_saved: rec.tokens_saved,
            tool_schema_saved: rec.tool_schema_saved,
            compression_savings_cost_usd: rec.compression_savings_cost_usd,
            tool_schema_savings_cost_usd: rec.tool_schema_savings_cost_usd,
            provider: rec.provider.map(str::to_string),
            project: rec.project.map(str::to_string),
            cache_read_tokens: rec.cache_read_tokens,
            cache_write_tokens: rec.cache_write_tokens,
            uncached_input_tokens: rec.uncached_input_tokens,
            total_input_tokens: rec.total_input_tokens,
            total_input_cost_usd: rec.total_input_cost_usd,
            timestamp: ts,
            output_tokens_saved: rec.output_tokens_saved,
            output_tokens: rec.output_tokens,
            attempted_input_tokens: rec.attempted_input_tokens,
            cache_write_5m_tokens: rec.cache_write_5m_tokens,
            cache_write_1h_tokens: rec.cache_write_1h_tokens,
            cached: rec.cached,
            stack: rec.stack.map(str::to_string),
            waste_signals: rec.waste_signals.clone(),
            offload_savings_usd: rec.offload_savings_usd,
        }) {
            return true;
        }
        let prev_tokens = st.lifetime.total_input_tokens;
        let prev_cost = st.lifetime.total_input_cost_usd;

        let next_tokens = (prev_tokens + delta_input_tokens).max(coerce_int(
            rec.total_input_tokens
                .unwrap_or(prev_tokens + delta_input_tokens),
        ));
        let next_cost = round_n(
            (prev_cost + delta_input_cost_usd).max(coerce_float(
                rec.total_input_cost_usd
                    .unwrap_or(prev_cost + delta_input_cost_usd),
            )),
            6,
        );
        let session_tokens_delta = (next_tokens - prev_tokens).max(0);
        let session_cost_delta = round_n((next_cost - prev_cost).max(0.0), 6);

        st.lifetime.requests += 1;
        st.lifetime.tokens_saved += delta_headline_saved;
        st.lifetime.compression_savings_usd =
            round_n(st.lifetime.compression_savings_usd + delta_savings_usd, 6);
        st.lifetime.total_input_tokens = next_tokens;
        st.lifetime.total_input_cost_usd = next_cost;
        st.lifetime.output_tokens_saved += delta_output_tokens_saved;
        st.lifetime.output_savings_usd =
            round_n(st.lifetime.output_savings_usd + delta_output_savings_usd, 6);
        st.lifetime.total_output_cost_usd =
            round_n(st.lifetime.total_output_cost_usd + delta_output_cost_usd, 6);
        st.lifetime.offload_savings_usd = round_n(
            st.lifetime.offload_savings_usd + delta_offload_savings_usd,
            6,
        );
        // Lifetime cumulative cache reads/savings feed the history points
        // below (Python `d1258055`). Cache-mode turns compress nothing, so
        // without these the history — and `/stats-history` charts — go blind
        // on exactly the turns cache mode exists for.
        st.lifetime.cache_read_tokens += coerce_int(rec.cache_read_tokens).max(0);
        st.lifetime.cache_savings_usd =
            round_n(st.lifetime.cache_savings_usd + delta_cache_savings_usd, 6);

        // Display-session rollover on inactivity.
        let expired = match st
            .display_session
            .last_activity_at
            .as_deref()
            .and_then(parse_timestamp)
        {
            None => true,
            Some(last) => self.is_display_session_expired(last, ts),
        };
        if expired {
            st.display_session = DisplaySession {
                started_at: Some(to_utc_iso(ts)),
                ..Default::default()
            };
        }
        let s = &mut st.display_session;
        s.requests += 1;
        s.tokens_saved += delta_headline_saved;
        s.compression_savings_usd = round_n(s.compression_savings_usd + delta_savings_usd, 6);
        s.cache_savings_usd = round_n(s.cache_savings_usd + delta_cache_savings_usd, 6);
        s.offload_savings_usd = round_n(s.offload_savings_usd + delta_offload_savings_usd, 6);
        s.total_input_tokens += session_tokens_delta;
        s.total_input_cost_usd = round_n(s.total_input_cost_usd + session_cost_delta, 6);
        s.savings_percent = cost_savings_percent(
            s.total_input_cost_usd,
            s.compression_savings_usd,
            s.offload_savings_usd,
        );
        s.last_activity_at = Some(to_utc_iso(ts));
        if s.started_at.is_none() {
            s.started_at = s.last_activity_at.clone();
        }

        self.record_project(
            &mut st,
            rec.project,
            ts,
            delta_headline_saved,
            delta_savings_usd,
            delta_input_tokens,
            delta_input_cost_usd,
        );

        // In cache mode headroom's own compression (`tokens_saved`) is
        // near-always 0 by design — the frozen prefix is byte-replayed, not
        // compressed. Gating on `tokens_saved` alone silently dropped every
        // history point on those turns even though real cache-read and
        // output-shaping savings occurred. Append whenever any savings
        // mechanism produced a saving (mirrors Python `d1258055`).
        if delta_headline_saved > 0
            || coerce_int(rec.cache_read_tokens).max(0) > 0
            || delta_output_tokens_saved > 0
        {
            let entry = HistoryEntry {
                timestamp: to_utc_iso(ts),
                provider: normalize_provider(rec.provider),
                model: normalize_model(Some(rec.model)),
                total_tokens_saved: st.lifetime.tokens_saved,
                compression_savings_usd: st.lifetime.compression_savings_usd,
                cache_read_tokens: st.lifetime.cache_read_tokens,
                cache_savings_usd: st.lifetime.cache_savings_usd,
                total_input_tokens: st.lifetime.total_input_tokens,
                total_input_cost_usd: st.lifetime.total_input_cost_usd,
                output_tokens_saved: st.lifetime.output_tokens_saved,
                output_savings_usd: st.lifetime.output_savings_usd,
                total_output_cost_usd: st.lifetime.total_output_cost_usd,
            };
            Self::push_history(&mut st, entry);
            self.trim_history(&mut st, ts);
        }

        // Durable lifetime counters. These are what make the savings question
        // answerable across restarts: compression savings on their own can
        // look good while cache busts quietly cost more than they save, so
        // both sides go into the same persisted blob.
        st.metrics
            .record_request(&crate::persistent_metrics::RecordRequest {
                provider: rec.provider.map(str::to_string),
                stack: rec.stack.map(str::to_string),
                model: Some(rec.model.to_string()),
                input_tokens: delta_input_tokens,
                output_tokens: coerce_int(rec.output_tokens),
                attempted_input_tokens: coerce_int(rec.attempted_input_tokens),
                tokens_saved: delta_tokens_saved,
                tool_schema_saved: coerce_int(rec.tool_schema_saved).max(0),
                cached: rec.cached,
                record_stack: true,
                cache_read_tokens: coerce_int(rec.cache_read_tokens),
                cache_write_tokens: coerce_int(rec.cache_write_tokens),
                cache_write_5m_tokens: coerce_int(rec.cache_write_5m_tokens),
                cache_write_1h_tokens: coerce_int(rec.cache_write_1h_tokens),
                uncached_input_tokens: coerce_int(rec.uncached_input_tokens),
                input_usd: delta_input_cost_usd,
                compression_savings_usd: delta_savings_usd,
                cache_savings_usd: delta_cache_savings_usd,
                waste_signals: rec.waste_signals.as_ref().map(|pairs| {
                    pairs
                        .iter()
                        .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                        .collect()
                }),
            });

        self.save(&mut st);
        true
    }

    /// Persist one failed client turn in a bucket excluded from every
    /// successful savings/session/project denominator.
    /// Book a rate-limited request in the persistent ledger, broken down
    /// by provider (mirrors the Prometheus
    /// `headroom_requests_rate_limited_total{source}` split).
    pub fn record_rate_limited(&self, provider: Option<&str>, source: &str) {
        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::RateLimited {
            provider: provider.map(str::to_string),
            source: source.to_string(),
        }) {
            return;
        }
        st.metrics.record_rate_limited(provider, None, source);
    }

    pub fn record_failed_work(&self, rec: &FailedWorkRecord) {
        let attempts = rec.upstream_attempts.max(1);
        let forwarded = coerce_int(rec.forwarded_tokens);
        let at_risk = forwarded.saturating_mul(attempts);
        let ts = rec.timestamp.unwrap_or_else(utc_now);

        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::FailedWork {
            provider: rec.provider.clone(),
            status_code: rec.status_code,
            upstream_attempts: rec.upstream_attempts,
            forwarded_tokens: rec.forwarded_tokens,
            provider_input_tokens: rec.provider_input_tokens,
            provider_output_tokens: rec.provider_output_tokens,
            timestamp: ts,
        }) {
            return;
        }
        st.metrics.record_failed(rec.provider.as_deref(), None);
        let failed = &mut st.failed_work;
        failed.requests = failed.requests.saturating_add(1);
        failed.upstream_attempts = failed.upstream_attempts.saturating_add(attempts);
        failed.forwarded_tokens = failed.forwarded_tokens.saturating_add(forwarded);
        failed.forwarded_tokens_at_risk = failed.forwarded_tokens_at_risk.saturating_add(at_risk);
        if rec.provider_input_tokens.is_some() || rec.provider_output_tokens.is_some() {
            failed.provider_usage_observed_requests =
                failed.provider_usage_observed_requests.saturating_add(1);
            failed.provider_reported_input_tokens = failed
                .provider_reported_input_tokens
                .saturating_add(coerce_int(rec.provider_input_tokens.unwrap_or(0)));
            failed.provider_reported_output_tokens = failed
                .provider_reported_output_tokens
                .saturating_add(coerce_int(rec.provider_output_tokens.unwrap_or(0)));
        }
        let status = rec.status_code.max(0).to_string();
        let count = failed.by_status.entry(status).or_default();
        *count = count.saturating_add(1);
        failed.last_activity_at = Some(to_utc_iso(ts));
        self.save(&mut st);
    }

    /// Record a prefix-cache bust: `tokens_lost` had to be re-created because
    /// something inside the cached prefix changed.
    ///
    /// Separate from [`Self::record_request`] because the bust is detected on
    /// the response side, one turn after the request that caused it.
    pub fn record_cache_bust(&self, tokens_lost: i64) {
        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::CacheBust { tokens_lost }) {
            return;
        }
        st.metrics.record_cache_bust(tokens_lost);
        self.save(&mut st);
    }

    /// Record why the prefix cache missed (`ttl_expiry`, `prefix_change`, or
    /// `unknown`). `prefix_change` is the one that means we moved bytes.
    pub fn record_cache_miss(&self, provider: Option<&str>, reason: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::CacheMiss {
            provider: provider.map(str::to_string),
            reason: reason.map(str::to_string),
        }) {
            return;
        }
        st.metrics.record_cache_miss(provider, reason);
        self.save(&mut st);
    }

    /// Record what the proxy added to a request, in bytes of `tools` +
    /// `system`. See
    /// [`crate::persistent_metrics::PersistentMetricsState::record_proxy_overhead`].
    pub fn record_proxy_overhead(&self, before_bytes: i64, after_bytes: i64) {
        if before_bytes == after_bytes {
            return; // nothing moved; skip the lock and the write
        }
        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::ProxyOverhead {
            before_bytes,
            after_bytes,
        }) {
            return;
        }
        st.metrics.record_proxy_overhead(before_bytes, after_bytes);
        self.save(&mut st);
    }

    /// Record one turn dropped from the books because its stream ended without
    /// the terminal event carrying the usage totals. See
    /// [`crate::persistent_metrics::PersistentMetricsState::record_unbooked_turn`].
    pub fn record_unbooked_turn(&self, partial_input_tokens: i64, partial_output_tokens: i64) {
        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::UnbookedTurn {
            partial_input_tokens,
            partial_output_tokens,
        }) {
            return;
        }
        st.metrics
            .record_unbooked_turn(partial_input_tokens, partial_output_tokens);
        self.save(&mut st);
    }

    /// Record the tool definitions a request carried and the calls the model
    /// made, so tools nobody uses can be named and priced.
    pub fn record_tools(&self, definitions: &[(String, i64)], calls: &[(String, i64)]) {
        if definitions.is_empty() && calls.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::Tools {
            definitions: definitions.to_vec(),
            calls: calls.to_vec(),
        }) {
            return;
        }
        st.metrics.record_tools(definitions, calls);
        self.save(&mut st);
    }

    /// What the proxy added to requests, against what compaction gave back.
    pub fn proxy_overhead_report(&self) -> Value {
        let st = self.state.lock().unwrap();
        st.metrics.proxy_overhead_report()
    }

    /// Tool definitions the model never called, biggest first.
    pub fn tool_inventory_report(&self) -> Value {
        let st = self.state.lock().unwrap();
        st.metrics.tool_inventory_report()
    }

    /// The durable lifetime metrics, in API-safe form.
    pub fn metrics_snapshot(&self, persistence: &Value) -> Value {
        let st = self.state.lock().unwrap();
        st.metrics.snapshot(persistence)
    }

    /// Whether the proxy is paying for itself — compression savings net of the
    /// cache the proxy busted. See
    /// [`crate::persistent_metrics::PersistentMetricsState::savings_verdict`].
    pub fn savings_verdict(&self) -> Value {
        let st = self.state.lock().unwrap();
        st.metrics.savings_verdict()
    }

    /// Record one request's wire bytes against the usage the provider reported
    /// for it. Skipped when the provider sent no usage, so the byte and token
    /// halves always describe the same requests.
    pub fn record_wire_footprint(
        &self,
        bytes_in: i64,
        bytes_out: i64,
        input_tokens: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
    ) {
        let mut st = self.state.lock().unwrap();
        if self.divert(|| HandoffOp::WireFootprint {
            bytes_in,
            bytes_out,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
        }) {
            return;
        }
        st.metrics.record_wire_footprint(
            bytes_in,
            bytes_out,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
        );
        self.save(&mut st);
    }

    /// Wire bytes reconciled against provider-reported usage. See
    /// [`crate::persistent_metrics::PersistentMetricsState::wire_verdict`].
    pub fn wire_verdict(&self) -> Value {
        let st = self.state.lock().unwrap();
        st.metrics.wire_verdict()
    }

    #[allow(clippy::too_many_arguments)]
    fn record_project(
        &self,
        st: &mut State,
        project: Option<&str>,
        ts: DateTime<Utc>,
        tokens_saved_delta: i64,
        savings_usd_delta: f64,
        input_tokens_delta: i64,
        input_cost_usd_delta: f64,
    ) {
        let Some(name) = sanitize_project_name(project) else {
            return;
        };
        let entry = st.projects.entry(name.clone()).or_default();
        entry.requests += 1;
        entry.tokens_saved += tokens_saved_delta.max(0);
        entry.compression_savings_usd = round_n(
            entry.compression_savings_usd + savings_usd_delta.max(0.0),
            6,
        );
        entry.total_input_tokens += input_tokens_delta.max(0);
        entry.total_input_cost_usd = round_n(
            entry.total_input_cost_usd + input_cost_usd_delta.max(0.0),
            6,
        );
        entry.last_activity_at = Some(to_utc_iso(ts));

        if st.projects.len() > DEFAULT_MAX_PROJECTS {
            // Evict the smallest/oldest bucket other than the one just touched.
            if let Some(evict) = st
                .projects
                .iter()
                .filter(|(k, _)| *k != &name)
                .min_by(|(_, a), (_, b)| {
                    (
                        a.tokens_saved,
                        a.last_activity_at.clone().unwrap_or_default(),
                    )
                        .cmp(&(
                            b.tokens_saved,
                            b.last_activity_at.clone().unwrap_or_default(),
                        ))
                })
                .map(|(k, _)| k.clone())
            {
                st.projects.remove(&evict);
            }
        }
    }

    fn projects_snapshot(&self, st: &State) -> Value {
        let mut ranked: Vec<(&String, &ProjectEntry)> = st.projects.iter().collect();
        // Sort by tokens_saved desc (stable — BTreeMap iteration is key-sorted).
        ranked.sort_by_key(|(_, entry)| std::cmp::Reverse(entry.tokens_saved));
        let mut out = serde_json::Map::new();
        for (name, entry) in ranked {
            let total_before = entry.tokens_saved + entry.total_input_tokens;
            let savings_percent = if total_before > 0 {
                round_n(entry.tokens_saved as f64 / total_before as f64 * 100.0, 2)
            } else {
                0.0
            };
            out.insert(
                name.clone(),
                json!({
                    "requests": entry.requests,
                    "tokens_saved": entry.tokens_saved,
                    "compression_savings_usd": entry.compression_savings_usd,
                    "total_input_tokens": entry.total_input_tokens,
                    "total_input_cost_usd": entry.total_input_cost_usd,
                    "last_activity_at": entry.last_activity_at,
                    "savings_percent": savings_percent,
                }),
            );
        }
        Value::Object(out)
    }

    fn display_session_snapshot(&self, st: &State, reference: Option<DateTime<Utc>>) -> Value {
        let reference = reference.unwrap_or_else(utc_now);
        let s = &st.display_session;
        let expired = match s.last_activity_at.as_deref().and_then(parse_timestamp) {
            None => true,
            Some(last) => self.is_display_session_expired(last, reference),
        };
        if expired {
            return empty_display_session_value();
        }
        let savings_percent = cost_savings_percent(
            s.total_input_cost_usd,
            s.compression_savings_usd,
            s.offload_savings_usd,
        );
        json!({
            "requests": s.requests,
            "tokens_saved": s.tokens_saved,
            "compression_savings_usd": round_n(coerce_float(s.compression_savings_usd), 6),
            "cache_savings_usd": round_n(coerce_signed_float(s.cache_savings_usd), 6),
            "offload_savings_usd": round_n(coerce_float(s.offload_savings_usd), 6),
            "total_input_tokens": s.total_input_tokens,
            "total_input_cost_usd": round_n(coerce_float(s.total_input_cost_usd), 6),
            "savings_percent": savings_percent,
            "started_at": s.started_at,
            "last_activity_at": s.last_activity_at,
        })
    }

    /// Full state snapshot as a JSON value.
    pub fn snapshot(&self) -> Value {
        let st = self.state.lock().unwrap();
        self.snapshot_locked(&st)
    }

    fn snapshot_locked(&self, st: &State) -> Value {
        let history: Vec<Value> = st.history.iter().map(history_entry_value).collect();
        json!({
            "schema_version": SCHEMA_VERSION,
            "storage_path": self.path.to_string_lossy(),
            "lifetime": lifetime_value(&st.lifetime),
            "failed_work": failed_work_value(&st.failed_work),
            "display_session": self.display_session_snapshot(st, None),
            "display_session_policy": {
                "rollover_inactivity_minutes": self.display_session_inactivity_minutes,
            },
            "history": history,
            "retention": {
                "max_history_points": self.max_history_points,
                "max_history_age_days": self.max_history_age_days,
                "max_response_history_points": self.max_response_history_points,
            },
            "projects": self.projects_snapshot(st),
        })
    }

    /// Compact preview for `/stats`.
    pub fn stats_preview(&self, recent_points: usize) -> Value {
        let snap = self.snapshot();
        let history = snap["history"].as_array().cloned().unwrap_or_default();
        let recent: Vec<Value> = history
            .iter()
            .skip(history.len().saturating_sub(recent_points))
            .cloned()
            .collect();
        json!({
            "schema_version": snap["schema_version"],
            "storage_path": snap["storage_path"],
            "lifetime": snap["lifetime"],
            "failed_work": snap["failed_work"],
            "display_session": snap["display_session"],
            "display_session_policy": snap["display_session_policy"],
            "history_points": history.len(),
            "recent_history": recent,
            "retention": snap["retention"],
            "projects": snap["projects"],
            "projects_limit": DEFAULT_MAX_PROJECTS,
        })
    }

    /// Frontend-friendly historical data for `/stats-history`. `mode` is
    /// "compact" (default), "full", or "none".
    pub fn history_response(&self, mode: &str) -> Value {
        let st = self.state.lock().unwrap();
        let snap = self.snapshot_locked(&st);
        let raw: Vec<HistoryEntry> = st.history.clone();
        drop(st);

        let series = json!({
            "hourly": self.build_rollup(&raw, "hour"),
            "daily": self.build_rollup(&raw, "day"),
            "weekly": self.build_rollup(&raw, "week"),
            "monthly": self.build_rollup(&raw, "month"),
        });
        let history = self.history_for_response(&raw, mode);
        let stored = raw.len();
        let returned = history.len();
        json!({
            "schema_version": snap["schema_version"],
            "generated_at": to_utc_iso(utc_now()),
            "storage_path": snap["storage_path"],
            "lifetime": snap["lifetime"],
            "failed_work": snap["failed_work"],
            "display_session": snap["display_session"],
            "display_session_policy": snap["display_session_policy"],
            "history": history,
            "series": series,
            "exports": {
                "default_format": "json",
                "available_formats": ["json", "csv"],
                "available_series": ["history", "hourly", "daily", "weekly", "monthly"],
            },
            "retention": snap["retention"],
            "projects": snap["projects"],
            "history_summary": {
                "mode": mode,
                "stored_points": stored,
                "returned_points": returned,
                "compacted": returned < stored,
            },
        })
    }

    /// Export rows for history or a rollup series.
    pub fn export_rows(&self, series: &str) -> Vec<Value> {
        let response = self.history_response("compact");
        if series == "history" {
            return response["history"].as_array().cloned().unwrap_or_default();
        }
        response["series"][series]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Export history or a rollup series as CSV.
    pub fn export_csv(&self, series: &str) -> String {
        let rows = self.export_rows(series);
        let fieldnames: &[&str] = if series == "history" {
            &[
                "timestamp",
                "total_tokens_saved",
                "compression_savings_usd",
                "total_input_tokens",
                "total_input_cost_usd",
            ]
        } else {
            &[
                "timestamp",
                "tokens_saved",
                "compression_savings_usd_delta",
                "total_tokens_saved",
                "compression_savings_usd",
                "total_input_tokens_delta",
                "total_input_tokens",
                "total_input_cost_usd_delta",
                "total_input_cost_usd",
                "output_tokens_saved_delta",
                "output_savings_usd_delta",
            ]
        };
        let mut buf = String::new();
        buf.push_str(&fieldnames.join(","));
        buf.push_str("\r\n");
        for row in &rows {
            let cells: Vec<String> = fieldnames
                .iter()
                .map(|name| csv_cell(row.get(*name)))
                .collect();
            buf.push_str(&cells.join(","));
            buf.push_str("\r\n");
        }
        buf
    }

    // ── history maintenance ──

    /// Append an entry and render it into the mirror in one step, so the two
    /// vectors cannot fall out of step.
    fn push_history(st: &mut State, entry: HistoryEntry) {
        sync_history_rendered(st);
        st.history_rendered.push(history_entry_value(&entry));
        st.history.push(entry);
    }

    fn trim_history(&self, st: &mut State, reference: DateTime<Utc>) {
        if st.history.is_empty() {
            return;
        }
        sync_history_rendered(st);
        if self.max_history_age_days > 0 {
            let cutoff = reference - Duration::days(self.max_history_age_days);
            // One timestamp pass, then drop the same slots from both vectors.
            // This used to clone every surviving entry into a fresh vector on
            // every push, which on a full history was thousands of struct
            // clones per request.
            let keep: Vec<bool> = st
                .history
                .iter()
                .map(|item| parse_timestamp(&item.timestamp).unwrap_or_else(utc_now) >= cutoff)
                .collect();
            if keep.iter().any(|k| *k) {
                let mut i = 0;
                st.history.retain(|_| {
                    let k = keep[i];
                    i += 1;
                    k
                });
                let mut i = 0;
                st.history_rendered.retain(|_| {
                    let k = keep[i];
                    i += 1;
                    k
                });
            } else {
                // Everything aged out. Keep the newest point rather than
                // leaving nothing behind.
                let last = st.history.len() - 1;
                st.history.drain(..last);
                st.history_rendered.drain(..last);
            }
        }
        if self.max_history_points > 0 && st.history.len() > self.max_history_points {
            let start = st.history.len() - self.max_history_points;
            st.history.drain(..start);
            st.history_rendered.drain(..start);
        }
    }

    fn history_for_response(&self, history: &[HistoryEntry], mode: &str) -> Vec<Value> {
        match mode {
            "none" => vec![],
            "full" => history.iter().map(history_entry_value).collect(),
            _ => self.compact_history(history),
        }
    }

    fn compact_history(&self, history: &[HistoryEntry]) -> Vec<Value> {
        let cap = self.max_response_history_points;
        if history.len() <= cap {
            return history.iter().map(history_entry_value).collect();
        }
        let recent_points = ((cap / 3).max(50)).min(cap - 1);
        let split = history.len() - recent_points;
        let recent = &history[split..];
        let older = &history[..split];
        let older_slots = cap - recent.len();
        if older_slots == 0 || older.is_empty() {
            let start = recent.len().saturating_sub(cap);
            return recent[start..].iter().map(history_entry_value).collect();
        }
        let sampled_older: Vec<&HistoryEntry> = if older_slots == 1 {
            vec![&older[0]]
        } else {
            (0..older_slots)
                .map(|index| &older[((older.len() - 1) * index) / (older_slots - 1)])
                .collect()
        };

        let mut compacted: Vec<Value> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for point in sampled_older.into_iter().chain(recent.iter()) {
            if seen.insert(point.timestamp.clone()) {
                compacted.push(history_entry_value(point));
            }
        }
        compacted
    }

    fn build_rollup(&self, history: &[HistoryEntry], bucket: &str) -> Vec<Value> {
        if history.is_empty() {
            return vec![];
        }
        // Insertion-ordered aggregation keyed by bucket start.
        let mut order: Vec<String> = Vec::new();
        let mut agg: std::collections::HashMap<String, RollupEntry> =
            std::collections::HashMap::new();

        let mut prev_tokens = 0i64;
        let mut prev_usd = 0.0f64;
        let mut prev_input_tokens = 0i64;
        let mut prev_input_cost = 0.0f64;
        let mut prev_output_tokens = 0i64;
        let mut prev_output_usd = 0.0f64;
        let mut prev_output_cost = 0.0f64;

        for point in history {
            let Some(ts) = parse_timestamp(&point.timestamp) else {
                continue;
            };
            let bucket_start = bucket_start(ts, bucket);
            let key = to_utc_iso(bucket_start);
            let total_tokens = coerce_int(point.total_tokens_saved);
            let total_usd = coerce_float(point.compression_savings_usd);
            let total_input_tokens = coerce_int(point.total_input_tokens);
            let total_input_cost = coerce_float(point.total_input_cost_usd);
            let total_output_tokens = coerce_int(point.output_tokens_saved);
            let total_output_usd = coerce_float(point.output_savings_usd);
            let total_output_cost = coerce_float(point.total_output_cost_usd);
            let delta_tokens = (total_tokens - prev_tokens).max(0);
            let delta_usd = (total_usd - prev_usd).max(0.0);
            let delta_input_tokens = (total_input_tokens - prev_input_tokens).max(0);
            let delta_input_cost = (total_input_cost - prev_input_cost).max(0.0);
            let delta_output_tokens = (total_output_tokens - prev_output_tokens).max(0);
            let delta_output_usd = (total_output_usd - prev_output_usd).max(0.0);
            let delta_output_cost = (total_output_cost - prev_output_cost).max(0.0);

            prev_tokens = total_tokens;
            prev_usd = total_usd;
            prev_input_tokens = total_input_tokens;
            prev_input_cost = total_input_cost;
            prev_output_tokens = total_output_tokens;
            prev_output_usd = total_output_usd;
            prev_output_cost = total_output_cost;

            let entry = agg.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                RollupEntry::new(
                    &key,
                    total_tokens,
                    total_usd,
                    total_input_tokens,
                    total_input_cost,
                )
            });
            entry.tokens_saved += delta_tokens;
            entry.compression_savings_usd_delta =
                round_n(entry.compression_savings_usd_delta + delta_usd, 6);
            entry.total_input_tokens_delta += delta_input_tokens;
            entry.total_input_cost_usd_delta =
                round_n(entry.total_input_cost_usd_delta + delta_input_cost, 6);
            entry.total_tokens_saved = total_tokens;
            entry.compression_savings_usd = round_n(total_usd, 6);
            entry.total_input_tokens = total_input_tokens;
            entry.total_input_cost_usd = round_n(total_input_cost, 6);
            entry.output_tokens_saved_delta += delta_output_tokens;
            entry.output_savings_usd_delta =
                round_n(entry.output_savings_usd_delta + delta_output_usd, 6);
            entry.total_output_cost_usd_delta =
                round_n(entry.total_output_cost_usd_delta + delta_output_cost, 6);
            entry.total_output_cost_usd = round_n(total_output_cost, 6);

            if delta_tokens != 0
                || delta_usd != 0.0
                || delta_input_tokens != 0
                || delta_input_cost != 0.0
            {
                let prov = normalize_provider(Some(&point.provider));
                let p = entry.by_provider.entry(prov).or_default();
                p.tokens_saved += delta_tokens;
                p.compression_savings_usd_delta =
                    round_n(p.compression_savings_usd_delta + delta_usd, 6);
                p.total_input_tokens_delta += delta_input_tokens;
                p.total_input_cost_usd_delta =
                    round_n(p.total_input_cost_usd_delta + delta_input_cost, 6);

                let modl = normalize_model(Some(&point.model));
                let m = entry.by_model.entry(modl).or_default();
                m.tokens_saved += delta_tokens;
                m.compression_savings_usd_delta =
                    round_n(m.compression_savings_usd_delta + delta_usd, 6);
                m.total_input_tokens_delta += delta_input_tokens;
                m.total_input_cost_usd_delta =
                    round_n(m.total_input_cost_usd_delta + delta_input_cost, 6);
            }
        }

        order.iter().map(|k| agg[k].to_value()).collect()
    }

    // ── persistence ──

    fn load_state(&self) -> State {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return State::default();
        };
        let Ok(raw) = serde_json::from_str::<Value>(&text) else {
            return State::default();
        };
        self.sanitize_state(&raw)
    }

    fn sanitize_state(&self, raw: &Value) -> State {
        if !raw.is_object() {
            return State::default();
        }
        let mut history: Vec<HistoryEntry> = raw
            .get("history")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(normalize_history_entry).collect())
            .unwrap_or_default();
        history.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        let lr = raw.get("lifetime");
        let mut lifetime = Lifetime {
            requests: lr
                .and_then(|l| l.get("requests"))
                .and_then(Value::as_i64)
                .map(coerce_int)
                .unwrap_or(0),
            tokens_saved: lr
                .and_then(|l| l.get("tokens_saved"))
                .and_then(Value::as_i64)
                .map(coerce_int)
                .unwrap_or(0),
            compression_savings_usd: lr
                .and_then(|l| l.get("compression_savings_usd"))
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
            total_input_tokens: lr
                .and_then(|l| l.get("total_input_tokens"))
                .and_then(Value::as_i64)
                .map(coerce_int)
                .unwrap_or(0),
            total_input_cost_usd: lr
                .and_then(|l| l.get("total_input_cost_usd"))
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
            // Absent from state files written before output shaping existed;
            // default to zero rather than rejecting the file.
            output_tokens_saved: lr
                .and_then(|l| l.get("output_tokens_saved"))
                .and_then(Value::as_i64)
                .map(coerce_int)
                .unwrap_or(0),
            output_savings_usd: lr
                .and_then(|l| l.get("output_savings_usd"))
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
            total_output_cost_usd: lr
                .and_then(|l| l.get("total_output_cost_usd"))
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
            // Absent from state files written before router-offload
            // accounting; default to zero rather than rejecting the file.
            offload_savings_usd: lr
                .and_then(|l| l.get("offload_savings_usd"))
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
            // Absent from state files written before cache-mode history
            // gating; default to zero rather than rejecting the file.
            cache_read_tokens: lr
                .and_then(|l| l.get("cache_read_tokens"))
                .and_then(Value::as_i64)
                .map(coerce_int)
                .unwrap_or(0),
            cache_savings_usd: lr
                .and_then(|l| l.get("cache_savings_usd"))
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
        };
        if let Some(last) = history.last() {
            lifetime.tokens_saved = lifetime.tokens_saved.max(last.total_tokens_saved);
            lifetime.compression_savings_usd = lifetime
                .compression_savings_usd
                .max(coerce_float(last.compression_savings_usd));
            lifetime.total_input_tokens = lifetime
                .total_input_tokens
                .max(coerce_int(last.total_input_tokens));
            lifetime.total_input_cost_usd = lifetime
                .total_input_cost_usd
                .max(coerce_float(last.total_input_cost_usd));
            // The output side checkpoints the same way, so it recovers the
            // same way; otherwise a restart zeroed it while history kept the
            // old totals, and the rollup's clamped delta swallowed the gap.
            lifetime.output_tokens_saved =
                lifetime.output_tokens_saved.max(last.output_tokens_saved);
            lifetime.output_savings_usd = lifetime
                .output_savings_usd
                .max(coerce_float(last.output_savings_usd));
            lifetime.total_output_cost_usd = lifetime
                .total_output_cost_usd
                .max(coerce_float(last.total_output_cost_usd));
        }
        lifetime.compression_savings_usd = round_n(lifetime.compression_savings_usd, 6);
        lifetime.total_input_cost_usd = round_n(lifetime.total_input_cost_usd, 6);
        lifetime.output_savings_usd = round_n(lifetime.output_savings_usd, 6);
        lifetime.total_output_cost_usd = round_n(lifetime.total_output_cost_usd, 6);

        let mut st = State {
            lifetime,
            failed_work: normalize_failed_work(raw.get("failed_work")),
            display_session: normalize_display_session(raw.get("display_session")),
            history,
            // Rendered on first use; `sync_history_rendered` fills it.
            history_rendered: Vec::new(),
            projects: normalize_projects(raw.get("projects")),
            // Absent on files written before the metrics landed; `new`
            // treats a missing blob as a fresh zeroed state, so an older
            // savings file upgrades in place rather than being rejected.
            metrics: {
                let mut m = crate::persistent_metrics::PersistentMetricsState::new(
                    raw.get("lifetime_metrics"),
                );
                m.load_footprint(raw.get("lifetime_footprint"));
                m
            },
        };
        if let Some(last) = st.history.last() {
            let reference = parse_timestamp(&last.timestamp).unwrap_or_else(utc_now);
            self.trim_history(&mut st, reference);
        }
        st
    }

    /// Mark the state changed, and write it if the interval has elapsed.
    fn save(&self, st: &mut State) {
        if self.stateless {
            return;
        }
        self.dirty.store(true, Ordering::Relaxed);
        let due = {
            let mut last = match self.last_write.lock() {
                Ok(last) => last,
                Err(poisoned) => poisoned.into_inner(),
            };
            match *last {
                Some(at) if at.elapsed() < MIN_SAVE_INTERVAL => false,
                _ => {
                    *last = Some(Instant::now());
                    true
                }
            }
        };
        if due {
            self.write_state(st);
        }
    }

    /// Write the state file now, whatever the interval says.
    ///
    /// For shutdown and for callers that need the file current — a coalesced
    /// write can otherwise sit unwritten for as long as the interval.
    pub fn flush(&self) {
        // After a handoff the file belongs to the next proxy; this write would
        // replace its totals with ours. `Drop` lands here too.
        if self.stateless
            || !self.dirty.load(Ordering::Relaxed)
            || self.handed_off.load(Ordering::Relaxed)
        {
            return;
        }
        let mut st = match self.state.lock() {
            Ok(st) => st,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.write_state(&mut st);
        if let Ok(mut last) = self.last_write.lock() {
            *last = Some(Instant::now());
        }
    }

    /// Write the state file one last time and send every later `record_*`
    /// call to the handoff journal. Call it on the shutdown signal, before the
    /// listener closes: the restart script starts the next proxy only once the
    /// port is free, so that proxy loads a file that already holds everything
    /// recorded here up to the signal.
    ///
    /// Taken under the state lock, which every `record_*` call holds while it
    /// checks the flag, so no call can update memory after the final write.
    pub fn begin_handoff(&self) {
        if self.stateless {
            return;
        }
        let mut st = match self.state.lock() {
            Ok(st) => st,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.handed_off.load(Ordering::Relaxed) {
            return;
        }
        self.write_state(&mut st);
        self.handed_off.store(true, Ordering::Relaxed);
    }

    /// Apply the calls a draining proxy journaled, and empty the journal.
    /// Returns how many were applied. Cheap when there is nothing to do: one
    /// `stat`. A tracker that has itself handed off leaves the journal for the
    /// proxy after it.
    pub fn absorb_handoff(&self) -> usize {
        if self.stateless || self.handed_off.load(Ordering::Relaxed) {
            return 0;
        }
        let path = self.handoff_path();
        if !std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            return 0;
        }
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(&path) else {
            return 0;
        };
        if flock(file.as_fd(), FlockOperation::LockExclusive).is_err() {
            return 0;
        }
        // Read and emptied under the lock the writer takes for each line, so a
        // line lands either in this batch or in the next one, never in neither.
        let mut text = String::new();
        let emptied = file.read_to_string(&mut text).is_ok() && file.set_len(0).is_ok();
        let _ = flock(file.as_fd(), FlockOperation::Unlock);
        if !emptied {
            return 0;
        }
        text.lines()
            .filter_map(|line| serde_json::from_str::<HandoffOp>(line).ok())
            .map(|op| self.apply_handoff(op))
            .count()
    }

    fn handoff_path(&self) -> PathBuf {
        self.path.with_extension("handoff.jsonl")
    }

    /// With the state lock held: once handed off, journal the call and tell
    /// the caller to skip its update.
    fn divert(&self, op: impl FnOnce() -> HandoffOp) -> bool {
        if !self.handed_off.load(Ordering::Relaxed) {
            return false;
        }
        // Best effort, like the state file itself: a line that cannot be
        // written costs that one record.
        if let Ok(line) = serde_json::to_string(&op()) {
            let _ = append_locked_line(&self.handoff_path(), &line);
        }
        true
    }

    fn apply_handoff(&self, op: HandoffOp) {
        match op {
            HandoffOp::CompressionSavings {
                model,
                tokens_saved,
                provider,
                total_input_tokens,
                total_input_cost_usd,
                timestamp,
            } => {
                self.record_compression_savings(
                    &model,
                    tokens_saved,
                    provider.as_deref(),
                    total_input_tokens,
                    total_input_cost_usd,
                    Some(timestamp),
                );
            }
            HandoffOp::Request {
                model,
                input_tokens,
                tokens_saved,
                tool_schema_saved,
                compression_savings_cost_usd,
                tool_schema_savings_cost_usd,
                provider,
                project,
                cache_read_tokens,
                cache_write_tokens,
                uncached_input_tokens,
                total_input_tokens,
                total_input_cost_usd,
                timestamp,
                output_tokens_saved,
                output_tokens,
                attempted_input_tokens,
                cache_write_5m_tokens,
                cache_write_1h_tokens,
                cached,
                stack,
                waste_signals,
                offload_savings_usd,
            } => {
                self.record_request(&RequestRecord {
                    model: &model,
                    input_tokens,
                    tokens_saved,
                    tool_schema_saved,
                    compression_savings_cost_usd,
                    tool_schema_savings_cost_usd,
                    provider: provider.as_deref(),
                    project: project.as_deref(),
                    cache_read_tokens,
                    cache_write_tokens,
                    uncached_input_tokens,
                    total_input_tokens,
                    total_input_cost_usd,
                    timestamp: Some(timestamp),
                    output_tokens_saved,
                    output_tokens,
                    attempted_input_tokens,
                    cache_write_5m_tokens,
                    cache_write_1h_tokens,
                    cached,
                    stack: stack.as_deref(),
                    waste_signals,
                    offload_savings_usd,
                });
            }
            HandoffOp::RateLimited { provider, source } => {
                self.record_rate_limited(provider.as_deref(), &source)
            }
            HandoffOp::FailedWork {
                provider,
                status_code,
                upstream_attempts,
                forwarded_tokens,
                provider_input_tokens,
                provider_output_tokens,
                timestamp,
            } => self.record_failed_work(&FailedWorkRecord {
                provider,
                status_code,
                upstream_attempts,
                forwarded_tokens,
                provider_input_tokens,
                provider_output_tokens,
                timestamp: Some(timestamp),
            }),
            HandoffOp::CacheBust { tokens_lost } => self.record_cache_bust(tokens_lost),
            HandoffOp::CacheMiss { provider, reason } => {
                self.record_cache_miss(provider.as_deref(), reason.as_deref())
            }
            HandoffOp::ProxyOverhead {
                before_bytes,
                after_bytes,
            } => self.record_proxy_overhead(before_bytes, after_bytes),
            HandoffOp::UnbookedTurn {
                partial_input_tokens,
                partial_output_tokens,
            } => self.record_unbooked_turn(partial_input_tokens, partial_output_tokens),
            HandoffOp::Tools { definitions, calls } => self.record_tools(&definitions, &calls),
            HandoffOp::WireFootprint {
                bytes_in,
                bytes_out,
                input_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => self.record_wire_footprint(
                bytes_in,
                bytes_out,
                input_tokens,
                cache_read_tokens,
                cache_write_tokens,
            ),
        }
    }

    fn write_state(&self, st: &mut State) {
        if self.stateless {
            return;
        }
        let Some(parent) = self.path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        // Stamp before serialising so the file carries the time it was written,
        // and roll the stamp back on any failure below. `snapshot` reports this
        // field straight from state, so a save that didn't happen must not
        // leave a timestamp behind claiming it did.
        let previous_saved_at = st.metrics.state().persistence.last_saved_at.clone();
        st.metrics.set_last_saved_at(Some(to_utc_iso(utc_now())));
        // Serialised from borrowed parts rather than assembled into a `Value`
        // first, so the rendered history goes straight to the writer instead
        // of being cloned into the payload. Field order here is the file's key
        // order, so it matches what `json!` used to emit.
        #[derive(Serialize)]
        struct Payload<'a> {
            schema_version: i64,
            lifetime: Value,
            failed_work: Value,
            display_session: Value,
            history: &'a [Value],
            projects: Value,
            // Kept out of `lifetime_metrics` because that blob's shape is
            // asserted byte-exact against Python's and these counters are
            // Rust-only.
            lifetime_metrics: Value,
            lifetime_footprint: Value,
        }
        sync_history_rendered(st);
        let serialised = serde_json::to_string_pretty(&Payload {
            schema_version: SCHEMA_VERSION,
            lifetime: lifetime_value(&st.lifetime),
            failed_work: failed_work_value(&st.failed_work),
            display_session: display_session_value(&st.display_session),
            history: &st.history_rendered,
            projects: projects_persist_value(&st.projects),
            lifetime_metrics: st.metrics.to_dict(),
            lifetime_footprint: st.metrics.footprint_to_dict(),
        });
        let Ok(json_data) = serialised else {
            st.metrics.set_last_saved_at(previous_saved_at);
            return;
        };
        // Atomic temp-file + fsync + rename (Python parity, no tempfile crate).
        let tmp = parent.join(format!(
            ".proxy_savings_{}.tmp",
            utc_now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let write_result = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(json_data.as_bytes())?;
            f.flush()?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp);
            st.metrics.set_last_saved_at(previous_saved_at);
        } else {
            self.dirty.store(false, Ordering::Relaxed);
        }
    }
}

impl Drop for SavingsTracker {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Append one line under an exclusive `flock`, the lock
/// [`SavingsTracker::absorb_handoff`] takes to read and empty the file.
fn append_locked_line(path: &Path, line: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    flock(file.as_fd(), FlockOperation::LockExclusive)?;
    let written = file.write_all(format!("{line}\n").as_bytes());
    let _ = flock(file.as_fd(), FlockOperation::Unlock);
    written
}

// ── free helpers for JSON shaping ──

fn empty_display_session_value() -> Value {
    json!({
        "requests": 0,
        "tokens_saved": 0,
        "compression_savings_usd": 0.0,
        "cache_savings_usd": 0.0,
        "offload_savings_usd": 0.0,
        "total_input_tokens": 0,
        "total_input_cost_usd": 0.0,
        "savings_percent": 0.0,
        "started_at": Value::Null,
        "last_activity_at": Value::Null,
    })
}

fn lifetime_value(l: &Lifetime) -> Value {
    json!({
        "requests": l.requests,
        "tokens_saved": l.tokens_saved,
        "compression_savings_usd": l.compression_savings_usd,
        "total_input_tokens": l.total_input_tokens,
        "total_input_cost_usd": l.total_input_cost_usd,
        "output_tokens_saved": l.output_tokens_saved,
        "output_savings_usd": l.output_savings_usd,
        "total_output_cost_usd": l.total_output_cost_usd,
        "offload_savings_usd": l.offload_savings_usd,
        "cache_read_tokens": l.cache_read_tokens,
        "cache_savings_usd": l.cache_savings_usd,
    })
}

fn failed_work_value(f: &FailedWork) -> Value {
    json!({
        "requests": f.requests,
        "upstream_attempts": f.upstream_attempts,
        "forwarded_tokens": f.forwarded_tokens,
        "forwarded_tokens_at_risk": f.forwarded_tokens_at_risk,
        "provider_reported_input_tokens": f.provider_reported_input_tokens,
        "provider_reported_output_tokens": f.provider_reported_output_tokens,
        "provider_usage_observed_requests": f.provider_usage_observed_requests,
        "by_status": f.by_status,
        "last_activity_at": f.last_activity_at,
    })
}

fn normalize_failed_work(raw: Option<&Value>) -> FailedWork {
    let int = |key: &str| {
        raw.and_then(|v| v.get(key))
            .and_then(Value::as_i64)
            .map(coerce_int)
            .unwrap_or(0)
    };
    let by_status = raw
        .and_then(|v| v.get("by_status"))
        .and_then(Value::as_object)
        .map(|values| {
            values
                .iter()
                .filter_map(|(status, count)| {
                    count
                        .as_i64()
                        .map(coerce_int)
                        .map(|count| (status.clone(), count))
                })
                .collect()
        })
        .unwrap_or_default();
    FailedWork {
        requests: int("requests"),
        upstream_attempts: int("upstream_attempts"),
        forwarded_tokens: int("forwarded_tokens"),
        forwarded_tokens_at_risk: int("forwarded_tokens_at_risk"),
        provider_reported_input_tokens: int("provider_reported_input_tokens"),
        provider_reported_output_tokens: int("provider_reported_output_tokens"),
        provider_usage_observed_requests: int("provider_usage_observed_requests"),
        by_status,
        last_activity_at: raw
            .and_then(|v| v.get("last_activity_at"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn display_session_value(s: &DisplaySession) -> Value {
    json!({
        "requests": s.requests,
        "tokens_saved": s.tokens_saved,
        "compression_savings_usd": s.compression_savings_usd,
        "cache_savings_usd": s.cache_savings_usd,
        "offload_savings_usd": s.offload_savings_usd,
        "total_input_tokens": s.total_input_tokens,
        "total_input_cost_usd": s.total_input_cost_usd,
        "savings_percent": s.savings_percent,
        "started_at": s.started_at,
        "last_activity_at": s.last_activity_at,
    })
}

fn history_entry_value(e: &HistoryEntry) -> Value {
    json!({
        "timestamp": e.timestamp,
        "provider": e.provider,
        "model": e.model,
        "total_tokens_saved": e.total_tokens_saved,
        "compression_savings_usd": e.compression_savings_usd,
        "cache_read_tokens": e.cache_read_tokens,
        "cache_savings_usd": e.cache_savings_usd,
        "total_input_tokens": e.total_input_tokens,
        "total_input_cost_usd": e.total_input_cost_usd,
        "output_tokens_saved": e.output_tokens_saved,
        "output_savings_usd": e.output_savings_usd,
        "total_output_cost_usd": e.total_output_cost_usd,
    })
}

fn projects_persist_value(projects: &BTreeMap<String, ProjectEntry>) -> Value {
    let mut out = serde_json::Map::new();
    for (name, e) in projects {
        out.insert(
            name.clone(),
            json!({
                "requests": e.requests,
                "tokens_saved": e.tokens_saved,
                "compression_savings_usd": e.compression_savings_usd,
                "total_input_tokens": e.total_input_tokens,
                "total_input_cost_usd": e.total_input_cost_usd,
                "last_activity_at": e.last_activity_at,
            }),
        );
    }
    Value::Object(out)
}

fn normalize_history_entry(entry: &Value) -> Option<HistoryEntry> {
    let (timestamp, provider, model, tts, csu, crt, crs, tit, tic, ots, osu, toc) =
        if let Some(obj) = entry.as_object() {
            (
                parse_timestamp(obj.get("timestamp").and_then(Value::as_str).unwrap_or(""))?,
                normalize_provider(obj.get("provider").and_then(Value::as_str)),
                normalize_model(obj.get("model").and_then(Value::as_str)),
                obj.get("total_tokens_saved")
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                obj.get("compression_savings_usd")
                    .and_then(Value::as_f64)
                    .map(coerce_float)
                    .unwrap_or(0.0),
                // Older history points predate cache-mode history gating and omit
                // these keys entirely; default to 0/0.0 rather than dropping the
                // entry, matching how every other field here handles legacy shapes.
                obj.get("cache_read_tokens")
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                obj.get("cache_savings_usd")
                    .and_then(Value::as_f64)
                    .map(coerce_float)
                    .unwrap_or(0.0),
                obj.get("total_input_tokens")
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                obj.get("total_input_cost_usd")
                    .and_then(Value::as_f64)
                    .map(coerce_float)
                    .unwrap_or(0.0),
                obj.get("output_tokens_saved")
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                obj.get("output_savings_usd")
                    .and_then(Value::as_f64)
                    .map(coerce_float)
                    .unwrap_or(0.0),
                obj.get("total_output_cost_usd")
                    .and_then(Value::as_f64)
                    .map(coerce_float)
                    .unwrap_or(0.0),
            )
        } else if let Some(arr) = entry.as_array() {
            if arr.len() < 2 {
                return None;
            }
            (
                parse_timestamp(arr[0].as_str().unwrap_or(""))?,
                PROVIDER_UNKNOWN.to_string(),
                MODEL_UNKNOWN.to_string(),
                arr.get(1)
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                arr.get(2)
                    .and_then(Value::as_f64)
                    .map(coerce_float)
                    .unwrap_or(0.0),
                0,
                0.0,
                arr.get(3)
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                arr.get(4)
                    .and_then(Value::as_f64)
                    .map(coerce_float)
                    .unwrap_or(0.0),
                0,
                0.0,
                0.0,
            )
        } else {
            return None;
        };
    Some(HistoryEntry {
        timestamp: to_utc_iso(timestamp),
        provider,
        model,
        total_tokens_saved: tts,
        compression_savings_usd: round_n(csu, 6),
        cache_read_tokens: crt,
        cache_savings_usd: round_n(crs, 6),
        total_input_tokens: tit,
        total_input_cost_usd: round_n(tic, 6),
        output_tokens_saved: ots,
        output_savings_usd: round_n(osu, 6),
        total_output_cost_usd: round_n(toc, 6),
    })
}

fn normalize_display_session(entry: Option<&Value>) -> DisplaySession {
    let Some(obj) = entry.and_then(Value::as_object) else {
        return DisplaySession::default();
    };
    let started = obj
        .get("started_at")
        .and_then(Value::as_str)
        .and_then(parse_timestamp);
    let last = obj
        .get("last_activity_at")
        .and_then(Value::as_str)
        .and_then(parse_timestamp);
    let (Some(started), Some(last)) = (started, last) else {
        return DisplaySession::default();
    };
    if last < started {
        return DisplaySession::default();
    }
    let tokens_saved = obj
        .get("tokens_saved")
        .and_then(Value::as_i64)
        .map(coerce_int)
        .unwrap_or(0);
    let total_input_tokens = obj
        .get("total_input_tokens")
        .and_then(Value::as_i64)
        .map(coerce_int)
        .unwrap_or(0);
    let compression_savings_usd = round_n(
        obj.get("compression_savings_usd")
            .and_then(Value::as_f64)
            .map(coerce_float)
            .unwrap_or(0.0),
        6,
    );
    let cache_savings_usd = round_n(
        obj.get("cache_savings_usd")
            .and_then(Value::as_f64)
            .map(coerce_signed_float)
            .unwrap_or(0.0),
        6,
    );
    let total_input_cost_usd = round_n(
        obj.get("total_input_cost_usd")
            .and_then(Value::as_f64)
            .map(coerce_float)
            .unwrap_or(0.0),
        6,
    );
    let offload_savings_usd = round_n(
        obj.get("offload_savings_usd")
            .and_then(Value::as_f64)
            .map(coerce_float)
            .unwrap_or(0.0),
        6,
    );
    let savings_percent = cost_savings_percent(
        total_input_cost_usd,
        compression_savings_usd,
        offload_savings_usd,
    );
    DisplaySession {
        requests: obj
            .get("requests")
            .and_then(Value::as_i64)
            .map(coerce_int)
            .unwrap_or(0),
        tokens_saved,
        compression_savings_usd,
        cache_savings_usd,
        offload_savings_usd,
        total_input_tokens,
        total_input_cost_usd,
        savings_percent,
        started_at: Some(to_utc_iso(started)),
        last_activity_at: Some(to_utc_iso(last)),
    }
}

fn normalize_projects(raw: Option<&Value>) -> BTreeMap<String, ProjectEntry> {
    let mut projects = BTreeMap::new();
    let Some(obj) = raw.and_then(Value::as_object) else {
        return projects;
    };
    for (name, entry) in obj {
        let Some(cleaned) = sanitize_project_name(Some(name)) else {
            continue;
        };
        let Some(e) = entry.as_object() else { continue };
        let last = e
            .get("last_activity_at")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        projects.insert(
            cleaned,
            ProjectEntry {
                requests: e
                    .get("requests")
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                tokens_saved: e
                    .get("tokens_saved")
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                compression_savings_usd: round_n(
                    e.get("compression_savings_usd")
                        .and_then(Value::as_f64)
                        .map(coerce_float)
                        .unwrap_or(0.0),
                    6,
                ),
                total_input_tokens: e
                    .get("total_input_tokens")
                    .and_then(Value::as_i64)
                    .map(coerce_int)
                    .unwrap_or(0),
                total_input_cost_usd: round_n(
                    e.get("total_input_cost_usd")
                        .and_then(Value::as_f64)
                        .map(coerce_float)
                        .unwrap_or(0.0),
                    6,
                ),
                last_activity_at: last.map(to_utc_iso),
            },
        );
    }
    if projects.len() > DEFAULT_MAX_PROJECTS {
        let mut ranked: Vec<(String, ProjectEntry)> = projects.into_iter().collect();
        ranked.sort_by(|a, b| {
            (
                b.1.tokens_saved,
                b.1.last_activity_at.clone().unwrap_or_default(),
            )
                .cmp(&(
                    a.1.tokens_saved,
                    a.1.last_activity_at.clone().unwrap_or_default(),
                ))
        });
        ranked.truncate(DEFAULT_MAX_PROJECTS);
        projects = ranked.into_iter().collect();
    }
    projects
}

fn bucket_start(ts: DateTime<Utc>, bucket: &str) -> DateTime<Utc> {
    match bucket {
        "hour" => ts
            .with_minute(0)
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_nanosecond(0))
            .unwrap_or(ts),
        "day" => ts
            .with_hour(0)
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_nanosecond(0))
            .unwrap_or(ts),
        "week" => {
            let day_start = ts
                .with_hour(0)
                .and_then(|d| d.with_minute(0))
                .and_then(|d| d.with_second(0))
                .and_then(|d| d.with_nanosecond(0))
                .unwrap_or(ts);
            day_start - Duration::days(day_start.weekday().num_days_from_monday() as i64)
        }
        "month" => ts
            .with_day(1)
            .and_then(|d| d.with_hour(0))
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_second(0))
            .and_then(|d| d.with_nanosecond(0))
            .unwrap_or(ts),
        _ => ts,
    }
}

fn csv_cell(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => csv_escape(s),
        Some(other) => csv_escape(&other.to_string()),
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// Rollup accumulator (mirrors the Python dict entry).
#[derive(Default)]
struct RollupDelta {
    tokens_saved: i64,
    compression_savings_usd_delta: f64,
    total_input_tokens_delta: i64,
    total_input_cost_usd_delta: f64,
}

struct RollupEntry {
    timestamp: String,
    tokens_saved: i64,
    compression_savings_usd_delta: f64,
    total_tokens_saved: i64,
    compression_savings_usd: f64,
    total_input_tokens_delta: i64,
    total_input_tokens: i64,
    total_input_cost_usd_delta: f64,
    total_input_cost_usd: f64,
    /// Slice #2: per-bucket output-shaping rollup, mirroring Python
    /// `_build_rollup` (`output_tokens_saved_delta`,
    /// `output_savings_usd_delta`). Deltas only — no output totals, same as
    /// Python. Dashboard-only value; the provider/model splits stay
    /// input-side, also matching Python.
    output_tokens_saved_delta: i64,
    output_savings_usd_delta: f64,
    /// Output spend per bucket and cumulative (upstream `c766bdb2`).
    total_output_cost_usd_delta: f64,
    total_output_cost_usd: f64,
    by_provider: BTreeMap<String, RollupDelta>,
    by_model: BTreeMap<String, RollupDelta>,
}

impl RollupEntry {
    fn new(
        key: &str,
        total_tokens: i64,
        total_usd: f64,
        total_input_tokens: i64,
        total_input_cost: f64,
    ) -> Self {
        Self {
            timestamp: key.to_string(),
            tokens_saved: 0,
            compression_savings_usd_delta: 0.0,
            total_tokens_saved: total_tokens,
            compression_savings_usd: total_usd,
            total_input_tokens_delta: 0,
            total_input_tokens,
            total_input_cost_usd_delta: 0.0,
            total_input_cost_usd: total_input_cost,
            output_tokens_saved_delta: 0,
            output_savings_usd_delta: 0.0,
            total_output_cost_usd_delta: 0.0,
            total_output_cost_usd: 0.0,
            by_provider: BTreeMap::new(),
            by_model: BTreeMap::new(),
        }
    }

    fn to_value(&self) -> Value {
        let map_deltas = |m: &BTreeMap<String, RollupDelta>| -> Value {
            let mut out = serde_json::Map::new();
            for (k, d) in m {
                out.insert(
                    k.clone(),
                    json!({
                        "tokens_saved": d.tokens_saved,
                        "compression_savings_usd_delta": d.compression_savings_usd_delta,
                        "total_input_tokens_delta": d.total_input_tokens_delta,
                        "total_input_cost_usd_delta": d.total_input_cost_usd_delta,
                    }),
                );
            }
            Value::Object(out)
        };
        json!({
            "timestamp": self.timestamp,
            "tokens_saved": self.tokens_saved,
            "compression_savings_usd_delta": self.compression_savings_usd_delta,
            "total_tokens_saved": self.total_tokens_saved,
            "compression_savings_usd": self.compression_savings_usd,
            "total_input_tokens_delta": self.total_input_tokens_delta,
            "total_input_tokens": self.total_input_tokens,
            "total_input_cost_usd_delta": self.total_input_cost_usd_delta,
            "total_input_cost_usd": self.total_input_cost_usd,
            "output_tokens_saved_delta": self.output_tokens_saved_delta,
            "output_savings_usd_delta": self.output_savings_usd_delta,
            "total_output_cost_usd_delta": self.total_output_cost_usd_delta,
            "total_output_cost_usd": self.total_output_cost_usd,
            "by_provider": map_deltas(&self.by_provider),
            "by_model": map_deltas(&self.by_model),
        })
    }
}

#[cfg(test)]
mod tests;

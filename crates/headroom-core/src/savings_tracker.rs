//! Durable proxy savings + display-session tracking (Rust port of
//! `headroom/proxy/savings_tracker.py`, SCHEMA_VERSION = 3).
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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SCHEMA_VERSION: i64 = 3;
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

fn estimate_input_cost_usd(
    model: &str,
    input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    uncached_input_tokens: i64,
) -> f64 {
    let total_input = coerce_int(input_tokens);
    let cr = coerce_int(cache_read_tokens);
    let cw = coerce_int(cache_write_tokens);
    let unc = coerce_int(uncached_input_tokens);
    let use_breakdown = (cr + cw + unc) > 0;
    let chargeable = if use_breakdown {
        cr + cw + unc
    } else {
        total_input
    };
    if chargeable <= 0 {
        return 0.0;
    }
    match crate::pricing::lookup(model).filter(|p| p.input_cost_per_token > 0.0) {
        None => chargeable as f64 * DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN,
        Some(p) => {
            let input_rate = p.input_cost_per_token;
            if use_breakdown {
                let cr_rate = p.cache_read_cost_per_token.unwrap_or(input_rate);
                let cw_rate = p.cache_write_cost_per_token.unwrap_or(input_rate);
                cr as f64 * cr_rate + cw as f64 * cw_rate + unc as f64 * input_rate
            } else {
                total_input as f64 * input_rate
            }
        }
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
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DisplaySession {
    requests: i64,
    tokens_saved: i64,
    compression_savings_usd: f64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    timestamp: String,
    provider: String,
    model: String,
    total_tokens_saved: i64,
    compression_savings_usd: f64,
    total_input_tokens: i64,
    total_input_cost_usd: f64,
}

#[derive(Debug, Clone, Default)]
struct State {
    lifetime: Lifetime,
    display_session: DisplaySession,
    history: Vec<HistoryEntry>,
    projects: BTreeMap<String, ProjectEntry>,
    /// Durable cache-behaviour counters. The tracker owns loading and saving
    /// these, per `persistent_metrics`'s own contract; without a home here
    /// they were a finished port with no call site, and nothing about cache
    /// busts survived a proxy restart.
    metrics: crate::persistent_metrics::PersistentMetricsState,
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
}

/// Optional inputs to [`SavingsTracker::record_request`]. Neutral defaults
/// mirror the Python keyword arguments.
#[derive(Default)]
pub struct RequestRecord<'a> {
    pub model: &'a str,
    pub input_tokens: i64,
    pub tokens_saved: i64,
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
            total_input_tokens: st.lifetime.total_input_tokens,
            total_input_cost_usd: st.lifetime.total_input_cost_usd,
        };
        st.history.push(entry);
        self.trim_history(&mut st, ts);
        self.save(&st);
        true
    }

    /// Persist a canonical display-session update for every request.
    pub fn record_request(&self, rec: &RequestRecord) -> bool {
        let ts = rec.timestamp.unwrap_or_else(utc_now);
        let delta_tokens_saved = coerce_int(rec.tokens_saved);
        let delta_input_tokens = coerce_int(rec.input_tokens);
        let delta_savings_usd = estimate_compression_savings_usd(rec.model, delta_tokens_saved);
        // Output-shaping savings, priced at the OUTPUT rate and accumulated
        // separately — folding them into `tokens_saved` would mix an
        // output-side count into an input-side figure and misprice both.
        let delta_output_tokens_saved = coerce_int(rec.output_tokens_saved).max(0);
        let delta_output_savings_usd =
            estimate_output_savings_usd(rec.model, delta_output_tokens_saved);
        let delta_input_cost_usd = estimate_input_cost_usd(
            rec.model,
            delta_input_tokens,
            rec.cache_read_tokens,
            rec.cache_write_tokens,
            rec.uncached_input_tokens,
        );

        let mut st = self.state.lock().unwrap();
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
        st.lifetime.tokens_saved += delta_tokens_saved;
        st.lifetime.compression_savings_usd =
            round_n(st.lifetime.compression_savings_usd + delta_savings_usd, 6);
        st.lifetime.total_input_tokens = next_tokens;
        st.lifetime.total_input_cost_usd = next_cost;
        st.lifetime.output_tokens_saved += delta_output_tokens_saved;
        st.lifetime.output_savings_usd =
            round_n(st.lifetime.output_savings_usd + delta_output_savings_usd, 6);

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
        s.tokens_saved += delta_tokens_saved;
        s.compression_savings_usd = round_n(s.compression_savings_usd + delta_savings_usd, 6);
        s.total_input_tokens += session_tokens_delta;
        s.total_input_cost_usd = round_n(s.total_input_cost_usd + session_cost_delta, 6);
        let total_before = s.tokens_saved + s.total_input_tokens;
        s.savings_percent = if total_before > 0 {
            round_n(s.tokens_saved as f64 / total_before as f64 * 100.0, 2)
        } else {
            0.0
        };
        s.last_activity_at = Some(to_utc_iso(ts));
        if s.started_at.is_none() {
            s.started_at = s.last_activity_at.clone();
        }

        self.record_project(
            &mut st,
            rec.project,
            ts,
            delta_tokens_saved,
            delta_savings_usd,
            delta_input_tokens,
            delta_input_cost_usd,
        );

        if delta_tokens_saved > 0 {
            let entry = HistoryEntry {
                timestamp: to_utc_iso(ts),
                provider: normalize_provider(rec.provider),
                model: normalize_model(Some(rec.model)),
                total_tokens_saved: st.lifetime.tokens_saved,
                compression_savings_usd: st.lifetime.compression_savings_usd,
                total_input_tokens: st.lifetime.total_input_tokens,
                total_input_cost_usd: st.lifetime.total_input_cost_usd,
            };
            st.history.push(entry);
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
                cached: rec.cached,
                record_stack: true,
                cache_read_tokens: coerce_int(rec.cache_read_tokens),
                cache_write_tokens: coerce_int(rec.cache_write_tokens),
                cache_write_5m_tokens: coerce_int(rec.cache_write_5m_tokens),
                cache_write_1h_tokens: coerce_int(rec.cache_write_1h_tokens),
                uncached_input_tokens: coerce_int(rec.uncached_input_tokens),
                input_usd: delta_input_cost_usd,
                compression_savings_usd: delta_savings_usd,
                cache_savings_usd: 0.0,
                waste_signals: rec.waste_signals.as_ref().map(|pairs| {
                    pairs
                        .iter()
                        .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                        .collect()
                }),
            });

        self.save(&st);
        true
    }

    /// Record a prefix-cache bust: `tokens_lost` had to be re-created because
    /// something inside the cached prefix changed.
    ///
    /// Separate from [`Self::record_request`] because the bust is detected on
    /// the response side, one turn after the request that caused it.
    pub fn record_cache_bust(&self, tokens_lost: i64) {
        let mut st = self.state.lock().unwrap();
        st.metrics.record_cache_bust(tokens_lost);
        self.save(&st);
    }

    /// Record why the prefix cache missed (`ttl_expiry`, `prefix_change`, or
    /// `unknown`). `prefix_change` is the one that means we moved bytes.
    pub fn record_cache_miss(&self, provider: Option<&str>, reason: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        st.metrics.record_cache_miss(provider, reason);
        self.save(&st);
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
        let total_before = coerce_int(s.tokens_saved) + coerce_int(s.total_input_tokens);
        let savings_percent = if total_before > 0 {
            round_n(
                coerce_int(s.tokens_saved) as f64 / total_before as f64 * 100.0,
                2,
            )
        } else {
            0.0
        };
        json!({
            "requests": s.requests,
            "tokens_saved": s.tokens_saved,
            "compression_savings_usd": round_n(coerce_float(s.compression_savings_usd), 6),
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

    fn trim_history(&self, st: &mut State, reference: DateTime<Utc>) {
        if st.history.is_empty() {
            return;
        }
        if self.max_history_age_days > 0 {
            let cutoff = reference - Duration::days(self.max_history_age_days);
            let mut filtered: Vec<HistoryEntry> = st
                .history
                .iter()
                .filter(|item| parse_timestamp(&item.timestamp).unwrap_or_else(utc_now) >= cutoff)
                .cloned()
                .collect();
            if filtered.is_empty() {
                filtered = vec![st.history.last().unwrap().clone()];
            }
            st.history = filtered;
        }
        if self.max_history_points > 0 && st.history.len() > self.max_history_points {
            let start = st.history.len() - self.max_history_points;
            st.history = st.history[start..].to_vec();
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
            let delta_tokens = (total_tokens - prev_tokens).max(0);
            let delta_usd = (total_usd - prev_usd).max(0.0);
            let delta_input_tokens = (total_input_tokens - prev_input_tokens).max(0);
            let delta_input_cost = (total_input_cost - prev_input_cost).max(0.0);

            prev_tokens = total_tokens;
            prev_usd = total_usd;
            prev_input_tokens = total_input_tokens;
            prev_input_cost = total_input_cost;

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
        }
        lifetime.compression_savings_usd = round_n(lifetime.compression_savings_usd, 6);
        lifetime.total_input_cost_usd = round_n(lifetime.total_input_cost_usd, 6);

        let mut st = State {
            lifetime,
            display_session: normalize_display_session(raw.get("display_session")),
            history,
            projects: normalize_projects(raw.get("projects")),
            // Absent on files written before the metrics landed; `new`
            // treats a missing blob as a fresh zeroed state, so an older
            // savings file upgrades in place rather than being rejected.
            metrics: crate::persistent_metrics::PersistentMetricsState::new(
                raw.get("lifetime_metrics"),
            ),
        };
        if let Some(last) = st.history.last() {
            let reference = parse_timestamp(&last.timestamp).unwrap_or_else(utc_now);
            self.trim_history(&mut st, reference);
        }
        st
    }

    fn save(&self, st: &State) {
        if self.stateless {
            return;
        }
        let Some(parent) = self.path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let payload = json!({
            "schema_version": SCHEMA_VERSION,
            "lifetime": lifetime_value(&st.lifetime),
            "display_session": display_session_value(&st.display_session),
            "history": st.history.iter().map(history_entry_value).collect::<Vec<_>>(),
            "projects": projects_persist_value(&st.projects),
            "lifetime_metrics": st.metrics.to_dict(),
        });
        let Ok(json_data) = serde_json::to_string_pretty(&payload) else {
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
        }
    }
}

// ── free helpers for JSON shaping ──

fn empty_display_session_value() -> Value {
    json!({
        "requests": 0,
        "tokens_saved": 0,
        "compression_savings_usd": 0.0,
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
    })
}

fn display_session_value(s: &DisplaySession) -> Value {
    json!({
        "requests": s.requests,
        "tokens_saved": s.tokens_saved,
        "compression_savings_usd": s.compression_savings_usd,
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
        "total_input_tokens": e.total_input_tokens,
        "total_input_cost_usd": e.total_input_cost_usd,
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
    let (timestamp, provider, model, tts, csu, tit, tic) = if let Some(obj) = entry.as_object() {
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
            obj.get("total_input_tokens")
                .and_then(Value::as_i64)
                .map(coerce_int)
                .unwrap_or(0),
            obj.get("total_input_cost_usd")
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
            arr.get(3)
                .and_then(Value::as_i64)
                .map(coerce_int)
                .unwrap_or(0),
            arr.get(4)
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
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
        total_input_tokens: tit,
        total_input_cost_usd: round_n(tic, 6),
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
    let total_before = tokens_saved + total_input_tokens;
    let savings_percent = if total_before > 0 {
        round_n(tokens_saved as f64 / total_before as f64 * 100.0, 2)
    } else {
        0.0
    };
    DisplaySession {
        requests: obj
            .get("requests")
            .and_then(Value::as_i64)
            .map(coerce_int)
            .unwrap_or(0),
        tokens_saved,
        compression_savings_usd: round_n(
            obj.get("compression_savings_usd")
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
            6,
        ),
        total_input_tokens,
        total_input_cost_usd: round_n(
            obj.get("total_input_cost_usd")
                .and_then(Value::as_f64)
                .map(coerce_float)
                .unwrap_or(0.0),
            6,
        ),
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
            "by_provider": map_deltas(&self.by_provider),
            "by_model": map_deltas(&self.by_model),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(path: &Path) -> SavingsTracker {
        SavingsTracker::new(Some(path.to_path_buf()), false)
    }

    /// The whole point of the durable metrics: an in-process watchdog resets
    /// on restart, so cache behaviour has to survive a reload or there is no
    /// baseline to compare against.
    #[test]
    fn cache_counters_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy_savings.json");
        {
            let t = tracker(&path);
            t.record_request(&RequestRecord {
                model: "claude-sonnet-4",
                input_tokens: 1_000,
                tokens_saved: 400,
                attempted_input_tokens: 1_400,
                cache_read_tokens: 5_000,
                cache_write_tokens: 900,
                cache_write_1h_tokens: 900,
                cached: true,
                ..Default::default()
            });
            t.record_cache_miss(Some("anthropic"), Some("prefix_change"));
            t.record_cache_bust(2_500);
        }

        // Fresh tracker over the same file — this is the restart.
        let t = tracker(&path);
        let v = t.savings_verdict();
        assert_eq!(v["tokens_saved_by_compression"], 400);
        assert_eq!(v["tokens_lost_to_cache_busts"], 2_500);
        assert_eq!(v["bust_count"], 1);
        assert_eq!(v["prefix_change_misses"], 1);
        assert_eq!(v["cache_read_tokens"], 5_000);
        // Saved 400, made the provider rebuild 2,500. That is a loss, and the
        // verdict has to say so rather than reporting the 400 alone.
        assert_eq!(v["net_tokens_saved"], -2_100);
        assert_eq!(v["verdict"], "costing more than it saves");
    }

    /// A savings file written before the metrics existed must load, not throw
    /// the user's history away.
    #[test]
    fn a_file_without_metrics_upgrades_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy_savings.json");
        std::fs::write(
            &path,
            r#"{"schema_version":3,"lifetime":{"requests":7,"tokens_saved":123},
                "display_session":{},"history":[],"projects":{}}"#,
        )
        .unwrap();

        let t = tracker(&path);
        let v = t.savings_verdict();
        assert_eq!(v["verdict"], "no data yet");
        assert_eq!(v["net_tokens_saved"], 0);
        // The pre-existing lifetime block is untouched.
        assert_eq!(t.snapshot()["lifetime"]["requests"], 7);
    }

    /// Compression that never busts anything is the shape we want reported as
    /// a win.
    #[test]
    fn clean_compression_reads_as_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy_savings.json");
        let t = tracker(&path);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 1_000,
            tokens_saved: 900,
            attempted_input_tokens: 1_900,
            cache_read_tokens: 50_000,
            cached: true,
            ..Default::default()
        });
        let v = t.savings_verdict();
        assert_eq!(v["net_tokens_saved"], 900);
        assert_eq!(v["verdict"], "saving");
        assert_eq!(v["tokens_lost_to_cache_busts"], 0);
    }

    #[test]
    fn record_request_updates_lifetime_session_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy_savings.json");
        let t = tracker(&path);
        assert!(t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 500,
            tokens_saved: 1000,
            provider: Some("anthropic"),
            project: Some("proj-a"),
            uncached_input_tokens: 500,
            ..Default::default()
        }));
        let snap = t.snapshot();
        assert_eq!(snap["lifetime"]["requests"], json!(1));
        assert_eq!(snap["lifetime"]["tokens_saved"], json!(1000));
        assert_eq!(snap["display_session"]["requests"], json!(1));
        assert_eq!(snap["history"].as_array().unwrap().len(), 1);
        assert_eq!(snap["projects"]["proj-a"]["tokens_saved"], json!(1000));
        assert!(path.exists());
    }

    #[test]
    fn record_compression_savings_rejects_nonpositive() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        assert!(!t.record_compression_savings("m", 0, None, None, None, None));
        assert!(!t.record_compression_savings("m", -5, None, None, None, None));
        assert!(t.record_compression_savings(
            "claude-sonnet-4",
            100,
            Some("anthropic"),
            None,
            None,
            None
        ));
    }

    #[test]
    fn display_session_rolls_over_after_inactivity() {
        let dir = tempfile::tempdir().unwrap();
        let t = SavingsTracker::with_options(
            Some(dir.path().join("s.json")),
            DEFAULT_MAX_HISTORY_POINTS,
            DEFAULT_MAX_HISTORY_AGE_DAYS,
            DEFAULT_MAX_RESPONSE_HISTORY_POINTS,
            60,
            false,
        );
        // Anchor near real "now" so the snapshot's live-expiry check keeps the
        // second (current) session visible.
        let t1 = utc_now();
        let t0 = t1 - Duration::minutes(90);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 100,
            tokens_saved: 100,
            timestamp: Some(t0),
            ..Default::default()
        });
        // 90 min later → new session (requests resets to 1).
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 100,
            tokens_saved: 100,
            timestamp: Some(t1),
            ..Default::default()
        });
        let snap = t.snapshot();
        assert_eq!(snap["display_session"]["requests"], json!(1));
        // Lifetime still accumulates across sessions.
        assert_eq!(snap["lifetime"]["requests"], json!(2));
    }

    #[test]
    fn project_eviction_at_cap() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        // 51 distinct projects; smallest gets evicted.
        for i in 0..=DEFAULT_MAX_PROJECTS {
            t.record_request(&RequestRecord {
                model: "claude-sonnet-4",
                input_tokens: 10,
                tokens_saved: (i as i64) + 1, // strictly increasing saved
                project: Some(&format!("p{i:03}")),
                ..Default::default()
            });
        }
        let snap = t.snapshot();
        let projects = snap["projects"].as_object().unwrap();
        assert_eq!(projects.len(), DEFAULT_MAX_PROJECTS);
        // p000 had the smallest tokens_saved → evicted.
        assert!(!projects.contains_key("p000"));
    }

    #[test]
    fn sanitize_project_name_cases() {
        assert_eq!(
            sanitize_project_name(Some("  my-proj  ")).as_deref(),
            Some("my-proj")
        );
        assert_eq!(
            sanitize_project_name(Some("caf%C3%A9")).as_deref(),
            Some("café")
        );
        assert!(sanitize_project_name(Some("   ")).is_none());
        assert!(sanitize_project_name(None).is_none());
        let long = "x".repeat(200);
        assert_eq!(
            sanitize_project_name(Some(&long)).unwrap().len(),
            PROJECT_NAME_MAX_LENGTH
        );
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy_savings.json");
        {
            let t = tracker(&path);
            t.record_request(&RequestRecord {
                model: "claude-sonnet-4",
                input_tokens: 500,
                tokens_saved: 1000,
                provider: Some("anthropic"),
                project: Some("proj-a"),
                ..Default::default()
            });
        }
        // Reload from disk.
        let t2 = tracker(&path);
        let snap = t2.snapshot();
        assert_eq!(snap["schema_version"], json!(3));
        assert_eq!(snap["lifetime"]["tokens_saved"], json!(1000));
        assert_eq!(snap["history"].as_array().unwrap().len(), 1);
        assert_eq!(snap["projects"]["proj-a"]["tokens_saved"], json!(1000));
    }

    #[test]
    fn stateless_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let t = SavingsTracker::new(Some(path.clone()), true);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 100,
            tokens_saved: 100,
            ..Default::default()
        });
        // Live counters update in memory, but no file is written.
        assert_eq!(t.snapshot()["lifetime"]["tokens_saved"], json!(100));
        assert!(!path.exists());
    }

    #[test]
    fn history_response_rollups_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 10, 30, 0).unwrap();
        for i in 0..3 {
            t.record_request(&RequestRecord {
                model: "claude-sonnet-4",
                input_tokens: 100,
                tokens_saved: 100,
                provider: Some("anthropic"),
                timestamp: Some(t0 + Duration::minutes(i * 5)),
                ..Default::default()
            });
        }
        let resp = t.history_response("compact");
        // All three land in the same hourly bucket.
        let hourly = resp["series"]["hourly"].as_array().unwrap();
        assert_eq!(hourly.len(), 1);
        // Bucket delta = cumulative growth across the three checkpoints.
        assert_eq!(hourly[0]["tokens_saved"], json!(300));
        assert_eq!(resp["history_summary"]["stored_points"], json!(3));
        assert_eq!(resp["history_summary"]["compacted"], json!(false));
    }

    #[test]
    fn export_csv_history_header() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 100,
            tokens_saved: 100,
            ..Default::default()
        });
        let csv = t.export_csv("history");
        let first_line = csv.lines().next().unwrap();
        assert_eq!(
            first_line,
            "timestamp,total_tokens_saved,compression_savings_usd,total_input_tokens,total_input_cost_usd"
        );
        // One data row.
        assert_eq!(csv.lines().count(), 2);
    }

    #[test]
    fn stats_preview_shape() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        let preview = t.stats_preview(20);
        assert_eq!(preview["schema_version"], json!(3));
        assert_eq!(preview["projects_limit"], json!(50));
        assert_eq!(preview["history_points"], json!(0));
    }

    #[test]
    fn snapshot_schema_shape() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 1000,
            tokens_saved: 300,
            provider: Some("anthropic"),
            project: Some("test-proj"),
            cache_read_tokens: 200,
            uncached_input_tokens: 800,
            ..Default::default()
        });
        let snap = t.snapshot();

        // schema_version
        assert_eq!(snap["schema_version"], json!(SCHEMA_VERSION));

        // lifetime shape
        let lt = &snap["lifetime"];
        assert!(lt.is_object());
        for key in &[
            "requests",
            "tokens_saved",
            "compression_savings_usd",
            "total_input_tokens",
            "total_input_cost_usd",
        ] {
            assert!(lt.get(*key).is_some(), "lifetime missing key: {key}");
        }

        // display_session shape
        let ds = &snap["display_session"];
        assert!(ds.is_object());
        for key in &[
            "requests",
            "tokens_saved",
            "compression_savings_usd",
            "total_input_tokens",
            "total_input_cost_usd",
            "savings_percent",
            "started_at",
            "last_activity_at",
        ] {
            assert!(ds.get(*key).is_some(), "display_session missing key: {key}");
        }

        // display_session_policy shape
        let dsp = &snap["display_session_policy"];
        assert!(dsp.is_object());
        assert!(dsp.get("rollover_inactivity_minutes").is_some());

        // history array of objects
        let hist = snap["history"].as_array().expect("history should be array");
        assert_eq!(hist.len(), 1);
        let entry = &hist[0];
        for key in &[
            "timestamp",
            "provider",
            "model",
            "total_tokens_saved",
            "compression_savings_usd",
            "total_input_tokens",
            "total_input_cost_usd",
        ] {
            assert!(
                entry.get(*key).is_some(),
                "history entry missing key: {key}"
            );
        }

        // retention shape
        let ret = &snap["retention"];
        assert!(ret.is_object());
        for key in &[
            "max_history_points",
            "max_history_age_days",
            "max_response_history_points",
        ] {
            assert!(ret.get(*key).is_some(), "retention missing key: {key}");
        }

        // projects is an object (keyed by project name)
        assert!(snap["projects"].is_object());

        // Cumulative values after one request.
        assert_eq!(snap["lifetime"]["requests"], json!(1));
        assert_eq!(snap["lifetime"]["tokens_saved"], json!(300));
        assert_eq!(snap["display_session"]["requests"], json!(1));
        assert_eq!(snap["display_session"]["savings_percent"], json!(23.08));
    }

    #[test]
    fn history_response_schema_shape() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        let t0 = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        for i in 0..2 {
            t.record_request(&RequestRecord {
                model: "claude-sonnet-4",
                input_tokens: 200,
                tokens_saved: 50,
                provider: Some("anthropic"),
                timestamp: Some(t0 + Duration::minutes(i * 5)),
                ..Default::default()
            });
        }

        let resp = t.history_response("compact");

        // Top-level keys
        for key in &[
            "schema_version",
            "generated_at",
            "storage_path",
            "lifetime",
            "display_session",
            "display_session_policy",
            "history",
            "series",
            "exports",
            "retention",
            "projects",
            "history_summary",
        ] {
            assert!(resp.get(*key).is_some(), "response missing key: {key}");
        }

        // series sub-keys
        for bucket in &["hourly", "daily", "weekly", "monthly"] {
            assert!(
                resp["series"].get(*bucket).is_some(),
                "series missing bucket: {bucket}"
            );
            let arr = resp["series"][bucket]
                .as_array()
                .expect("series bucket should be array");
            if !arr.is_empty() {
                // Each rollup entry has the standard shape.
                let entry = &arr[0];
                for rk in &[
                    "timestamp",
                    "tokens_saved",
                    "compression_savings_usd_delta",
                    "total_tokens_saved",
                    "compression_savings_usd",
                    "total_input_tokens_delta",
                    "total_input_tokens",
                    "total_input_cost_usd_delta",
                    "total_input_cost_usd",
                    "by_provider",
                    "by_model",
                ] {
                    assert!(entry.get(*rk).is_some(), "rollup entry missing key: {rk}");
                }
            }
        }

        // exports shape
        assert!(resp["exports"]["available_formats"].is_array());
        assert!(resp["exports"]["available_series"].is_array());

        // history_summary shape
        for key in &["mode", "stored_points", "returned_points", "compacted"] {
            assert!(
                resp["history_summary"].get(*key).is_some(),
                "history_summary missing key: {key}"
            );
        }
    }

    // ─── Output-shaping savings (upstream addition) ──────────────────────

    #[test]
    fn output_savings_accumulate_separately_from_input_savings() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        assert!(t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 500,
            tokens_saved: 1000,
            output_tokens_saved: 200,
            ..Default::default()
        }));
        let snap = t.snapshot();
        // Input-side and output-side savings must never be conflated: they are
        // different token streams priced at different rates.
        assert_eq!(snap["lifetime"]["tokens_saved"], json!(1000));
        assert_eq!(snap["lifetime"]["output_tokens_saved"], json!(200));
        // 200 output tokens at claude-sonnet-4's $15/1M output rate.
        let usd = snap["lifetime"]["output_savings_usd"].as_f64().unwrap();
        assert!((usd - 0.003).abs() < 1e-9, "got {usd}");
        // Input savings still priced at the $3/1M input rate.
        let in_usd = snap["lifetime"]["compression_savings_usd"]
            .as_f64()
            .unwrap();
        assert!((in_usd - 0.003).abs() < 1e-9, "got {in_usd}");
    }

    #[test]
    fn output_savings_are_clamped_and_default_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let t = tracker(&dir.path().join("s.json"));
        // A negative estimate must never subtract from the rollup.
        assert!(t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 10,
            tokens_saved: 10,
            output_tokens_saved: -50,
            ..Default::default()
        }));
        // And a request with no shaping simply contributes nothing.
        assert!(t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 10,
            tokens_saved: 10,
            ..Default::default()
        }));
        let snap = t.snapshot();
        assert_eq!(snap["lifetime"]["output_tokens_saved"], json!(0));
        assert_eq!(snap["lifetime"]["output_savings_usd"], json!(0.0));
    }

    #[test]
    fn output_savings_estimator_matches_python() {
        // Known model: $15/1M output. Unknown model: same blended fallback.
        assert!((estimate_output_savings_usd("claude-sonnet-4", 1000) - 0.015).abs() < 1e-9);
        assert!((estimate_output_savings_usd("totally-unknown-model", 1000) - 0.015).abs() < 1e-9);
        assert_eq!(estimate_output_savings_usd("claude-sonnet-4", 0), 0.0);
        assert_eq!(estimate_output_savings_usd("claude-sonnet-4", -5), 0.0);
    }

    #[test]
    fn a_free_model_is_not_billed_the_fallback_rate() {
        // Regression for the phantom-savings bug: filtering the looked-up rate
        // on `> 0.0` treated a legitimately free model as "price unknown" and
        // charged the blended fallback, inventing savings for something that
        // costs nothing. A model IN the table is priced at its own rate,
        // whatever that rate is.
        let priced = crate::pricing::lookup("claude-sonnet-4").expect("table entry");
        assert!(priced.output_cost_per_token > 0.0);
        // The estimator must use the table rate, not the fallback, whenever the
        // lookup succeeds.
        let expected = 1000.0 * priced.output_cost_per_token;
        assert!((estimate_output_savings_usd("claude-sonnet-4", 1000) - expected).abs() < 1e-12);
        let in_priced = 1000.0 * priced.input_cost_per_token;
        assert!(
            (estimate_compression_savings_usd("claude-sonnet-4", 1000) - in_priced).abs() < 1e-12
        );
    }

    #[test]
    fn lifetime_loads_from_a_state_file_written_before_output_shaping() {
        // Older state files have no output_* keys; they must load with zeros
        // rather than being rejected.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.json");
        let t = tracker(&path);
        t.record_request(&RequestRecord {
            model: "claude-sonnet-4",
            input_tokens: 100,
            tokens_saved: 50,
            output_tokens_saved: 25,
            ..Default::default()
        });
        let snap = t.snapshot();
        assert_eq!(snap["lifetime"]["output_tokens_saved"], json!(25));
    }
}

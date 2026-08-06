//! Data models for Anthropic subscription window tracking (port of
//! `headroom/subscription/models.py`).
//!
//! `to_dict`/`to_value` methods emit the exact JSON shapes (field names,
//! rounding) the Python implementation persists, so state files round-trip
//! between both.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};

use super::{parse_timestamp, round_half_even, to_utc_iso, utc_now};

fn get_f64(data: &Value, key: &str) -> Option<f64> {
    data.get(key).and_then(|v| match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    })
}

fn get_i64(data: &Value, key: &str) -> Option<i64> {
    data.get(key).and_then(|v| match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// RateLimitWindow
// ---------------------------------------------------------------------------

/// A single rolling rate-limit window returned by the Anthropic usage API.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimitWindow {
    pub used: i64,
    pub limit: i64,
    pub utilization_pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl RateLimitWindow {
    pub fn from_api_dict(data: &Value) -> Self {
        Self {
            used: get_i64(data, "used").unwrap_or(0),
            limit: get_i64(data, "limit").unwrap_or(0),
            utilization_pct: get_f64(data, "utilization").unwrap_or(0.0),
            resets_at: data
                .get("resets_at")
                .and_then(|v| v.as_str())
                .and_then(parse_timestamp),
        }
    }

    pub fn seconds_to_reset(&self, now: Option<DateTime<Utc>>) -> Option<f64> {
        let resets_at = self.resets_at?;
        let now = now.unwrap_or_else(utc_now);
        Some(((resets_at - now).num_milliseconds() as f64 / 1000.0).max(0.0))
    }

    pub fn to_value(&self) -> Value {
        json!({
            "used": self.used,
            "limit": self.limit,
            "utilization_pct": round_half_even(self.utilization_pct, 2),
            "resets_at": self.resets_at.map(|d| to_utc_iso(&d)),
            "seconds_to_reset": self.seconds_to_reset(None),
        })
    }
}

// ---------------------------------------------------------------------------
// Display-time synthesis
// ---------------------------------------------------------------------------

/// Render a rate-limit window for the dashboard, synthesizing post-reset.
///
/// If `now` is past `window.resets_at`, the cached snapshot is stale: returns a
/// synthesized value whose `used` is `used_since_reset` (capped at `limit`) and
/// whose `resets_at` advances by `window_duration`. Otherwise returns the
/// cached values with `synthesized=false`. Never panics.
pub fn synthesize_window_render(
    window: Option<&RateLimitWindow>,
    used_since_reset: Option<i64>,
    now: Option<DateTime<Utc>>,
    window_duration: Duration,
) -> Value {
    let window = match window {
        Some(w) => w,
        None => {
            return json!({
                "used": 0,
                "limit": 0,
                "utilization_pct": 0.0,
                "resets_at": Value::Null,
                "seconds_to_reset": Value::Null,
                "synthesized": false,
                "resets_at_estimated": false,
            });
        }
    };

    let mut cached = window.to_value();
    if let Value::Object(ref mut m) = cached {
        m.insert("synthesized".into(), json!(false));
        m.insert("resets_at_estimated".into(), json!(false));
    }

    let resets_at = match window.resets_at {
        Some(r) => r,
        None => return cached,
    };

    let current_now = now.unwrap_or_else(utc_now);
    if current_now < resets_at {
        return cached;
    }

    // Past the reset boundary — synthesize.
    let limit = window.limit.max(0);
    let used_local = used_since_reset.unwrap_or(0).max(0);
    let capped_used = if limit > 0 {
        used_local.min(limit)
    } else {
        used_local
    };
    let utilization_pct = if limit > 0 {
        capped_used as f64 / limit as f64 * 100.0
    } else {
        0.0
    };

    let mut next_reset = resets_at;
    while next_reset <= current_now {
        next_reset += window_duration;
    }
    let seconds_to_reset = ((next_reset - current_now).num_milliseconds() as f64 / 1000.0).max(0.0);

    json!({
        "used": capped_used,
        "limit": limit,
        "utilization_pct": round_half_even(utilization_pct, 2),
        "resets_at": to_utc_iso(&next_reset),
        "seconds_to_reset": seconds_to_reset,
        "synthesized": true,
        "resets_at_estimated": true,
    })
}

// ---------------------------------------------------------------------------
// ExtraUsage
// ---------------------------------------------------------------------------

/// Overage / extra-usage block. Cents as returned by the API.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtraUsage {
    pub is_enabled: bool,
    pub monthly_limit_cents: Option<i64>,
    pub used_credits_cents: Option<i64>,
    pub utilization_pct: Option<f64>,
}

impl ExtraUsage {
    pub fn from_api_dict(data: &Value) -> Self {
        Self {
            is_enabled: data
                .get("is_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            monthly_limit_cents: get_i64(data, "monthly_limit"),
            used_credits_cents: get_i64(data, "used_credits"),
            utilization_pct: get_f64(data, "utilization"),
        }
    }

    pub fn monthly_limit_usd(&self) -> Option<f64> {
        self.monthly_limit_cents.map(|c| c as f64 / 100.0)
    }

    pub fn used_credits_usd(&self) -> Option<f64> {
        self.used_credits_cents.map(|c| c as f64 / 100.0)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "is_enabled": self.is_enabled,
            "monthly_limit_usd": self.monthly_limit_usd().map(|v| round_half_even(v, 2)),
            "used_credits_usd": self.used_credits_usd().map(|v| round_half_even(v, 4)),
            "utilization_pct": self.utilization_pct.map(|v| round_half_even(v, 2)),
        })
    }
}

// ---------------------------------------------------------------------------
// SubscriptionSnapshot
// ---------------------------------------------------------------------------

/// One complete poll of `GET /api/oauth/usage`.
#[derive(Debug, Clone)]
pub struct SubscriptionSnapshot {
    pub five_hour: RateLimitWindow,
    pub seven_day: RateLimitWindow,
    pub seven_day_opus: Option<RateLimitWindow>,
    pub seven_day_sonnet: Option<RateLimitWindow>,
    pub extra_usage: ExtraUsage,
    pub polled_at: DateTime<Utc>,
    /// First 8 chars of the OAuth token (for multi-account detection).
    pub token_prefix: String,
}

impl Default for SubscriptionSnapshot {
    fn default() -> Self {
        Self {
            five_hour: RateLimitWindow::default(),
            seven_day: RateLimitWindow::default(),
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: ExtraUsage::default(),
            polled_at: utc_now(),
            token_prefix: String::new(),
        }
    }
}

fn nonempty_window(data: &Value, key: &str) -> Option<RateLimitWindow> {
    let inner = data.get(key)?;
    // Python guards `if key in data and data[key]` — treat null / empty / false
    // as absent.
    let present = match inner {
        Value::Null => false,
        Value::Object(m) => !m.is_empty(),
        Value::Bool(b) => *b,
        _ => true,
    };
    if present {
        Some(RateLimitWindow::from_api_dict(inner))
    } else {
        None
    }
}

impl SubscriptionSnapshot {
    pub fn from_api_response(data: &Value, token: &str) -> Self {
        let mut snap = Self {
            token_prefix: token.chars().take(8).collect(),
            ..Default::default()
        };
        if let Some(w) = nonempty_window(data, "five_hour") {
            snap.five_hour = w;
        }
        if let Some(w) = nonempty_window(data, "seven_day") {
            snap.seven_day = w;
        }
        snap.seven_day_opus = nonempty_window(data, "seven_day_opus");
        snap.seven_day_sonnet = nonempty_window(data, "seven_day_sonnet");
        if let Some(extra) = data.get("extra_usage") {
            let present = match extra {
                Value::Null => false,
                Value::Object(m) => !m.is_empty(),
                _ => true,
            };
            if present {
                snap.extra_usage = ExtraUsage::from_api_dict(extra);
            }
        }
        snap
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("five_hour".into(), self.five_hour.to_value());
        m.insert("seven_day".into(), self.seven_day.to_value());
        m.insert("extra_usage".into(), self.extra_usage.to_value());
        m.insert("polled_at".into(), json!(to_utc_iso(&self.polled_at)));
        m.insert("token_prefix".into(), json!(self.token_prefix));
        if let Some(ref w) = self.seven_day_opus {
            m.insert("seven_day_opus".into(), w.to_value());
        }
        if let Some(ref w) = self.seven_day_sonnet {
            m.insert("seven_day_sonnet".into(), w.to_value());
        }
        Value::Object(m)
    }
}

// ---------------------------------------------------------------------------
// WindowTokens
// ---------------------------------------------------------------------------

/// Token breakdown from Claude transcript JSONL files for one time window.
#[derive(Debug, Clone, Default)]
pub struct WindowTokens {
    pub input: i64,
    pub output: i64,
    pub cache_reads: i64,
    pub cache_writes_5m: i64,
    pub cache_writes_1h: i64,
    pub cache_writes_total: i64,
    /// Per-model breakdown (model id -> {input, output, ...}).
    pub by_model: Map<String, Value>,
    /// Sonnet-normalised weighted total (opus×2, sonnet×1, haiku×0.5).
    pub weighted_token_equivalent: f64,
}

impl WindowTokens {
    pub fn total_raw(&self) -> i64 {
        self.input + self.output + self.cache_reads + self.cache_writes_total
    }

    pub fn to_value(&self) -> Value {
        json!({
            "input": self.input,
            "output": self.output,
            "cache_reads": self.cache_reads,
            "cache_writes_5m": self.cache_writes_5m,
            "cache_writes_1h": self.cache_writes_1h,
            "cache_writes_total": self.cache_writes_total,
            "total_raw": self.total_raw(),
            "weighted_token_equivalent": round_half_even(self.weighted_token_equivalent, 1),
            "by_model": Value::Object(self.by_model.clone()),
        })
    }
}

// ---------------------------------------------------------------------------
// HeadroomContribution
// ---------------------------------------------------------------------------

/// Tokens conserved within the current 5h window by Headroom's layers.
#[derive(Debug, Clone, Default)]
pub struct HeadroomContribution {
    pub tokens_submitted: i64,
    pub tokens_saved_compression: i64,
    pub tokens_saved_cli_filtering: i64,
    /// Deprecated alias for CLI filtering tokens from older persisted state.
    pub tokens_saved_rtk: i64,
    pub tokens_saved_cache_reads: i64,
    pub compression_savings_usd: f64,
    pub cache_savings_usd: f64,
}

impl HeadroomContribution {
    pub fn cli_filtering_saved(&self) -> i64 {
        self.tokens_saved_cli_filtering.max(self.tokens_saved_rtk)
    }

    pub fn total_saved(&self) -> i64 {
        self.tokens_saved_compression + self.cli_filtering_saved() + self.tokens_saved_cache_reads
    }

    pub fn compression_saved(&self) -> i64 {
        self.tokens_saved_compression + self.cli_filtering_saved()
    }

    pub fn total_savings_usd(&self) -> f64 {
        self.compression_savings_usd + self.cache_savings_usd
    }

    pub fn raw_without_headroom(&self) -> i64 {
        self.tokens_submitted + self.tokens_saved_compression + self.cli_filtering_saved()
    }

    pub fn efficiency_pct(&self) -> f64 {
        let raw = self.raw_without_headroom();
        if raw == 0 {
            return 0.0;
        }
        round_half_even(self.total_saved() as f64 / raw as f64 * 100.0, 1)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "tokens_submitted": self.tokens_submitted,
            "tokens_saved": {
                "compression": self.compression_saved(),
                "proxy_compression": self.tokens_saved_compression,
                "cli_filtering": self.cli_filtering_saved(),
                "rtk": self.cli_filtering_saved(),
                "cli_filtering_raw": self.tokens_saved_cli_filtering,
                "rtk_raw": self.tokens_saved_rtk,
                "cache_reads": self.tokens_saved_cache_reads,
                "total": self.total_saved(),
            },
            "raw_without_headroom": self.raw_without_headroom(),
            "efficiency_pct": self.efficiency_pct(),
            "savings_usd": {
                "compression": round_half_even(self.compression_savings_usd, 4),
                "cache": round_half_even(self.cache_savings_usd, 4),
                "total": round_half_even(self.total_savings_usd(), 4),
            },
        })
    }
}

// ---------------------------------------------------------------------------
// WindowDiscrepancy
// ---------------------------------------------------------------------------

/// Detected anomaly between expected and API-reported utilization.
#[derive(Debug, Clone)]
pub struct WindowDiscrepancy {
    /// `surge_pricing` | `cache_miss` | `none`
    pub kind: String,
    pub description: String,
    /// `info` | `warning` | `alert`
    pub severity: String,
    pub expected_utilization_pct: Option<f64>,
    pub actual_utilization_pct: Option<f64>,
    pub delta_pct: Option<f64>,
}

impl Default for WindowDiscrepancy {
    fn default() -> Self {
        Self {
            kind: String::new(),
            description: String::new(),
            severity: "info".into(),
            expected_utilization_pct: None,
            actual_utilization_pct: None,
            delta_pct: None,
        }
    }
}

impl WindowDiscrepancy {
    pub fn to_value(&self) -> Value {
        json!({
            "kind": self.kind,
            "description": self.description,
            "severity": self.severity,
            "expected_utilization_pct": self.expected_utilization_pct,
            "actual_utilization_pct": self.actual_utilization_pct,
            "delta_pct": self.delta_pct,
        })
    }
}

// ---------------------------------------------------------------------------
// SubscriptionState
// ---------------------------------------------------------------------------

const MAX_HISTORY: usize = 100;
const MAX_DISCREPANCIES: usize = 20;

/// Persistent state for the subscription tracker.
#[derive(Debug, Clone, Default)]
pub struct SubscriptionState {
    pub latest: Option<SubscriptionSnapshot>,
    pub window_tokens: Option<WindowTokens>,
    pub contribution: HeadroomContribution,
    pub discrepancies: Vec<WindowDiscrepancy>,
    pub history: Vec<SubscriptionSnapshot>,
    pub poll_count: i64,
    pub poll_errors: i64,
    pub last_error: Option<String>,
    pub last_active_at: Option<DateTime<Utc>>,
}

impl SubscriptionState {
    pub fn add_snapshot(&mut self, snapshot: SubscriptionSnapshot) {
        self.latest = Some(snapshot.clone());
        self.history.push(snapshot);
        if self.history.len() > MAX_HISTORY {
            let start = self.history.len() - MAX_HISTORY;
            self.history.drain(0..start);
        }
        self.poll_count += 1;
    }

    pub fn mark_error(&mut self, msg: &str) {
        self.poll_errors += 1;
        self.last_error = Some(msg.to_string());
    }

    pub fn add_discrepancy(&mut self, d: WindowDiscrepancy) {
        self.discrepancies.push(d);
        if self.discrepancies.len() > MAX_DISCREPANCIES {
            let start = self.discrepancies.len() - MAX_DISCREPANCIES;
            self.discrepancies.drain(0..start);
        }
    }

    pub fn is_active(&self, active_window_s: f64) -> bool {
        match self.last_active_at {
            None => false,
            Some(t) => {
                let elapsed = (utc_now() - t).num_milliseconds() as f64 / 1000.0;
                elapsed <= active_window_s
            }
        }
    }

    pub fn to_value(&self) -> Value {
        let last5: Vec<Value> = self
            .discrepancies
            .iter()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|d| d.to_value())
            .collect();
        json!({
            "latest": self.latest.as_ref().map(|s| s.to_value()),
            "window_tokens": self.window_tokens.as_ref().map(|w| w.to_value()),
            "contribution": self.contribution.to_value(),
            "discrepancies": last5,
            "poll_count": self.poll_count,
            "poll_errors": self.poll_errors,
            "last_error": self.last_error,
            "last_active_at": self.last_active_at.map(|d| to_utc_iso(&d)),
        })
    }

    pub fn to_persist_value(&self) -> Value {
        let mut base = self.to_value();
        let history: Vec<Value> = self
            .history
            .iter()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|s| s.to_value())
            .collect();
        if let Value::Object(ref mut m) = base {
            m.insert("history".into(), Value::Array(history));
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_window_from_api_and_to_value() {
        let data = json!({
            "used": 44000,
            "limit": 100000,
            "utilization": 44.0,
            "resets_at": "2026-06-17T12:00:00Z",
        });
        let w = RateLimitWindow::from_api_dict(&data);
        assert_eq!(w.used, 44000);
        assert_eq!(w.limit, 100000);
        assert_eq!(w.utilization_pct, 44.0);
        let v = w.to_value();
        assert_eq!(v["resets_at"], json!("2026-06-17T12:00:00Z"));
    }

    #[test]
    fn snapshot_omits_optional_windows() {
        let data = json!({
            "five_hour": {"used": 1, "limit": 2, "utilization": 50.0},
            "seven_day": {"used": 3, "limit": 4, "utilization": 75.0},
        });
        let snap = SubscriptionSnapshot::from_api_response(&data, "tok12345678");
        assert_eq!(snap.token_prefix, "tok12345");
        let v = snap.to_value();
        assert!(v.get("seven_day_opus").is_none());
        assert_eq!(v["five_hour"]["used"], json!(1));
    }

    #[test]
    fn contribution_uses_max_of_cli_and_rtk() {
        let c = HeadroomContribution {
            tokens_saved_cli_filtering: 30,
            tokens_saved_rtk: 50,
            ..Default::default()
        };
        assert_eq!(c.cli_filtering_saved(), 50);
        let v = c.to_value();
        assert_eq!(v["tokens_saved"]["cli_filtering_raw"], json!(30));
        assert_eq!(v["tokens_saved"]["rtk_raw"], json!(50));
        assert_eq!(v["tokens_saved"]["cli_filtering"], json!(50));
    }

    #[test]
    fn synthesize_within_window_returns_cached() {
        let resets_at = utc_now() + Duration::minutes(30);
        let w = RateLimitWindow {
            used: 44000,
            limit: 100000,
            utilization_pct: 44.0,
            resets_at: Some(resets_at),
        };
        let v = synthesize_window_render(Some(&w), None, None, Duration::hours(5));
        assert_eq!(v["synthesized"], json!(false));
        assert_eq!(v["utilization_pct"], json!(44.0));
        assert_eq!(v["used"], json!(44000));
    }

    #[test]
    fn synthesize_after_reset_caps_at_limit() {
        let resets_at = utc_now() - Duration::minutes(5);
        let w = RateLimitWindow {
            used: 44000,
            limit: 100000,
            utilization_pct: 44.0,
            resets_at: Some(resets_at),
        };
        let v = synthesize_window_render(Some(&w), Some(999_999), None, Duration::hours(5));
        assert_eq!(v["synthesized"], json!(true));
        assert_eq!(v["used"], json!(100000));
        assert_eq!(v["utilization_pct"], json!(100.0));
    }

    #[test]
    fn synthesize_missing_resets_at_passthrough() {
        let w = RateLimitWindow {
            used: 10000,
            limit: 100000,
            utilization_pct: 10.0,
            resets_at: None,
        };
        let v = synthesize_window_render(Some(&w), Some(12345), None, Duration::hours(5));
        assert_eq!(v["synthesized"], json!(false));
        assert_eq!(v["used"], json!(10000));
    }
}

//! Pure subscription-window tracker state machine (port of the logic in
//! `headroom/subscription/tracker.py`).
//!
//! This carries the tracker's *pure* behaviour: state, persistence,
//! reconciliation from a fetched snapshot, surge/cache-miss detection,
//! headroom-contribution accounting, 5h-window rollover reset, and post-reset
//! render synthesis. The async poll loop, the reqwest-backed fetcher, and the
//! on-demand-poll wiring live in `headroom-proxy` and drive [`SubscriptionTracker::poll_once`].
//!
//! Deviations from Python, deliberate for the Rust split:
//! * The RTK subprocess polling (`_poll_rtk_delta` → `headroom.proxy.helpers`)
//!   and its multi-worker `fcntl` poll-lock election are **not** ported — they
//!   depend on the Python proxy helper layer. The pure delta bookkeeping is
//!   preserved as [`SubscriptionTracker::apply_rtk_session_total`], so a caller
//!   that has RTK stats can still feed session-incremental deltas in.
//! * `update_contribution` takes explicit deltas only (no implicit RTK poll).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Duration;
use serde_json::Value;

use super::client::SubscriptionFetcher;
use super::models::{
    synthesize_window_render, HeadroomContribution, RateLimitWindow, SubscriptionSnapshot,
    SubscriptionState, WindowDiscrepancy, WindowTokens,
};
use super::{session_tracking, utc_now};
use crate::subscription::base::QuotaTracker;

pub const DEFAULT_POLL_INTERVAL_S: u64 = 300;
pub const DEFAULT_ACTIVE_WINDOW_S: f64 = 60.0;
pub const DEFAULT_ON_DEMAND_POLL_FLOOR_S: f64 = 60.0;

fn five_hour_window() -> Duration {
    Duration::hours(5)
}
fn seven_day_window() -> Duration {
    Duration::days(7)
}
/// Only a forward jump larger than this counts as a real 5h rollover (the API
/// reports `resets_at` with sub-second jitter within the same window).
fn rollover_min_advance() -> Duration {
    Duration::minutes(1)
}
const SURGE_THRESHOLD_PCT: f64 = 15.0;
const CACHE_MISS_RATIO_THRESHOLD: f64 = 0.10;

#[derive(Default)]
struct Inner {
    state: SubscriptionState,
    /// PR-F3: one-way hash + last-4 of the most recent OAuth bearer, for
    /// debugging only. The raw token is never stored (token-leak hardening);
    /// polling reads the credentials file / env var at poll time instead.
    current_token_id: Option<String>,
    full_tokens: std::collections::HashMap<String, i64>,
    last_rtk_tokens_saved: i64,
    last_on_demand_poll: f64,
}

/// Background tracker for Anthropic Claude Code subscription windows.
pub struct SubscriptionTracker {
    enabled: bool,
    poll_interval_s: u64,
    active_window_s: f64,
    on_demand_poll_floor_s: f64,
    persist_path: PathBuf,
    fetcher: Box<dyn SubscriptionFetcher>,
    inner: Mutex<Inner>,
}

/// Per-call contribution deltas fed to [`SubscriptionTracker::update_contribution`].
/// `None` means "caller omitted; treat as 0" — an explicit `Some(0)` is honored
/// verbatim (mirrors Python's `None`-guard semantics).
#[derive(Default)]
pub struct ContributionDelta {
    pub tokens_submitted: i64,
    pub tokens_saved_compression: i64,
    pub tokens_saved_cli_filtering: Option<i64>,
    pub tokens_saved_rtk: Option<i64>,
    pub tokens_saved_cache_reads: i64,
    pub compression_savings_usd: f64,
    pub cache_savings_usd: f64,
}

impl SubscriptionTracker {
    pub fn new(
        fetcher: Box<dyn SubscriptionFetcher>,
        enabled: bool,
        poll_interval_s: u64,
        active_window_s: f64,
        persist_path: Option<PathBuf>,
    ) -> Self {
        let persist_path =
            persist_path.unwrap_or_else(|| crate::paths::subscription_state_path(None));
        let tracker = Self {
            enabled,
            poll_interval_s: poll_interval_s.clamp(1, 3600),
            active_window_s: active_window_s.max(5.0),
            on_demand_poll_floor_s: DEFAULT_ON_DEMAND_POLL_FLOOR_S,
            persist_path,
            fetcher,
            inner: Mutex::new(Inner::default()),
        };
        tracker.load_persisted_state();
        tracker
    }

    pub fn poll_interval_s(&self) -> u64 {
        self.poll_interval_s
    }

    pub fn persist_path(&self) -> &Path {
        &self.persist_path
    }

    // ------------------------------------------------------------------
    // Proxy integration hooks
    // ------------------------------------------------------------------

    /// Called by the proxy when an OAuth request comes through. Marks the
    /// tracker recently active. Ignores API keys.
    ///
    /// PR-F3: only a one-way hash + last-4 of the token is retained (for
    /// debugging); the raw bearer never persists past this call.
    pub fn notify_active(&self, token: &str) {
        let raw = match token.strip_prefix("Bearer ") {
            Some(r) => r,
            None => return,
        };
        if raw.starts_with("sk-ant-api") {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.current_token_id = Some(token_id(raw));
        inner.state.last_active_at = Some(utc_now());
        let prefix: String = raw.chars().take(8).collect();
        *inner.full_tokens.entry(prefix).or_insert(0) += 1;
    }

    /// Update headroom contribution counters for the current session window.
    pub fn update_contribution(&self, delta: ContributionDelta) {
        let mut inner = self.inner.lock().unwrap();
        let c = &mut inner.state.contribution;
        let cli_filtering = delta.tokens_saved_cli_filtering.unwrap_or(0);
        let rtk = delta.tokens_saved_rtk.unwrap_or(0);
        c.tokens_submitted += delta.tokens_submitted.max(0);
        c.tokens_saved_compression += delta.tokens_saved_compression.max(0);
        c.tokens_saved_cli_filtering += cli_filtering.max(0);
        c.tokens_saved_rtk += rtk.max(0);
        c.tokens_saved_cache_reads += delta.tokens_saved_cache_reads.max(0);
        c.compression_savings_usd += delta.compression_savings_usd.max(0.0);
        c.cache_savings_usd += delta.cache_savings_usd.max(0.0);
    }

    /// Pure port of `_poll_rtk_delta`'s bookkeeping: given the current
    /// session-incremental RTK `tokens_saved` total, return the non-negative
    /// delta since the last observed total and advance the baseline. A counter
    /// regression (session reset / RTK DB rebuild) re-baselines and returns 0.
    pub fn apply_rtk_session_total(&self, current_total: i64) -> i64 {
        let mut inner = self.inner.lock().unwrap();
        let last = inner.last_rtk_tokens_saved;
        if current_total < last {
            inner.last_rtk_tokens_saved = current_total;
            return 0;
        }
        let delta = current_total - last;
        if delta > 0 {
            inner.last_rtk_tokens_saved = current_total;
        }
        delta
    }

    // ------------------------------------------------------------------
    // State access
    // ------------------------------------------------------------------

    pub fn state(&self) -> Value {
        self.inner.lock().unwrap().state.to_value()
    }

    pub fn latest_snapshot(&self) -> Option<SubscriptionSnapshot> {
        self.inner.lock().unwrap().state.latest.clone()
    }

    pub fn is_active(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .state
            .is_active(self.active_window_s)
    }

    /// Token to poll with, read from `$CLAUDE_CODE_OAUTH_TOKEN` or the
    /// credentials file at poll time. `None` when neither is available.
    ///
    /// PR-F3: the tracker no longer keeps a raw in-memory copy of the proxied
    /// bearer, so polling always goes to the source of truth on disk.
    pub fn poll_token(&self) -> Option<String> {
        super::client::read_cached_oauth_token()
    }

    /// PR-F3: hashed identifier of the last proxied OAuth token (never the
    /// raw bearer). For debugging/telemetry only.
    pub fn current_token_id(&self) -> Option<String> {
        self.inner.lock().unwrap().current_token_id.clone()
    }

    // ------------------------------------------------------------------
    // Display-time rendering
    // ------------------------------------------------------------------

    /// Dashboard-facing state dict, synthesizing post-reset windows.
    pub fn render_state(&self) -> Value {
        let (mut base, snapshot) = {
            let inner = self.inner.lock().unwrap();
            (inner.state.to_value(), inner.state.latest.clone())
        };
        let snapshot = match snapshot {
            Some(s) => s,
            None => return base,
        };
        if base.get("latest").map(|v| v.is_null()).unwrap_or(true) {
            return base;
        }

        let latest = base.get_mut("latest").unwrap();
        latest["five_hour"] = self.render_window(Some(&snapshot.five_hour), five_hour_window());
        latest["seven_day"] = self.render_window(Some(&snapshot.seven_day), seven_day_window());
        if let Some(ref w) = snapshot.seven_day_opus {
            latest["seven_day_opus"] = self.render_window(Some(w), seven_day_window());
        }
        if let Some(ref w) = snapshot.seven_day_sonnet {
            latest["seven_day_sonnet"] = self.render_window(Some(w), seven_day_window());
        }
        base
    }

    fn render_window(&self, window: Option<&RateLimitWindow>, dur: Duration) -> Value {
        let used_since_reset = self.compute_used_since_reset(window);
        synthesize_window_render(window, used_since_reset, None, dur)
    }

    /// Read transcripts to count tokens spent strictly after `window.resets_at`.
    fn compute_used_since_reset(&self, window: Option<&RateLimitWindow>) -> Option<i64> {
        let window = window?;
        let resets_at = window.resets_at?;
        let now = utc_now();
        if now < resets_at {
            return None;
        }
        let tokens = session_tracking::compute_window_tokens(
            resets_at.timestamp() as f64,
            now.timestamp() as f64,
        );
        let weighted = tokens.weighted_token_equivalent as i64;
        Some(if weighted != 0 {
            weighted
        } else {
            tokens.total_raw()
        })
    }

    /// On-demand poll gate: returns `true` at most once per floor window and
    /// records the trigger time. Mirrors the floor logic of
    /// `maybe_poll_on_demand` (the actual poll is the caller's job).
    pub fn should_poll_on_demand(&self, now_ts: f64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let elapsed = now_ts - inner.last_on_demand_poll;
        if elapsed < self.on_demand_poll_floor_s {
            return false;
        }
        inner.last_on_demand_poll = now_ts;
        true
    }

    // ------------------------------------------------------------------
    // Poll (pure — caller supplies the async loop)
    // ------------------------------------------------------------------

    /// Perform one reconciliation cycle: fetch a snapshot, compute window
    /// tokens, detect anomalies, update state, and persist. Returns `true` when
    /// a snapshot was recorded.
    pub fn poll_once(&self) -> bool {
        let token = match self.poll_token() {
            Some(t) => t,
            None => return false,
        };

        let snapshot = match self.fetcher.fetch(Some(&token)) {
            Some(s) => s,
            None => {
                let mut inner = self.inner.lock().unwrap();
                inner.state.mark_error("fetch returned None");
                return false;
            }
        };

        let window_tokens = compute_window_tokens_for_snapshot(&snapshot);
        let discrepancies = detect_discrepancies(&snapshot, &window_tokens);

        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.add_snapshot(snapshot);
            inner.state.window_tokens = Some(window_tokens);
            for d in discrepancies {
                inner.state.add_discrepancy(d);
            }
            inner.state.last_error = None;
            maybe_reset_contribution(&mut inner.state);
        }

        self.persist_state();
        true
    }

    // ------------------------------------------------------------------
    // Persistence
    // ------------------------------------------------------------------

    pub fn persist_state(&self) {
        let data = {
            let inner = self.inner.lock().unwrap();
            inner.state.to_persist_value()
        };
        let _ = self.persist_state_inner(&data);
    }

    fn persist_state_inner(&self, data: &Value) -> std::io::Result<()> {
        if let Some(parent) = self.persist_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self
            .persist_path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(data)?)?;
        std::fs::rename(&tmp, &self.persist_path)?;
        Ok(())
    }

    fn load_persisted_state(&self) {
        let content = match std::fs::read_to_string(&self.persist_path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let raw: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return,
        };

        let contrib = raw.get("contribution").cloned().unwrap_or(Value::Null);
        let saved = contrib.get("tokens_saved").cloned().unwrap_or(Value::Null);
        let get = |v: &Value, k: &str| -> i64 {
            v.get(k)
                .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
                .unwrap_or(0)
        };
        let getf = |v: &Value, k: &str| -> f64 { v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) };

        let mut inner = self.inner.lock().unwrap();
        let c = &mut inner.state.contribution;
        c.tokens_submitted = get(&contrib, "tokens_submitted");
        // Prefer the raw proxy field so loading doesn't double-count CLI filtering.
        c.tokens_saved_compression = saved
            .get("proxy_compression")
            .or_else(|| saved.get("compression"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let cli_filtering = saved
            .get("cli_filtering_raw")
            .or_else(|| saved.get("cli_filtering"))
            .or_else(|| saved.get("rtk"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        c.tokens_saved_cli_filtering = cli_filtering;
        // Legacy state (no rtk_raw): pre-G2 `rtk` mirrored `cli_filtering`;
        // carry it forward rather than zeroing accumulated history.
        let is_legacy = saved.get("rtk_raw").is_none();
        c.tokens_saved_rtk = if is_legacy {
            saved
                .get("rtk")
                .and_then(|x| x.as_i64())
                .unwrap_or(cli_filtering)
        } else {
            get(&saved, "rtk_raw")
        };
        c.tokens_saved_cache_reads = get(&saved, "cache_reads");
        let savings_usd = contrib.get("savings_usd").cloned().unwrap_or(Value::Null);
        c.compression_savings_usd = getf(&savings_usd, "compression");
        c.cache_savings_usd = getf(&savings_usd, "cache");
        inner.state.poll_count = get(&raw, "poll_count");
    }
}

impl QuotaTracker for SubscriptionTracker {
    fn key(&self) -> &str {
        "subscription_window"
    }
    fn label(&self) -> &str {
        "Anthropic Claude Code"
    }
    fn is_available(&self) -> bool {
        self.enabled
    }
    fn get_stats(&self) -> Option<Value> {
        Some(self.state())
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// PR-F3: one-way token identifier — `sha256:<16 hex>…<last 4 chars>`. Enough
/// to correlate "same token?" across log lines and eyeball the tail against a
/// credentials file, while unrecoverable as a bearer.
fn token_id(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let last4: String = raw
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("sha256:{hex}…{last4}")
}

/// Reset contribution counters when the 5h window rolls over (a forward jump in
/// `five_hour.resets_at` larger than the jitter threshold).
fn maybe_reset_contribution(state: &mut SubscriptionState) {
    let n = state.history.len();
    if n < 2 {
        return;
    }
    let prev_resets = state.history[n - 2].five_hour.resets_at;
    let curr_resets = state.history[n - 1].five_hour.resets_at;
    if let (Some(prev), Some(curr)) = (prev_resets, curr_resets) {
        if curr - prev > rollover_min_advance() {
            state.contribution = HeadroomContribution::default();
        }
    }
}

/// Read Claude transcript files and sum tokens for the current 5h window.
pub fn compute_window_tokens_for_snapshot(snapshot: &SubscriptionSnapshot) -> WindowTokens {
    let resets_at = match snapshot.five_hour.resets_at {
        Some(r) => r,
        None => return WindowTokens::default(),
    };
    let window_duration_s = 5.0 * 3600.0;
    let end_ts = resets_at.timestamp() as f64;
    let start_ts = end_ts - window_duration_s;
    session_tracking::compute_window_tokens(start_ts, end_ts)
}

/// Detect surge-pricing or cache-miss anomalies in the snapshot.
pub fn detect_discrepancies(
    snapshot: &SubscriptionSnapshot,
    window_tokens: &WindowTokens,
) -> Vec<WindowDiscrepancy> {
    let mut out = Vec::new();

    if snapshot.five_hour.limit > 0 && window_tokens.weighted_token_equivalent > 0.0 {
        let expected_pct =
            window_tokens.weighted_token_equivalent / snapshot.five_hour.limit as f64 * 100.0;
        let actual_pct = snapshot.five_hour.utilization_pct;
        let delta = actual_pct - expected_pct;
        if delta > SURGE_THRESHOLD_PCT {
            out.push(WindowDiscrepancy {
                kind: "surge_pricing".into(),
                description: format!(
                    "API 5h utilization ({actual_pct:.1}%) is {delta:.1}% higher than \
                     transcript-implied ({expected_pct:.1}%); possible surge weighting."
                ),
                severity: if delta < 30.0 { "warning" } else { "alert" }.into(),
                expected_utilization_pct: Some(super::round_half_even(expected_pct, 2)),
                actual_utilization_pct: Some(super::round_half_even(actual_pct, 2)),
                delta_pct: Some(super::round_half_even(delta, 2)),
            });
        }
    }

    let total_input = window_tokens.input;
    let total_cache_reads = window_tokens.cache_reads;
    if total_input > 50_000
        && (total_cache_reads as f64) < total_input as f64 * CACHE_MISS_RATIO_THRESHOLD
    {
        let cache_ratio = if total_input != 0 {
            total_cache_reads as f64 / total_input as f64
        } else {
            0.0
        };
        out.push(WindowDiscrepancy {
            kind: "cache_miss".into(),
            description: format!(
                "Cache-read ratio is {:.1}% (threshold {:.0}%); system may not be \
                 using prefix cache effectively.",
                cache_ratio * 100.0,
                CACHE_MISS_RATIO_THRESHOLD * 100.0
            ),
            severity: "warning".into(),
            ..Default::default()
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use serde_json::json;

    struct StubFetcher(Option<SubscriptionSnapshot>);
    impl SubscriptionFetcher for StubFetcher {
        fn fetch(&self, _token: Option<&str>) -> Option<SubscriptionSnapshot> {
            self.0.clone()
        }
    }

    fn tracker_with(snapshot: Option<SubscriptionSnapshot>) -> SubscriptionTracker {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        // leak the tempdir so the path stays valid for the test lifetime
        std::mem::forget(dir);
        SubscriptionTracker::new(Box::new(StubFetcher(snapshot)), true, 300, 60.0, Some(path))
    }

    fn window(
        used: i64,
        limit: i64,
        pct: f64,
        resets_at: Option<DateTime<Utc>>,
    ) -> RateLimitWindow {
        RateLimitWindow {
            used,
            limit,
            utilization_pct: pct,
            resets_at,
        }
    }

    #[test]
    fn render_within_window_returns_cached() {
        let tracker = tracker_with(None);
        let resets = utc_now() + Duration::minutes(30);
        let snap = SubscriptionSnapshot {
            five_hour: window(44_000, 100_000, 44.0, Some(resets)),
            ..Default::default()
        };
        {
            let mut inner = tracker.inner.lock().unwrap();
            inner.state.latest = Some(snap.clone());
            inner.state.history.push(snap);
        }
        let rendered = tracker.render_state();
        let fh = &rendered["latest"]["five_hour"];
        assert_eq!(fh["synthesized"], json!(false));
        assert_eq!(fh["utilization_pct"], json!(44.0));
        assert_eq!(fh["used"], json!(44_000));
    }

    #[test]
    fn notify_active_ignores_api_keys() {
        let tracker = tracker_with(None);
        tracker.notify_active("Bearer sk-ant-api-xyz");
        assert!(!tracker.is_active());
        tracker.notify_active("Bearer oauth-token-abc");
        assert!(tracker.is_active());
    }

    #[test]
    fn raw_token_not_stored_in_memory() {
        // PR-F3: the tracker keeps only a one-way hash + last-4 of the bearer.
        let tracker = tracker_with(None);
        tracker.notify_active("Bearer super-secret-oauth-bearer-wxyz");
        let id = tracker.current_token_id().expect("token id recorded");
        assert!(id.starts_with("sha256:"), "id is hash-scoped: {id}");
        assert!(
            id.ends_with("wxyz"),
            "id keeps the last-4 for debugging: {id}"
        );
        assert!(
            !id.contains("super-secret") && !id.contains("oauth-bearer"),
            "raw token material leaked into the id: {id}"
        );
    }

    #[test]
    fn update_contribution_honors_explicit_zero() {
        let tracker = tracker_with(None);
        tracker.update_contribution(ContributionDelta {
            tokens_saved_compression: 100,
            tokens_saved_cli_filtering: Some(0),
            tokens_saved_rtk: Some(50),
            ..Default::default()
        });
        let state = tracker.state();
        let ts = &state["contribution"]["tokens_saved"];
        assert_eq!(ts["proxy_compression"], json!(100));
        assert_eq!(ts["cli_filtering_raw"], json!(0));
        assert_eq!(ts["rtk_raw"], json!(50));
    }

    #[test]
    fn rtk_delta_bookkeeping() {
        let tracker = tracker_with(None);
        assert_eq!(tracker.apply_rtk_session_total(100), 100);
        assert_eq!(tracker.apply_rtk_session_total(150), 50);
        // regression re-baselines and returns 0
        assert_eq!(tracker.apply_rtk_session_total(40), 0);
        assert_eq!(tracker.apply_rtk_session_total(60), 20);
    }

    #[test]
    fn poll_once_records_snapshot_and_persists() {
        let resets = utc_now() + Duration::hours(4);
        let snap = SubscriptionSnapshot {
            five_hour: window(10, 100, 10.0, Some(resets)),
            ..Default::default()
        };
        let tracker = tracker_with(Some(snap));
        // PR-F3: polling reads the token from the environment/credentials
        // file, never from tracker memory.
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "oauth-abc");
        assert!(tracker.poll_once());
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        let state = tracker.state();
        assert_eq!(state["poll_count"], json!(1));
        // persisted file exists and round-trips poll_count
        let persisted = std::fs::read_to_string(tracker.persist_path()).unwrap();
        assert!(persisted.contains("\"poll_count\""));
    }

    #[test]
    fn detect_surge_pricing() {
        let snap = SubscriptionSnapshot {
            five_hour: window(0, 1000, 80.0, None),
            ..Default::default()
        };
        let wt = WindowTokens {
            weighted_token_equivalent: 500.0, // expected 50%, actual 80% -> +30 -> alert
            ..Default::default()
        };
        let d = detect_discrepancies(&snap, &wt);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "surge_pricing");
        assert_eq!(d[0].severity, "alert");
    }

    #[test]
    fn load_persisted_contribution_legacy_rtk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        // legacy state: rtk mirrors cli_filtering, no rtk_raw
        std::fs::write(
            &path,
            json!({
                "poll_count": 7,
                "contribution": {
                    "tokens_submitted": 10,
                    "tokens_saved": {"compression": 200, "cli_filtering": 80, "rtk": 80, "cache_reads": 5},
                    "savings_usd": {"compression": 1.5, "cache": 0.5}
                }
            })
            .to_string(),
        )
        .unwrap();
        let tracker =
            SubscriptionTracker::new(Box::new(StubFetcher(None)), true, 300, 60.0, Some(path));
        let state = tracker.state();
        assert_eq!(state["poll_count"], json!(7));
        let ts = &state["contribution"]["tokens_saved"];
        // compression key omitted proxy_compression, so falls back to compression=200
        assert_eq!(ts["proxy_compression"], json!(200));
        assert_eq!(ts["cli_filtering_raw"], json!(80));
        assert_eq!(ts["rtk_raw"], json!(80));
    }
}

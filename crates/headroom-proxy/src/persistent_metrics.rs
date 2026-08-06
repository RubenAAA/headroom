//! Pure, bounded aggregate state for the durable Dashboard Lifetime view.
//!
//! Holds the lifetime counters the dashboard shows: requests, tokens,
//! prefix-cache behaviour, cost, waste signals and per-model activity. Every
//! dimension is bounded — provider and stack labels compact into an `other`
//! bucket past their cap, and the tracked-model map is pruned once it grows
//! past [`MAX_TRACKED_MODELS`] — so the persisted blob can never grow without
//! limit.
//!
//! The state itself does no I/O. [`PersistentMetricsState::to_dict`] returns
//! the persisted form and [`PersistentMetricsState::snapshot`] the API-safe
//! form with every percentage derived at read time; the savings tracker owns
//! loading and saving.
//!
//! Ports Python's `headroom/proxy/persistent_metrics.py`.

use std::sync::Arc;

use chrono::{DateTime, Timelike, Utc};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

/// Bumped whenever the shape of [`PersistentMetricsState::snapshot`] changes.
pub const SCHEMA_VERSION: i64 = 5;
/// Cap on distinct provider labels, per count map.
pub const MAX_PROVIDER_VALUES: usize = 32;
/// Cap on distinct stack labels.
pub const MAX_STACK_VALUES: usize = 64;
/// Tracked-model count that triggers pruning.
pub const MAX_TRACKED_MODELS: usize = 200;
/// Models kept after pruning, and models exposed by a snapshot.
pub const MAX_EXPOSED_MODELS: usize = 100;
/// Labels are truncated to this many characters before use as a map key.
pub const MAX_LABEL_LENGTH: usize = 128;

/// Cache-miss reasons that keep their own bucket; anything else is `unknown`.
pub const KNOWN_MISS_REASONS: [&str; 3] = ["ttl_expiry", "prefix_change", "unknown"];

/// Waste signals that keep their own bucket.
pub const KNOWN_WASTE_SIGNALS: [&str; 8] = [
    "json_noise",
    "html_noise",
    "base64",
    "whitespace",
    "dynamic_date",
    "repetition",
    "reread",
    "reread_compressed",
];

/// The current UTC time without sub-second noise in persisted state.
pub fn utc_now() -> DateTime<Utc> {
    Utc::now().with_nanosecond(0).unwrap_or_else(Utc::now)
}

/// Render a timestamp the way the persisted state stores it: UTC, whole
/// seconds, `Z` suffix.
fn to_iso(value: DateTime<Utc>) -> String {
    value
        .with_nanosecond(0)
        .unwrap_or(value)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Coerce an arbitrary JSON value to a non-negative integer.
///
/// Mirrors Python's `int(value)` inside a `try`: booleans and numeric strings
/// convert, floats truncate toward zero, anything else (or a negative result)
/// becomes `0`.
fn coerce_int(value: Option<&Value>) -> i64 {
    let Some(value) = value else { return 0 };
    let raw = match value {
        Value::Bool(flag) => i64::from(*flag),
        Value::Number(num) => {
            if let Some(int) = num.as_i64() {
                int
            } else if let Some(uint) = num.as_u64() {
                // Python has arbitrary-precision ints; we saturate.
                i64::try_from(uint).unwrap_or(i64::MAX)
            } else {
                match num.as_f64() {
                    Some(float) if float.is_finite() => clamp_f64_to_i64(float),
                    // `int(inf)` raises OverflowError, `int(nan)` ValueError.
                    _ => 0,
                }
            }
        }
        // `int("12")` succeeds, `int("12.5")` raises ValueError.
        Value::String(text) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    };
    raw.max(0)
}

fn clamp_f64_to_i64(value: f64) -> i64 {
    let truncated = value.trunc();
    if truncated >= i64::MAX as f64 {
        i64::MAX
    } else if truncated <= i64::MIN as f64 {
        i64::MIN
    } else {
        truncated as i64
    }
}

/// Coerce an arbitrary JSON value to a non-negative, finite float.
fn coerce_float(value: Option<&Value>) -> f64 {
    let Some(value) = value else { return 0.0 };
    let result = match value {
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        Value::Number(num) => num.as_f64().unwrap_or(0.0),
        Value::String(text) => text.trim().parse::<f64>().unwrap_or(0.0),
        _ => return 0.0,
    };
    if result.is_finite() && result >= 0.0 {
        result
    } else {
        0.0
    }
}

/// Round to 6 decimal places the way Python's `round(value, 6)` does.
///
/// Both languages round the exact binary value, ties to even, so formatting
/// and re-parsing reproduces CPython's result without a decimal library.
fn round6(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.6}").parse::<f64>().unwrap_or(value)
}

/// Normalise a label: non-strings and blanks become `other`, and the rest is
/// trimmed and cut to [`MAX_LABEL_LENGTH`] characters.
fn label(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "other".to_string();
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "other".to_string();
    }
    trimmed.chars().take(MAX_LABEL_LENGTH).collect()
}

/// The dict under `value`, or an empty one when it isn't an object.
fn dict_or_empty(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn get<'a>(map: Option<&'a Map<String, Value>>, key: &str) -> Option<&'a Value> {
    map.and_then(|inner| inner.get(key))
}

// ---------------------------------------------------------------------------
// Insertion-ordered maps
// ---------------------------------------------------------------------------

/// Insertion-ordered `label -> count` map.
///
/// Python uses a plain `dict`, which preserves insertion order and keeps the
/// position of a key that is re-assigned. Removal shifts the remaining keys up
/// rather than swapping the last one into the hole, so a `Vec` of pairs models
/// it exactly (and every map here is capped at 64 entries).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CountMap(Vec<(String, i64)>);

impl CountMap {
    /// The count stored under `key`, or `0`.
    pub fn get(&self, key: &str) -> i64 {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map_or(0, |(_, count)| *count)
    }

    /// Add `delta` to `key`, appending it when it is new.
    pub fn add(&mut self, key: &str, delta: i64) {
        if let Some(slot) = self.0.iter_mut().find(|(name, _)| name == key) {
            slot.1 += delta;
            return;
        }
        self.0.push((key.to_string(), delta));
    }

    /// Remove `key` and return its count, shifting later keys up.
    fn take(&mut self, key: &str) -> i64 {
        match self.0.iter().position(|(name, _)| name == key) {
            Some(index) => self.0.remove(index).1,
            None => 0,
        }
    }

    /// Number of distinct labels.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the map holds no labels.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate labels and counts in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, i64)> {
        self.0.iter().map(|(name, count)| (name.as_str(), *count))
    }

    /// Fold everything past `limit` named labels into `other`.
    ///
    /// The smallest count is evicted first, ties broken by label so the result
    /// does not depend on iteration order. `other` itself never counts against
    /// the limit.
    fn compact(&mut self, limit: usize) {
        loop {
            let named = self.0.iter().filter(|(name, _)| name != "other").count();
            if named <= limit {
                return;
            }
            let Some(evicted) = self
                .0
                .iter()
                .filter(|(name, _)| name != "other")
                .min_by(|left, right| (left.1, &left.0).cmp(&(right.1, &right.0)))
                .map(|(name, _)| name.clone())
            else {
                return;
            };
            let moved = self.take(&evicted);
            self.add("other", moved);
        }
    }
}

impl Serialize for CountMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(name, count)| (name, count)))
    }
}

/// Insertion-ordered `model name -> entry` map, with the same semantics as
/// [`CountMap`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMap(Vec<(String, ModelEntry)>);

impl ModelMap {
    /// The entry stored under `name`.
    pub fn get(&self, name: &str) -> Option<&ModelEntry> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, entry)| entry)
    }

    /// Insert or replace `name`, keeping the position of an existing key.
    fn insert(&mut self, name: &str, entry: ModelEntry) {
        if let Some(slot) = self.0.iter_mut().find(|(key, _)| key == name) {
            slot.1 = entry;
            return;
        }
        self.0.push((name.to_string(), entry));
    }

    fn entry_mut(&mut self, name: &str) -> &mut ModelEntry {
        if let Some(index) = self.0.iter().position(|(key, _)| key == name) {
            return &mut self.0[index].1;
        }
        self.0.push((name.to_string(), ModelEntry::default()));
        let last = self.0.len() - 1;
        &mut self.0[last].1
    }

    /// Number of tracked models.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no model is tracked.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate names and entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ModelEntry)> {
        self.0.iter().map(|(name, entry)| (name.as_str(), entry))
    }

    /// Entries ordered by the snapshot ranking: most observed tokens first,
    /// then most recent activity, then name.
    fn ranked(&self) -> Vec<(String, ModelEntry)> {
        let mut ranked = self.0.clone();
        ranked.sort_by(|left, right| model_rank(left).cmp(&model_rank(right)));
        ranked
    }
}

impl Serialize for ModelMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(name, entry)| (name, entry)))
    }
}

/// Sort key for the model ranking: `(-observed_tokens, last_activity, name)`.
fn model_rank(item: &(String, ModelEntry)) -> (i64, String, String) {
    let (name, entry) = item;
    let observed = entry.input_tokens + entry.output_tokens;
    (
        -observed,
        entry.last_activity_at.clone().unwrap_or_default(),
        name.clone(),
    )
}

// ---------------------------------------------------------------------------
// State shape
// ---------------------------------------------------------------------------

/// One model's lifetime totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    /// Completed requests attributed to this model.
    pub requests: i64,
    /// Input tokens actually sent upstream.
    pub input_tokens: i64,
    /// Output tokens received.
    pub output_tokens: i64,
    /// Input tokens before compression.
    pub attempted_input_tokens: i64,
    /// Input tokens compression removed.
    pub tokens_saved: i64,
    /// ISO timestamp of the last request, if any.
    pub last_activity_at: Option<String>,
}

impl ModelEntry {
    /// Build an entry from a raw persisted value, coercing every field.
    fn from_raw(raw: Option<&Value>) -> Self {
        let raw = dict_or_empty(raw);
        Self {
            requests: coerce_int(get(raw, "requests")),
            input_tokens: coerce_int(get(raw, "input_tokens")),
            output_tokens: coerce_int(get(raw, "output_tokens")),
            attempted_input_tokens: coerce_int(get(raw, "attempted_input_tokens")),
            tokens_saved: coerce_int(get(raw, "tokens_saved")),
            last_activity_at: get(raw, "last_activity_at")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    /// Fold `source` into `self`, keeping the later activity timestamp.
    fn merge(&mut self, source: &ModelEntry) {
        self.requests += source.requests;
        self.input_tokens += source.input_tokens;
        self.output_tokens += source.output_tokens;
        self.attempted_input_tokens += source.attempted_input_tokens;
        self.tokens_saved += source.tokens_saved;
        let replace = match (&self.last_activity_at, &source.last_activity_at) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(destination), Some(candidate)) => candidate > destination,
        };
        if replace {
            self.last_activity_at = source.last_activity_at.clone();
        }
    }
}

/// Request counters and their bounded label breakdowns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RequestsState {
    /// Completed top-level requests.
    pub total: i64,
    /// Requests that hit the prefix cache.
    pub cached: i64,
    /// Requests that failed.
    pub failed: i64,
    /// Requests rejected with a rate limit.
    pub rate_limited: i64,
    /// Requests per provider label.
    pub by_provider: CountMap,
    /// Requests per calling stack label.
    pub by_stack: CountMap,
}

/// Lifetime token totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TokensState {
    /// Input tokens sent upstream.
    pub input: i64,
    /// Output tokens received.
    pub output: i64,
    /// Input tokens before compression.
    pub attempted_input: i64,
    /// Input tokens compression removed.
    pub saved: i64,
}

/// Prefix-cache counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PrefixCacheState {
    /// Requests that could have used the cache.
    pub requests: i64,
    /// Requests that read from the cache.
    pub hit_requests: i64,
    /// Tokens read from the cache.
    pub cache_read_tokens: i64,
    /// Tokens written to the cache.
    pub cache_write_tokens: i64,
    /// Tokens written with the 5-minute TTL.
    pub cache_write_5m_tokens: i64,
    /// Tokens written with the 1-hour TTL.
    pub cache_write_1h_tokens: i64,
    /// Input tokens billed at the uncached rate.
    pub uncached_input_tokens: i64,
    /// Observed cache busts.
    pub bust_count: i64,
    /// Tokens lost to cache busts.
    pub bust_tokens: i64,
    /// Misses per reason, restricted to [`KNOWN_MISS_REASONS`].
    pub misses_by_reason: CountMap,
    /// Cache-eligible requests per provider.
    pub by_provider: CountMap,
}

/// Dollar totals, rounded to six decimals on every update.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CostState {
    /// Spend on input tokens.
    pub input_usd: f64,
    /// Dollars compression avoided.
    pub compression_savings_usd: f64,
    /// Dollars the prefix cache avoided.
    pub cache_savings_usd: f64,
}

/// Tracked models plus the overflow bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModelsState {
    /// Named models, bounded by [`MAX_TRACKED_MODELS`].
    pub tracked: ModelMap,
    /// Everything evicted, unnamed, or literally called `other`/`unknown`.
    pub other: ModelEntry,
}

/// I/O metadata the tracker fills in after a successful save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PersistenceState {
    /// ISO timestamp of the last successful save.
    pub last_saved_at: Option<String>,
}

/// The full persisted aggregate.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MetricsSnapshotState {
    /// First recorded activity, ever.
    pub started_at: Option<String>,
    /// Most recent recorded activity.
    pub last_activity_at: Option<String>,
    /// First activity recorded under the current schema.
    pub full_fidelity_started_at: Option<String>,
    /// Request counters.
    pub requests: RequestsState,
    /// Token totals.
    pub tokens: TokensState,
    /// Prefix-cache counters.
    pub prefix_cache: PrefixCacheState,
    /// Dollar totals.
    pub cost: CostState,
    /// Waste-signal tokens per bucket.
    pub waste_signals: CountMap,
    /// Per-model totals.
    pub models: ModelsState,
    /// Save metadata.
    pub persistence: PersistenceState,
}

// ---------------------------------------------------------------------------
// record_request arguments
// ---------------------------------------------------------------------------

/// One completed top-level request, as handed to
/// [`PersistentMetricsState::record_request`].
///
/// Python takes these as keyword arguments typed `Any` and coerces each one;
/// here the numeric fields are already typed, and the same clamping (negatives
/// and non-finite values become zero) is applied on the way in.
#[derive(Debug, Clone)]
pub struct RecordRequest {
    /// Upstream provider label.
    pub provider: Option<String>,
    /// Calling stack label.
    pub stack: Option<String>,
    /// Model name.
    pub model: Option<String>,
    /// Input tokens sent upstream.
    pub input_tokens: i64,
    /// Output tokens received.
    pub output_tokens: i64,
    /// Input tokens before compression.
    pub attempted_input_tokens: i64,
    /// Input tokens compression removed.
    pub tokens_saved: i64,
    /// Whether the request hit the prefix cache.
    pub cached: bool,
    /// Whether to count the stack label; `false` when the caller already did.
    pub record_stack: bool,
    /// Tokens read from the prefix cache.
    pub cache_read_tokens: i64,
    /// Tokens written to the prefix cache.
    pub cache_write_tokens: i64,
    /// Tokens written with the 5-minute TTL.
    pub cache_write_5m_tokens: i64,
    /// Tokens written with the 1-hour TTL.
    pub cache_write_1h_tokens: i64,
    /// Input tokens billed at the uncached rate.
    pub uncached_input_tokens: i64,
    /// Spend on input tokens.
    pub input_usd: f64,
    /// Dollars compression avoided.
    pub compression_savings_usd: f64,
    /// Dollars the prefix cache avoided.
    pub cache_savings_usd: f64,
    /// Waste-signal token counts, keyed by signal name.
    pub waste_signals: Option<Map<String, Value>>,
}

impl Default for RecordRequest {
    fn default() -> Self {
        Self {
            provider: None,
            stack: None,
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            attempted_input_tokens: 0,
            tokens_saved: 0,
            cached: false,
            // Python's default is `record_stack=True`.
            record_stack: true,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_5m_tokens: 0,
            cache_write_1h_tokens: 0,
            uncached_input_tokens: 0,
            input_usd: 0.0,
            compression_savings_usd: 0.0,
            cache_savings_usd: 0.0,
            waste_signals: None,
        }
    }
}

/// Apply the same clamp `_coerce_int` applies to an already-typed integer.
fn clamp_int(value: i64) -> i64 {
    value.max(0)
}

/// Apply the same clamp `_coerce_float` applies to an already-typed float.
fn clamp_float(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// PersistentMetricsState
// ---------------------------------------------------------------------------

/// In-memory Lifetime aggregate with deterministic, bounded dimensions.
pub struct PersistentMetricsState {
    now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    state: MetricsSnapshotState,
}

impl std::fmt::Debug for PersistentMetricsState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistentMetricsState")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Default for PersistentMetricsState {
    fn default() -> Self {
        Self::new(None)
    }
}

impl PersistentMetricsState {
    /// Build from a previously persisted blob, normalising every field.
    ///
    /// Anything unreadable — a missing key, the wrong type, a negative or
    /// non-finite number — falls back to the empty value rather than raising,
    /// so a corrupt file degrades to zeroed counters.
    pub fn new(raw: Option<&Value>) -> Self {
        Self::with_now(raw, Arc::new(utc_now))
    }

    /// Same as [`Self::new`], with an injectable clock for tests.
    pub fn with_now(
        raw: Option<&Value>,
        now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Self {
        let mut state = Self {
            now,
            state: normalize(raw),
        };
        state.compact_models();
        state
    }

    // -- recording ----------------------------------------------------------

    /// Stamp the activity timestamps and return the stamp used.
    fn record_activity(&mut self) -> String {
        let timestamp = to_iso((self.now)());
        if self.state.started_at.is_none() {
            self.state.started_at = Some(timestamp.clone());
        }
        if self.state.full_fidelity_started_at.is_none() {
            self.state.full_fidelity_started_at = Some(timestamp.clone());
        }
        self.state.last_activity_at = Some(timestamp.clone());
        timestamp
    }

    /// Accumulate one completed top-level request after coercing all deltas.
    pub fn record_request(&mut self, request: &RecordRequest) {
        let timestamp = self.record_activity();
        let input_delta = clamp_int(request.input_tokens);
        let output_delta = clamp_int(request.output_tokens);
        let attempted_delta = clamp_int(request.attempted_input_tokens);
        let saved_delta = clamp_int(request.tokens_saved);
        let provider_label = label(request.provider.as_deref());
        let stack_label = label(request.stack.as_deref());

        let requests = &mut self.state.requests;
        requests.total += 1;
        requests.cached += i64::from(request.cached);
        increment_count(
            &mut requests.by_provider,
            &provider_label,
            MAX_PROVIDER_VALUES,
        );
        if request.record_stack {
            increment_count(&mut requests.by_stack, &stack_label, MAX_STACK_VALUES);
        }

        let tokens = &mut self.state.tokens;
        tokens.input += input_delta;
        tokens.output += output_delta;
        tokens.attempted_input += attempted_delta;
        tokens.saved += saved_delta;

        let cache = &mut self.state.prefix_cache;
        cache.requests += 1;
        cache.hit_requests += i64::from(request.cached);
        cache.cache_read_tokens += clamp_int(request.cache_read_tokens);
        cache.cache_write_tokens += clamp_int(request.cache_write_tokens);
        cache.cache_write_5m_tokens += clamp_int(request.cache_write_5m_tokens);
        cache.cache_write_1h_tokens += clamp_int(request.cache_write_1h_tokens);
        cache.uncached_input_tokens += clamp_int(request.uncached_input_tokens);
        increment_count(&mut cache.by_provider, &provider_label, MAX_PROVIDER_VALUES);

        let cost = &mut self.state.cost;
        cost.input_usd = round6(cost.input_usd + clamp_float(request.input_usd));
        cost.compression_savings_usd =
            round6(cost.compression_savings_usd + clamp_float(request.compression_savings_usd));
        cost.cache_savings_usd =
            round6(cost.cache_savings_usd + clamp_float(request.cache_savings_usd));

        if let Some(signals) = &request.waste_signals {
            for (name, token_count) in signals {
                let bucket = if KNOWN_WASTE_SIGNALS.contains(&name.as_str()) {
                    name.as_str()
                } else {
                    "other"
                };
                self.state
                    .waste_signals
                    .add(bucket, coerce_int(Some(token_count)));
            }
        }

        self.record_model(
            request.model.as_deref(),
            &timestamp,
            input_delta,
            output_delta,
            attempted_delta,
            saved_delta,
        );
    }

    fn record_model(
        &mut self,
        model: Option<&str>,
        timestamp: &str,
        input_tokens: i64,
        output_tokens: i64,
        attempted_input_tokens: i64,
        tokens_saved: i64,
    ) {
        let name = model_name(model);
        let entry = if name == "other" {
            &mut self.state.models.other
        } else {
            self.state.models.tracked.entry_mut(&name)
        };
        entry.requests += 1;
        entry.input_tokens += input_tokens;
        entry.output_tokens += output_tokens;
        entry.attempted_input_tokens += attempted_input_tokens;
        entry.tokens_saved += tokens_saved;
        entry.last_activity_at = Some(timestamp.to_string());
        self.compact_models();
    }

    /// Accumulate the existing inbound stack label without adding a request.
    pub fn record_stack(&mut self, stack: Option<&str>) {
        let Some(stack) = stack else { return };
        self.record_activity();
        let stack_label = label(Some(stack));
        increment_count(
            &mut self.state.requests.by_stack,
            &stack_label,
            MAX_STACK_VALUES,
        );
    }

    /// Record a failed request without changing the completed-request
    /// denominator.
    ///
    /// `provider` and `model` are accepted for call-site parity and, as in
    /// Python, deliberately unused.
    pub fn record_failed(&mut self, _provider: Option<&str>, _model: Option<&str>) {
        self.record_activity();
        self.state.requests.failed += 1;
    }

    /// Record a rate-limited request without redefining total request
    /// semantics. `provider` and `model` are unused, as in Python.
    pub fn record_rate_limited(&mut self, _provider: Option<&str>, _model: Option<&str>) {
        self.record_activity();
        self.state.requests.rate_limited += 1;
    }

    /// Record one prefix-cache bust and the tokens it cost.
    pub fn record_cache_bust(&mut self, tokens_lost: i64) {
        self.record_activity();
        self.state.prefix_cache.bust_count += 1;
        self.state.prefix_cache.bust_tokens += clamp_int(tokens_lost);
    }

    /// Record one prefix-cache miss, bucketed by reason.
    ///
    /// `provider` is accepted for call-site parity and unused, as in Python.
    pub fn record_cache_miss(&mut self, _provider: Option<&str>, reason: Option<&str>) {
        self.record_activity();
        let bucket = match reason {
            Some(value) if KNOWN_MISS_REASONS.contains(&value) => value,
            _ => "unknown",
        };
        self.state.prefix_cache.misses_by_reason.add(bucket, 1);
    }

    /// Stamp the last successful save.
    pub fn set_last_saved_at(&mut self, value: Option<String>) {
        self.state.persistence.last_saved_at = value;
    }

    // -- reading ------------------------------------------------------------

    /// Borrow the in-memory state.
    pub fn state(&self) -> &MetricsSnapshotState {
        &self.state
    }

    /// The persisted form, without derived values or I/O metadata.
    pub fn to_dict(&self) -> Value {
        serde_json::to_value(&self.state).unwrap_or(Value::Null)
    }

    /// Fold the tracked models past [`MAX_TRACKED_MODELS`] into `other`.
    fn compact_models(&mut self) {
        if self.state.models.tracked.len() <= MAX_TRACKED_MODELS {
            return;
        }
        let ranked = self.state.models.tracked.ranked();
        for (_, entry) in ranked.iter().skip(MAX_EXPOSED_MODELS) {
            self.state.models.other.merge(entry);
        }
        self.state.models.tracked = ModelMap(
            ranked
                .into_iter()
                .take(MAX_EXPOSED_MODELS)
                .collect::<Vec<_>>(),
        );
    }

    /// `numerator / denominator` as a percentage, or `None` when there is no
    /// denominator to divide by.
    fn percent(numerator: i64, denominator: i64) -> Option<f64> {
        if denominator <= 0 {
            return None;
        }
        Some(round6(numerator as f64 / denominator as f64 * 100.0))
    }

    /// Top models by observed tokens, with everything else folded into
    /// `other`. Read-only: the in-memory state is left alone.
    fn by_model_snapshot(&self) -> Value {
        let ranked = self.state.models.tracked.ranked();
        let mut other = self.state.models.other.clone();
        for (_, entry) in ranked.iter().skip(MAX_EXPOSED_MODELS) {
            other.merge(entry);
        }
        let mut result = Map::new();
        for (name, entry) in ranked.iter().take(MAX_EXPOSED_MODELS) {
            result.insert(name.clone(), to_value(entry));
        }
        result.insert("other".to_string(), to_value(&other));
        Value::Object(result)
    }

    /// An API-safe aggregate with all percentages derived at read time.
    ///
    /// `persistence` is merged in as-is and then `last_saved_at` is
    /// overwritten from the state, so a caller cannot report a save that
    /// didn't happen.
    pub fn snapshot(&self, persistence: &Value) -> Value {
        let cache = &self.state.prefix_cache;
        let cache_write_total = cache.cache_write_1h_tokens + cache.cache_write_5m_tokens;
        let tokens = &self.state.tokens;

        let mut tokens_out = object_of(&tokens);
        tokens_out.insert(
            "token_savings_percent".to_string(),
            to_value(&Self::percent(tokens.saved, tokens.attempted_input)),
        );

        let mut cache_out = object_of(&cache);
        cache_out.insert(
            "cache_hit_rate".to_string(),
            to_value(&Self::percent(cache.hit_requests, cache.requests)),
        );
        cache_out.insert(
            "ttl_1h_percent".to_string(),
            to_value(&Self::percent(
                cache.cache_write_1h_tokens,
                cache_write_total,
            )),
        );
        cache_out.insert(
            "ttl_5m_percent".to_string(),
            to_value(&Self::percent(
                cache.cache_write_5m_tokens,
                cache_write_total,
            )),
        );

        let mut persistence_out = persistence.as_object().cloned().unwrap_or_default();
        persistence_out.insert(
            "last_saved_at".to_string(),
            to_value(&self.state.persistence.last_saved_at),
        );

        let mut result = Map::new();
        result.insert("scope".to_string(), Value::from("lifetime"));
        result.insert("schema_version".to_string(), Value::from(SCHEMA_VERSION));
        result.insert(
            "generated_at".to_string(),
            Value::from(to_iso((self.now)())),
        );
        result.insert("started_at".to_string(), to_value(&self.state.started_at));
        result.insert(
            "last_activity_at".to_string(),
            to_value(&self.state.last_activity_at),
        );
        result.insert(
            "full_fidelity_started_at".to_string(),
            to_value(&self.state.full_fidelity_started_at),
        );
        result.insert("requests".to_string(), to_value(&self.state.requests));
        result.insert("tokens".to_string(), Value::Object(tokens_out));
        result.insert("prefix_cache".to_string(), Value::Object(cache_out));
        result.insert("cost".to_string(), to_value(&self.state.cost));
        result.insert(
            "waste_signals".to_string(),
            to_value(&self.state.waste_signals),
        );
        result.insert("by_model".to_string(), self.by_model_snapshot());
        result.insert("persistence".to_string(), Value::Object(persistence_out));
        Value::Object(result)
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn object_of<T: Serialize>(value: &T) -> Map<String, Value> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Add one to `label` and re-apply the cap.
fn increment_count(values: &mut CountMap, label: &str, limit: usize) {
    values.add(label, 1);
    values.compact(limit);
}

/// Never retain the legacy unknown model bucket as a named model.
fn model_name(value: Option<&str>) -> String {
    let name = label(value);
    if name.to_lowercase() == "unknown" {
        "other".to_string()
    } else {
        name
    }
}

/// Sum a raw map into an insertion-ordered count map and apply the cap.
fn normalize_count_map(raw: Option<&Value>, limit: usize) -> CountMap {
    let mut result = CountMap::default();
    let Some(raw) = dict_or_empty(raw) else {
        return result;
    };
    for (key, value) in raw {
        result.add(&label(Some(key)), coerce_int(Some(value)));
    }
    result.compact(limit);
    result
}

/// Sum a raw map into a count map keyed only by `allowed` labels; anything
/// else lands in `unknown`.
fn normalize_enum_map(raw: Option<&Value>, allowed: &[&str]) -> CountMap {
    let mut result = CountMap::default();
    let Some(raw) = dict_or_empty(raw) else {
        return result;
    };
    for (key, value) in raw {
        let bucket = if allowed.contains(&key.as_str()) {
            key.as_str()
        } else {
            "unknown"
        };
        result.add(bucket, coerce_int(Some(value)));
    }
    result
}

/// Turn a raw persisted blob into a fully normalised state.
fn normalize(raw: Option<&Value>) -> MetricsSnapshotState {
    let source = dict_or_empty(raw);
    let mut result = MetricsSnapshotState::default();

    result.started_at = get(source, "started_at")
        .and_then(Value::as_str)
        .map(str::to_string);
    result.last_activity_at = get(source, "last_activity_at")
        .and_then(Value::as_str)
        .map(str::to_string);
    result.full_fidelity_started_at = get(source, "full_fidelity_started_at")
        .and_then(Value::as_str)
        .map(str::to_string);

    let raw_requests = dict_or_empty(get(source, "requests"));
    result.requests = RequestsState {
        total: coerce_int(get(raw_requests, "total")),
        cached: coerce_int(get(raw_requests, "cached")),
        failed: coerce_int(get(raw_requests, "failed")),
        rate_limited: coerce_int(get(raw_requests, "rate_limited")),
        by_provider: normalize_count_map(get(raw_requests, "by_provider"), MAX_PROVIDER_VALUES),
        by_stack: normalize_count_map(get(raw_requests, "by_stack"), MAX_STACK_VALUES),
    };

    let raw_tokens = dict_or_empty(get(source, "tokens"));
    result.tokens = TokensState {
        input: coerce_int(get(raw_tokens, "input")),
        output: coerce_int(get(raw_tokens, "output")),
        attempted_input: coerce_int(get(raw_tokens, "attempted_input")),
        saved: coerce_int(get(raw_tokens, "saved")),
    };

    let raw_cache = dict_or_empty(get(source, "prefix_cache"));
    result.prefix_cache = PrefixCacheState {
        requests: coerce_int(get(raw_cache, "requests")),
        hit_requests: coerce_int(get(raw_cache, "hit_requests")),
        cache_read_tokens: coerce_int(get(raw_cache, "cache_read_tokens")),
        cache_write_tokens: coerce_int(get(raw_cache, "cache_write_tokens")),
        cache_write_5m_tokens: coerce_int(get(raw_cache, "cache_write_5m_tokens")),
        cache_write_1h_tokens: coerce_int(get(raw_cache, "cache_write_1h_tokens")),
        uncached_input_tokens: coerce_int(get(raw_cache, "uncached_input_tokens")),
        bust_count: coerce_int(get(raw_cache, "bust_count")),
        bust_tokens: coerce_int(get(raw_cache, "bust_tokens")),
        misses_by_reason: normalize_enum_map(
            get(raw_cache, "misses_by_reason"),
            &KNOWN_MISS_REASONS,
        ),
        by_provider: normalize_count_map(get(raw_cache, "by_provider"), MAX_PROVIDER_VALUES),
    };

    let raw_cost = dict_or_empty(get(source, "cost"));
    result.cost = CostState {
        input_usd: round6(coerce_float(get(raw_cost, "input_usd"))),
        compression_savings_usd: round6(coerce_float(get(raw_cost, "compression_savings_usd"))),
        cache_savings_usd: round6(coerce_float(get(raw_cost, "cache_savings_usd"))),
    };

    result.waste_signals = normalize_enum_map(get(source, "waste_signals"), &KNOWN_WASTE_SIGNALS);

    let raw_models = dict_or_empty(get(source, "models"));
    if let Some(tracked) = dict_or_empty(get(raw_models, "tracked")) {
        for (name, entry) in tracked {
            let normalized_name = model_name(Some(name));
            if normalized_name == "other" {
                let parsed = ModelEntry::from_raw(Some(entry));
                result.models.other.merge(&parsed);
                continue;
            }
            result
                .models
                .tracked
                .insert(&normalized_name, ModelEntry::from_raw(Some(entry)));
        }
    }
    let raw_other = ModelEntry::from_raw(get(raw_models, "other"));
    result.models.other.merge(&raw_other);

    let raw_persistence = dict_or_empty(get(source, "persistence"));
    result.persistence.last_saved_at = get(raw_persistence, "last_saved_at")
        .and_then(Value::as_str)
        .map(str::to_string);

    result
}

#[cfg(test)]
mod tests {
    // Every expected value below was measured by running the Python
    // reference (`headroom/proxy/persistent_metrics.py`) on the same input,
    // not derived by hand.
    use super::*;
    use serde_json::json;

    fn fixed_clock(iso: &str) -> Arc<dyn Fn() -> DateTime<Utc> + Send + Sync> {
        let parsed: DateTime<Utc> = iso.parse().unwrap();
        Arc::new(move || parsed)
    }

    fn state_at(iso: &str) -> PersistentMetricsState {
        PersistentMetricsState::with_now(None, fixed_clock(iso))
    }

    fn compact(value: &Value) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn empty_state_shape_matches_python() {
        let state = PersistentMetricsState::new(None);
        assert_eq!(
            compact(&state.to_dict()),
            r#"{"started_at":null,"last_activity_at":null,"full_fidelity_started_at":null,"requests":{"total":0,"cached":0,"failed":0,"rate_limited":0,"by_provider":{},"by_stack":{}},"tokens":{"input":0,"output":0,"attempted_input":0,"saved":0},"prefix_cache":{"requests":0,"hit_requests":0,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_5m_tokens":0,"cache_write_1h_tokens":0,"uncached_input_tokens":0,"bust_count":0,"bust_tokens":0,"misses_by_reason":{},"by_provider":{}},"cost":{"input_usd":0.0,"compression_savings_usd":0.0,"cache_savings_usd":0.0},"waste_signals":{},"models":{"tracked":{},"other":{"requests":0,"input_tokens":0,"output_tokens":0,"attempted_input_tokens":0,"tokens_saved":0,"last_activity_at":null}},"persistence":{"last_saved_at":null}}"#
        );
    }

    #[test]
    fn coerce_int_matches_python() {
        let cases: [(Value, i64); 9] = [
            (json!(5), 5),
            (json!(-5), 0),
            (json!(3.9), 3),
            (json!("12"), 12),
            (json!("12.5"), 0),
            (json!(true), 1),
            (json!(null), 0),
            (json!([1]), 0),
            (json!({}), 0),
        ];
        for (input, expected) in cases {
            assert_eq!(coerce_int(Some(&input)), expected, "input={input}");
        }
    }

    #[test]
    fn coerce_float_matches_python() {
        let cases: [(Value, f64); 7] = [
            (json!(1.5), 1.5),
            (json!(-1.5), 0.0),
            (json!("2.25"), 2.25),
            (json!("nope"), 0.0),
            (json!(true), 1.0),
            (json!(null), 0.0),
            (json!([1]), 0.0),
        ];
        for (input, expected) in cases {
            assert_eq!(coerce_float(Some(&input)), expected, "input={input}");
        }
    }

    #[test]
    fn round6_matches_python() {
        // `round(x, 6)` measured in CPython.
        assert_eq!(round6(0.1 + 0.2), 0.3);
        assert_eq!(round6(1.0 / 3.0), 0.333333);
        assert_eq!(round6(2.0 / 3.0), 0.666667);
        assert_eq!(round6(0.0000005), 0.0);
        assert_eq!(round6(0.0000015), 0.000002);
    }

    #[test]
    fn label_normalisation_matches_python() {
        assert_eq!(label(Some("  openai  ")), "openai");
        assert_eq!(label(Some("   ")), "other");
        assert_eq!(label(None), "other");
        assert_eq!(label(Some(&"x".repeat(200))).len(), MAX_LABEL_LENGTH);
        assert_eq!(model_name(Some("UNKNOWN")), "other");
        assert_eq!(model_name(Some("unknown")), "other");
        assert_eq!(model_name(Some("gpt-4")), "gpt-4");
    }

    #[test]
    fn record_request_accumulates_every_dimension() {
        let mut state = state_at("2026-07-27T12:00:00Z");
        state.record_request(&RecordRequest {
            provider: Some("openai".to_string()),
            stack: Some("codex".to_string()),
            model: Some("gpt-5".to_string()),
            input_tokens: 100,
            output_tokens: 20,
            attempted_input_tokens: 150,
            tokens_saved: 50,
            cached: true,
            cache_read_tokens: 40,
            cache_write_tokens: 10,
            cache_write_5m_tokens: 6,
            cache_write_1h_tokens: 4,
            uncached_input_tokens: 60,
            input_usd: 0.001234567,
            compression_savings_usd: 0.5,
            cache_savings_usd: 0.25,
            waste_signals: Some(
                json!({"json_noise": 12, "made_up": 3})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ..Default::default()
        });
        assert_eq!(
            compact(&state.to_dict()),
            r#"{"started_at":"2026-07-27T12:00:00Z","last_activity_at":"2026-07-27T12:00:00Z","full_fidelity_started_at":"2026-07-27T12:00:00Z","requests":{"total":1,"cached":1,"failed":0,"rate_limited":0,"by_provider":{"openai":1},"by_stack":{"codex":1}},"tokens":{"input":100,"output":20,"attempted_input":150,"saved":50},"prefix_cache":{"requests":1,"hit_requests":1,"cache_read_tokens":40,"cache_write_tokens":10,"cache_write_5m_tokens":6,"cache_write_1h_tokens":4,"uncached_input_tokens":60,"bust_count":0,"bust_tokens":0,"misses_by_reason":{},"by_provider":{"openai":1}},"cost":{"input_usd":0.001235,"compression_savings_usd":0.5,"cache_savings_usd":0.25},"waste_signals":{"json_noise":12,"other":3},"models":{"tracked":{"gpt-5":{"requests":1,"input_tokens":100,"output_tokens":20,"attempted_input_tokens":150,"tokens_saved":50,"last_activity_at":"2026-07-27T12:00:00Z"}},"other":{"requests":0,"input_tokens":0,"output_tokens":0,"attempted_input_tokens":0,"tokens_saved":0,"last_activity_at":null}},"persistence":{"last_saved_at":null}}"#
        );
    }

    #[test]
    fn snapshot_derives_percentages() {
        let mut state = state_at("2026-07-27T12:00:00Z");
        state.record_request(&RecordRequest {
            provider: Some("anthropic".to_string()),
            stack: Some("claude-code".to_string()),
            model: Some("sonnet".to_string()),
            input_tokens: 100,
            output_tokens: 20,
            attempted_input_tokens: 150,
            tokens_saved: 50,
            cached: true,
            cache_write_5m_tokens: 6,
            cache_write_1h_tokens: 4,
            ..Default::default()
        });
        state.set_last_saved_at(Some("2026-07-27T12:00:05Z".to_string()));
        let snapshot = state.snapshot(&json!({"path": "/tmp/x.json", "enabled": true}));
        assert_eq!(
            compact(&snapshot),
            r#"{"scope":"lifetime","schema_version":5,"generated_at":"2026-07-27T12:00:00Z","started_at":"2026-07-27T12:00:00Z","last_activity_at":"2026-07-27T12:00:00Z","full_fidelity_started_at":"2026-07-27T12:00:00Z","requests":{"total":1,"cached":1,"failed":0,"rate_limited":0,"by_provider":{"anthropic":1},"by_stack":{"claude-code":1}},"tokens":{"input":100,"output":20,"attempted_input":150,"saved":50,"token_savings_percent":33.333333},"prefix_cache":{"requests":1,"hit_requests":1,"cache_read_tokens":0,"cache_write_tokens":0,"cache_write_5m_tokens":6,"cache_write_1h_tokens":4,"uncached_input_tokens":0,"bust_count":0,"bust_tokens":0,"misses_by_reason":{},"by_provider":{"anthropic":1},"cache_hit_rate":100.0,"ttl_1h_percent":40.0,"ttl_5m_percent":60.0},"cost":{"input_usd":0.0,"compression_savings_usd":0.0,"cache_savings_usd":0.0},"waste_signals":{},"by_model":{"sonnet":{"requests":1,"input_tokens":100,"output_tokens":20,"attempted_input_tokens":150,"tokens_saved":50,"last_activity_at":"2026-07-27T12:00:00Z"},"other":{"requests":0,"input_tokens":0,"output_tokens":0,"attempted_input_tokens":0,"tokens_saved":0,"last_activity_at":null}},"persistence":{"path":"/tmp/x.json","enabled":true,"last_saved_at":"2026-07-27T12:00:05Z"}}"#
        );
    }

    #[test]
    fn empty_snapshot_percentages_are_null() {
        let state = state_at("2026-07-27T12:00:00Z");
        let snapshot = state.snapshot(&json!({}));
        assert_eq!(snapshot["tokens"]["token_savings_percent"], Value::Null);
        assert_eq!(snapshot["prefix_cache"]["cache_hit_rate"], Value::Null);
        assert_eq!(snapshot["prefix_cache"]["ttl_1h_percent"], Value::Null);
        assert_eq!(snapshot["prefix_cache"]["ttl_5m_percent"], Value::Null);
    }

    #[test]
    fn corrupt_input_degrades_to_empty() {
        // Python: junk types normalise away without raising.
        let raw = json!({
            "started_at": 5,
            "requests": "nope",
            "tokens": {"input": "abc", "output": -3},
            "prefix_cache": [1, 2],
            "cost": {"input_usd": "x"},
            "waste_signals": ["a"],
            "models": {"tracked": "nope", "other": 7},
            "persistence": {"last_saved_at": 9},
        });
        let state = PersistentMetricsState::new(Some(&raw));
        assert_eq!(state.to_dict(), PersistentMetricsState::new(None).to_dict());
    }

    #[test]
    fn normalize_merges_unknown_model_into_other() {
        let raw = json!({
            "models": {
                "tracked": {
                    "unknown": {"requests": 2, "input_tokens": 5, "last_activity_at": "2026-01-01T00:00:00Z"},
                    "gpt-4": {"requests": 1, "input_tokens": 9}
                },
                "other": {"requests": 3, "output_tokens": 4, "last_activity_at": "2026-02-01T00:00:00Z"}
            }
        });
        let state = PersistentMetricsState::new(Some(&raw));
        assert_eq!(
            compact(&state.to_dict()["models"]),
            r#"{"tracked":{"gpt-4":{"requests":1,"input_tokens":9,"output_tokens":0,"attempted_input_tokens":0,"tokens_saved":0,"last_activity_at":null}},"other":{"requests":5,"input_tokens":5,"output_tokens":4,"attempted_input_tokens":0,"tokens_saved":0,"last_activity_at":"2026-02-01T00:00:00Z"}}"#
        );
    }

    #[test]
    fn count_maps_compact_smallest_first() {
        let mut state = state_at("2026-07-27T12:00:00Z");
        for index in 0..(MAX_PROVIDER_VALUES + 5) {
            let hits = if index < 5 { 1 } else { 3 };
            for _ in 0..hits {
                state.record_request(&RecordRequest {
                    provider: Some(format!("p{index}")),
                    record_stack: false,
                    ..Default::default()
                });
            }
        }
        let providers = &state.state().requests.by_provider;
        // Measured in Python: 32 named labels plus `other`, holding 7 — not 5.
        // Eviction runs on every increment, so a provider can be evicted
        // after its first hit and before its second, which folds more than
        // the five 1-count labels into `other`.
        assert_eq!(providers.len(), MAX_PROVIDER_VALUES + 1);
        assert_eq!(providers.get("other"), 7);
        assert_eq!(providers.get("p0"), 0);
        assert_eq!(providers.get("p5"), 3);
    }

    #[test]
    fn tracked_models_prune_past_the_cap() {
        let mut state = state_at("2026-07-27T12:00:00Z");
        for index in 0..(MAX_TRACKED_MODELS + 1) {
            state.record_request(&RecordRequest {
                model: Some(format!("m{index:04}")),
                input_tokens: index as i64,
                record_stack: false,
                ..Default::default()
            });
        }
        assert_eq!(state.state().models.tracked.len(), MAX_EXPOSED_MODELS);
        // Kept: the 100 highest input-token models, m0101..m0200.
        assert!(state.state().models.tracked.get("m0200").is_some());
        assert!(state.state().models.tracked.get("m0101").is_some());
        assert!(state.state().models.tracked.get("m0100").is_none());
        assert_eq!(state.state().models.other.requests, 101);
    }

    #[test]
    fn record_helpers_stamp_activity() {
        let mut state = state_at("2026-07-27T12:00:00Z");
        state.record_stack(None);
        assert!(state.state().started_at.is_none());

        state.record_stack(Some("  codex  "));
        state.record_failed(Some("openai"), Some("gpt-5"));
        state.record_rate_limited(None, None);
        state.record_cache_bust(1200);
        state.record_cache_miss(Some("openai"), Some("ttl_expiry"));
        state.record_cache_miss(None, Some("bogus"));

        assert_eq!(
            state.state().started_at.as_deref(),
            Some("2026-07-27T12:00:00Z")
        );
        assert_eq!(state.state().requests.by_stack.get("codex"), 1);
        assert_eq!(state.state().requests.total, 0);
        assert_eq!(state.state().requests.failed, 1);
        assert_eq!(state.state().requests.rate_limited, 1);
        assert_eq!(state.state().prefix_cache.bust_count, 1);
        assert_eq!(state.state().prefix_cache.bust_tokens, 1200);
        assert_eq!(
            compact(&state.to_dict()["prefix_cache"]["misses_by_reason"]),
            r#"{"ttl_expiry":1,"unknown":1}"#
        );
    }

    #[test]
    fn round_trip_through_the_persisted_form_is_stable() {
        let mut state = state_at("2026-07-27T12:00:00Z");
        state.record_request(&RecordRequest {
            provider: Some("openai".to_string()),
            stack: Some("codex".to_string()),
            model: Some("gpt-5".to_string()),
            input_tokens: 10,
            output_tokens: 2,
            cached: true,
            input_usd: 0.125,
            ..Default::default()
        });
        state.set_last_saved_at(Some("2026-07-27T12:00:01Z".to_string()));
        let persisted = state.to_dict();
        let reloaded = PersistentMetricsState::new(Some(&persisted));
        assert_eq!(reloaded.to_dict(), persisted);
    }
}

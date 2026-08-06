//! Cost tracking and budget management (Rust port of `headroom/proxy/cost.py`).
//!
//! [`CostTracker`] accumulates per-model token/cache counts, prices requests via
//! the vendored [`crate::pricing`] table (the Rust stand-in for Python's
//! `litellm`), enforces hourly/daily/monthly budgets, and produces a monotonic
//! `savings_usd` (saved tokens at the model's list input price).
//!
//! [`build_prefix_cache_stats`] and [`build_session_summary`] port the two
//! dashboard builders from Python's `cost.py`. In Python they are tightly
//! coupled to `PrometheusMetrics` and the proxy's request logger; the Rust
//! versions accept plain input structs (`PrefixCacheStatsInput`,
//! `SessionSummaryInput`) so they live in core with no proxy dependency.
//! `summarize_transforms` already lives in [`crate::request_outcome`].

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Re-export so callers use one canonical `summarize_transforms` (it lives in
/// [`crate::request_outcome`], not duplicated here).
pub use crate::request_outcome::summarize_transforms;

/// Hard cap on retained cost entries (memory bound).
pub const MAX_COST_ENTRIES: usize = 100_000;
/// Time-based retention: must be ≥ the longest budget period (monthly ≈ 31d).
pub const COST_RETENTION_HOURS: i64 = 744;

/// Provider cache discount multipliers: `(read_multiplier, write_multiplier,
/// label)` — the fraction of the input price a cache read/write costs.
pub struct CacheEconomics {
    pub read_multiplier: f64,
    pub write_multiplier: f64,
    pub label: &'static str,
}

/// Look up per-provider cache economics, defaulting to Anthropic's (matching
/// Python's `_CACHE_ECONOMICS.get(provider, _CACHE_ECONOMICS["anthropic"])`).
pub fn cache_economics(provider: &str) -> CacheEconomics {
    match provider {
        "openai" => CacheEconomics {
            read_multiplier: 0.5,
            write_multiplier: 1.0,
            label: "Automatic, no TTL control",
        },
        "gemini" => CacheEconomics {
            read_multiplier: 0.1,
            write_multiplier: 1.0,
            label: "Explicit cachedContent, configurable TTL",
        },
        "bedrock" => CacheEconomics {
            read_multiplier: 0.1,
            write_multiplier: 1.25,
            label: "Same as Anthropic (Bedrock)",
        },
        // "anthropic" and any unknown provider.
        _ => CacheEconomics {
            read_multiplier: 0.1,
            write_multiplier: 1.25,
            label: "Explicit breakpoints, 5-min TTL",
        },
    }
}

/// Strip enriched detail so each tag is safe in the comma-joined
/// `x-headroom-transforms` header. `smart_crush:<n>:<names>` and
/// `read_lifecycle:<state>:<path>` collapse to their legacy counter shape.
pub fn header_safe_transforms(transforms: &[String]) -> Vec<String> {
    transforms
        .iter()
        .map(|t| {
            for prefix in ["smart_crush:", "read_lifecycle:"] {
                if t.starts_with(prefix) {
                    let parts: Vec<&str> = t.split(':').collect();
                    if parts.len() >= 2 {
                        return format!("{}:{}", parts[0], parts[1]);
                    }
                    return t.clone();
                }
            }
            t.clone()
        })
        .collect()
}

/// Round half-to-even to `ndigits`, matching Python's `round`.
fn round_n(value: f64, ndigits: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let f = 10f64.powi(ndigits);
    (value * f).round_ties_even() / f
}

/// Merge compression, cache, and CLI savings into cost stats. Returns `None`
/// when `cost_stats` is `None` (mirrors Python). Keeps `savings_usd` as
/// compression-only (monotonic); cache savings stay a separate field.
pub fn merge_cost_stats(
    cost_stats: Option<&Value>,
    cache_stats: &Value,
    cli_tokens_avoided: i64,
) -> Option<Value> {
    let cost_stats = cost_stats?;
    let cache_net = cache_stats
        .get("totals")
        .and_then(|t| t.get("net_savings_usd"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let compression_savings = cost_stats
        .get("savings_usd")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    let mut out: Map<String, Value> = match cost_stats {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    out.insert("savings_usd".into(), json!(round_n(compression_savings, 4)));
    out.insert(
        "compression_savings_usd".into(),
        json!(round_n(compression_savings, 4)),
    );
    out.insert("cache_savings_usd".into(), json!(round_n(cache_net, 4)));
    out.insert("cli_tokens_avoided".into(), json!(cli_tokens_avoided));
    out.insert(
        "cli_filtering_tokens_avoided".into(),
        json!(cli_tokens_avoided),
    );
    out.insert("cli_tokens_included_in_compression".into(), json!(true));
    out.insert(
        "cli_filtering_tokens_included_in_compression".into(),
        json!(true),
    );
    Some(Value::Object(out))
}

/// Per-request token counts fed to [`CostTracker::record_tokens`]. Neutral
/// defaults mirror the Python keyword arguments.
#[derive(Default, Clone)]
pub struct TokenRecord {
    pub tokens_saved: i64,
    pub tokens_sent: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub uncached_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Default)]
struct PerModel {
    tokens_saved: HashMap<String, i64>,
    tokens_sent: HashMap<String, i64>,
    requests: HashMap<String, i64>,
    cache_read: HashMap<String, i64>,
    cache_write: HashMap<String, i64>,
    cache_write_5m: HashMap<String, i64>,
    cache_write_1h: HashMap<String, i64>,
    uncached: HashMap<String, i64>,
}

struct Inner {
    costs: VecDeque<(DateTime<Local>, f64)>,
    last_prune: DateTime<Local>,
    m: PerModel,
}

/// Tracks per-model costs and enforces budgets. Thread-safe (held as
/// `Arc<CostTracker>` on `AppState`).
pub struct CostTracker {
    budget_limit_usd: Option<f64>,
    /// "hourly" | "daily" | "monthly".
    budget_period: String,
    inner: Mutex<Inner>,
}

impl CostTracker {
    pub fn new(budget_limit_usd: Option<f64>, budget_period: &str) -> Self {
        Self {
            budget_limit_usd,
            budget_period: budget_period.to_string(),
            inner: Mutex::new(Inner {
                costs: VecDeque::new(),
                last_prune: Local::now(),
                m: PerModel::default(),
            }),
        }
    }

    /// Estimate USD cost via vendored pricing. `None` when the model has no
    /// pricing entry (mirrors Python returning `None` when litellm can't price).
    /// `input_tokens` excludes cache-read tokens.
    pub fn estimate_cost(
        &self,
        model: &str,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
    ) -> Option<f64> {
        let p = crate::pricing::lookup(model)?;
        let inp = input_tokens.max(0) as f64;
        let out = output_tokens.max(0) as f64;
        let cr = cache_read_tokens.max(0) as f64;
        let cw = cache_write_tokens.max(0) as f64;
        let cr_rate = p
            .cache_read_cost_per_token
            .unwrap_or(p.input_cost_per_token);
        let cw_rate = p
            .cache_write_cost_per_token
            .unwrap_or(p.input_cost_per_token);
        let total = inp * p.input_cost_per_token
            + out * p.output_cost_per_token
            + cr * cr_rate
            + cw * cw_rate;
        if total > 0.0 {
            Some(total)
        } else {
            None
        }
    }

    /// List input price ($/token) for a model, or `None`.
    fn list_price_per_token(model: &str) -> Option<f64> {
        crate::pricing::lookup(model).map(|p| p.input_cost_per_token)
    }

    /// `(cache_read, cache_write, uncached)` per-token prices, or `None` when
    /// the model has no input price. Missing cache prices fall back to the
    /// uncached rate (matching Python's `.get(..., uncached)`).
    fn cache_prices(model: &str) -> Option<(f64, f64, f64)> {
        let p = crate::pricing::lookup(model)?;
        let uncached = p.input_cost_per_token;
        if uncached <= 0.0 {
            return None;
        }
        Some((
            p.cache_read_cost_per_token.unwrap_or(uncached),
            p.cache_write_cost_per_token.unwrap_or(uncached),
            uncached,
        ))
    }

    /// Record per-model token counts and accumulate request cost for budget
    /// enforcement.
    pub fn record_tokens(&self, model: &str, rec: &TokenRecord) {
        // Budget input tokens: prefer the API-reported uncached count; when the
        // call site had no usage breakdown, fall back to tokens_sent.
        let mut input_tokens = rec.uncached_tokens;
        if rec.uncached_tokens == 0 && rec.cache_read_tokens == 0 && rec.cache_write_tokens == 0 {
            input_tokens = rec.tokens_sent;
        }
        let cost = self.estimate_cost(
            model,
            input_tokens,
            rec.output_tokens,
            rec.cache_read_tokens,
            rec.cache_write_tokens,
        );

        // Post-guard invariant (all providers): Headroom never forwards a
        // request larger than the original (handlers revert any inflation
        // before sending), so compression savings are >= 0 by construction. A
        // negative here is an intermediate token-count artifact that never
        // reached the model; clamp it so `total_tokens_saved` reflects
        // actually-forwarded bytes instead of surfacing spurious negatives.
        let tokens_saved = if rec.tokens_saved < 0 {
            tracing::debug!(
                model = %model,
                tokens_saved = rec.tokens_saved,
                "record_tokens: clamping negative tokens_saved to 0 (artifact; wire not inflated)"
            );
            0
        } else {
            rec.tokens_saved
        };

        let mut inner = self.inner.lock().unwrap();
        let m = &mut inner.m;
        *m.tokens_saved.entry(model.to_string()).or_default() += tokens_saved;
        *m.tokens_sent.entry(model.to_string()).or_default() += rec.tokens_sent;
        *m.requests.entry(model.to_string()).or_default() += 1;
        *m.cache_read.entry(model.to_string()).or_default() += rec.cache_read_tokens;
        *m.cache_write.entry(model.to_string()).or_default() += rec.cache_write_tokens;
        *m.cache_write_5m.entry(model.to_string()).or_default() += rec.cache_write_5m_tokens;
        *m.cache_write_1h.entry(model.to_string()).or_default() += rec.cache_write_1h_tokens;
        *m.uncached.entry(model.to_string()).or_default() += rec.uncached_tokens;

        if let Some(cost) = cost {
            inner.costs.push_back((Local::now(), cost));
            while inner.costs.len() > MAX_COST_ENTRIES {
                inner.costs.pop_front();
            }
            Self::prune_old(&mut inner);
        }
    }

    /// Remove entries older than the retention window, throttled to every 5 min.
    fn prune_old(inner: &mut Inner) {
        let now = Local::now();
        if (now - inner.last_prune).num_seconds() < 300 {
            return;
        }
        inner.last_prune = now;
        let cutoff = now - Duration::hours(COST_RETENTION_HOURS);
        while let Some(&(ts, _)) = inner.costs.front() {
            if ts < cutoff {
                inner.costs.pop_front();
            } else {
                break;
            }
        }
    }

    /// Cost accumulated in the current budget period.
    pub fn get_period_cost(&self) -> f64 {
        let now = Local::now();
        let cutoff = match self.budget_period.as_str() {
            "hourly" => now - Duration::hours(1),
            "monthly" => now
                .with_day(1)
                .unwrap_or(now)
                .with_hour(0)
                .and_then(|d| d.with_minute(0))
                .and_then(|d| d.with_second(0))
                .and_then(|d| d.with_nanosecond(0))
                .unwrap_or(now),
            // "daily" (default): midnight today.
            _ => now
                .with_hour(0)
                .and_then(|d| d.with_minute(0))
                .and_then(|d| d.with_second(0))
                .and_then(|d| d.with_nanosecond(0))
                .unwrap_or(now),
        };
        let inner = self.inner.lock().unwrap();
        inner
            .costs
            .iter()
            .filter(|(ts, _)| *ts >= cutoff)
            .map(|(_, c)| c)
            .sum()
    }

    /// `(allowed, remaining)`. Unlimited budget → `(true, +inf)`.
    pub fn check_budget(&self) -> (bool, f64) {
        let Some(limit) = self.budget_limit_usd else {
            return (true, f64::INFINITY);
        };
        let period_cost = self.get_period_cost();
        let remaining = limit - period_cost;
        (remaining > 0.0, remaining.max(0.0))
    }

    /// Per-model token statistics + monotonic `savings_usd`, shaped to match
    /// Python's `CostTracker.stats()` dict.
    pub fn stats(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let m = &inner.m;

        let mut models: Vec<&String> = m.tokens_saved.keys().collect();
        models.sort();

        let mut per_model = Map::new();
        let mut total_saved = 0i64;
        for model in &models {
            let saved = *m.tokens_saved.get(*model).unwrap_or(&0);
            let sent = *m.tokens_sent.get(*model).unwrap_or(&0);
            let reqs = *m.requests.get(*model).unwrap_or(&0);
            total_saved += saved;
            let reduction_pct = if saved + sent > 0 {
                round_n(saved as f64 / (saved + sent) as f64 * 100.0, 1)
            } else {
                0.0
            };
            per_model.insert(
                (*model).clone(),
                json!({
                    "requests": reqs,
                    "tokens_saved": saved,
                    "tokens_sent": sent,
                    "cache_write_5m_tokens": m.cache_write_5m.get(*model).copied().unwrap_or(0),
                    "cache_write_1h_tokens": m.cache_write_1h.get(*model).copied().unwrap_or(0),
                    "reduction_pct": reduction_pct,
                }),
            );
        }

        let mut cost_with_headroom = 0.0f64;
        let mut total_input_tokens = 0i64;
        for model in m.tokens_saved.keys() {
            let sent = *m.tokens_sent.get(model).unwrap_or(&0);
            let cr = *m.cache_read.get(model).unwrap_or(&0);
            let cw = *m.cache_write.get(model).unwrap_or(&0);
            let uncached = *m.uncached.get(model).unwrap_or(&0);
            total_input_tokens += sent;
            if let Some((cr_p, cw_p, unc_p)) = Self::cache_prices(model) {
                if cr + cw + uncached > 0 {
                    cost_with_headroom +=
                        cr as f64 * cr_p + cw as f64 * cw_p + uncached as f64 * unc_p;
                } else {
                    cost_with_headroom += sent as f64 * unc_p;
                }
            }
        }

        // Compression savings: saved tokens at the model's list input price.
        let mut savings_usd = 0.0f64;
        for (model, &saved) in &m.tokens_saved {
            if saved <= 0 {
                continue;
            }
            if let Some(price) = Self::list_price_per_token(model) {
                savings_usd += saved as f64 * price;
            }
        }

        let sum5m: i64 = m.cache_write_5m.values().sum();
        let sum1h: i64 = m.cache_write_1h.values().sum();

        json!({
            "total_tokens_saved": total_saved,
            "total_input_tokens": total_input_tokens,
            "total_input_cost_usd": round_n(cost_with_headroom, 4),
            "cache_write_5m_tokens": sum5m,
            "cache_write_1h_tokens": sum1h,
            "per_model": Value::Object(per_model),
            "cost_with_headroom_usd": round_n(cost_with_headroom, 4),
            "savings_usd": round_n(savings_usd, 4),
            "budget_limit_usd": self.budget_limit_usd,
            "budget_period": self.budget_period,
        })
    }

    /// Reset all in-memory counters (test/debug helper).
    pub fn reset_runtime(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.costs.clear();
        inner.last_prune = Local::now();
        inner.m = PerModel::default();
    }
}

// ── dashboard builder helpers (decoupled from Python's PrometheusMetrics) ──

/// Look up a model's list input price per token from a caller-supplied
/// map of `model_name → input_price_per_token`. Returns the first model
/// whose name matches the provider via simple prefix heuristics.
pub fn find_model_input_price(provider: &str, model_prices: &HashMap<String, f64>) -> Option<f64> {
    for (model, price) in model_prices {
        if provider_model_matches(provider, model) {
            return Some(*price);
        }
    }
    None
}

/// Simple model→provider matching (mirrors Python's prefix heuristics in
/// `build_prefix_cache_stats`).
fn provider_model_matches(provider: &str, model: &str) -> bool {
    match provider {
        "anthropic" | "bedrock" => model.contains("claude"),
        "openai" => ["gpt", "o1", "o3", "o4"].iter().any(|p| model.contains(p)),
        "gemini" => model.contains("gemini"),
        _ => false,
    }
}

/// Provider-level cache input (one entry per provider from
/// `metrics.cache_by_provider`).
#[derive(Debug, Clone, Default)]
pub struct ProviderCacheInput {
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub cache_write_5m_requests: i64,
    pub cache_write_1h_requests: i64,
    pub uncached_input_tokens: i64,
    pub requests: i64,
    pub hit_requests: i64,
    pub bust_count: i64,
    pub bust_write_tokens: i64,
}

/// Per-provider cache-miss attribution counts.
#[derive(Debug, Clone, Default)]
pub struct ProviderMissAttribution {
    pub ttl_expiry: i64,
    pub prefix_change: i64,
    pub unknown: i64,
}

/// Prefix freeze observability counters from `metrics.prefix_freeze_*`.
#[derive(Debug, Clone, Default)]
pub struct PrefixFreezeInput {
    pub busts_avoided: i64,
    pub tokens_preserved: i64,
    pub compression_foregone_tokens: i64,
}

/// Compression-vs-cache counters from `metrics.*`.
#[derive(Debug, Clone, Default)]
pub struct CompressionVsCacheInput {
    pub tokens_saved_by_compression: i64,
    pub tokens_lost_to_cache_bust: i64,
    pub cache_bust_count: i64,
}

/// Everything the caller needs to pass to [`build_prefix_cache_stats`].
/// Replaces the Python function's `(PrometheusMetrics, CostTracker)` pair
/// with plain, decoupled data.
pub struct PrefixCacheStatsInput<'a> {
    pub providers: &'a HashMap<String, ProviderCacheInput>,
    /// `model_name → list_input_price_per_token`. The builder picks the
    /// first model matching each provider via [`find_model_input_price`].
    pub model_prices: &'a HashMap<String, f64>,
    pub miss_attribution: &'a HashMap<String, ProviderMissAttribution>,
    pub prefix_freeze: &'a PrefixFreezeInput,
    pub compression_vs_cache: &'a CompressionVsCacheInput,
    pub tokens_saved_by_compression: i64,
}

// ── output structs for `build_prefix_cache_stats` ──

/// Hit-rate breakdown for a provider or aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheHitRates {
    pub token_hit_rate: f64,
    pub request_hit_rate: f64,
}

/// Observed TTL bucket stats (5m / 1h).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheTtlBuckets {
    pub tokens_5m: i64,
    pub requests_5m: i64,
    pub tokens_1h: i64,
    pub requests_1h: i64,
    pub total_tokens: i64,
    pub mix_5m_pct: f64,
    pub mix_1h_pct: f64,
    pub active_buckets: Vec<String>,
}

/// Per-provider cache statistics (the `by_provider` entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCacheStats {
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub cache_write_5m_requests: i64,
    pub cache_write_1h_requests: i64,
    pub uncached_input_tokens: i64,
    pub requests: i64,
    pub hit_requests: i64,
    pub hit_rates: CacheHitRates,
    pub bust_count: i64,
    pub bust_write_tokens: i64,
    pub read_discount: String,
    pub write_premium: String,
    pub savings_usd: f64,
    pub write_premium_usd: f64,
    pub net_savings_usd: f64,
    pub label: String,
    pub observed_ttl_buckets: CacheTtlBuckets,
}

/// Aggregate totals across all providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheProviderTotals {
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub cache_write_5m_requests: i64,
    pub cache_write_1h_requests: i64,
    pub uncached_input_tokens: i64,
    pub requests: i64,
    pub hit_requests: i64,
    pub hit_rates: CacheHitRates,
    pub bust_count: i64,
    pub bust_write_tokens: i64,
    pub savings_usd: f64,
    pub write_premium_usd: f64,
    pub net_savings_usd: f64,
    pub observed_ttl_buckets: CacheTtlBuckets,
}

/// Cache-miss attribution totals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMissAttributionTotals {
    pub ttl_expiry: i64,
    pub prefix_change: i64,
    pub unknown: i64,
    pub total: i64,
    pub ttl_expiry_pct: f64,
    pub prefix_change_pct: f64,
}

/// Cache-miss attribution (aggregate + per-provider).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMissAttribution {
    pub totals: CacheMissAttributionTotals,
    pub by_provider: HashMap<String, HashMap<String, i64>>,
}

/// Prefix freeze observability stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefixFreezeStats {
    pub busts_avoided: i64,
    pub tokens_preserved: i64,
    pub compression_foregone_tokens: i64,
    pub net_benefit_tokens: i64,
}

/// Compression-vs-cache net benefit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionVsCacheStats {
    pub tokens_saved_by_compression: i64,
    pub tokens_lost_to_cache_bust: i64,
    pub cache_bust_count: i64,
    pub net_tokens: i64,
}

/// Full prefix-cache stats payload (returned by [`build_prefix_cache_stats`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefixCacheStats {
    pub by_provider: HashMap<String, ProviderCacheStats>,
    pub totals: CacheProviderTotals,
    pub miss_attribution: CacheMissAttribution,
    pub prefix_freeze: PrefixFreezeStats,
    pub compression_vs_cache: CompressionVsCacheStats,
}

/// Build provider-aware prefix-cache statistics for the dashboard.
///
/// Pure data — no dependency on `PrometheusMetrics` or `CostTracker`.
/// The caller provides [`PrefixCacheStatsInput`] which is the plain,
/// decoupled equivalent of the Python `(metrics, cost_tracker)` pair.
pub fn build_prefix_cache_stats(input: &PrefixCacheStatsInput) -> PrefixCacheStats {
    let mut by_provider: HashMap<String, ProviderCacheStats> = HashMap::new();

    let mut totals = CacheProviderTotals {
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_5m_tokens: 0,
        cache_write_1h_tokens: 0,
        cache_write_5m_requests: 0,
        cache_write_1h_requests: 0,
        uncached_input_tokens: 0,
        requests: 0,
        hit_requests: 0,
        hit_rates: CacheHitRates {
            token_hit_rate: 0.0,
            request_hit_rate: 0.0,
        },
        bust_count: 0,
        bust_write_tokens: 0,
        savings_usd: 0.0,
        write_premium_usd: 0.0,
        net_savings_usd: 0.0,
        observed_ttl_buckets: CacheTtlBuckets {
            tokens_5m: 0,
            requests_5m: 0,
            tokens_1h: 0,
            requests_1h: 0,
            total_tokens: 0,
            mix_5m_pct: 0.0,
            mix_1h_pct: 0.0,
            active_buckets: vec![],
        },
    };

    for (provider, pc) in input.providers {
        if pc.requests == 0 {
            continue;
        }

        let econ = cache_economics(provider);
        let read_mult = econ.read_multiplier;
        let write_mult = econ.write_multiplier;

        let input_price = find_model_input_price(provider, input.model_prices);

        let mut savings_usd = 0.0;
        let mut write_premium_usd = 0.0;
        if let Some(price) = input_price {
            savings_usd = pc.cache_read_tokens as f64 * price * (1.0 - read_mult);
            if write_mult > 1.0 {
                write_premium_usd = pc.cache_write_tokens as f64 * price * (write_mult - 1.0);
            }
        }

        let total_input = pc.cache_read_tokens + pc.cache_write_tokens + pc.uncached_input_tokens;
        let hit_rate = if total_input > 0 {
            round_n(pc.cache_read_tokens as f64 / total_input as f64 * 100.0, 1)
        } else {
            0.0
        };
        let request_hit_rate = if pc.requests > 0 {
            round_n(pc.hit_requests as f64 / pc.requests as f64 * 100.0, 1)
        } else {
            0.0
        };

        let total_ttl_tokens = pc.cache_write_5m_tokens + pc.cache_write_1h_tokens;
        let (mix_5m_pct, mix_1h_pct, active_buckets) = if total_ttl_tokens > 0 {
            let m5 = round_n(
                pc.cache_write_5m_tokens as f64 / total_ttl_tokens as f64 * 100.0,
                1,
            );
            let m1 = round_n(
                pc.cache_write_1h_tokens as f64 / total_ttl_tokens as f64 * 100.0,
                1,
            );
            let mut active = Vec::new();
            if pc.cache_write_5m_tokens > 0 {
                active.push("5m".into());
            }
            if pc.cache_write_1h_tokens > 0 {
                active.push("1h".into());
            }
            (m5, m1, active)
        } else {
            (0.0, 0.0, vec![])
        };

        by_provider.insert(
            provider.clone(),
            ProviderCacheStats {
                cache_read_tokens: pc.cache_read_tokens,
                cache_write_tokens: pc.cache_write_tokens,
                cache_write_5m_tokens: pc.cache_write_5m_tokens,
                cache_write_1h_tokens: pc.cache_write_1h_tokens,
                cache_write_5m_requests: pc.cache_write_5m_requests,
                cache_write_1h_requests: pc.cache_write_1h_requests,
                uncached_input_tokens: pc.uncached_input_tokens,
                requests: pc.requests,
                hit_requests: pc.hit_requests,
                hit_rates: CacheHitRates {
                    token_hit_rate: hit_rate,
                    request_hit_rate,
                },
                bust_count: pc.bust_count,
                bust_write_tokens: pc.bust_write_tokens,
                read_discount: format!("{:.0}%", (1.0 - read_mult) * 100.0),
                write_premium: if write_mult > 1.0 {
                    format!("{:.0}%", (write_mult - 1.0) * 100.0)
                } else {
                    "none".into()
                },
                savings_usd: round_n(savings_usd, 4),
                write_premium_usd: round_n(write_premium_usd, 4),
                net_savings_usd: round_n(savings_usd - write_premium_usd, 4),
                label: econ.label.to_string(),
                observed_ttl_buckets: CacheTtlBuckets {
                    tokens_5m: pc.cache_write_5m_tokens,
                    requests_5m: pc.cache_write_5m_requests,
                    tokens_1h: pc.cache_write_1h_tokens,
                    requests_1h: pc.cache_write_1h_requests,
                    total_tokens: total_ttl_tokens,
                    mix_5m_pct,
                    mix_1h_pct,
                    active_buckets,
                },
            },
        );

        totals.cache_read_tokens += pc.cache_read_tokens;
        totals.cache_write_tokens += pc.cache_write_tokens;
        totals.cache_write_5m_tokens += pc.cache_write_5m_tokens;
        totals.cache_write_1h_tokens += pc.cache_write_1h_tokens;
        totals.cache_write_5m_requests += pc.cache_write_5m_requests;
        totals.cache_write_1h_requests += pc.cache_write_1h_requests;
        totals.uncached_input_tokens += pc.uncached_input_tokens;
        totals.requests += pc.requests;
        totals.hit_requests += pc.hit_requests;
        totals.bust_count += pc.bust_count;
        totals.bust_write_tokens += pc.bust_write_tokens;
        totals.savings_usd += savings_usd;
        totals.write_premium_usd += write_premium_usd;
    }

    // Totals hit rates.
    let _total_input =
        totals.cache_read_tokens + totals.cache_write_tokens + totals.uncached_input_tokens;
    totals.hit_rates = CacheHitRates {
        token_hit_rate: if _total_input > 0 {
            round_n(
                totals.cache_read_tokens as f64 / _total_input as f64 * 100.0,
                1,
            )
        } else {
            0.0
        },
        request_hit_rate: if totals.requests > 0 {
            round_n(
                totals.hit_requests as f64 / totals.requests as f64 * 100.0,
                1,
            )
        } else {
            0.0
        },
    };
    totals.net_savings_usd = round_n(totals.savings_usd - totals.write_premium_usd, 4);
    totals.savings_usd = round_n(totals.savings_usd, 4);
    totals.write_premium_usd = round_n(totals.write_premium_usd, 4);

    // Totals observed TTL buckets.
    let total_observed = totals.cache_write_5m_tokens + totals.cache_write_1h_tokens;
    let (m5, m1, active) = if total_observed > 0 {
        let m5 = round_n(
            totals.cache_write_5m_tokens as f64 / total_observed as f64 * 100.0,
            1,
        );
        let m1 = round_n(
            totals.cache_write_1h_tokens as f64 / total_observed as f64 * 100.0,
            1,
        );
        let mut active = Vec::new();
        if totals.cache_write_5m_tokens > 0 {
            active.push("5m".into());
        }
        if totals.cache_write_1h_tokens > 0 {
            active.push("1h".into());
        }
        (m5, m1, active)
    } else {
        (0.0, 0.0, vec![])
    };
    totals.observed_ttl_buckets = CacheTtlBuckets {
        tokens_5m: totals.cache_write_5m_tokens,
        requests_5m: totals.cache_write_5m_requests,
        tokens_1h: totals.cache_write_1h_tokens,
        requests_1h: totals.cache_write_1h_requests,
        total_tokens: total_observed,
        mix_5m_pct: m5,
        mix_1h_pct: m1,
        active_buckets: active,
    };

    // Cache-miss attribution (#1313).
    let mut miss_by_provider: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut miss_totals = CacheMissAttributionTotals {
        ttl_expiry: 0,
        prefix_change: 0,
        unknown: 0,
        total: 0,
        ttl_expiry_pct: 0.0,
        prefix_change_pct: 0.0,
    };

    for (provider, reasons) in input.miss_attribution {
        let total = reasons.ttl_expiry + reasons.prefix_change + reasons.unknown;
        if total == 0 {
            continue;
        }
        let mut provider_reasons = HashMap::new();
        provider_reasons.insert("ttl_expiry".into(), reasons.ttl_expiry);
        provider_reasons.insert("prefix_change".into(), reasons.prefix_change);
        provider_reasons.insert("unknown".into(), reasons.unknown);
        provider_reasons.insert("total".into(), total);
        miss_by_provider.insert(provider.clone(), provider_reasons);

        miss_totals.ttl_expiry += reasons.ttl_expiry;
        miss_totals.prefix_change += reasons.prefix_change;
        miss_totals.unknown += reasons.unknown;
        miss_totals.total += total;
    }

    let attributed = miss_totals.ttl_expiry + miss_totals.prefix_change;
    miss_totals.ttl_expiry_pct = if attributed > 0 {
        round_n(miss_totals.ttl_expiry as f64 / attributed as f64 * 100.0, 1)
    } else {
        0.0
    };
    miss_totals.prefix_change_pct = if attributed > 0 {
        round_n(
            miss_totals.prefix_change as f64 / attributed as f64 * 100.0,
            1,
        )
    } else {
        0.0
    };

    PrefixCacheStats {
        by_provider,
        totals,
        miss_attribution: CacheMissAttribution {
            totals: miss_totals,
            by_provider: miss_by_provider,
        },
        prefix_freeze: PrefixFreezeStats {
            busts_avoided: input.prefix_freeze.busts_avoided,
            tokens_preserved: input.prefix_freeze.tokens_preserved,
            compression_foregone_tokens: input.prefix_freeze.compression_foregone_tokens,
            net_benefit_tokens: input.prefix_freeze.tokens_preserved
                - input.prefix_freeze.compression_foregone_tokens,
        },
        compression_vs_cache: CompressionVsCacheStats {
            tokens_saved_by_compression: input.compression_vs_cache.tokens_saved_by_compression,
            tokens_lost_to_cache_bust: input.compression_vs_cache.tokens_lost_to_cache_bust,
            cache_bust_count: input.compression_vs_cache.cache_bust_count,
            net_tokens: input.compression_vs_cache.tokens_saved_by_compression
                - input.compression_vs_cache.tokens_lost_to_cache_bust,
        },
    }
}

// ── build_session_summary ──

/// Per-request compression log entry (decoupled from Python's `_RequestLogEntry`).
#[derive(Debug, Clone, Default)]
pub struct CompressedRequestLog {
    pub savings_percent: f64,
    pub tokens_saved: i64,
    pub input_tokens_original: i64,
    pub input_tokens_optimized: i64,
    pub transforms_applied: Option<Vec<String>>,
    pub is_passthrough: bool,
}

/// Cost stats from [`CostTracker::stats()`] (the `cost_stats` dict).
#[derive(Debug, Clone, Default)]
pub struct CostSummary {
    pub cost_with_headroom_usd: f64,
    pub savings_usd: f64,
}

/// MCP-side compression events (from `_aggregate_mcp_events`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpEvents {
    pub compressions: i64,
    pub tokens_removed: i64,
    pub retrievals: i64,
}

/// Codex WS session counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexWsStats {
    pub units_total: i64,
    pub units_modified: i64,
    pub tokens_saved: i64,
}

/// Everything the caller needs to pass to [`build_session_summary`].
pub struct SessionSummaryInput<'a> {
    pub mode: &'a str,
    pub compressed_requests: &'a [CompressedRequestLog],
    pub cache_net_savings_usd: f64,
    pub cli_tokens_avoided: i64,
    pub total_tokens_before: i64,
    pub tokens_saved_total: i64,
    pub requests_by_model: &'a HashMap<String, i64>,
    pub cost_stats: Option<&'a CostSummary>,
    pub mcp_events: Option<&'a McpEvents>,
    pub codex_ws: Option<&'a CodexWsStats>,
}

/// Compression metrics for the session summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionSummary {
    pub requests_compressed: usize,
    pub avg_compression_pct: f64,
    pub best_compression_pct: f64,
    pub best_detail: String,
    pub total_tokens_removed: i64,
    pub cli_filtering_tokens_avoided: i64,
    pub total_tokens_saved_with_cli_filtering: i64,
    pub total_tokens_before_with_cli_filtering: i64,
    pub rtk_tokens_avoided: i64,
    pub total_tokens_saved_with_rtk: i64,
    pub total_tokens_before_with_rtk: i64,
}

/// Cost breakdown by savings layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub cache_savings_usd: f64,
    pub compression_savings_usd: f64,
}

/// Cost summary for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSummaryOutput {
    pub without_headroom_usd: f64,
    pub with_headroom_usd: f64,
    pub total_saved_usd: f64,
    pub savings_pct: f64,
    pub breakdown: CostBreakdown,
}

/// Full session summary payload (returned by [`build_session_summary`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub mode: String,
    pub api_requests: i64,
    pub primary_model: String,
    pub compression: CompressionSummary,
    pub uncompressed_requests: HashMap<String, i64>,
    pub cost: CostSummaryOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpEvents>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_ws: Option<CodexWsStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
}

/// Summarize why requests weren't compressed, mirroring Python's
/// `uncompressed_reasons` categorization from `_RequestLogEntry` data.
pub fn summarize_uncompressed_reasons(requests: &[CompressedRequestLog]) -> HashMap<String, i64> {
    let mut reasons: HashMap<String, i64> = HashMap::new();
    reasons.insert("prefix_frozen".into(), 0);
    reasons.insert("too_small".into(), 0);
    reasons.insert("passthrough".into(), 0);
    reasons.insert("no_compressible_content".into(), 0);

    for entry in requests {
        if entry.is_passthrough {
            *reasons.entry("passthrough".into()).or_default() += 1;
            continue;
        }
        if entry.tokens_saved > 0 {
            // This is a compressed request — skip.
            continue;
        }
        if entry.input_tokens_original > 0 {
            let transforms = entry.transforms_applied.as_deref().unwrap_or(&[]);
            if transforms.is_empty() {
                *reasons.entry("prefix_frozen".into()).or_default() += 1;
            } else if transforms
                .iter()
                .all(|t| t.contains("excluded") || t.contains("protected"))
            {
                *reasons.entry("no_compressible_content".into()).or_default() += 1;
            } else if entry.input_tokens_original < 500 {
                *reasons.entry("too_small".into()).or_default() += 1;
            } else {
                *reasons.entry("prefix_frozen".into()).or_default() += 1;
            }
        }
    }
    reasons
}

/// Build a human-readable session summary from plain data.
///
/// This is the headline view users see first in `/stats` — designed to
/// answer "is Headroom working?" at a glance. Decoupled from the Python
/// `proxy` and `metrics` objects.
pub fn build_session_summary(input: &SessionSummaryInput) -> SessionSummary {
    let compressed: Vec<&CompressedRequestLog> = input
        .compressed_requests
        .iter()
        .filter(|r| r.tokens_saved > 0)
        .collect();

    let avg_compression = if !compressed.is_empty() {
        round_n(
            compressed.iter().map(|r| r.savings_percent).sum::<f64>() / compressed.len() as f64,
            1,
        )
    } else {
        0.0
    };
    let (best_compression, best_detail) = compressed
        .iter()
        .max_by(|a, b| {
            a.savings_percent
                .partial_cmp(&b.savings_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|best| {
            (
                best.savings_percent,
                format!(
                    "{} → {} tokens",
                    best.input_tokens_original, best.input_tokens_optimized
                ),
            )
        })
        .unwrap_or((0.0, String::new()));

    let cost_with = input
        .cost_stats
        .map(|c| c.cost_with_headroom_usd)
        .unwrap_or(0.0);
    let compression_savings = input.cost_stats.map(|c| c.savings_usd).unwrap_or(0.0);
    let total_saved_usd = round_n(compression_savings, 2);
    let cost_without = cost_with + compression_savings;
    let savings_pct_cost = if cost_without > 0.0 {
        round_n(total_saved_usd / cost_without * 100.0, 1)
    } else {
        0.0
    };

    let primary_model = input
        .requests_by_model
        .iter()
        .max_by_key(|(_, &count)| count)
        .map(|(m, _)| m.clone())
        .unwrap_or_else(|| "unknown".into());
    let api_requests: i64 = input
        .requests_by_model
        .iter()
        .filter(|(k, _)| !k.contains("count_tokens"))
        .map(|(_, &v)| v)
        .sum();

    let uncompressed = summarize_uncompressed_reasons(input.compressed_requests);

    let mut summary = SessionSummary {
        mode: input.mode.to_string(),
        api_requests,
        primary_model,
        compression: CompressionSummary {
            requests_compressed: compressed.len(),
            avg_compression_pct: avg_compression,
            best_compression_pct: best_compression,
            best_detail,
            total_tokens_removed: input.tokens_saved_total,
            cli_filtering_tokens_avoided: input.cli_tokens_avoided,
            total_tokens_saved_with_cli_filtering: input.tokens_saved_total
                + input.cli_tokens_avoided,
            total_tokens_before_with_cli_filtering: input.total_tokens_before,
            rtk_tokens_avoided: input.cli_tokens_avoided,
            total_tokens_saved_with_rtk: input.tokens_saved_total + input.cli_tokens_avoided,
            total_tokens_before_with_rtk: input.total_tokens_before,
        },
        uncompressed_requests: uncompressed.into_iter().filter(|(_, v)| *v > 0).collect(),
        cost: CostSummaryOutput {
            without_headroom_usd: round_n(cost_without, 2),
            with_headroom_usd: round_n(cost_with, 2),
            total_saved_usd,
            savings_pct: savings_pct_cost,
            breakdown: CostBreakdown {
                cache_savings_usd: round_n(input.cache_net_savings_usd, 2),
                compression_savings_usd: round_n(compression_savings, 2),
            },
        },
        mcp: input.mcp_events.cloned(),
        codex_ws: input.codex_ws.cloned(),
        tip: None,
    };

    if input.mode == "cache" {
        let prefix_frozen = summary
            .uncompressed_requests
            .get("prefix_frozen")
            .copied()
            .unwrap_or(0);
        if prefix_frozen > 10 {
            summary.tip = Some(
                "Most requests are prefix-frozen. Set HEADROOM_MODE=token \
                 to compress frozen messages and extend your session by ~25-35%."
                    .into(),
            );
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_economics_defaults_to_anthropic() {
        assert_eq!(cache_economics("openai").read_multiplier, 0.5);
        assert_eq!(cache_economics("gemini").write_multiplier, 1.0);
        // Unknown provider → anthropic defaults.
        let a = cache_economics("mystery");
        assert_eq!(a.read_multiplier, 0.1);
        assert_eq!(a.write_multiplier, 1.25);
    }

    #[test]
    fn header_safe_collapses_enriched_tags() {
        let t = vec![
            "smart_crush:3:foo,bar".to_string(),
            "read_lifecycle:stale:/a/b,c".to_string(),
            "router:excluded:tool".to_string(),
        ];
        assert_eq!(
            header_safe_transforms(&t),
            vec![
                "smart_crush:3",
                "read_lifecycle:stale",
                "router:excluded:tool"
            ]
        );
    }

    #[test]
    fn merge_cost_stats_none_passthrough() {
        assert!(merge_cost_stats(None, &json!({}), 0).is_none());
    }

    #[test]
    fn merge_cost_stats_separates_layers() {
        let cost = json!({"savings_usd": 1.23456, "cost_with_headroom_usd": 5.0});
        let cache = json!({"totals": {"net_savings_usd": 0.5}});
        let merged = merge_cost_stats(Some(&cost), &cache, 42).unwrap();
        assert_eq!(merged["savings_usd"], json!(1.2346));
        assert_eq!(merged["compression_savings_usd"], json!(1.2346));
        assert_eq!(merged["cache_savings_usd"], json!(0.5));
        assert_eq!(merged["cli_tokens_avoided"], json!(42));
        assert_eq!(merged["cli_tokens_included_in_compression"], json!(true));
        // Original fields preserved.
        assert_eq!(merged["cost_with_headroom_usd"], json!(5.0));
    }

    #[test]
    fn estimate_cost_known_and_unknown() {
        let t = CostTracker::new(None, "daily");
        // Known model prices; unknown → None.
        assert!(t.estimate_cost("claude-sonnet-4", 1000, 0, 0, 0).is_some());
        assert!(t.estimate_cost("test-model", 1000, 0, 0, 0).is_none());
        // Zero tokens → total 0 → None.
        assert!(t.estimate_cost("claude-sonnet-4", 0, 0, 0, 0).is_none());
    }

    #[test]
    fn record_and_stats_monotonic_savings() {
        let t = CostTracker::new(None, "daily");
        t.record_tokens(
            "claude-sonnet-4",
            &TokenRecord {
                tokens_saved: 1000,
                tokens_sent: 500,
                uncached_tokens: 500,
                output_tokens: 100,
                ..Default::default()
            },
        );
        let s = t.stats();
        assert_eq!(s["total_tokens_saved"], json!(1000));
        assert_eq!(s["per_model"]["claude-sonnet-4"]["requests"], json!(1));
        // reduction_pct = 1000/1500 = 66.7
        assert_eq!(
            s["per_model"]["claude-sonnet-4"]["reduction_pct"],
            json!(66.7)
        );
        // savings_usd = 1000 * (3/1e6) = 0.003
        assert_eq!(s["savings_usd"], json!(0.003));
    }

    #[test]
    fn record_tokens_clamps_negative_savings_to_zero() {
        let t = CostTracker::new(None, "daily");
        // A negative tokens_saved is an intermediate token-count artifact; the
        // wire is never inflated, so it must not drag total savings below what
        // was actually forwarded.
        t.record_tokens(
            "claude-sonnet-4",
            &TokenRecord {
                tokens_saved: -250,
                tokens_sent: 500,
                uncached_tokens: 500,
                output_tokens: 100,
                ..Default::default()
            },
        );
        assert_eq!(t.stats()["total_tokens_saved"], json!(0));
        // A subsequent real saving accumulates from 0, not from -250.
        t.record_tokens(
            "claude-sonnet-4",
            &TokenRecord {
                tokens_saved: 400,
                tokens_sent: 500,
                uncached_tokens: 500,
                output_tokens: 100,
                ..Default::default()
            },
        );
        assert_eq!(t.stats()["total_tokens_saved"], json!(400));
    }

    #[test]
    fn budget_unlimited_and_limited() {
        let t = CostTracker::new(None, "daily");
        let (allowed, remaining) = t.check_budget();
        assert!(allowed && remaining.is_infinite());

        let t = CostTracker::new(Some(1000.0), "daily");
        let (allowed, _remaining) = t.check_budget();
        assert!(allowed); // no cost recorded yet
    }

    #[test]
    fn stats_empty_tracker() {
        let t = CostTracker::new(Some(5.0), "monthly");
        let s = t.stats();
        assert_eq!(s["total_tokens_saved"], json!(0));
        assert_eq!(s["budget_limit_usd"], json!(5.0));
        assert_eq!(s["budget_period"], json!("monthly"));
    }

    // ── build_prefix_cache_stats tests ──

    #[test]
    fn prefix_cache_stats_basic() {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".into(),
            ProviderCacheInput {
                cache_read_tokens: 10000,
                cache_write_tokens: 5000,
                uncached_input_tokens: 2000,
                requests: 50,
                hit_requests: 30,
                bust_count: 2,
                bust_write_tokens: 200,
                ..Default::default()
            },
        );
        let mut model_prices = HashMap::new();
        model_prices.insert("claude-sonnet-4".into(), 3.0 / 1_000_000.0);

        let input = PrefixCacheStatsInput {
            providers: &providers,
            model_prices: &model_prices,
            miss_attribution: &HashMap::new(),
            prefix_freeze: &PrefixFreezeInput::default(),
            compression_vs_cache: &CompressionVsCacheInput::default(),
            tokens_saved_by_compression: 0,
        };
        let stats = build_prefix_cache_stats(&input);

        assert_eq!(stats.totals.cache_read_tokens, 10000);
        assert_eq!(stats.totals.requests, 50);
        assert_eq!(stats.totals.bust_count, 2);
        // gross savings = 10000 * 3e-6 * (1.0 - 0.1) = 0.027
        assert!((stats.totals.savings_usd - 0.027).abs() < 0.001);
        // write premium = 5000 * 3e-6 * (1.25 - 1.0) = 0.00375
        assert!((stats.totals.write_premium_usd - 0.00375).abs() < 0.0001);
        // net savings subtract the write premium: 0.027 - 0.00375 = 0.02325
        assert!((stats.totals.net_savings_usd - 0.02325).abs() < 0.0001);
        // token_hit_rate = 10000 / 17000 * 100 ≈ 58.8
        assert!((stats.totals.hit_rates.token_hit_rate - 58.8).abs() < 0.2);

        let anthropic = &stats.by_provider["anthropic"];
        assert_eq!(anthropic.read_discount, "90%");
        assert_eq!(anthropic.write_premium, "25%");
    }

    #[test]
    fn prefix_cache_stats_net_subtracts_write_premium() {
        // Regression for #1800: net_savings_usd must subtract the cache-write
        // premium (1.25x multiplier) rather than report gross read savings.
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".into(),
            ProviderCacheInput {
                cache_read_tokens: 10000,
                cache_write_tokens: 40000,
                uncached_input_tokens: 0,
                requests: 10,
                hit_requests: 5,
                ..Default::default()
            },
        );
        let mut model_prices = HashMap::new();
        model_prices.insert("claude-sonnet-4".into(), 3.0 / 1_000_000.0);

        let input = PrefixCacheStatsInput {
            providers: &providers,
            model_prices: &model_prices,
            miss_attribution: &HashMap::new(),
            prefix_freeze: &PrefixFreezeInput::default(),
            compression_vs_cache: &CompressionVsCacheInput::default(),
            tokens_saved_by_compression: 0,
        };
        let stats = build_prefix_cache_stats(&input);

        // gross savings = 10000 * 3e-6 * 0.9 = 0.027
        // write premium = 40000 * 3e-6 * 0.25 = 0.03 (exceeds the read discount)
        // net = 0.027 - 0.03 = -0.003 (workload is cache-write dominated)
        let provider = &stats.by_provider["anthropic"];
        assert!((provider.savings_usd - 0.027).abs() < 0.0001);
        assert!((provider.write_premium_usd - 0.03).abs() < 0.0001);
        assert!((provider.net_savings_usd - (-0.003)).abs() < 0.0001);

        // Totals mirror the provider-level net.
        assert!((stats.totals.savings_usd - 0.027).abs() < 0.0001);
        assert!((stats.totals.write_premium_usd - 0.03).abs() < 0.0001);
        assert!((stats.totals.net_savings_usd - (-0.003)).abs() < 0.0001);
    }

    #[test]
    fn prefix_cache_stats_empty_providers() {
        let input = PrefixCacheStatsInput {
            providers: &HashMap::new(),
            model_prices: &HashMap::new(),
            miss_attribution: &HashMap::new(),
            prefix_freeze: &PrefixFreezeInput::default(),
            compression_vs_cache: &CompressionVsCacheInput::default(),
            tokens_saved_by_compression: 0,
        };
        let stats = build_prefix_cache_stats(&input);
        assert_eq!(stats.totals.requests, 0);
        assert!(stats.by_provider.is_empty());
        assert_eq!(stats.totals.net_savings_usd, 0.0);
    }

    #[test]
    fn prefix_cache_stats_miss_attribution() {
        let mut miss = HashMap::new();
        miss.insert(
            "anthropic".into(),
            ProviderMissAttribution {
                ttl_expiry: 30,
                prefix_change: 20,
                unknown: 5,
            },
        );
        let input = PrefixCacheStatsInput {
            providers: &HashMap::new(),
            model_prices: &HashMap::new(),
            miss_attribution: &miss,
            prefix_freeze: &PrefixFreezeInput::default(),
            compression_vs_cache: &CompressionVsCacheInput::default(),
            tokens_saved_by_compression: 0,
        };
        let stats = build_prefix_cache_stats(&input);
        let m = &stats.miss_attribution.totals;
        assert_eq!(m.ttl_expiry, 30);
        assert_eq!(m.prefix_change, 20);
        assert_eq!(m.total, 55);
        // 30 / (30+20) * 100 = 60.0
        assert!((m.ttl_expiry_pct - 60.0).abs() < 0.1);
        assert!((m.prefix_change_pct - 40.0).abs() < 0.1);
    }

    #[test]
    fn prefix_cache_stats_ttl_buckets() {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".into(),
            ProviderCacheInput {
                cache_write_5m_tokens: 3000,
                cache_write_1h_tokens: 7000,
                cache_write_5m_requests: 3,
                cache_write_1h_requests: 7,
                requests: 10,
                ..Default::default()
            },
        );
        let input = PrefixCacheStatsInput {
            providers: &providers,
            model_prices: &HashMap::new(),
            miss_attribution: &HashMap::new(),
            prefix_freeze: &PrefixFreezeInput::default(),
            compression_vs_cache: &CompressionVsCacheInput::default(),
            tokens_saved_by_compression: 0,
        };
        let stats = build_prefix_cache_stats(&input);
        let ttl = &stats.by_provider["anthropic"].observed_ttl_buckets;
        assert_eq!(ttl.tokens_5m, 3000);
        assert_eq!(ttl.tokens_1h, 7000);
        assert_eq!(ttl.total_tokens, 10000);
        // 3000/10000 * 100 = 30.0
        assert!((ttl.mix_5m_pct - 30.0).abs() < 0.1);
        assert!(ttl.active_buckets.iter().any(|b| b == "5m"));
        assert!(ttl.active_buckets.iter().any(|b| b == "1h"));
    }

    #[test]
    fn provider_model_matches_basic() {
        assert!(provider_model_matches("anthropic", "claude-sonnet-4"));
        assert!(provider_model_matches("bedrock", "claude-3-haiku"));
        assert!(provider_model_matches("openai", "gpt-4o"));
        assert!(provider_model_matches("openai", "o1-mini"));
        assert!(provider_model_matches("gemini", "gemini-2.5-pro"));
        assert!(!provider_model_matches("anthropic", "gpt-4o"));
        assert!(!provider_model_matches("openai", "claude-sonnet-4"));
    }

    #[test]
    fn find_model_price_basic() {
        let mut prices = HashMap::new();
        prices.insert("claude-sonnet-4".into(), 3e-6);
        prices.insert("gpt-4o".into(), 5e-6);
        assert_eq!(find_model_input_price("anthropic", &prices), Some(3e-6));
        assert_eq!(find_model_input_price("openai", &prices), Some(5e-6));
        assert_eq!(find_model_input_price("gemini", &prices), None);
    }

    // ── build_session_summary tests ──

    #[test]
    fn session_summary_basic() {
        let requests = vec![
            CompressedRequestLog {
                savings_percent: 25.0,
                tokens_saved: 500,
                input_tokens_original: 2000,
                input_tokens_optimized: 1500,
                ..Default::default()
            },
            CompressedRequestLog {
                input_tokens_original: 300,
                ..Default::default()
            },
        ];
        let mut models = HashMap::new();
        models.insert("claude-sonnet-4".into(), 42);
        models.insert("count_tokens".into(), 5);

        let input = SessionSummaryInput {
            mode: "token",
            compressed_requests: &requests,
            cache_net_savings_usd: 0.0,
            cli_tokens_avoided: 100,
            total_tokens_before: 10000,
            tokens_saved_total: 500,
            requests_by_model: &models,
            cost_stats: Some(&CostSummary {
                cost_with_headroom_usd: 1.5,
                savings_usd: 0.5,
            }),
            mcp_events: None,
            codex_ws: None,
        };
        let s = build_session_summary(&input);

        assert_eq!(s.mode, "token");
        assert_eq!(s.api_requests, 42);
        assert_eq!(s.primary_model, "claude-sonnet-4");
        assert_eq!(s.compression.requests_compressed, 1);
        assert_eq!(s.compression.total_tokens_removed, 500);
        assert_eq!(s.compression.cli_filtering_tokens_avoided, 100);
        assert_eq!(s.cost.with_headroom_usd, 1.5);
        assert_eq!(s.cost.total_saved_usd, 0.5);
        // cost_without = 1.5 + 0.5 = 2.0, savings_pct = 0.5/2.0*100 = 25.0
        assert!((s.cost.savings_pct - 25.0).abs() < 0.1);
        // uncompressed: 1 entry with tokens_saved=0 and no transforms → prefix_frozen
        assert_eq!(
            *s.uncompressed_requests.get("prefix_frozen").unwrap_or(&0),
            1
        );
    }

    #[test]
    fn session_summary_tip_when_prefix_frozen() {
        // >10 prefix_frozen entries + mode=cache → tip
        let requests: Vec<CompressedRequestLog> = (0..15)
            .map(|_| CompressedRequestLog {
                input_tokens_original: 1000,
                ..Default::default()
            })
            .collect();
        let models = HashMap::new();

        let input = SessionSummaryInput {
            mode: "cache",
            compressed_requests: &requests,
            cache_net_savings_usd: 0.0,
            cli_tokens_avoided: 0,
            total_tokens_before: 0,
            tokens_saved_total: 0,
            requests_by_model: &models,
            cost_stats: None,
            mcp_events: None,
            codex_ws: None,
        };
        let s = build_session_summary(&input);
        assert!(s.tip.is_some());
        assert!(s.tip.unwrap().contains("HEADROOM_MODE=token"));
    }

    #[test]
    fn session_summary_no_tip_in_token_mode() {
        let requests: Vec<CompressedRequestLog> = (0..15)
            .map(|_| CompressedRequestLog {
                input_tokens_original: 1000,
                ..Default::default()
            })
            .collect();
        let models = HashMap::new();

        let input = SessionSummaryInput {
            mode: "token",
            compressed_requests: &requests,
            cache_net_savings_usd: 0.0,
            cli_tokens_avoided: 0,
            total_tokens_before: 0,
            tokens_saved_total: 0,
            requests_by_model: &models,
            cost_stats: None,
            mcp_events: None,
            codex_ws: None,
        };
        let s = build_session_summary(&input);
        assert!(s.tip.is_none());
    }

    #[test]
    fn summarize_uncompressed_reasons_categories() {
        let requests = vec![
            // prefix_frozen: no transforms
            CompressedRequestLog {
                input_tokens_original: 1000,
                ..Default::default()
            },
            // too_small
            CompressedRequestLog {
                input_tokens_original: 400,
                transforms_applied: Some(vec!["other:stuff".into()]),
                ..Default::default()
            },
            // no_compressible_content
            CompressedRequestLog {
                input_tokens_original: 1000,
                transforms_applied: Some(vec!["excluded:tool".into(), "protected:msg".into()]),
                ..Default::default()
            },
            // passthrough
            CompressedRequestLog {
                is_passthrough: true,
                ..Default::default()
            },
        ];
        let reasons = summarize_uncompressed_reasons(&requests);
        assert_eq!(reasons["prefix_frozen"], 1);
        assert_eq!(reasons["too_small"], 1);
        assert_eq!(reasons["no_compressible_content"], 1);
        assert_eq!(reasons["passthrough"], 1);
    }
}

//! Vendored static model-pricing table (Rust port of the pricing surface in
//! `headroom/pricing/litellm_pricing.py` + `headroom/pricing/{anthropic,openai}_prices.py`).
//!
//! The Python proxy prices models via LiteLLM's community-maintained cost
//! database, which is unavailable in Rust. We vendor a static subset of current
//! prices for the `claude-*`, `gpt-*`, and `gemini-*` families and resolve a
//! model name to pricing with exact-then-longest-prefix matching (mirroring the
//! resolver rules in `litellm_pricing.py::_resolve_litellm_model_uncached` /
//! `get_model_pricing`, which try an exact key first and then family prefixes).
//!
//! **Staleness warning:** these prices are a point-in-time snapshot (verified
//! ~2026-07, USD per 1M tokens, converted to per-token here). LLM pricing
//! changes frequently — treat this table as best-effort and refresh it from the
//! upstream provider pricing pages. A refresh mechanism is a documented
//! follow-up (see the port plan). Costs are per **token** (per-1M / 1e6).

/// Context size at which the major catalogs publish a second, higher price
/// tier (LiteLLM spells it `*_above_200k_tokens`). A request's billed prompt is
/// compared against this to pick which rate applies. Port of
/// `cost.py::_LONG_CONTEXT_THRESHOLD_TOKENS`.
pub const LONG_CONTEXT_THRESHOLD_TOKENS: i64 = 200_000;

/// True when a billed prompt of `billed_prompt_tokens` falls in the
/// above-200k tier. Strictly greater, matching Python.
pub fn is_long_context(billed_prompt_tokens: i64) -> bool {
    billed_prompt_tokens > LONG_CONTEXT_THRESHOLD_TOKENS
}

/// Per-token pricing for one model. `None` cache fields mean the family has no
/// published cache pricing (or we do not vendor it).
///
/// The `*_above_200k` fields are the catalog's long-context tier: on the models
/// that publish one, a prompt past [`LONG_CONTEXT_THRESHOLD_TOKENS`] re-prices
/// the *whole* request — input, output and cache alike — not just the tokens
/// past the threshold. `None` means the family is flat-rated across its window,
/// and the accessors below fall back per rate to the base one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    pub cache_read_cost_per_token: Option<f64>,
    /// Cache write billed at the 5-minute TTL (Anthropic: 1.25x input).
    pub cache_write_cost_per_token: Option<f64>,
    /// Cache write billed at the 1-hour TTL (Anthropic: 2.0x input). `None`
    /// where the family publishes no 1h rate; callers fall back to the 5m rate.
    pub cache_write_1h_cost_per_token: Option<f64>,
    pub input_cost_per_token_above_200k: Option<f64>,
    pub output_cost_per_token_above_200k: Option<f64>,
    pub cache_read_cost_per_token_above_200k: Option<f64>,
    pub cache_write_cost_per_token_above_200k: Option<f64>,
}

impl ModelPricing {
    /// Input rate for the given context tier, falling back to the base rate for
    /// a model with no published tier (Python's `info.get(above) or base`).
    pub fn input_rate(&self, long_context: bool) -> f64 {
        if long_context {
            self.input_cost_per_token_above_200k
                .unwrap_or(self.input_cost_per_token)
        } else {
            self.input_cost_per_token
        }
    }

    /// Completion rate for the given context tier.
    pub fn output_rate(&self, long_context: bool) -> f64 {
        if long_context {
            self.output_cost_per_token_above_200k
                .unwrap_or(self.output_cost_per_token)
        } else {
            self.output_cost_per_token
        }
    }

    /// Cache-read rate for the given context tier, or `None` when the family
    /// publishes no cache-read price at all.
    pub fn cache_read_rate(&self, long_context: bool) -> Option<f64> {
        if long_context {
            self.cache_read_cost_per_token_above_200k
                .or(self.cache_read_cost_per_token)
        } else {
            self.cache_read_cost_per_token
        }
    }

    /// Cache-write (5m TTL) rate for the given context tier.
    pub fn cache_write_rate(&self, long_context: bool) -> Option<f64> {
        if long_context {
            self.cache_write_cost_per_token_above_200k
                .or(self.cache_write_cost_per_token)
        } else {
            self.cache_write_cost_per_token
        }
    }
}

/// Build a `ModelPricing` from USD-per-1M figures (the unit the provider
/// pricing pages publish), converting to per-token.
const fn per_1m(
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
) -> ModelPricing {
    const M: f64 = 1_000_000.0;
    ModelPricing {
        input_cost_per_token: input / M,
        output_cost_per_token: output / M,
        cache_read_cost_per_token: match cache_read {
            Some(v) => Some(v / M),
            None => None,
        },
        cache_write_cost_per_token: match cache_write {
            Some(v) => Some(v / M),
            None => None,
        },
        cache_write_1h_cost_per_token: None,
        input_cost_per_token_above_200k: None,
        output_cost_per_token_above_200k: None,
        cache_read_cost_per_token_above_200k: None,
        cache_write_cost_per_token_above_200k: None,
    }
}

/// Build a `ModelPricing` for a family that publishes both cache-write TTLs.
/// Anthropic bills a 5-minute write at 1.25x input and a 1-hour write at 2.0x,
/// and reports which of the two a request used in
/// `usage.cache_creation.ephemeral_{5m,1h}_input_tokens` — so both rates are
/// table data, not a guess from the configured TTL.
const fn per_1m_ttl(
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
) -> ModelPricing {
    const M: f64 = 1_000_000.0;
    ModelPricing {
        input_cost_per_token: input / M,
        output_cost_per_token: output / M,
        cache_read_cost_per_token: Some(cache_read / M),
        cache_write_cost_per_token: Some(cache_write_5m / M),
        cache_write_1h_cost_per_token: Some(cache_write_1h / M),
        input_cost_per_token_above_200k: None,
        output_cost_per_token_above_200k: None,
        cache_read_cost_per_token_above_200k: None,
        cache_write_cost_per_token_above_200k: None,
    }
}

/// Build a `ModelPricing` for a family that also publishes an above-200k tier.
///
/// Only the Sonnet 4 / 4.5 family is tiered: Opus, and Sonnet 4.6 onward, are
/// flat-rated across their whole window. Numbers come from LiteLLM's
/// `*_above_200k_tokens` fields ($3 -> $6 in, $15 -> $22.50 out, $0.30 -> $0.60
/// cache read, and a 5m write at 1.25x the tier's input rate).
#[allow(clippy::too_many_arguments)]
const fn per_1m_ttl_long(
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
    input_long: f64,
    output_long: f64,
    cache_read_long: f64,
    cache_write_5m_long: f64,
) -> ModelPricing {
    const M: f64 = 1_000_000.0;
    ModelPricing {
        input_cost_per_token: input / M,
        output_cost_per_token: output / M,
        cache_read_cost_per_token: Some(cache_read / M),
        cache_write_cost_per_token: Some(cache_write_5m / M),
        cache_write_1h_cost_per_token: Some(cache_write_1h / M),
        input_cost_per_token_above_200k: Some(input_long / M),
        output_cost_per_token_above_200k: Some(output_long / M),
        cache_read_cost_per_token_above_200k: Some(cache_read_long / M),
        cache_write_cost_per_token_above_200k: Some(cache_write_5m_long / M),
    }
}

/// Static (prefix, pricing) table. `lookup` prefers an exact key, then falls
/// back to the entry whose key is the **longest** prefix of the query — so a
/// versioned name like `claude-sonnet-4-5-20250929` resolves via the
/// `claude-sonnet-4` entry, and shorter family keys (`claude-`) catch the rest.
static TABLE: &[(&str, ModelPricing)] = &[
    // ---- Anthropic (cache write: 5m TTL = 1.25x input, 1h TTL = 2.0x) ----
    // Fable 5 and Mythos 5 cost twice what Opus 5 does. Without their own
    // entries they fell through to the `claude-` catch-all at $3/MTok and
    // under-reported by more than half.
    ("claude-fable-5", per_1m_ttl(10.0, 50.0, 1.0, 12.5, 20.0)),
    ("claude-mythos-5", per_1m_ttl(10.0, 50.0, 1.0, 12.5, 20.0)),
    ("claude-opus-5", per_1m_ttl(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-4-8", per_1m_ttl(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-4-7", per_1m_ttl(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-4-6", per_1m_ttl(5.0, 25.0, 0.5, 6.25, 10.0)),
    ("claude-opus-4-5", per_1m_ttl(5.0, 25.0, 0.5, 6.25, 10.0)),
    // Opus 4 / 4.1 are retired (Bedrock/Google Cloud only) and kept their
    // original, higher price — do not fold into the claude-opus-4-* rate above.
    ("claude-opus-4", per_1m_ttl(15.0, 75.0, 1.5, 18.75, 30.0)),
    ("claude-opus", per_1m_ttl(5.0, 25.0, 0.5, 6.25, 10.0)),
    // Same rates the `claude-` fallback already resolved to, made explicit so a
    // Sonnet-5 turn is priced by a table entry rather than by a default.
    ("claude-sonnet-5", per_1m_ttl(2.0, 10.0, 0.2, 2.5, 4.0)),
    // Sonnet 4.6 is flat-rated across its whole window, so it needs its own
    // entry: without one the `claude-sonnet-4` prefix below would hand it the
    // Sonnet-4.5 long-context tier.
    ("claude-sonnet-4-6", per_1m_ttl(3.0, 15.0, 0.3, 3.75, 6.0)),
    // Sonnet 4 / 4.5 are the only tiered family: past 200k the whole request
    // re-prices at 2x input, 1.5x output, 2x cache read.
    (
        "claude-sonnet-4",
        per_1m_ttl_long(3.0, 15.0, 0.3, 3.75, 6.0, 6.0, 22.5, 0.6, 7.5),
    ),
    (
        "claude-4-sonnet",
        per_1m_ttl_long(3.0, 15.0, 0.3, 3.75, 6.0, 6.0, 22.5, 0.6, 7.5),
    ),
    ("claude-3-7-sonnet", per_1m_ttl(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-3-5-sonnet", per_1m_ttl(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-3-sonnet", per_1m_ttl(3.0, 15.0, 0.3, 3.75, 6.0)),
    ("claude-haiku-4", per_1m_ttl(1.0, 5.0, 0.1, 1.25, 2.0)),
    ("claude-3-5-haiku", per_1m_ttl(0.8, 4.0, 0.08, 1.0, 1.6)),
    ("claude-3-haiku", per_1m_ttl(0.25, 1.25, 0.03, 0.30, 0.50)),
    ("claude-3-opus", per_1m_ttl(15.0, 75.0, 1.5, 18.75, 30.0)),
    // Family fallback for any other claude-* (Sonnet-class default).
    ("claude-", per_1m_ttl(3.0, 15.0, 0.3, 3.75, 6.0)),
    // ---- OpenAI (cache_write not published → None) ----
    ("gpt-5-mini", per_1m(0.25, 2.0, Some(0.025), None)),
    ("gpt-5-nano", per_1m(0.05, 0.4, Some(0.005), None)),
    ("gpt-5", per_1m(1.25, 10.0, Some(0.125), None)),
    ("gpt-4o-mini", per_1m(0.15, 0.60, Some(0.075), None)),
    ("gpt-4o", per_1m(2.5, 10.0, Some(1.25), None)),
    ("gpt-4.1-nano", per_1m(0.10, 0.40, Some(0.025), None)),
    ("gpt-4.1-mini", per_1m(0.40, 1.60, Some(0.10), None)),
    ("gpt-4.1", per_1m(2.0, 8.0, Some(0.50), None)),
    ("gpt-4-turbo", per_1m(10.0, 30.0, Some(5.0), None)),
    ("gpt-3.5-turbo", per_1m(0.50, 1.50, Some(0.25), None)),
    // Family fallback for any other gpt-* (gpt-4o-class default).
    ("gpt-", per_1m(2.5, 10.0, Some(1.25), None)),
    // ---- OpenAI reasoning (o-series) ----
    ("o4-mini", per_1m(1.10, 4.40, Some(0.275), None)),
    ("o3-mini", per_1m(1.10, 4.40, Some(0.55), None)),
    ("o3", per_1m(2.0, 8.0, Some(0.50), None)),
    ("o1-mini", per_1m(1.10, 4.40, Some(0.55), None)),
    ("o1", per_1m(15.0, 60.0, Some(7.50), None)),
    // ---- Google Gemini ----
    ("gemini-2.5-pro", per_1m(1.25, 10.0, Some(0.31), None)),
    ("gemini-2.5-flash", per_1m(0.30, 2.50, Some(0.075), None)),
    ("gemini-2.0-flash", per_1m(0.10, 0.40, Some(0.025), None)),
    ("gemini-1.5-pro", per_1m(1.25, 5.0, Some(0.3125), None)),
    ("gemini-1.5-flash", per_1m(0.075, 0.30, Some(0.01875), None)),
    // Family fallback for any other gemini-* (2.5-flash-class default).
    ("gemini-", per_1m(0.30, 2.50, Some(0.075), None)),
];

/// Resolve a model name to vendored pricing.
///
/// Matching order mirrors the Python resolver: an exact table key wins; failing
/// that, the entry whose key is the longest prefix of `model` (case-sensitive,
/// like the LiteLLM keys). Returns `None` for unknown families (e.g.
/// `test-model`, `deepseek-*`) so callers fall back to a blended rate.
pub fn lookup(model: &str) -> Option<&'static ModelPricing> {
    // The verbatim name first: table keys are case-sensitive, and
    // `name_candidates` lowercases, so trying it first keeps every
    // currently-correct resolution byte-identical.
    if let Some(p) = lookup_exact_or_prefix(model) {
        return Some(p);
    }
    // Gateways wrap the bare id (`bedrock/anthropic.claude-…`, `vertex_ai/…`,
    // `us.anthropic.claude-…`). Every table key is a bare id, so a wrapped
    // name matches nothing and the caller silently falls back to the blended
    // rate. Mirrors Python's `unwrapped_model_forms`.
    crate::tokenizer::name_candidates(model)
        .iter()
        .find_map(|candidate| lookup_exact_or_prefix(candidate))
}

fn lookup_exact_or_prefix(model: &str) -> Option<&'static ModelPricing> {
    // Exact match first.
    if let Some((_, p)) = TABLE.iter().find(|(k, _)| *k == model) {
        return Some(p);
    }
    // Longest matching prefix.
    let mut best: Option<(&'static str, &'static ModelPricing)> = None;
    for (k, p) in TABLE.iter() {
        if model.starts_with(k) {
            match best {
                Some((bk, _)) if bk.len() >= k.len() => {}
                _ => best = Some((k, p)),
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Estimate USD cost for a request from token counts.
///
/// Uses vendored pricing when `model` resolves via [`lookup`]; otherwise every
/// token is priced at `fallback_rate` ($/token, blended). Cache-read and
/// cache-write tokens use their family rate when published, else `fallback_rate`
/// (cache read) / the input rate (cache write). Negative counts are clamped to 0.
pub fn estimate_cost_usd(
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    fallback_rate: f64,
) -> f64 {
    let inp = input_tokens.max(0) as f64;
    let out = output_tokens.max(0) as f64;
    let cr = cache_read_tokens.max(0) as f64;
    let cw = cache_write_tokens.max(0) as f64;

    // The billed prompt (uncached + cache read + cache write) picks the price
    // tier, as it does in `CostTracker::record_tokens`.
    let long =
        is_long_context(input_tokens.max(0) + cache_read_tokens.max(0) + cache_write_tokens.max(0));

    match lookup(model) {
        Some(p) => {
            let cache_read_rate = p.cache_read_rate(long).unwrap_or(fallback_rate);
            let cache_write_rate = p.cache_write_rate(long).unwrap_or(p.input_rate(long));
            inp * p.input_rate(long)
                + out * p.output_rate(long)
                + cr * cache_read_rate
                + cw * cache_write_rate
        }
        None => (inp + out + cr + cw) * fallback_rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_wins() {
        // Versioned name resolves via the `claude-sonnet-4` prefix entry.
        let p = lookup("claude-sonnet-4-5-20250929").unwrap();
        assert!((p.input_cost_per_token - 3.0 / 1e6).abs() < 1e-18);
        assert!((p.output_cost_per_token - 15.0 / 1e6).abs() < 1e-18);
    }

    #[test]
    fn longest_prefix_beats_family_fallback() {
        // `claude-3-5-haiku-20241022` must resolve to the haiku entry, not the
        // generic `claude-` Sonnet-class fallback.
        let p = lookup("claude-3-5-haiku-20241022").unwrap();
        assert!((p.input_cost_per_token - 0.8 / 1e6).abs() < 1e-18);
    }

    #[test]
    fn opus_family() {
        let p = lookup("claude-opus-4-1-20250805").unwrap();
        assert!((p.input_cost_per_token - 15.0 / 1e6).abs() < 1e-18);
    }

    #[test]
    fn generation_5_models_have_their_own_entries() {
        let opus = lookup("claude-opus-5").unwrap();
        assert!((opus.input_cost_per_token - 5.0 / 1e6).abs() < 1e-18);
        assert!((opus.cache_read_cost_per_token.unwrap() - 0.5 / 1e6).abs() < 1e-18);
        let sonnet = lookup("claude-sonnet-5-20260115").unwrap();
        assert!((sonnet.input_cost_per_token - 2.0 / 1e6).abs() < 1e-18);
        assert!((sonnet.cache_read_cost_per_token.unwrap() - 0.2 / 1e6).abs() < 1e-18);
    }

    #[test]
    fn fable_and_mythos_do_not_fall_through_to_the_family_rate() {
        for id in ["claude-fable-5", "claude-mythos-5-20260301"] {
            let p = lookup(id).unwrap();
            assert!(
                (p.input_cost_per_token - 10.0 / 1e6).abs() < 1e-18,
                "{id} should use $10/MTok, not the $3/MTok claude- fallback"
            );
            assert!((p.output_cost_per_token - 50.0 / 1e6).abs() < 1e-18);
            assert!((p.cache_read_cost_per_token.unwrap() - 1.0 / 1e6).abs() < 1e-18);
        }
    }

    #[test]
    fn opus_4_5_through_4_8_use_current_pricing_not_retired_opus_4_rate() {
        for id in [
            "claude-opus-4-5-20260101",
            "claude-opus-4-6-20260201",
            "claude-opus-4-7-20260301",
            "claude-opus-4-8-20260401",
        ] {
            let p = lookup(id).unwrap();
            assert!(
                (p.input_cost_per_token - 5.0 / 1e6).abs() < 1e-18,
                "{id} should use $5/MTok, not the retired $15/MTok opus-4 rate"
            );
        }
    }

    #[test]
    fn gpt_and_gemini_families() {
        assert!(
            (lookup("gpt-4o-2024-11-20").unwrap().input_cost_per_token - 2.5 / 1e6).abs() < 1e-18
        );
        assert!((lookup("gpt-4o-mini").unwrap().input_cost_per_token - 0.15 / 1e6).abs() < 1e-18);
        assert!(
            (lookup("gemini-2.5-pro").unwrap().output_cost_per_token - 10.0 / 1e6).abs() < 1e-18
        );
        // o-series reasoning models.
        assert!((lookup("o1").unwrap().input_cost_per_token - 15.0 / 1e6).abs() < 1e-18);
    }

    #[test]
    fn family_fallback_for_unknown_version() {
        // Unknown gpt variant falls to the `gpt-` family default.
        let p = lookup("gpt-9-supernova").unwrap();
        assert!((p.input_cost_per_token - 2.5 / 1e6).abs() < 1e-18);
    }

    #[test]
    fn unknown_model_is_none() {
        assert!(lookup("test-model").is_none());
        assert!(lookup("deepseek-v4-pro").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn estimate_uses_vendored_pricing() {
        // 1M input + 1M output on sonnet-class. A 1M prompt is past the 200k
        // threshold, so the whole request bills at the tier: $6 + $22.50.
        let cost = estimate_cost_usd("claude-sonnet-4", 1_000_000, 1_000_000, 0, 0, 1e-6);
        assert!((cost - 28.5).abs() < 1e-9);
        // Under the threshold the same call bills at $3 + $15.
        let cost = estimate_cost_usd("claude-sonnet-4", 100_000, 100_000, 0, 0, 1e-6);
        assert!((cost - 1.8).abs() < 1e-9);
    }

    #[test]
    fn estimate_cache_tokens() {
        // 100k cache-read tokens at the sonnet cache-read rate ($0.30/M).
        let cost = estimate_cost_usd("claude-sonnet-4", 0, 0, 100_000, 0, 1e-6);
        assert!((cost - 0.03).abs() < 1e-9);
        // 100k cache-write tokens at the sonnet cache-write rate ($3.75/M).
        let cost = estimate_cost_usd("claude-sonnet-4", 0, 0, 0, 100_000, 1e-6);
        assert!((cost - 0.375).abs() < 1e-9);
        // Past 200k both rates double to their above-200k tier.
        let cost = estimate_cost_usd("claude-sonnet-4", 0, 0, 1_000_000, 0, 1e-6);
        assert!((cost - 0.60).abs() < 1e-9);
        let cost = estimate_cost_usd("claude-sonnet-4", 0, 0, 0, 1_000_000, 1e-6);
        assert!((cost - 7.50).abs() < 1e-9);
    }

    #[test]
    fn sonnet_4_family_publishes_the_above_200k_tier() {
        for id in [
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-20250514",
            "claude-4-sonnet-20250514",
        ] {
            let p = lookup(id).unwrap();
            assert!((p.input_rate(true) - 6.0 / 1e6).abs() < 1e-18, "{id} input");
            assert!(
                (p.output_rate(true) - 22.5 / 1e6).abs() < 1e-18,
                "{id} output"
            );
            assert!((p.cache_read_rate(true).unwrap() - 0.6 / 1e6).abs() < 1e-18);
            assert!((p.cache_write_rate(true).unwrap() - 7.5 / 1e6).abs() < 1e-18);
            // Base tier untouched.
            assert!((p.input_rate(false) - 3.0 / 1e6).abs() < 1e-18);
            assert!((p.output_rate(false) - 15.0 / 1e6).abs() < 1e-18);
        }
    }

    /// Only Sonnet 4 / 4.5 are tiered. Sonnet 4.6 onward and every Opus are
    /// flat-rated, so their long-context rates must equal their base ones.
    #[test]
    fn flat_rated_models_have_no_tier() {
        for id in [
            "claude-sonnet-4-6-20260210",
            "claude-sonnet-5",
            "claude-opus-5",
            "claude-fable-5",
            "gpt-5",
            "gemini-2.5-pro",
        ] {
            let p = lookup(id).unwrap();
            assert!(p.input_cost_per_token_above_200k.is_none(), "{id}");
            assert_eq!(p.input_rate(true), p.input_cost_per_token, "{id}");
            assert_eq!(p.output_rate(true), p.output_cost_per_token, "{id}");
            assert_eq!(p.cache_read_rate(true), p.cache_read_cost_per_token, "{id}");
        }
        // Sonnet 4.6 keeps the Sonnet base rates, it is not a different price.
        let p = lookup("claude-sonnet-4-6").unwrap();
        assert!((p.input_cost_per_token - 3.0 / 1e6).abs() < 1e-18);
        assert!((p.output_cost_per_token - 15.0 / 1e6).abs() < 1e-18);
    }

    #[test]
    fn threshold_is_strictly_above_200k() {
        assert!(!is_long_context(200_000));
        assert!(is_long_context(200_001));
        assert!(!is_long_context(0));
    }

    #[test]
    fn estimate_prices_a_long_prompt_at_the_tier() {
        // 300k uncached input on a tiered model: $6/M, not $3/M.
        let cost = estimate_cost_usd("claude-sonnet-4-5", 300_000, 0, 0, 0, 1e-6);
        assert!((cost - 300_000.0 * 6.0 / 1e6).abs() < 1e-12);
        // The same prompt on a flat-rated model is unchanged.
        let flat = estimate_cost_usd("claude-opus-5", 300_000, 0, 0, 0, 1e-6);
        assert!((flat - 300_000.0 * 5.0 / 1e6).abs() < 1e-12);
        // Under the threshold the tiered model keeps its base rate.
        let base = estimate_cost_usd("claude-sonnet-4-5", 100_000, 0, 0, 0, 1e-6);
        assert!((base - 100_000.0 * 3.0 / 1e6).abs() < 1e-12);
    }

    #[test]
    fn estimate_falls_back_for_unknown() {
        // Unknown model prices every token at the fallback rate.
        let cost = estimate_cost_usd("test-model", 1000, 500, 0, 0, 1e-6);
        assert!((cost - 1500.0 * 1e-6).abs() < 1e-12);
    }

    #[test]
    fn estimate_clamps_negative() {
        assert_eq!(estimate_cost_usd("test-model", -5, -5, -5, -5, 1e-6), 0.0);
    }

    /// Gateway-wrapped ids used to match no table key and silently fall back
    /// to the blended rate — the same price as an unknown model.
    #[test]
    fn wrapped_model_ids_resolve_to_real_pricing() {
        let bare = lookup("claude-sonnet-4-5").expect("bare id is in the table");
        for wrapped in [
            "bedrock/anthropic.claude-sonnet-4-5",
            "us.anthropic.claude-sonnet-4-5",
            "vertex_ai/claude-sonnet-4-5",
            "openrouter/anthropic/claude-sonnet-4-5",
        ] {
            let got = lookup(wrapped)
                .unwrap_or_else(|| panic!("{wrapped} must resolve, not hit the blended fallback"));
            assert_eq!(
                got.input_cost_per_token, bare.input_cost_per_token,
                "{wrapped} priced differently from its bare form"
            );
        }
    }

    /// Unwrapping must not turn a genuinely unknown model into a false match.
    #[test]
    fn unknown_models_still_return_none() {
        assert!(lookup("test-model").is_none());
        assert!(lookup("bedrock/some-unlisted-vendor.mystery-model").is_none());
    }
}

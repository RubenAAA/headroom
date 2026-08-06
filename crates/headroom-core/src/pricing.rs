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

/// Per-token pricing for one model. `None` cache fields mean the family has no
/// published cache pricing (or we do not vendor it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    pub cache_read_cost_per_token: Option<f64>,
    pub cache_write_cost_per_token: Option<f64>,
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
    }
}

/// Static (prefix, pricing) table. `lookup` prefers an exact key, then falls
/// back to the entry whose key is the **longest** prefix of the query — so a
/// versioned name like `claude-sonnet-4-5-20250929` resolves via the
/// `claude-sonnet-4` entry, and shorter family keys (`claude-`) catch the rest.
static TABLE: &[(&str, ModelPricing)] = &[
    // ---- Anthropic (cache write = 5m TTL rate, 1.25x input) ----
    ("claude-opus-4", per_1m(15.0, 75.0, Some(1.5), Some(18.75))),
    ("claude-opus", per_1m(15.0, 75.0, Some(1.5), Some(18.75))),
    ("claude-sonnet-4", per_1m(3.0, 15.0, Some(0.3), Some(3.75))),
    (
        "claude-3-7-sonnet",
        per_1m(3.0, 15.0, Some(0.3), Some(3.75)),
    ),
    (
        "claude-3-5-sonnet",
        per_1m(3.0, 15.0, Some(0.3), Some(3.75)),
    ),
    ("claude-3-sonnet", per_1m(3.0, 15.0, Some(0.3), Some(3.75))),
    ("claude-haiku-4", per_1m(1.0, 5.0, Some(0.1), Some(1.25))),
    ("claude-3-5-haiku", per_1m(0.8, 4.0, Some(0.08), Some(1.0))),
    ("claude-3-haiku", per_1m(0.25, 1.25, Some(0.03), Some(0.30))),
    ("claude-3-opus", per_1m(15.0, 75.0, Some(1.5), Some(18.75))),
    // Family fallback for any other claude-* (Sonnet-class default).
    ("claude-", per_1m(3.0, 15.0, Some(0.3), Some(3.75))),
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

    match lookup(model) {
        Some(p) => {
            let cache_read_rate = p.cache_read_cost_per_token.unwrap_or(fallback_rate);
            let cache_write_rate = p
                .cache_write_cost_per_token
                .unwrap_or(p.input_cost_per_token);
            inp * p.input_cost_per_token
                + out * p.output_cost_per_token
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
        // 1M input + 1M output on sonnet-class → $3 + $15.
        let cost = estimate_cost_usd("claude-sonnet-4", 1_000_000, 1_000_000, 0, 0, 1e-6);
        assert!((cost - 18.0).abs() < 1e-9);
    }

    #[test]
    fn estimate_cache_tokens() {
        // 1M cache-read tokens at sonnet cache-read rate ($0.30/M).
        let cost = estimate_cost_usd("claude-sonnet-4", 0, 0, 1_000_000, 0, 1e-6);
        assert!((cost - 0.30).abs() < 1e-9);
        // 1M cache-write tokens at sonnet cache-write rate ($3.75/M).
        let cost = estimate_cost_usd("claude-sonnet-4", 0, 0, 0, 1_000_000, 1e-6);
        assert!((cost - 3.75).abs() < 1e-9);
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
}

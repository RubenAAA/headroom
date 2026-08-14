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
    /// Cache write billed at the 5-minute TTL (Anthropic: 1.25x input).
    pub cache_write_cost_per_token: Option<f64>,
    /// Cache write billed at the 1-hour TTL (Anthropic: 2.0x input). `None`
    /// where the family publishes no 1h rate; callers fall back to the 5m rate.
    pub cache_write_1h_cost_per_token: Option<f64>,
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
    }
}

/// Static (prefix, pricing) table. `lookup` prefers an exact key, then falls
/// back to the entry whose key is the **longest** prefix of the query — so a
/// versioned name like `claude-sonnet-4-5-20250929` resolves via the
/// `claude-sonnet-4` entry, and shorter family keys (`claude-`) catch the rest.
static TABLE: &[(&str, ModelPricing)] = &[
    // ---- Anthropic (cache write: 5m TTL = 1.25x input, 1h TTL = 2.0x) ----
    ("claude-opus-4", per_1m_ttl(15.0, 75.0, 1.5, 18.75, 30.0)),
    ("claude-opus", per_1m_ttl(15.0, 75.0, 1.5, 18.75, 30.0)),
    ("claude-sonnet-4", per_1m_ttl(3.0, 15.0, 0.3, 3.75, 6.0)),
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

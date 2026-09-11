//! Dashboard display-provider labels for OpenAI-compatible upstreams.
//!
//! Port of upstream `helpers.classify_openai_upstream` /
//! `helpers.resolve_display_provider` + `server._remap_provider_counts`
//! (issue #1533).
//!
//! Display only: requests whose internal provider is `openai` are relabeled
//! for stats/dashboard surfaces (e.g. `OpenRouter`, `Groq`). Anthropic,
//! Bedrock, and Gemini keep their own labels. Pricing and request formatting
//! still key on the internal provider, so a remap can never move spend.

use std::collections::HashMap;

/// Well-known OpenAI-compatible upstreams, matched by host against the
/// configured upstream URL. Order matches upstream (first match wins, but
/// the needles are disjoint so order is cosmetic).
pub const OPENAI_COMPATIBLE_HOSTS: &[(&str, &str)] = &[
    ("openrouter.ai", "OpenRouter"),
    ("api.groq.com", "Groq"),
    ("api.together.xyz", "Together AI"),
    ("api.fireworks.ai", "Fireworks AI"),
    ("api.deepseek.com", "DeepSeek"),
    ("api.mistral.ai", "Mistral"),
    ("api.perplexity.ai", "Perplexity"),
    ("openai.azure.com", "Azure OpenAI"),
    ("api.openai.com", "OpenAI"),
];

/// Map an upstream URL to a well-known provider display name.
///
/// Matches the URL host against [`OPENAI_COMPATIBLE_HOSTS`] (exact or
/// subdomain). Returns `None` when no URL is set, it does not parse, or the
/// host is unrecognized — callers then fall back to an explicit
/// `--provider-name` or the raw `openai` label.
pub fn classify_openai_upstream(url: Option<&str>) -> Option<&'static str> {
    let url = url.filter(|u| !u.is_empty())?;
    let host = url::Url::parse(url).ok()?.host_str()?.to_lowercase();
    if host.is_empty() {
        return None;
    }
    for (needle, name) in OPENAI_COMPATIBLE_HOSTS {
        if host == *needle || host.ends_with(&format!(".{needle}")) {
            return Some(name);
        }
    }
    None
}

/// Resolve the dashboard display provider for a logged request.
///
/// Only requests whose internal provider is `openai` are reclassified.
/// Precedence: explicit `provider_name` > host detection on the configured
/// upstream URL > raw provider. Empty/missing raw providers become
/// `"unknown"`.
pub fn resolve_display_provider(
    raw_provider: Option<&str>,
    openai_api_url: Option<&str>,
    provider_name: Option<&str>,
) -> String {
    let raw = raw_provider.unwrap_or("").trim();
    if !raw.eq_ignore_ascii_case("openai") {
        return if raw.is_empty() {
            "unknown".to_string()
        } else {
            raw.to_string()
        };
    }
    if let Some(name) = provider_name.filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    classify_openai_upstream(openai_api_url)
        .map(str::to_string)
        .unwrap_or_else(|| raw.to_string())
}

/// Relabel `openai` counts with the configured display provider.
///
/// Display only — the stored metrics key stays `openai` (issue #1533).
/// Collisions (none today) are summed defensively.
pub fn remap_provider_counts(
    counts: &HashMap<String, i64>,
    openai_api_url: Option<&str>,
    provider_name: Option<&str>,
) -> HashMap<String, i64> {
    let mut out: HashMap<String, i64> = HashMap::new();
    for (provider, count) in counts {
        let display = resolve_display_provider(Some(provider), openai_api_url, provider_name);
        *out.entry(display).or_default() += *count;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_exact_hosts() {
        assert_eq!(
            classify_openai_upstream(Some("https://openrouter.ai/api/v1")),
            Some("OpenRouter")
        );
        assert_eq!(
            classify_openai_upstream(Some("https://api.openai.com/v1")),
            Some("OpenAI")
        );
        assert_eq!(
            classify_openai_upstream(Some("https://openai.azure.com/openai")),
            Some("Azure OpenAI")
        );
    }

    #[test]
    fn classifies_subdomains_and_case() {
        assert_eq!(
            classify_openai_upstream(Some("https://gateway.openrouter.ai/v1")),
            Some("OpenRouter")
        );
        assert_eq!(
            classify_openai_upstream(Some("https://API.GROQ.COM/openai")),
            Some("Groq")
        );
    }

    #[test]
    fn unrecognized_unset_or_bad_url_yields_none() {
        assert_eq!(classify_openai_upstream(None), None);
        assert_eq!(classify_openai_upstream(Some("")), None);
        assert_eq!(
            classify_openai_upstream(Some("https://proxy.internal:8787/v1")),
            None
        );
        assert_eq!(classify_openai_upstream(Some("not a url")), None);
        // Suffix without a dot boundary is not a subdomain.
        assert_eq!(
            classify_openai_upstream(Some("https://notopenrouter.ai/v1")),
            None
        );
    }

    #[test]
    fn non_openai_providers_keep_their_label() {
        assert_eq!(
            resolve_display_provider(Some("anthropic"), Some("https://openrouter.ai"), None),
            "anthropic"
        );
        assert_eq!(
            resolve_display_provider(Some("bedrock"), None, Some("Override")),
            "bedrock"
        );
        assert_eq!(resolve_display_provider(None, None, None), "unknown");
        assert_eq!(resolve_display_provider(Some("  "), None, None), "unknown");
    }

    #[test]
    fn explicit_name_beats_host_detection() {
        assert_eq!(
            resolve_display_provider(
                Some("openai"),
                Some("https://openrouter.ai/api/v1"),
                Some("My Gateway")
            ),
            "My Gateway"
        );
    }

    #[test]
    fn host_detection_beats_raw_label() {
        assert_eq!(
            resolve_display_provider(Some("openai"), Some("https://api.groq.com/openai"), None),
            "Groq"
        );
        // Unrecognized host falls back to the raw label.
        assert_eq!(
            resolve_display_provider(Some("openai"), Some("https://proxy.internal/v1"), None),
            "openai"
        );
        assert_eq!(
            resolve_display_provider(Some("openai"), None, None),
            "openai"
        );
    }

    #[test]
    fn remap_sums_collisions() {
        let counts: HashMap<String, i64> =
            [("openai".to_string(), 3), ("anthropic".to_string(), 5)]
                .into_iter()
                .collect();
        let remapped = remap_provider_counts(&counts, Some("https://openrouter.ai"), None);
        assert_eq!(remapped.get("OpenRouter"), Some(&3));
        assert_eq!(remapped.get("anthropic"), Some(&5));
        assert!(!remapped.contains_key("openai"));
    }
}

//! Proxy run mode helpers.
//!
//! Canonical modes:
//! - token: prioritize compression (history may be rewritten for max savings)
//! - cache: prioritize provider prefix cache stability (freeze prior turns)

/// Canonical mode: prioritize compression.
pub const PROXY_MODE_TOKEN: &str = "token";

/// Canonical mode: prioritize provider prefix cache stability.
pub const PROXY_MODE_CACHE: &str = "cache";

/// Normalize a user-provided proxy mode to canonical token/cache values.
///
/// Unknown modes fall back to `default` (token by default).
pub fn normalize_proxy_mode(mode: Option<&str>, default: &str) -> String {
    let key = mode.unwrap_or("").trim().to_lowercase();
    if key.is_empty() {
        return default.to_string();
    }

    match key.as_str() {
        "token" | "token_mode" | "token_savings" | "token_headroom" => PROXY_MODE_TOKEN.to_string(),
        "cache" | "cache_mode" | "cost_savings" => PROXY_MODE_CACHE.to_string(),
        _ => default.to_string(),
    }
}

/// Return true when mode resolves to token mode.
pub fn is_token_mode(mode: Option<&str>) -> bool {
    normalize_proxy_mode(mode, PROXY_MODE_TOKEN) == PROXY_MODE_TOKEN
}

/// Return true when mode resolves to cache mode.
pub fn is_cache_mode(mode: Option<&str>) -> bool {
    normalize_proxy_mode(mode, PROXY_MODE_TOKEN) == PROXY_MODE_CACHE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_canonical_values() {
        assert_eq!(
            normalize_proxy_mode(Some("token"), PROXY_MODE_TOKEN),
            PROXY_MODE_TOKEN
        );
        assert_eq!(
            normalize_proxy_mode(Some("cache"), PROXY_MODE_TOKEN),
            PROXY_MODE_CACHE
        );
    }

    #[test]
    fn normalizes_legacy_aliases() {
        assert_eq!(
            normalize_proxy_mode(Some("token_headroom"), PROXY_MODE_TOKEN),
            PROXY_MODE_TOKEN
        );
        assert_eq!(
            normalize_proxy_mode(Some("token_savings"), PROXY_MODE_TOKEN),
            PROXY_MODE_TOKEN
        );
        assert_eq!(
            normalize_proxy_mode(Some("cost_savings"), PROXY_MODE_TOKEN),
            PROXY_MODE_CACHE
        );
        assert_eq!(
            normalize_proxy_mode(Some("cache_mode"), PROXY_MODE_TOKEN),
            PROXY_MODE_CACHE
        );
    }

    #[test]
    fn invalid_falls_back_to_default() {
        assert_eq!(
            normalize_proxy_mode(Some("wat"), PROXY_MODE_CACHE),
            PROXY_MODE_CACHE
        );
    }

    #[test]
    fn predicates() {
        assert!(is_token_mode(Some("token_headroom")));
        assert!(is_cache_mode(Some("cost_savings")));
    }

    #[test]
    fn empty_string_uses_default() {
        assert_eq!(
            normalize_proxy_mode(Some(""), PROXY_MODE_TOKEN),
            PROXY_MODE_TOKEN
        );
        assert_eq!(
            normalize_proxy_mode(None, PROXY_MODE_CACHE),
            PROXY_MODE_CACHE
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            normalize_proxy_mode(Some("TOKEN"), PROXY_MODE_TOKEN),
            PROXY_MODE_TOKEN
        );
        assert_eq!(
            normalize_proxy_mode(Some("Cache"), PROXY_MODE_TOKEN),
            PROXY_MODE_CACHE
        );
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            normalize_proxy_mode(Some("  token  "), PROXY_MODE_TOKEN),
            PROXY_MODE_TOKEN
        );
    }
}

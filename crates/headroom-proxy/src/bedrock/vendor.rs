//! Canonical Bedrock vendor resolution.
//!
//! Bedrock model IDs are `<vendor>.<model>-<date>-<rev>` (e.g.
//! `anthropic.claude-3-haiku-20240307-v1:0`). Cross-region inference
//! profiles prepend a geo routing token before the vendor
//! (`eu.anthropic.…`, `us.anthropic.…`, `apac.anthropic.…`,
//! `global.anthropic.…`). Matching the bare `anthropic.` prefix alone
//! silently skips compression for those profiles, so we strip a known
//! geo prefix first, then take the leading dot-segment as the vendor.
//! Literal matching only — no regexes (project rule).

/// Cross-region inference-profile routing prefixes AWS prepends to the
/// vendor segment. Stripped (once) before vendor resolution. Kept to a
/// closed, known set so an unrelated `something.anthropic.x` id is not
/// mistaken for an Anthropic inference profile.
const GEO_PREFIXES: [&str; 4] = ["eu.", "us.", "apac.", "global."];

/// Resolve the canonical vendor of a Bedrock model id, stripping a
/// cross-region inference-profile geo prefix first.
///
/// `eu.anthropic.claude-…` → `anthropic`; `amazon.titan-…` → `amazon`;
/// `global.amazon.nova-…` → `amazon`.
pub fn canonical_vendor(model_id: &str) -> &str {
    let stripped = GEO_PREFIXES
        .iter()
        .find_map(|p| model_id.strip_prefix(p))
        .unwrap_or(model_id);
    stripped.split('.').next().unwrap_or(stripped)
}

/// Whether the model id is an Anthropic-shape model — a foundation
/// model (`anthropic.…`) or a cross-region inference profile
/// (`<geo>.anthropic.…`) — i.e. eligible for the live-zone Anthropic
/// compression + envelope pipeline.
pub fn is_anthropic_model_id(model_id: &str) -> bool {
    canonical_vendor(model_id) == "anthropic"
}

/// Apply an operator override (`HEADROOM_BEDROCK_MODEL_MAP`) to the
/// inbound `model_id`. Keyed by the exact model_id the client sent in
/// the URL path.
///
/// Returns `Some((effective_model_id, arn))` when an override matches:
/// - `effective_model_id` is the override target that replaces the
///   inbound model_id in the upstream URL path.
/// - `arn` is `true` when the target is an application-inference-profile
///   ARN (`arn:aws:…`). ARN targets must use the converse route — the
///   invoke route rejects ARNs with HTTP 400 — so the caller forces the
///   converse (or converse-stream) action when this is `true`.
///
/// Returns `None` when no override matches, leaving passthrough
/// behaviour unchanged. Ports the override branch of Python's
/// `map_model_id` (#1795); the LiteLLM `provider/` prefix branch has no
/// Rust equivalent (this proxy has no provider-prefix concept), so a
/// non-ARN target is forwarded verbatim as the model_id.
pub fn resolve_bedrock_model_override(
    map: &std::collections::HashMap<String, String>,
    model_id: &str,
) -> Option<(String, bool)> {
    map.get(model_id)
        .map(|target| (target.clone(), target.starts_with("arn:aws:")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_vendor_strips_known_cross_region_prefix() {
        assert_eq!(
            canonical_vendor("anthropic.claude-3-haiku-20240307-v1:0"),
            "anthropic"
        );
        assert_eq!(
            canonical_vendor("eu.anthropic.claude-haiku-4-5-20251001-v1:0"),
            "anthropic"
        );
        assert_eq!(canonical_vendor("amazon.titan-text-express-v1"), "amazon");
        assert_eq!(canonical_vendor("global.amazon.nova-lite-v1:0"), "amazon");
        // Unknown leading token is NOT stripped — stays as its own vendor.
        assert_eq!(canonical_vendor("random.anthropic.x"), "random");
    }

    #[test]
    fn anthropic_model_id_matches_foundation_and_inference_profiles() {
        assert!(is_anthropic_model_id(
            "anthropic.claude-3-haiku-20240307-v1:0"
        ));
        assert!(is_anthropic_model_id(
            "eu.anthropic.claude-haiku-4-5-20251001-v1:0"
        ));
        assert!(is_anthropic_model_id(
            "us.anthropic.claude-3-5-sonnet-20241022-v2:0"
        ));
        assert!(is_anthropic_model_id(
            "apac.anthropic.claude-3-5-sonnet-20240620-v1:0"
        ));
        assert!(is_anthropic_model_id(
            "global.anthropic.claude-haiku-4-5-20251001-v1:0"
        ));
        // Non-Anthropic vendors, including geo-prefixed, stay false.
        assert!(!is_anthropic_model_id("amazon.titan-text-express-v1"));
        assert!(!is_anthropic_model_id("meta.llama3-70b-instruct-v1:0"));
        assert!(!is_anthropic_model_id("eu.amazon.nova-lite-v1:0"));
        assert!(!is_anthropic_model_id("mistral.voxtral-mini-3b-2507"));
    }

    #[test]
    fn override_pins_plain_name_to_app_profile_arn() {
        let arn = "arn:aws:bedrock:ap-southeast-1:1:application-inference-profile/x57j1esjrt66";
        let mut map = std::collections::HashMap::new();
        map.insert("claude-sonnet-5".to_string(), arn.to_string());
        let (target, is_arn) =
            resolve_bedrock_model_override(&map, "claude-sonnet-5").expect("override");
        assert_eq!(target, arn);
        assert!(is_arn, "arn:aws target must force the converse route");
    }

    #[test]
    fn override_non_arn_target_forwarded_verbatim() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "claude-sonnet-5".to_string(),
            "us.anthropic.claude-sonnet-5-v1:0".to_string(),
        );
        let (target, is_arn) =
            resolve_bedrock_model_override(&map, "claude-sonnet-5").expect("override");
        assert_eq!(target, "us.anthropic.claude-sonnet-5-v1:0");
        assert!(!is_arn, "non-arn target keeps the inbound action");
    }

    #[test]
    fn override_absent_returns_none() {
        let mut map = std::collections::HashMap::new();
        map.insert("claude-sonnet-5".to_string(), "arn:aws:x".to_string());
        assert!(resolve_bedrock_model_override(&map, "claude-opus-4-8").is_none());
        assert!(resolve_bedrock_model_override(&std::collections::HashMap::new(), "x").is_none());
    }
}

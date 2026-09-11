//! Tool-schema savings attribution.
//!
//! Headroom saves input tokens in two accounting shapes, and the split follows
//! from what each transform can observe:
//!
//! * **Compaction** rewrites the tool array in place, so both endpoints are
//!   countable. Handlers fold the delta into `original_tokens` /
//!   `optimized_tokens`. It is therefore ALREADY inside `tokens_saved` and
//!   must never be added again.
//! * **Deferral / hook shrink** (tool search, turn hooks) removes schemas that
//!   counting never saw, so it cannot move `original_tokens`. It is recorded
//!   in per-request tags and is ADDITIVE to `tokens_saved`.
//!
//! The one rule a caller needs: the headline is
//! `tokens_saved + tool_schema_saved_from_tags(tags)`. Use
//! [`headline_tokens_saved`] rather than open-coding the sum.
//!
//! Adding a new tool-schema-shrinking feature? If it moves the tool array,
//! fold it in the handler like the compaction sites do. If it defers schemas,
//! add its tag name to [`TOOL_SCHEMA_SAVINGS_TAGS`] and every surface picks
//! it up.
//!
//! Lives in core (not the proxy crate) so the PERF emitter in
//! [`crate::request_outcome`] and the analyzer in [`crate::perf_analyzer`]
//! share the one definition with the proxy's stats surfaces.

use std::collections::HashMap;

/// Per-request tags whose values are tool-definition tokens Headroom kept out
/// of context by deferring schemas. Counted only when Headroom performed the
/// deferral — not when the client already had tool search enabled.
pub const TOOL_SCHEMA_SAVINGS_TAGS: &[&str] = &[
    "tool_search_deferred_tokens",
    "turn_hook_tools_saved_tokens",
];

/// Tool-definition tokens Headroom kept out of context for one request.
///
/// The summed tags are set only on paths where Headroom performed the
/// deferral, so requests without deferral contribute zero. Non-numeric or
/// missing tag values contribute zero (never an error).
///
/// These tags are additive to `tokens_saved` — see the module docstring.
pub fn tool_schema_saved_from_tags(tags: &HashMap<String, String>) -> i64 {
    let mut total: i64 = 0;
    for key in TOOL_SCHEMA_SAVINGS_TAGS {
        let value = tags
            .get(*key)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0);
        total = total.saturating_add(value);
    }
    total
}

/// The single "Tokens saved" figure for one request: message compression plus
/// the tool-definition tokens that never entered the context.
///
/// Clamped at zero: handlers already revert any inflation before forwarding,
/// so a negative is a token-count artifact that never reached the model.
pub fn headline_tokens_saved(tokens_saved: i64, tags: &HashMap<String, String>) -> i64 {
    tokens_saved
        .saturating_add(tool_schema_saved_from_tags(tags))
        .max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn empty_tags_save_nothing() {
        assert_eq!(tool_schema_saved_from_tags(&HashMap::new()), 0);
    }

    #[test]
    fn sums_both_deferral_tags() {
        let t = tags(&[
            ("tool_search_deferred_tokens", "1200"),
            ("turn_hook_tools_saved_tokens", "300"),
        ]);
        assert_eq!(tool_schema_saved_from_tags(&t), 1500);
    }

    #[test]
    fn unrelated_tags_are_ignored() {
        let t = tags(&[("auth_mode", "payg"), ("tool_search_deferred_tokens", "50")]);
        assert_eq!(tool_schema_saved_from_tags(&t), 50);
    }

    #[test]
    fn non_numeric_values_contribute_zero() {
        let t = tags(&[
            ("tool_search_deferred_tokens", "many"),
            ("turn_hook_tools_saved_tokens", ""),
        ]);
        assert_eq!(tool_schema_saved_from_tags(&t), 0);
    }

    #[test]
    fn headline_adds_tags_and_clamps_at_zero() {
        let t = tags(&[("tool_search_deferred_tokens", "200")]);
        assert_eq!(headline_tokens_saved(1000, &t), 1200);
        assert_eq!(headline_tokens_saved(0, &HashMap::new()), 0);
        // Negative totals are counting artifacts, never real savings.
        assert_eq!(headline_tokens_saved(-500, &HashMap::new()), 0);
    }
}

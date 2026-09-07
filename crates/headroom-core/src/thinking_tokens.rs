//! Separate thinking tokens from visible output tokens (Rust port of
//! `headroom/proxy/thinking_tokens.py`).
//!
//! `output_tokens` is a single number, and it pools two quantities that
//! different levers move in opposite directions. Reasoning-effort routing cuts
//! *thinking*; verbosity steering cuts *visible text*. Summed into one
//! counter, neither can be attributed: a request whose thinking dropped 4,000
//! tokens while its prose grew 200 looks identical to one where nothing
//! happened.
//!
//! What each provider actually tells us:
//!
//! * **OpenAI chat** — `usage.completion_tokens_details.reasoning_tokens`.
//! * **OpenAI Responses** — `usage.output_tokens_details.reasoning_tokens`.
//! * **Gemini** — `usageMetadata.thoughtsTokenCount`.
//! * **Anthropic** — *nothing*. The Messages API reports no thinking count at
//!   all. The thinking text is in the response content, so it can be
//!   estimated, but an estimate must never be handed back as if the provider
//!   had reported it.
//!
//! Unknown is not zero: returning `0` for Anthropic would assert "no thinking
//! happened", which is false whenever extended thinking is on — and it would
//! quietly corrupt every average computed over mixed-provider traffic. So the
//! result is `None` for "we cannot tell" and an integer for "we know", with
//! [`ThinkingTokens::inferred`] marking a count Headroom derived rather than
//! received. That mirrors `RequestOutcome.cache_inferred`, which exists for
//! the same reason on the input side.
//!
//! Pure module: no tokenizer import and no I/O. The estimator is injected, so
//! callers that have a tokenizer can pass one and callers that do not still
//! get a correct `None`.

use serde_json::Value;

/// A thinking-token count, and whether it was reported or derived.
///
/// `tokens` is the count, or `None` when the provider reported nothing and no
/// estimate could be made. `0` means "the provider told us there was no
/// thinking" — a genuinely different claim from `None`. `inferred` is true
/// when Headroom derived the number (by tokenizing the response's thinking
/// blocks) rather than reading it from usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinkingTokens {
    pub tokens: Option<i64>,
    pub inferred: bool,
}

impl ThinkingTokens {
    /// True when the split is known (reported or inferred).
    pub fn known(&self) -> bool {
        self.tokens.is_some()
    }

    /// Visible output tokens, or `None` when the split is unknown.
    ///
    /// Clamped at zero: an inferred count uses Headroom's tokenizer while
    /// `output_tokens` is on the provider's scale, so the two can disagree
    /// slightly and a naive subtraction can go negative on a short response.
    pub fn visible_from(&self, output_tokens: i64) -> Option<i64> {
        self.tokens.map(|t| (output_tokens - t).max(0))
    }
}

/// Coerce a usage field to a non-negative int, or `None` if absent/bad.
///
/// Distinguishes a missing field from a zero one, which is the whole point of
/// this module — so it deliberately does not floor everything to 0 the way
/// the proxy's `_usage_int` helpers do.
fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Bool(_) => None,
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
            // Non-finite floats (NaN/inf, only constructible programmatically
            // — parsed JSON never carries them) coerce to nothing, matching
            // upstream's `int()` raising on them: unknown, never zero.
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
            .map(|v| v.max(0)),
        Value::String(s) => s.trim().parse::<i64>().ok().map(|v| v.max(0)),
        _ => None,
    }
}

/// Read `reasoning_tokens` from whichever details block is present.
///
/// Chat spells the container `completion_tokens_details`; Responses spells it
/// `output_tokens_details`. Some gateways echo both, so check each and take
/// the first that carries a usable number rather than assuming a format.
fn details_reasoning(usage: &serde_json::Map<String, Value>) -> Option<i64> {
    for container_key in ["output_tokens_details", "completion_tokens_details"] {
        if let Some(Value::Object(container)) = usage.get(container_key) {
            if let Some(v) = container.get("reasoning_tokens").and_then(as_int) {
                return Some(v);
            }
        }
    }
    None
}

/// Concatenate the text of every thinking block in an Anthropic response.
///
/// Anthropic reports no thinking count, but it does return the thinking
/// itself, so the content is the only available basis for an estimate.
/// Handles both `thinking` and `redacted_thinking` blocks; a redacted block
/// carries no readable text but still cost output tokens, so its `data`
/// payload is included — an approximation, and flagged as inferred by the
/// caller.
pub fn anthropic_thinking_text(payload: &Value) -> String {
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in content {
        let Some(obj) = block.as_object() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                if let Some(text) = obj.get("thinking").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            Some("redacted_thinking") => {
                if let Some(data) = obj.get("data").and_then(Value::as_str) {
                    parts.push(data.to_string());
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// Read the thinking count from a bare provider `usage` dict.
///
/// The full-payload [`extract_thinking_tokens`] is the general entry point,
/// but several proxy call sites have already destructured `usage` out of the
/// response and never keep the body around. Giving them a direct path avoids
/// forcing an artificial re-wrap — and avoids the temptation to pass an empty
/// object and silently record a zero.
pub fn extract_from_usage(usage: &Value) -> ThinkingTokens {
    let Some(obj) = usage.as_object() else {
        return ThinkingTokens::default();
    };
    if let Some(reported) = details_reasoning(obj) {
        return ThinkingTokens {
            tokens: Some(reported),
            inferred: false,
        };
    }
    if let Some(thoughts) = obj.get("thoughtsTokenCount").and_then(as_int) {
        return ThinkingTokens {
            tokens: Some(thoughts),
            inferred: false,
        };
    }
    ThinkingTokens::default()
}

/// Extract the thinking-token count from any provider response shape.
///
/// `estimator` is an optional `text -> token count`, supplied by callers that
/// have a tokenizer; used only for Anthropic, which reports no count of its
/// own. Without it, Anthropic responses return `None` (unknown) rather than
/// a fabricated zero.
pub fn extract_thinking_tokens(
    payload: &Value,
    estimator: Option<&dyn Fn(&str) -> i64>,
) -> ThinkingTokens {
    let Some(obj) = payload.as_object() else {
        return ThinkingTokens::default();
    };

    // Gemini: a top-level usageMetadata rather than usage.
    if let Some(Value::Object(meta)) = obj.get("usageMetadata") {
        return ThinkingTokens {
            tokens: meta.get("thoughtsTokenCount").and_then(as_int),
            inferred: false,
        };
    }

    if let Some(usage) = obj.get("usage") {
        let reported = extract_from_usage(usage);
        if reported.known() {
            return reported;
        }
    }

    // Anthropic: no usage field for this, so fall back to the content.
    let text = anthropic_thinking_text(payload);
    if text.is_empty() {
        // Distinguish "an Anthropic response with no thinking blocks" — which
        // is a genuine zero — from "not an Anthropic response at all", where
        // we know nothing. Only a response that actually carries content
        // blocks can support the former claim.
        if obj.get("content").and_then(Value::as_array).is_some() {
            return ThinkingTokens {
                tokens: Some(0),
                inferred: false,
            };
        }
        return ThinkingTokens::default();
    }

    let Some(estimate) = estimator else {
        return ThinkingTokens::default();
    };
    ThinkingTokens {
        tokens: Some(estimate(&text).max(0)),
        inferred: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_chat_reasoning_tokens_reported() {
        let usage = json!({
            "completion_tokens": 100,
            "completion_tokens_details": {"reasoning_tokens": 30}
        });
        let t = extract_from_usage(&usage);
        assert_eq!(t.tokens, Some(30));
        assert!(!t.inferred);
        assert!(t.known());
    }

    #[test]
    fn openai_responses_reasoning_tokens_reported() {
        let usage = json!({
            "output_tokens": 100,
            "output_tokens_details": {"reasoning_tokens": 45}
        });
        assert_eq!(extract_from_usage(&usage).tokens, Some(45));
    }

    #[test]
    fn gemini_thoughts_count_reported() {
        let payload = json!({
            "usageMetadata": {"thoughtsTokenCount": 120, "totalTokenCount": 500}
        });
        let t = extract_thinking_tokens(&payload, None);
        assert_eq!(t.tokens, Some(120));
        assert!(!t.inferred);
    }

    #[test]
    fn non_dict_usage_is_unknown_not_zero() {
        for usage in [json!(null), json!("x"), json!([1]), json!(42)] {
            assert_eq!(extract_from_usage(&usage), ThinkingTokens::default());
        }
        assert_eq!(
            extract_thinking_tokens(&json!([1, 2]), None),
            ThinkingTokens::default()
        );
    }

    #[test]
    fn missing_fields_are_unknown_not_zero() {
        // A usage block with no thinking fields: None, never 0.
        let t = extract_from_usage(&json!({"completion_tokens": 50}));
        assert_eq!(t.tokens, None);
        // A zero that WAS reported stays zero.
        let t = extract_from_usage(&json!({"completion_tokens_details": {"reasoning_tokens": 0}}));
        assert_eq!(t.tokens, Some(0));
    }

    #[test]
    fn anthropic_without_estimator_is_unknown() {
        let payload = json!({
            "content": [
                {"type": "thinking", "thinking": "let me think..."},
                {"type": "text", "text": "the answer"}
            ]
        });
        assert_eq!(
            extract_thinking_tokens(&payload, None),
            ThinkingTokens::default()
        );
    }

    #[test]
    fn anthropic_with_estimator_is_inferred() {
        let payload = json!({
            "content": [
                {"type": "thinking", "thinking": "one two three"},
                {"type": "text", "text": "the answer"}
            ]
        });
        let t = extract_thinking_tokens(
            &payload,
            Some(&|text: &str| text.split_whitespace().count() as i64),
        );
        assert_eq!(t.tokens, Some(3));
        assert!(t.inferred);
    }

    #[test]
    fn anthropic_redacted_thinking_counts_as_text() {
        let payload = json!({
            "content": [{"type": "redacted_thinking", "data": "opaque-bytes"}]
        });
        assert_eq!(anthropic_thinking_text(&payload), "opaque-bytes");
        let t = extract_thinking_tokens(&payload, Some(&|_: &str| 7));
        assert_eq!(
            t,
            ThinkingTokens {
                tokens: Some(7),
                inferred: true
            }
        );
    }

    #[test]
    fn anthropic_response_with_no_thinking_blocks_is_genuine_zero() {
        let payload = json!({
            "content": [{"type": "text", "text": "direct answer"}]
        });
        let t = extract_thinking_tokens(&payload, None);
        assert_eq!(t.tokens, Some(0));
        assert!(!t.inferred);
    }

    #[test]
    fn non_anthropic_shape_without_content_is_unknown() {
        // No `content` list at all: we know nothing, not zero.
        let t = extract_thinking_tokens(&json!({"id": "x"}), None);
        assert_eq!(t.tokens, None);
    }

    #[test]
    fn visible_from_subtracts_and_clamps() {
        let t = ThinkingTokens {
            tokens: Some(30),
            inferred: false,
        };
        assert_eq!(t.visible_from(100), Some(70));
        // Inferred counts use Headroom's tokenizer on the provider's scale;
        // the two can disagree by a token or two on a short response.
        let t = ThinkingTokens {
            tokens: Some(10),
            inferred: true,
        };
        assert_eq!(t.visible_from(8), Some(0));
        assert_eq!(ThinkingTokens::default().visible_from(100), None);
    }
}

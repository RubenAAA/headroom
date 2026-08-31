//! Output token shaping for proxied Anthropic requests.
//!
//! Headroom's transforms compress what goes INTO the model. This module is the
//! first request-side lever on what comes OUT of it. The proxy never generates
//! output tokens, so every lever here works by reshaping the request:
//!
//! 1. Verbosity steering — a deterministic instruction block appended to the
//!    TAIL of the system prompt (after any `cache_control` breakpoint).
//!
//! 2. Effort routing — on turns classified as mechanical we lower an
//!    explicitly-present effort; on errors or new user asks we leave it alone.
//!
//! Turn classification is purely structural (block types, roles, `is_error`
//! flags) — no content regexes or keyword patterns.

use serde_json::Value;

/// Documented Anthropic API minimum for thinking.budget_tokens on models
/// that still accept the legacy enabled/budget_tokens form.
pub const LEGACY_THINKING_FLOOR: i64 = 1024;

/// Ordering for output_config.effort values.
fn effort_rank(s: &str) -> Option<i32> {
    match s {
        "low" => Some(0),
        "medium" => Some(1),
        "high" => Some(2),
        "xhigh" => Some(3),
        "max" => Some(4),
        _ => None,
    }
}

/// The effort the client asked for, if it named one.
///
/// Claude Code's `/effort` and `--effort` travel as `output_config.effort` —
/// `low`, `medium`, `high` or `xhigh` — on every request, including ones for a
/// routed alias. `thinking` comes alongside as `{"type": "adaptive"}` and
/// carries no budget, so a reader looking only at `thinking.budget_tokens`
/// sees nothing and the setting is silently lost.
pub fn requested_effort(body: &Value) -> Option<&str> {
    let effort = body
        .get("output_config")?
        .get("effort")?
        .as_str()?
        .trim();
    effort_rank(effort).map(|_| effort)
}

const STEERING_SENTINEL: &str = "<headroom_output_shaping>";
const STEERING_SUFFIX: &str = "</headroom_output_shaping>";

/// Verbosity level texts. Levels are cumulative: each includes everything above.
/// Text must stay byte-stable across releases for prefix-cache friendliness.
fn verbosity_text(level: i32) -> Option<&'static str> {
    match level {
        1 => Some(
            "Skip preamble and postamble. Do not announce what you are about to \
             do or recap what you just did; start with the substance.",
        ),
        2 => Some(
            "Skip preamble and postamble; start with the substance. Never restate \
             code, file contents, diffs, or tool output that already appear in \
             this conversation — reference them by path and line instead. After a \
             tool call succeeds, continue without narrating the result.",
        ),
        3 => Some(
            "Skip preamble and postamble. Never restate code, file contents, \
             diffs, or tool output already in this conversation — reference by \
             path and line. Give conclusions only; omit rationale unless the user \
             asks why. Prefer the smallest edit over rewriting whole files. Keep \
             prose to the minimum needed to be unambiguous.",
        ),
        4 => Some(
            "Minimum tokens. Fragments fine. No preamble, no postamble, no \
             restating context, no rationale. Answer, smallest-possible edits, \
             nothing else.",
        ),
        _ => None,
    }
}

/// Structural classification of the latest conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    NewUserAsk,
    MechanicalContinuation,
    ErrorContinuation,
    Unknown,
}

/// Classify the latest turn from message structure alone.
pub fn classify_turn(messages: &[Value]) -> TurnKind {
    let last = match messages.last() {
        Some(Value::Object(m)) => m,
        _ => return TurnKind::Unknown,
    };

    if last.get("role").and_then(Value::as_str) != Some("user") {
        return TurnKind::Unknown;
    }

    match last.get("content") {
        Some(Value::String(s)) => {
            if s.trim().is_empty() {
                TurnKind::Unknown
            } else {
                TurnKind::NewUserAsk
            }
        }
        Some(Value::Array(blocks)) if !blocks.is_empty() => {
            let mut saw_tool_result = false;
            let mut saw_error = false;

            for block in blocks {
                let obj = match block.as_object() {
                    Some(o) => o,
                    None => return TurnKind::Unknown,
                };

                match obj.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        saw_tool_result = true;
                        if obj.get("is_error").and_then(Value::as_bool) == Some(true) {
                            saw_error = true;
                        }
                    }
                    Some("text") => return TurnKind::NewUserAsk,
                    Some("image" | "document") => return TurnKind::NewUserAsk,
                    _ => {}
                }
            }

            if saw_error {
                TurnKind::ErrorContinuation
            } else if saw_tool_result {
                TurnKind::MechanicalContinuation
            } else {
                TurnKind::Unknown
            }
        }
        _ => TurnKind::Unknown,
    }
}

/// The full steering block for a verbosity level, or None for level 0.
pub fn steering_text(level: i32) -> Option<String> {
    verbosity_text(level).map(|text| format!("{STEERING_SENTINEL}\n{text}\n{STEERING_SUFFIX}"))
}

/// Append the steering block to the tail of the system prompt.
///
/// Appending AFTER the last system block keeps any `cache_control`
/// breakpoint on an earlier block intact.
pub fn apply_verbosity_steering(body: &mut Value, level: i32) -> bool {
    let text = match steering_text(level) {
        Some(t) => t,
        None => return false,
    };

    let system = body.get("system");

    if system.is_none() {
        body["system"] = serde_json::json!([{"type": "text", "text": text}]);
        return true;
    }

    if let Some(Value::String(s)) = system {
        let original = s.clone();
        body["system"] = serde_json::json!([
            {"type": "text", "text": original},
            {"type": "text", "text": text}
        ]);
        return true;
    }

    if let Some(Value::Array(blocks)) = system {
        // Check if already applied at this level
        for block in blocks {
            if let Some(obj) = block.as_object() {
                if let Some(Value::String(t)) = obj.get("text") {
                    if t.starts_with(STEERING_SENTINEL) {
                        if *t == text {
                            return false; // already applied at this level
                        }
                        // Level changed — would need to replace in place
                        // but we can't mutate through the Value easily.
                        // Append and let dedup handle it.
                    }
                }
            }
        }

        let mut new_blocks = blocks.clone();
        new_blocks.push(serde_json::json!({"type": "text", "text": text}));
        body["system"] = Value::Array(new_blocks);
        return true;
    }

    false
}

/// Lower thinking/effort spend on mechanical continuations.
///
/// Returns labels for each mutation made (empty list = untouched).
pub fn route_effort(body: &mut Value, kind: TurnKind, mechanical_effort: &str) -> Vec<String> {
    if kind != TurnKind::MechanicalContinuation {
        return vec![];
    }

    let mut labels = Vec::new();

    // Modern lever: output_config.effort
    if let Some(output_config) = body.get_mut("output_config").and_then(Value::as_object_mut) {
        if let Some(effort_val) = output_config.get("effort").and_then(Value::as_str) {
            if let (Some(current_rank), Some(target_rank)) =
                (effort_rank(effort_val), effort_rank(mechanical_effort))
            {
                if current_rank > target_rank {
                    let old = effort_val.to_string();
                    output_config["effort"] = Value::String(mechanical_effort.to_string());
                    labels.push(format!("output_shaper:effort:{old}->{mechanical_effort}"));
                }
            }
        }
    }

    // Legacy lever: clamp thinking.budget_tokens
    if let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) {
        if thinking.get("type").and_then(Value::as_str) == Some("enabled") {
            if let Some(budget) = thinking.get("budget_tokens").and_then(Value::as_i64) {
                if budget > LEGACY_THINKING_FLOOR {
                    thinking["budget_tokens"] = Value::Number(LEGACY_THINKING_FLOOR.into());
                    labels.push(format!(
                        "output_shaper:thinking_budget:{budget}->{LEGACY_THINKING_FLOOR}"
                    ));
                }
            }
        }
    }

    labels
}

/// Result of output shaping.
#[derive(Debug, Default)]
pub struct ShapeResult {
    pub changed: bool,
    pub labels: Vec<String>,
}

/// Apply all output-shaping levers to an Anthropic request body in place.
pub fn shape_request(
    body: &mut Value,
    enabled: bool,
    verbosity_level: i32,
    effort_router_enabled: bool,
    mechanical_effort: &str,
) -> ShapeResult {
    let mut result = ShapeResult::default();
    if !enabled {
        return result;
    }

    if verbosity_level > 0 && apply_verbosity_steering(body, verbosity_level) {
        result.changed = true;
        result
            .labels
            .push(format!("output_shaper:verbosity:L{verbosity_level}"));
    }

    if effort_router_enabled {
        let messages: Vec<Value> = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let kind = classify_turn(&messages);
        let labels = route_effort(body, kind, mechanical_effort);
        if !labels.is_empty() {
            result.changed = true;
            result.labels.extend(labels);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_result(is_error: bool) -> Value {
        let mut block = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_01",
            "content": "ok",
        });
        if is_error {
            block["is_error"] = json!(true);
        }
        block
    }

    fn mechanical_messages() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "fix the bug in foo.py"}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "Reading the file."},
                {"type": "tool_use", "id": "toolu_01", "name": "Read", "input": {}}
            ]}),
            json!({"role": "user", "content": [tool_result(false)]}),
        ]
    }

    // ── classify_turn ─────────────────────────────────────────────

    #[test]
    fn string_user_message_is_new_ask() {
        assert_eq!(
            classify_turn(&[json!({"role": "user", "content": "explain this"})]),
            TurnKind::NewUserAsk
        );
    }

    #[test]
    fn clean_tool_result_is_mechanical() {
        assert_eq!(
            classify_turn(&mechanical_messages()),
            TurnKind::MechanicalContinuation
        );
    }

    #[test]
    fn error_tool_result_is_error_continuation() {
        let mut msgs = mechanical_messages();
        msgs[2]["content"] = json!([tool_result(false), tool_result(true)]);
        assert_eq!(classify_turn(&msgs), TurnKind::ErrorContinuation);
    }

    #[test]
    fn text_block_alongside_tool_result_is_new_ask() {
        let mut msgs = mechanical_messages();
        msgs[2]["content"] =
            json!([tool_result(false), {"type": "text", "text": "also check bar.py"}]);
        assert_eq!(classify_turn(&msgs), TurnKind::NewUserAsk);
    }

    #[test]
    fn image_block_is_new_ask() {
        assert_eq!(
            classify_turn(&[json!({"role": "user", "content": [{"type": "image", "source": {}}]})]),
            TurnKind::NewUserAsk
        );
    }

    #[test]
    fn assistant_last_is_unknown() {
        assert_eq!(
            classify_turn(&[json!({"role": "assistant", "content": "hello"})]),
            TurnKind::Unknown
        );
    }

    #[test]
    fn empty_messages_is_unknown() {
        assert_eq!(classify_turn(&[]), TurnKind::Unknown);
    }

    #[test]
    fn empty_content_list_is_unknown() {
        assert_eq!(
            classify_turn(&[json!({"role": "user", "content": []})]),
            TurnKind::Unknown
        );
    }

    #[test]
    fn whitespace_string_content_is_unknown() {
        assert_eq!(
            classify_turn(&[json!({"role": "user", "content": "  "})]),
            TurnKind::Unknown
        );
    }

    // ── apply_verbosity_steering ──────────────────────────────────

    #[test]
    fn level_zero_is_noop() {
        let mut body = json!({"system": "You are helpful."});
        assert!(!apply_verbosity_steering(&mut body, 0));
        assert_eq!(body["system"], json!("You are helpful."));
    }

    #[test]
    fn string_system_converted_to_blocks() {
        let mut body = json!({"system": "You are helpful."});
        assert!(apply_verbosity_steering(&mut body, 2));
        assert_eq!(body["system"][0]["text"], "You are helpful.");
        assert_eq!(body["system"][1]["text"], steering_text(2).unwrap());
    }

    #[test]
    fn missing_system_creates_steering_only_block() {
        let mut body = json!({});
        assert!(apply_verbosity_steering(&mut body, 2));
        assert_eq!(body["system"][0]["text"], steering_text(2).unwrap());
    }

    #[test]
    fn block_system_appends_after_cache_control() {
        let cached = json!({"type": "text", "text": "Big system prompt.", "cache_control": {"type": "ephemeral"}});
        let mut body = json!({"system": [cached.clone()]});
        assert!(apply_verbosity_steering(&mut body, 2));
        assert_eq!(body["system"][0], cached);
        assert_eq!(body["system"][1]["text"], steering_text(2).unwrap());
        assert!(body["system"][1].get("cache_control").is_none());
    }

    #[test]
    fn steering_text_is_deterministic() {
        for level in 1..=4 {
            assert_eq!(steering_text(level), steering_text(level));
        }
    }

    // ── route_effort ──────────────────────────────────────────────

    #[test]
    fn lowers_explicit_effort_on_mechanical_turn() {
        let mut body = json!({"output_config": {"effort": "xhigh"}});
        let labels = route_effort(&mut body, TurnKind::MechanicalContinuation, "low");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(labels, vec!["output_shaper:effort:xhigh->low"]);
    }

    #[test]
    fn never_injects_effort_when_absent() {
        let mut body = json!({"messages": []});
        let labels = route_effort(&mut body, TurnKind::MechanicalContinuation, "low");
        assert!(body.get("output_config").is_none());
        assert!(labels.is_empty());
    }

    #[test]
    fn effort_untouched_on_new_ask() {
        let mut body = json!({"output_config": {"effort": "xhigh"}});
        assert!(route_effort(&mut body, TurnKind::NewUserAsk, "low").is_empty());
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn effort_already_at_target_untouched() {
        let mut body = json!({"output_config": {"effort": "low"}});
        assert!(route_effort(&mut body, TurnKind::MechanicalContinuation, "low").is_empty());
    }

    #[test]
    fn unknown_effort_value_untouched() {
        let mut body = json!({"output_config": {"effort": "turbo"}});
        assert!(route_effort(&mut body, TurnKind::MechanicalContinuation, "low").is_empty());
        assert_eq!(body["output_config"]["effort"], "turbo");
    }

    #[test]
    fn legacy_thinking_budget_clamped() {
        let mut body = json!({"thinking": {"type": "enabled", "budget_tokens": 32000}});
        let labels = route_effort(&mut body, TurnKind::MechanicalContinuation, "low");
        assert_eq!(body["thinking"]["budget_tokens"], LEGACY_THINKING_FLOOR);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(
            labels,
            vec![format!(
                "output_shaper:thinking_budget:32000->{LEGACY_THINKING_FLOOR}"
            )]
        );
    }

    #[test]
    fn legacy_budget_at_floor_untouched() {
        let mut body =
            json!({"thinking": {"type": "enabled", "budget_tokens": LEGACY_THINKING_FLOOR}});
        assert!(route_effort(&mut body, TurnKind::MechanicalContinuation, "low").is_empty());
    }

    #[test]
    fn adaptive_thinking_untouched() {
        let mut body = json!({"thinking": {"type": "adaptive"}});
        assert!(route_effort(&mut body, TurnKind::MechanicalContinuation, "low").is_empty());
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    }

    // ── shape_request (end to end) ────────────────────────────────

    #[test]
    fn disabled_is_noop() {
        let mut body = json!({
            "system": "Sys.",
            "messages": mechanical_messages(),
            "output_config": {"effort": "xhigh"}
        });
        let snapshot = body.clone();
        let result = shape_request(&mut body, false, 2, true, "low");
        assert!(!result.changed);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn enabled_applies_steering_and_effort_routing() {
        let mut body = json!({
            "system": "Sys.",
            "messages": mechanical_messages(),
            "output_config": {"effort": "xhigh"},
            "thinking": {"type": "adaptive"}
        });
        let result = shape_request(&mut body, true, 2, true, "low");
        assert!(result.changed);
        assert_eq!(
            result.labels,
            vec![
                "output_shaper:verbosity:L2",
                "output_shaper:effort:xhigh->low",
            ]
        );
        assert_eq!(body["output_config"]["effort"], "low");
    }

    #[test]
    fn new_ask_gets_steering_but_keeps_effort() {
        let mut body = json!({
            "system": "Sys.",
            "messages": [{"role": "user", "content": "design a cache layer"}],
            "output_config": {"effort": "xhigh"}
        });
        let result = shape_request(&mut body, true, 2, true, "low");
        assert_eq!(result.labels, vec!["output_shaper:verbosity:L2"]);
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn second_pass_is_stable() {
        let mut body = json!({"system": "Sys.", "messages": mechanical_messages()});
        shape_request(&mut body, true, 2, true, "low");
        let snapshot = body.clone();
        let result = shape_request(&mut body, true, 2, true, "low");
        assert!(!result.changed);
        assert_eq!(body, snapshot);
    }
}

#[cfg(test)]
mod requested_effort_tests {
    use super::*;
    use serde_json::json;

    /// The exact shape Claude Code 2.1.250 sends for a routed alias, captured
    /// off the wire: the effort is in `output_config`, and `thinking` is
    /// adaptive with no budget to read.
    #[test]
    fn the_effort_is_read_from_the_shape_claude_code_sends() {
        let body = json!({
            "model": "claude-codex-5.6-sol",
            "max_tokens": 32000,
            "thinking": {"type": "adaptive", "display": "omitted"},
            "output_config": {"effort": "xhigh"},
        });
        assert_eq!(requested_effort(&body), Some("xhigh"));
    }

    #[test]
    fn a_body_without_an_effort_reports_none() {
        assert_eq!(requested_effort(&json!({"model": "m"})), None);
        assert_eq!(requested_effort(&json!({"output_config": {}})), None);
        assert_eq!(
            requested_effort(&json!({"output_config": {"format": "json"}})),
            None
        );
    }

    /// An unrecognised level is reported as absent rather than passed on to a
    /// backend that would reject it.
    #[test]
    fn an_unknown_level_is_not_reported() {
        assert_eq!(
            requested_effort(&json!({"output_config": {"effort": "turbo"}})),
            None
        );
    }
}

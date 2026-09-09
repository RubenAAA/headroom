//! Output token shaping for proxied Anthropic requests.
//!
//! Headroom's transforms compress what goes INTO the model. This module is the
//! first request-side lever on what comes OUT of it. The proxy never generates
//! output tokens, so every lever here works by reshaping the request:
//!
//! 1. Verbosity steering — a deterministic instruction block appended to the
//!    TAIL of the system prompt (after any `cache_control` breakpoint).
//!
//! Effort routing (lowering `output_config.effort` / clamping
//! `thinking.budget_tokens` on mechanical turns) was removed, mirroring
//! upstream: live measurement showed per-turn effort routing ~15x underwater
//! on mechanical turns. The shaper now only steers verbosity.
//!
//! Turn classification is purely structural (block types, roles, `is_error`
//! flags) — no content regexes or keyword patterns. It is retained for
//! stratum labelling; the shaper itself no longer branches on it.

use serde_json::Value;

/// Ordering for output_config.effort values.
///
/// Kept as a read-only helper: [`requested_effort`] reports the effort the
/// client asked for (used by the OpenAI request translator and the cursor
/// agent resolver). Nothing lowers it anymore.
fn effort_rank(s: &str) -> Option<i32> {
    match s {
        // `minimal` is the Responses floor (killing reasoning entirely is a
        // 400); without this arm a client asking for it got the backend
        // default instead — a silent upgrade to `high`.
        "minimal" => Some(-1),
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
/// `minimal`, `low`, `medium`, `high` or `xhigh` — on every request, including ones for a
/// routed alias. `thinking` comes alongside as `{"type": "adaptive"}` and
/// carries no budget, so a reader looking only at `thinking.budget_tokens`
/// sees nothing and the setting is silently lost.
pub fn requested_effort(body: &Value) -> Option<&str> {
    let effort = body.get("output_config")?.get("effort")?.as_str()?.trim();
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
             diffs, or tool output already in this conversation — cite the exact \
             file path and line or symbol instead, always; a reference that omits \
             the location is not a reference. Give conclusions only; omit \
             rationale unless the user asks why. Prefer the smallest edit over \
             rewriting whole files. Keep prose to the minimum needed to be \
             unambiguous. Never drop anything the turn or task needs to be \
             correct, including negations (not, never, no, only, except) — shorten \
             how you say it, not what you say. Use full prose for destructive or \
             irreversible actions, security warnings, and any multi-step sequence \
             where brevity would create ambiguity.",
        ),
        4 => Some(
            "Minimum tokens. Fragments fine. No preamble, no postamble, no \
             restating context, no rationale. Answer, smallest-possible edits, \
             nothing else. Never drop anything the turn or task needs to be \
             correct, including negations (not, never, no, only, except). Use \
             full prose for destructive or irreversible actions, security \
             warnings, and any multi-step sequence where brevity would create \
             ambiguity.",
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

/// Result of output shaping.
#[derive(Debug, Default)]
pub struct ShapeResult {
    pub changed: bool,
    pub labels: Vec<String>,
}

/// False when the proxy runs in prefix-freezing cache mode.
///
/// `mode="cache"` freezes prior turns specifically to keep the provider's
/// prefix-cache key byte-stable. Verbosity steering is the one lever that
/// writes into that key: it appends to the system-prompt tail, and on a body
/// with no `system` field it creates one. Running it there trades a large,
/// certain cache cost for a small, uncertain output saving. Ports Python
/// `output_shaper.steering_allowed_for`.
pub fn steering_allowed_for(mode: &str) -> bool {
    !crate::modes::is_cache_mode(Some(mode))
}

/// Apply verbosity steering to an Anthropic request body in place, honoring
/// the proxy run mode.
///
/// In cache mode the level is forced to 0 — the documented "no steering"
/// value — which disables the only cache-key-mutating lever. This
/// deliberately outranks a configured level: a level set on the command line
/// must not reintroduce a prefix mutation the mode exists to prevent. Ports
/// the `steering_enabled` branch of Python `resolve_verbosity_level`.
pub fn shape_request_for_mode(
    body: &mut Value,
    enabled: bool,
    verbosity_level: i32,
    mode: &str,
) -> ShapeResult {
    let level = if steering_allowed_for(mode) {
        verbosity_level
    } else {
        0
    };
    shape_request(body, enabled, level)
}

/// Apply verbosity steering to an Anthropic request body in place.
pub fn shape_request(body: &mut Value, enabled: bool, verbosity_level: i32) -> ShapeResult {
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

    // ── shape_request (end to end) ────────────────────────────────

    #[test]
    fn disabled_is_noop() {
        let mut body = json!({
            "system": "Sys.",
            "messages": mechanical_messages(),
            "output_config": {"effort": "xhigh"}
        });
        let snapshot = body.clone();
        let result = shape_request(&mut body, false, 2);
        assert!(!result.changed);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn enabled_applies_steering_only() {
        let mut body = json!({
            "system": "Sys.",
            "messages": mechanical_messages(),
            "output_config": {"effort": "xhigh"},
            "thinking": {"type": "adaptive"}
        });
        let result = shape_request(&mut body, true, 2);
        assert!(result.changed);
        assert_eq!(result.labels, vec!["output_shaper:verbosity:L2",]);
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    /// Cache mode freezes the provider prefix-cache key, so the one lever
    /// that writes into it must not run. The body has to come out of the
    /// shaper stage byte-identical.
    #[test]
    fn cache_mode_leaves_body_byte_identical() {
        let mut body = json!({
            "system": "Sys.",
            "messages": mechanical_messages(),
            "output_config": {"effort": "xhigh"}
        });
        let before = serde_json::to_vec(&body).unwrap();

        let result = shape_request_for_mode(&mut body, true, 3, crate::modes::PROXY_MODE_CACHE);

        assert!(!result.changed);
        assert!(result.labels.is_empty());
        assert_eq!(serde_json::to_vec(&body).unwrap(), before);
    }

    /// A body carrying no `system` field is the sharper case: steering would
    /// create one, which is a bigger prefix break than an append.
    #[test]
    fn cache_mode_does_not_create_a_system_field() {
        let mut body = json!({"messages": mechanical_messages()});
        let before = serde_json::to_vec(&body).unwrap();

        shape_request_for_mode(&mut body, true, 3, crate::modes::PROXY_MODE_CACHE);

        assert!(body.get("system").is_none());
        assert_eq!(serde_json::to_vec(&body).unwrap(), before);
    }

    /// The gate outranks a configured level, and cache-mode aliases resolve
    /// the same way the mode flag does.
    #[test]
    fn cache_mode_aliases_also_disable_steering() {
        for mode in ["cache", "cache_mode", "cost_savings"] {
            let mut body = json!({"system": "Sys.", "messages": mechanical_messages()});
            let before = serde_json::to_vec(&body).unwrap();

            shape_request_for_mode(&mut body, true, 4, mode);

            assert_eq!(serde_json::to_vec(&body).unwrap(), before, "mode={mode}");
            assert!(!steering_allowed_for(mode), "mode={mode}");
        }
    }

    #[test]
    fn token_mode_still_shapes() {
        let mut body = json!({
            "system": "Sys.",
            "messages": mechanical_messages(),
            "output_config": {"effort": "xhigh"}
        });

        let result = shape_request_for_mode(&mut body, true, 2, crate::modes::PROXY_MODE_TOKEN);

        assert!(result.changed);
        assert_eq!(result.labels, vec!["output_shaper:verbosity:L2"]);
        let tail = body["system"].as_array().unwrap().last().unwrap();
        assert!(tail["text"]
            .as_str()
            .unwrap()
            .starts_with(STEERING_SENTINEL));
        assert!(steering_allowed_for(crate::modes::PROXY_MODE_TOKEN));
    }

    /// Effort routing was removed upstream (per-turn routing measured ~15x
    /// underwater on mechanical turns): even a mechanical continuation keeps
    /// the effort the client sent.
    #[test]
    fn mechanical_turn_keeps_explicit_effort() {
        let mut body = json!({
            "system": "Sys.",
            "messages": mechanical_messages(),
            "output_config": {"effort": "xhigh"},
            "thinking": {"type": "enabled", "budget_tokens": 32000}
        });
        let result = shape_request(&mut body, true, 2);
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(body["thinking"]["budget_tokens"], 32000);
        assert_eq!(result.labels, vec!["output_shaper:verbosity:L2"]);
    }

    #[test]
    fn new_ask_gets_steering_but_keeps_effort() {
        let mut body = json!({
            "system": "Sys.",
            "messages": [{"role": "user", "content": "design a cache layer"}],
            "output_config": {"effort": "xhigh"}
        });
        let result = shape_request(&mut body, true, 2);
        assert_eq!(result.labels, vec!["output_shaper:verbosity:L2"]);
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn second_pass_is_stable() {
        let mut body = json!({"system": "Sys.", "messages": mechanical_messages()});
        shape_request(&mut body, true, 2);
        let snapshot = body.clone();
        let result = shape_request(&mut body, true, 2);
        assert!(!result.changed);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn l3_cites_exact_location_and_carries_completeness_floor() {
        let text = steering_text(3).unwrap();
        assert!(text.contains("cite the exact file path and line or symbol instead, always"));
        assert!(text.contains("Never drop anything the turn or task needs to be correct"));
        assert!(text.contains("negations"));
    }

    #[test]
    fn l4_carries_completeness_floor_and_clarity_exception() {
        let text = steering_text(4).unwrap();
        assert!(text.contains("Never drop anything the turn or task needs to be correct"));
        assert!(text.contains("destructive or irreversible actions"));
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

    /// `minimal` is a real Responses effort and must survive the rank gate,
    /// or the backend silently upgrades it to its default.
    #[test]
    fn minimal_is_reported_not_dropped() {
        assert_eq!(
            requested_effort(&json!({"output_config": {"effort": "minimal"}})),
            Some("minimal")
        );
    }
}

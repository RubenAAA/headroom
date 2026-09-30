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

impl TurnKind {
    /// The name the savings strata use (Python `TurnKind.value`).
    pub fn as_str(self) -> &'static str {
        match self {
            TurnKind::NewUserAsk => "new_user_ask",
            TurnKind::MechanicalContinuation => "mechanical_continuation",
            TurnKind::ErrorContinuation => "error_continuation",
            TurnKind::Unknown => "unknown",
        }
    }
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
            if let Some(obj) = block.as_object()
                && let Some(Value::String(t)) = obj.get("text")
                && t.starts_with(STEERING_SENTINEL)
                && *t == text
            {
                return false; // already applied at this level
            }
            // Level changed — would need to replace in place
            // but we can't mutate through the Value easily.
            // Append and let dedup handle it.
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

/// The conversation a request belongs to, as a hex digest, for holdout
/// assignment.
///
/// Claude Code's session id comes first. Upstream's key hashes the model and
/// the first user text block, which on Claude Code is a `<system-reminder>`
/// that repeats across sessions, so most sessions would share one key and one
/// arm. Subagents share their parent's session id, so a whole tree lands in
/// one arm; bodies without a session id fall back to upstream's key.
pub fn holdout_conversation_key(body: &Value) -> String {
    use sha2::{Digest, Sha256};
    match crate::cache_stabilization::capture::client_session_id(body) {
        Some(id) => hex::encode(Sha256::digest(format!("session:{id}").as_bytes())),
        None => headroom_core::output_savings::conversation_key_from_body(body),
    }
}

/// Input size for the savings stratum: characters of every string in the
/// body over four. The strata buckets are a factor of four apart, and both
/// arms are sized the same way, which is all the holdout comparison needs.
fn approx_input_tokens(v: &Value) -> i64 {
    fn chars(v: &Value) -> usize {
        match v {
            Value::String(s) => s.len(),
            Value::Array(a) => a.iter().map(chars).sum(),
            Value::Object(o) => o.values().map(chars).sum(),
            _ => 0,
        }
    }
    (chars(v) / 4) as i64
}

/// [`shape_request`] behind the output-savings holdout.
///
/// Assigns the request's conversation an arm (`holdout` is the control
/// share), labels it with the arm, stratum and conversation for the savings
/// ledger, and steers only the treatment arm. The arm is fixed per
/// conversation, so a conversation's system prompt never flips between turns.
/// Labels are returned even when nothing was steered; `changed` says whether
/// the body was.
pub fn shape_with_holdout(body: &mut Value, verbosity_level: i32, holdout: f64) -> ShapeResult {
    use headroom_core::output_savings::{
        assign_arm, conversation_label, stratum_key, stratum_label,
    };
    let conversation = holdout_conversation_key(body);
    let arm = assign_arm(&conversation, holdout);
    let turn_kind = body
        .get("messages")
        .and_then(Value::as_array)
        .map_or(TurnKind::Unknown, |m| classify_turn(m));
    let stratum = stratum_key(
        turn_kind.as_str(),
        approx_input_tokens(body),
        body.get("model").and_then(Value::as_str).unwrap_or(""),
        body.get("tools")
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty()),
    );
    let mut result = if arm == "treatment" {
        shape_request(body, true, verbosity_level)
    } else {
        ShapeResult::default()
    };
    result.labels.push(stratum_label(arm, &stratum));
    result.labels.push(conversation_label(&conversation));
    result
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

// ---------------------------------------------------------------------------
// OpenAI wire formats: chat/completions and Responses (HTTP + WebSocket)
// ---------------------------------------------------------------------------

/// Replace an existing steering block in `existing`, or append one at the tail.
/// Returns the new text and whether it differs from `existing`.
fn replace_or_append_steering_block(existing: &str, block: &str) -> (String, bool) {
    let updated = match existing.find(STEERING_SENTINEL) {
        Some(start) => {
            let end = existing[start..]
                .find(STEERING_SUFFIX)
                .map_or(existing.len(), |i| start + i + STEERING_SUFFIX.len());
            let prefix = existing[..start].trim_end();
            let suffix = existing[end..].trim_start_matches('\n');
            [prefix, block, suffix]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        None if existing.trim().is_empty() => block.to_string(),
        None => format!("{}\n\n{block}", existing.trim_end()),
    };
    let changed = updated != existing;
    (updated, changed)
}

/// Append or replace the steering block in an OpenAI chat/completions body.
///
/// Chat carries the system prompt as a `system` or `developer` message in
/// `messages`. The block goes on the tail of the last such message, so it
/// stays byte-stable across turns and re-applies idempotently through the
/// sentinel. With no system message, one is inserted at the front. Returns
/// true only when the body changed.
pub fn apply_openai_chat_verbosity_steering(body: &mut Value, level: i32) -> bool {
    let Some(text) = steering_text(level) else {
        return false;
    };
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let target = messages.iter().rposition(|m| {
        matches!(
            m.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        )
    });
    let Some(target) = target else {
        messages.insert(0, serde_json::json!({"role": "system", "content": text}));
        return true;
    };
    let message = &mut messages[target];
    match message.get_mut("content") {
        None | Some(Value::Null) => {
            message["content"] = Value::String(text);
            true
        }
        Some(Value::String(existing)) => {
            let (updated, changed) = replace_or_append_steering_block(existing, &text);
            if changed {
                *existing = updated;
            }
            changed
        }
        Some(Value::Array(parts)) => {
            // A content-part list: `[{"type": "text", "text": ...}]`.
            for part in parts.iter_mut() {
                let is_block = part.get("type").and_then(Value::as_str) == Some("text")
                    && part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.starts_with(STEERING_SENTINEL));
                if is_block {
                    if part["text"].as_str() == Some(text.as_str()) {
                        return false;
                    }
                    part["text"] = Value::String(text);
                    return true;
                }
            }
            parts.push(serde_json::json!({"type": "text", "text": text}));
            true
        }
        Some(_) => false,
    }
}

/// Append or replace the steering block in a Responses `instructions` string.
///
/// `instructions` is the Responses cache hot zone: the block is byte-stable
/// per level, so a conversation sends identical instructions bytes on every
/// shaped turn.
pub fn apply_openai_responses_verbosity_steering(body: &mut Value, level: i32) -> bool {
    let Some(text) = steering_text(level) else {
        return false;
    };
    match body.get_mut("instructions") {
        None | Some(Value::Null) => {
            body["instructions"] = Value::String(text);
            true
        }
        Some(Value::String(existing)) => {
            let (updated, changed) = replace_or_append_steering_block(existing, &text);
            if changed {
                *existing = updated;
            }
            changed
        }
        Some(_) => false,
    }
}

/// Trailing `input` item types that are tool output going back to the model,
/// the Responses counterpart of an Anthropic `tool_result`.
const RESPONSES_TOOL_OUTPUT_TYPES: [&str; 4] = [
    "custom_tool_call_output",
    "function_call_output",
    "local_shell_call_output",
    "apply_patch_call_output",
];

/// Structural error sniff on a Responses tool-output item. The format has no
/// `is_error`, so failure shows in the `output` payload as a JSON object with
/// a nonzero `exit_code`, `success: false`, or a truthy `error`. Only those
/// fields are read, never prose.
fn responses_tool_output_is_error(item: &Value) -> bool {
    let parsed;
    let data = match item.get("output") {
        Some(Value::String(s)) => {
            let s = s.trim();
            if !(s.starts_with('{') && s.ends_with('}')) {
                return false;
            }
            match serde_json::from_str::<Value>(s) {
                Ok(v) => {
                    parsed = v;
                    &parsed
                }
                Err(_) => return false,
            }
        }
        Some(v) => v,
        None => return false,
    };
    if !data.is_object() {
        return false;
    }
    // Direct fields, plus the common `{"output": ..., "metadata": {...}}` nesting.
    [Some(data), data.get("metadata").filter(|m| m.is_object())]
        .into_iter()
        .flatten()
        .any(|scope| {
            scope
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|c| c != 0)
                || scope.get("success") == Some(&Value::Bool(false))
                || scope.get("error").is_some_and(|e| match e {
                    Value::Null | Value::Bool(false) => false,
                    Value::String(s) => !s.is_empty(),
                    Value::Number(n) => n.as_f64() != Some(0.0),
                    Value::Array(a) => !a.is_empty(),
                    Value::Object(o) => !o.is_empty(),
                    Value::Bool(true) => true,
                })
        })
}

/// Classify a Responses request's turn from its `input` field. The trailing
/// run of tool-output items decides: a trailing user message is a new ask;
/// tool outputs are mechanical unless one carries a structural error marker.
pub fn classify_responses_turn(input: Option<&Value>) -> TurnKind {
    let items = match input {
        Some(Value::String(s)) => {
            return if s.trim().is_empty() {
                TurnKind::Unknown
            } else {
                TurnKind::NewUserAsk
            };
        }
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return TurnKind::Unknown,
    };
    let mut saw_tool_output = false;
    let mut saw_error = false;
    for item in items.iter().rev() {
        let Some(obj) = item.as_object() else {
            return TurnKind::Unknown;
        };
        let item_type = obj.get("type").and_then(Value::as_str);
        if item_type.is_some_and(|t| RESPONSES_TOOL_OUTPUT_TYPES.contains(&t)) {
            saw_tool_output = true;
            saw_error |= responses_tool_output_is_error(item);
            continue;
        }
        // The first non-tool-output item ends the trailing run.
        if saw_tool_output {
            break;
        }
        let is_message =
            item_type == Some("message") || (item_type.is_none() && obj.contains_key("role"));
        return if is_message && obj.get("role").and_then(Value::as_str) == Some("user") {
            TurnKind::NewUserAsk
        } else {
            TurnKind::Unknown
        };
    }
    if saw_error {
        TurnKind::ErrorContinuation
    } else if saw_tool_output {
        TurnKind::MechanicalContinuation
    } else {
        TurnKind::Unknown
    }
}

/// The conversation a Responses payload belongs to, as a hex digest.
///
/// A stable client identifier comes first: Codex sets `prompt_cache_key` to
/// its session id, and other clients send a conversation or session id at top
/// level or in `client_metadata` / `metadata`. Upstream keys on the model and
/// the first user text alone, which repeats across sessions for a harness
/// that opens every one with the same instructions, so it is the fallback.
pub fn responses_conversation_key(body: &Value) -> String {
    use sha2::{Digest, Sha256};
    fn id_of(v: &Value) -> Option<&str> {
        let s = match v {
            Value::String(s) => Some(s.as_str()),
            Value::Object(o) => ["id", "conversation_id", "session_id", "thread_id"]
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty())),
            _ => None,
        };
        s.filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("auto"))
    }
    let stable = [
        "prompt_cache_key",
        "conversation",
        "conversation_id",
        "session_id",
        "thread_id",
    ]
    .iter()
    .find_map(|k| body.get(*k).and_then(id_of).map(|v| format!("{k}:{v}")))
    .or_else(|| {
        ["client_metadata", "metadata"].iter().find_map(|c| {
            let container = body.get(*c)?.as_object()?;
            [
                "conversation_id",
                "conversation_key",
                "session_id",
                "thread_id",
                "codex_session_id",
            ]
            .iter()
            .find_map(|k| {
                container
                    .get(*k)
                    .and_then(id_of)
                    .map(|v| format!("{c}.{k}:{v}"))
            })
        })
    });
    let mut seed = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(stable) = stable {
        seed.push('\0');
        seed.push_str(&stable);
    } else {
        let first_user = match body.get("input") {
            Some(Value::String(s)) => Some(s.as_str()),
            Some(Value::Array(items)) => items
                .iter()
                .find(|i| i.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(|i| match i.get("content") {
                    Some(Value::String(s)) => Some(s.as_str()),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .find_map(|p| p.get("text").and_then(Value::as_str)),
                    _ => None,
                }),
            _ => None,
        };
        if let Some(text) = first_user {
            seed.push('\0');
            seed.extend(text.chars().take(512));
        }
    }
    hex::encode(Sha256::digest(seed.as_bytes()))
}

/// Which OpenAI wire format a body is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiFormat {
    Chat,
    Responses,
}

/// [`shape_with_holdout`] for an OpenAI body.
///
/// Same arm assignment, labels and treatment-only steering, but the steering
/// lands where that format keeps its system prompt: the last system or
/// developer message for chat, the `instructions` tail for Responses.
pub fn shape_openai_with_holdout(
    body: &mut Value,
    format: OpenAiFormat,
    verbosity_level: i32,
    holdout: f64,
) -> ShapeResult {
    use headroom_core::output_savings::{
        assign_arm, conversation_key_from_body, conversation_label, stratum_key, stratum_label,
    };
    let (conversation, turn_kind) = match format {
        OpenAiFormat::Chat => (
            conversation_key_from_body(body),
            body.get("messages")
                .and_then(Value::as_array)
                .map_or(TurnKind::Unknown, |m| classify_openai_chat_turn(m)),
        ),
        OpenAiFormat::Responses => (
            responses_conversation_key(body),
            classify_responses_turn(body.get("input")),
        ),
    };
    let arm = assign_arm(&conversation, holdout);
    let stratum = stratum_key(
        turn_kind.as_str(),
        approx_input_tokens(body),
        body.get("model").and_then(Value::as_str).unwrap_or(""),
        body.get("tools")
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty()),
    );
    let mut result = ShapeResult::default();
    if arm == "treatment" && verbosity_level > 0 {
        let changed = match format {
            OpenAiFormat::Chat => apply_openai_chat_verbosity_steering(body, verbosity_level),
            OpenAiFormat::Responses => {
                apply_openai_responses_verbosity_steering(body, verbosity_level)
            }
        };
        if changed {
            result.changed = true;
            result
                .labels
                .push(format!("output_shaper:verbosity:L{verbosity_level}"));
        }
    }
    result.labels.push(stratum_label(arm, &stratum));
    result.labels.push(conversation_label(&conversation));
    result
}

/// Chat turn kind: a trailing `tool` message is a tool result coming back
/// (mechanical; chat has no error flag to read), a trailing `user` message is
/// a new ask.
fn classify_openai_chat_turn(messages: &[Value]) -> TurnKind {
    match messages
        .last()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
    {
        Some("user") => classify_turn(messages),
        Some("tool") => TurnKind::MechanicalContinuation,
        _ => TurnKind::Unknown,
    }
}

/// Steer a client `response.create` WebSocket frame in place of the raw text.
///
/// Both envelopes are handled: `{"type":"response.create","response":{...}}`
/// and the bare payload with `type` at top level. Any other frame, or one
/// that will not parse, comes back byte-equal, as does a frame the shaper did
/// not change (a control-arm conversation), so client bytes are only
/// re-serialized when steering was added.
pub fn shape_response_create_frame(
    raw: String,
    verbosity_level: i32,
    holdout: f64,
    request_id: &str,
) -> String {
    let Ok(mut frame) = serde_json::from_str::<Value>(&raw) else {
        return raw;
    };
    if frame.get("type").and_then(Value::as_str) != Some("response.create") {
        return raw;
    }
    let wrapped = frame.get("response").is_some_and(Value::is_object);
    let inner = if wrapped {
        &mut frame["response"]
    } else {
        &mut frame
    };
    let shaped =
        shape_openai_with_holdout(inner, OpenAiFormat::Responses, verbosity_level, holdout);
    tracing::info!(
        request_id = %request_id,
        event = "output_shaper_arm",
        steered = shaped.changed,
        labels = ?shaped.labels,
        "output_shaper applied to codex ws frame"
    );
    if !shaped.changed {
        return raw;
    }
    serde_json::to_string(&frame).unwrap_or(raw)
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

    /// Cache mode steers too (upstream `c0292984`). What busts a cached
    /// prefix is the steering block changing, not it being there: the level
    /// is fixed at startup and the text is byte-stable per level, so the
    /// block lands in turn 1's prefix and every later turn repeats it.
    #[test]
    fn steering_is_byte_stable_across_turns() {
        let mut turn1 = json!({"messages": mechanical_messages()});
        let result = shape_request(&mut turn1, true, 2);
        assert!(result.changed);
        assert_eq!(result.labels, vec!["output_shaper:verbosity:L2"]);
        let tail = turn1["system"].as_array().unwrap().last().unwrap();
        assert!(
            tail["text"]
                .as_str()
                .unwrap()
                .starts_with(STEERING_SENTINEL)
        );

        // Turn 2 arrives without the block (the client never saw it) and
        // leaves with the same system bytes as turn 1.
        let mut turn2 = json!({"messages": mechanical_messages()});
        shape_request(&mut turn2, true, 2);
        assert_eq!(
            serde_json::to_vec(&turn2["system"]).unwrap(),
            serde_json::to_vec(&turn1["system"]).unwrap()
        );

        // A body that already carries the block is left alone.
        let before = serde_json::to_vec(&turn1).unwrap();
        shape_request(&mut turn1, true, 2);
        assert_eq!(serde_json::to_vec(&turn1).unwrap(), before);
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

    /// Unlike the Anthropic path, the OpenAI injectors replace a block whose
    /// level changed instead of stacking a second one, and re-apply as a no-op.
    #[test]
    fn openai_steering_replaces_a_stale_level_in_place() {
        let mut body = json!({"instructions": "You are Codex.", "input": []});
        assert!(apply_openai_responses_verbosity_steering(&mut body, 1));
        let at_l1 = body["instructions"].as_str().unwrap().to_string();
        assert!(!apply_openai_responses_verbosity_steering(&mut body, 1));
        assert!(apply_openai_responses_verbosity_steering(&mut body, 2));
        let at_l2 = body["instructions"].as_str().unwrap();
        assert_ne!(at_l1, at_l2);
        assert_eq!(at_l2.matches(STEERING_SENTINEL).count(), 1);
        assert!(at_l2.starts_with("You are Codex.\n\n"));

        let mut chat = json!({"messages": [
            {"role": "system", "content": [{"type": "text", "text": "Be brief."}]},
            {"role": "user", "content": "hi"}
        ]});
        assert!(apply_openai_chat_verbosity_steering(&mut chat, 1));
        assert!(apply_openai_chat_verbosity_steering(&mut chat, 2));
        assert!(!apply_openai_chat_verbosity_steering(&mut chat, 2));
        let parts = chat["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "one client part plus one steering part");
        assert_eq!(parts[1]["text"], steering_text(2).unwrap());
    }

    #[test]
    fn responses_turns_classify_by_the_trailing_tool_outputs() {
        let user = json!({"type": "message", "role": "user", "content": "go"});
        let ok = json!({"type": "function_call_output", "call_id": "1", "output": "fine"});
        let failed = json!({
            "type": "function_call_output", "call_id": "2",
            "output": "{\"output\": \"boom\", \"metadata\": {\"exit_code\": 1}}"
        });
        let kind = |items: Vec<Value>| classify_responses_turn(Some(&Value::Array(items)));
        assert_eq!(kind(vec![user.clone()]), TurnKind::NewUserAsk);
        assert_eq!(
            kind(vec![user.clone(), ok.clone()]),
            TurnKind::MechanicalContinuation
        );
        assert_eq!(
            kind(vec![user.clone(), ok, failed]),
            TurnKind::ErrorContinuation
        );
        assert_eq!(classify_responses_turn(None), TurnKind::Unknown);
    }
}

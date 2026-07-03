//! Inject Anthropic-native context-editing directives (`context_management`)
//! into outbound `/v1/messages` bodies. This hands subscription users the
//! server-side context GC (`clear_tool_uses_20250919`, `clear_thinking_20251015`)
//! that Claude Code gates behind ant-only flags. The directives are NOT part of
//! the cached prefix, so injecting them is cache-safe; clearing fires only past
//! the configured token trigger and is cache-aware server-side.

use serde_json::Value;

/// Beta header that enables the `context_management` parameter.
pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// Merge context-editing directives into `body["context_management"]["edits"]`.
///
/// Appends a `clear_tool_uses_20250919` edit (when `clear_tool_uses_keep` is
/// `Some`) and/or a `clear_thinking_20251015` edit (when
/// `clear_thinking_keep_turns` is `Some`). Never clobbers an existing
/// `context_management`, and skips any edit whose type family is already
/// present (Claude Code commonly sends `clear_thinking` itself). Returns
/// whether the body was modified.
pub fn inject_context_management(
    body: &mut Value,
    clear_tool_uses_keep: Option<u64>,
    clear_tool_uses_trigger: u64,
    clear_thinking_keep_turns: Option<u64>,
) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };

    let cm = obj
        .entry("context_management")
        .or_insert_with(|| serde_json::json!({}));
    let Some(cm_obj) = cm.as_object_mut() else {
        return false;
    };
    let edits = cm_obj
        .entry("edits")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(edits_arr) = edits.as_array_mut() else {
        return false;
    };

    let has_family = |arr: &[Value], family: &str| -> bool {
        arr.iter().any(|e| {
            e.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.starts_with(family))
        })
    };

    let mut changed = false;

    if let Some(keep) = clear_tool_uses_keep {
        if !has_family(edits_arr, "clear_tool_uses") {
            edits_arr.push(serde_json::json!({
                "type": "clear_tool_uses_20250919",
                "trigger": { "type": "input_tokens", "value": clear_tool_uses_trigger },
                "keep": { "type": "tool_uses", "value": keep },
            }));
            changed = true;
        }
    }

    if let Some(turns) = clear_thinking_keep_turns {
        if !has_family(edits_arr, "clear_thinking") {
            edits_arr.push(serde_json::json!({
                "type": "clear_thinking_20251015",
                "keep": { "type": "thinking_turns", "value": turns },
            }));
            changed = true;
        }
    }

    changed
}

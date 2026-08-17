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
/// Adds a `clear_thinking_20251015` edit (when `clear_thinking_keep_turns` is
/// `Some`) and/or a `clear_tool_uses_20250919` edit (when `clear_tool_uses_keep`
/// is `Some`). Never clobbers an existing `context_management`, and never adds a
/// second `clear_tool_uses` when one is already there. Returns whether the body
/// was modified.
///
/// `clear_thinking` is the exception to leaving the client's edits alone. Claude
/// Code sends that edit itself on every request with `keep: "all"`, which clears
/// nothing, so treating "family already present" as "nothing to do" left this
/// setting inert against its own main client. When `clear_thinking_keep_turns` is
/// set we therefore overwrite the `keep` value in place. The edit is also placed
/// first: the API requires `clear_thinking_20251015` to lead the array.
///
/// See `docs/context-editing-api-facts.md` for the schema and what clearing
/// costs in cache writes.
/// `clear_tool_uses_min_messages` leaves short conversations alone. The first
/// clear invalidates from the *oldest* tool result, so it re-creates nearly the
/// whole history however `keep` is set — measured at 109,035 creation tokens
/// against ~1,836 weighted saved per turn, about 60 turns to pay back. A
/// conversation that ends before then paid that fee for nothing. The gate does
/// not improve the ratio, which is structural; it only keeps the short ones out.
pub fn inject_context_management(
    body: &mut Value,
    clear_tool_uses_keep: Option<u64>,
    clear_tool_uses_min_messages: usize,
    clear_tool_uses_trigger: u64,
    clear_tool_uses_at_least: Option<u64>,
    clear_thinking_keep_turns: Option<u64>,
) -> bool {
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map_or(0, Vec::len);
    let clear_tool_uses_keep =
        clear_tool_uses_keep.filter(|_| messages >= clear_tool_uses_min_messages);
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

    let mut changed = false;

    // `clear_thinking` leads the array, per the API's ordering requirement.
    if let Some(turns) = clear_thinking_keep_turns {
        let keep = serde_json::json!({ "type": "thinking_turns", "value": turns });
        match edits_arr
            .iter_mut()
            .find(|e| is_family(e, "clear_thinking"))
        {
            Some(existing) => {
                if existing.get("keep") != Some(&keep) {
                    if let Some(obj) = existing.as_object_mut() {
                        obj.insert("keep".to_string(), keep);
                        changed = true;
                    }
                }
            }
            None => {
                edits_arr.insert(
                    0,
                    serde_json::json!({
                        "type": "clear_thinking_20251015",
                        "keep": keep,
                    }),
                );
                changed = true;
            }
        }
    }

    if let Some(keep) = clear_tool_uses_keep {
        if !edits_arr.iter().any(|e| is_family(e, "clear_tool_uses")) {
            let mut edit = serde_json::json!({
                "type": "clear_tool_uses_20250919",
                "trigger": { "type": "input_tokens", "value": clear_tool_uses_trigger },
                "keep": { "type": "tool_uses", "value": keep },
            });
            // Without this the API will clear a handful of tokens and charge a
            // full cache write for it; below the floor it skips the strategy
            // entirely and the cached prefix survives.
            if let Some(at_least) = clear_tool_uses_at_least {
                edit["clear_at_least"] =
                    serde_json::json!({ "type": "input_tokens", "value": at_least });
            }
            edits_arr.push(edit);
            changed = true;
        }
    }

    changed
}

/// Whether an edit belongs to a strategy family (`clear_thinking`,
/// `clear_tool_uses`), ignoring the dated suffix.
fn is_family(edit: &Value, family: &str) -> bool {
    edit.get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.starts_with(family))
}

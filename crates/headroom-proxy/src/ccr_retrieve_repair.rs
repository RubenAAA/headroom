//! CCR retrieve history repair (upstream #2814).
//!
//! Port of upstream `helpers.strip_unsupported_ccr_retrieve_blocks`.
//!
//! Claude Code replays one transcript across requests carrying different
//! `tools` arrays, and Anthropic validates every history `tool_use` against
//! the array of the request at hand. A side-request that the proxy forwards
//! without declaring `headroom_retrieve` then 400s on a historical `tool_use`
//! naming it. When the request does NOT declare the tool, each
//! `headroom_retrieve` `tool_use` block and its paired `tool_result` are
//! replaced with text blocks, so no dangling reference survives.
//!
//! Neutralize rather than drop: the `tool_use` (assistant turn) and its
//! `tool_result` (next user turn) live in DIFFERENT messages, so removing a
//! message could leave two same-role messages adjacent and break Anthropic's
//! user/assistant alternation. Replacing blocks in place keeps every message
//! and role intact, and preserves the retrieved text the model already saw.

use serde_json::Value;

const CALL_OMITTED: &str = "[headroom_retrieve call omitted: tool not available this turn]";
const RESULT_OMITTED: &str = "[headroom_retrieve result omitted]";

fn ccr_tool_name() -> &'static str {
    headroom_core::ccr::tool_injection::CCR_TOOL_NAME
}

/// Flatten a `tool_result` block's content to plain text, preserving what
/// the model already saw. Falls back to a short placeholder when there is
/// no textual content to keep.
fn result_as_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(Value::Array(items)) => {
            let joined = items
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(Value::as_str) != Some("text") {
                        return None;
                    }
                    let text = b.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.is_empty() {
                        None
                    } else {
                        Some(text)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if joined.trim().is_empty() {
                RESULT_OMITTED.to_string()
            } else {
                joined
            }
        }
        _ => RESULT_OMITTED.to_string(),
    }
}

/// Outcome of [`strip_unsupported_ccr_blocks`].
pub struct CcrRepairOutcome {
    /// Repaired messages, or the input moved back unchanged when nothing
    /// was neutralized. Callers must only re-serialize when
    /// `neutralized > 0`.
    pub messages: Vec<Value>,
    /// Number of blocks neutralized (`tool_use` + `tool_result` = 2 per
    /// call). Zero means unchanged.
    pub neutralized: usize,
}

/// Neutralize `headroom_retrieve` history references the outbound `tools`
/// array cannot support.
///
/// No-op returning the input unchanged when the tool is declared this turn
/// (top-level `name` match — the Anthropic shape; this repair only runs on
/// the Anthropic path) or when no retrieve references exist.
pub fn strip_unsupported_ccr_blocks(messages: Vec<Value>, tools: &[Value]) -> CcrRepairOutcome {
    let unchanged = |messages: Vec<Value>| CcrRepairOutcome {
        messages,
        neutralized: 0,
    };
    let name = ccr_tool_name();
    let declared = tools.iter().any(|t| {
        t.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n == name)
    });
    if declared {
        return unchanged(messages);
    }

    // First pass: collect the ids of retrieve `tool_use` blocks so their
    // paired `tool_result` blocks (in a later user turn) can be matched.
    let mut retrieve_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for message in &messages {
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_use")
                && block.get("name").and_then(Value::as_str) == Some(name)
            {
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    retrieve_ids.insert(id.to_string());
                }
            }
        }
    }
    if retrieve_ids.is_empty() {
        return unchanged(messages);
    }

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut neutralized = 0usize;
    let mut changed = false;
    for mut message in messages {
        let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
            out.push(message);
            continue;
        };
        let mut touched = false;
        for block in content.iter_mut() {
            let Some(obj) = block.as_object() else {
                continue;
            };
            if obj.get("type").and_then(Value::as_str) == Some("tool_use")
                && obj.get("name").and_then(Value::as_str) == Some(name)
            {
                *block = Value::Object(
                    [
                        ("type".to_string(), Value::String("text".to_string())),
                        ("text".to_string(), Value::String(CALL_OMITTED.to_string())),
                    ]
                    .into_iter()
                    .collect(),
                );
                neutralized += 1;
                touched = true;
            } else if obj.get("type").and_then(Value::as_str) == Some("tool_result")
                && obj
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| retrieve_ids.contains(id))
            {
                let text = result_as_text(block);
                *block = Value::Object(
                    [
                        ("type".to_string(), Value::String("text".to_string())),
                        ("text".to_string(), Value::String(text)),
                    ]
                    .into_iter()
                    .collect(),
                );
                neutralized += 1;
                touched = true;
            }
        }
        if touched {
            changed = true;
        }
        out.push(message);
    }
    if changed {
        CcrRepairOutcome {
            messages: out,
            neutralized,
        }
    } else {
        unchanged(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn retrieve_use(id: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": "headroom_retrieve",
               "input": {"hash": "abc"}})
    }

    fn retrieve_result(id: &str, content: Value) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": content})
    }

    fn declared_tools() -> Vec<Value> {
        vec![json!({"name": "headroom_retrieve", "input_schema": {"type": "object"}})]
    }

    #[test]
    fn declared_tool_is_untouched() {
        let messages = vec![json!({
            "role": "assistant",
            "content": [retrieve_use("r1")],
        })];
        let before = messages.clone();
        let out = strip_unsupported_ccr_blocks(messages, &declared_tools());
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn undeclared_pair_is_neutralized_in_place() {
        let messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "looking it up"},
                retrieve_use("r1"),
            ]}),
            json!({"role": "user", "content": [
                retrieve_result("r1", json!("the original content")),
            ]}),
        ];
        let out = strip_unsupported_ccr_blocks(messages, &[]);
        assert_eq!(out.neutralized, 2);
        // Roles and message count intact (alternation safety).
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[0]["role"], json!("assistant"));
        assert_eq!(out.messages[1]["role"], json!("user"));
        assert_eq!(out.messages[0]["content"][1]["text"], json!(CALL_OMITTED));
        assert_eq!(
            out.messages[1]["content"][0]["text"],
            json!("the original content")
        );
    }

    #[test]
    fn list_result_content_is_flattened() {
        let messages = vec![json!({"role": "user", "content": [
            retrieve_result("r1", json!([
                {"type": "text", "text": "part one"},
                {"type": "image", "source": {}},
                {"type": "text", "text": "part two"},
            ])),
        ]})];
        // Seed a retrieve id via a use block so the result pairs up.
        let messages = {
            let mut m = vec![json!({"role": "assistant", "content": [retrieve_use("r1")]})];
            m.extend(messages);
            m
        };
        let out = strip_unsupported_ccr_blocks(messages, &[]);
        assert_eq!(out.neutralized, 2);
        assert_eq!(
            out.messages[1]["content"][0]["text"],
            json!("part one\npart two")
        );
    }

    #[test]
    fn empty_result_falls_back_to_placeholder() {
        let messages = vec![
            json!({"role": "assistant", "content": [retrieve_use("r1")]}),
            json!({"role": "user", "content": [
                retrieve_result("r1", json!("")),
            ]}),
        ];
        let out = strip_unsupported_ccr_blocks(messages, &[]);
        assert_eq!(out.neutralized, 2);
        assert_eq!(out.messages[1]["content"][0]["text"], json!(RESULT_OMITTED));
    }

    #[test]
    fn foreign_blocks_are_untouched() {
        let messages = vec![json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {}},
        ]})];
        let before = messages.clone();
        let out = strip_unsupported_ccr_blocks(messages, &[json!({"name": "Read"})]);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn non_list_messages_pass_through() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let before = messages.clone();
        let out = strip_unsupported_ccr_blocks(messages, &[]);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }
}

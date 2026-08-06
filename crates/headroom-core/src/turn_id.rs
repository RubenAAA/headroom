//! Turn-id computation (Rust port of `compute_turn_id` /
//! `_strip_per_call_annotations` in `headroom/proxy/helpers.py` ~L2848-2928).
//!
//! A "turn" spans one user prompt plus every assistant tool-use / user
//! tool-result message appended while executing it. Hashing the message prefix
//! up to and including the last user *text* message yields an id that is stable
//! across the turn but rolls over on a new prompt. `cache_control` annotations
//! (which clients move between messages every call) are stripped first so the
//! id does not collapse to a per-request value.
//!
//! **Parity note:** Python hashes `json.dumps(prefix, sort_keys=True,
//! default=str)`. To keep the id identical across the Python↔Rust boundary,
//! [`python_json_dumps`] reproduces CPython's `json.dumps` default output
//! byte-for-byte: keys sorted, `", "` item / `": "` key separators, and
//! `ensure_ascii` escaping (all non-ASCII → `\uXXXX`).

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Recursively drop every `cache_control` key from an object tree. Mirrors
/// `_strip_per_call_annotations`.
pub fn strip_per_call_annotations(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, v)| (k.clone(), strip_per_call_annotations(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_per_call_annotations).collect()),
        other => other.clone(),
    }
}

/// Serialize a JSON value exactly like CPython `json.dumps(value,
/// sort_keys=True, default=str)` with default separators (`", "` / `": "`) and
/// `ensure_ascii=True`.
pub fn python_json_dumps(value: &Value) -> String {
    let mut out = String::new();
    dump(value, &mut out);
    out
}

fn dump(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => dump_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump_str(k, out);
                out.push_str(": ");
                dump(&map[*k], out);
            }
            out.push('}');
        }
    }
}

/// Emit a JSON string literal matching CPython `json.dumps` with
/// `ensure_ascii=True`.
fn dump_str(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                // ensure_ascii: escape everything >= 0x7f, using UTF-16
                // surrogate pairs for astral-plane code points.
                let cp = c as u32;
                if cp <= 0xFFFF {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    let v = cp - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", hi, lo));
                }
            }
        }
    }
    out.push('"');
}

/// Compute a stable turn id, or `None` when no user-text message is present.
///
/// `system` may be a JSON string, an array/object (Anthropic system blocks), or
/// `Value::Null`/absent. Ports `compute_turn_id`.
pub fn compute_turn_id(model: &str, system: &Value, messages: &[Value]) -> Option<String> {
    if messages.is_empty() {
        return None;
    }

    // Find the last user *text* message (text present, no tool_result blocks).
    let mut last_text_user_idx: Option<usize> = None;
    for i in (0..messages.len()).rev() {
        let msg = &messages[i];
        if !msg.is_object() || msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let content = msg.get("content");
        match content {
            Some(Value::String(s)) if !s.is_empty() => {
                last_text_user_idx = Some(i);
                break;
            }
            Some(Value::Array(blocks)) => {
                let has_text = blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("text"));
                let has_tool_result = blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
                if has_text && !has_tool_result {
                    last_text_user_idx = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }

    let idx = last_text_user_idx?;
    let prefix: Vec<Value> = messages[..=idx]
        .iter()
        .map(strip_per_call_annotations)
        .collect();
    let prefix_json = python_json_dumps(&Value::Array(prefix));

    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update([0u8]);
    match system {
        Value::String(s) => h.update(s.as_bytes()),
        Value::Null => {}
        other => {
            let normalized = strip_per_call_annotations(other);
            h.update(python_json_dumps(&normalized).as_bytes());
        }
    }
    h.update([0u8]);
    h.update(prefix_json.as_bytes());
    let digest = h.finalize();
    Some(hex::encode(digest)[..16].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_removes_cache_control_recursively() {
        let v = json!({
            "role": "user",
            "cache_control": {"type": "ephemeral"},
            "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]
        });
        let stripped = strip_per_call_annotations(&v);
        assert!(stripped.get("cache_control").is_none());
        assert!(stripped["content"][0].get("cache_control").is_none());
        assert_eq!(stripped["content"][0]["text"], json!("hi"));
    }

    #[test]
    fn python_json_dumps_sorts_keys_and_spaces() {
        let v = json!({"b": 1, "a": [1, 2], "c": "x"});
        assert_eq!(python_json_dumps(&v), r#"{"a": [1, 2], "b": 1, "c": "x"}"#);
    }

    #[test]
    fn python_json_dumps_ensure_ascii() {
        let v = json!("café → ☕");
        // Non-ASCII escaped as \uXXXX, matching json.dumps default (ensure_ascii).
        let expected = "\"caf\\u00e9 \\u2192 \\u2615\"";
        assert_eq!(python_json_dumps(&v), expected);
    }

    #[test]
    fn no_messages_is_none() {
        assert!(compute_turn_id("m", &Value::Null, &[]).is_none());
    }

    #[test]
    fn no_user_text_is_none() {
        // Only a tool_result continuation → no fresh user turn.
        let messages = vec![json!({
            "role": "user",
            "content": [{"type": "tool_result", "content": "ok"}]
        })];
        assert!(compute_turn_id("m", &Value::Null, &messages).is_none());
    }

    #[test]
    fn string_content_yields_id() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let id = compute_turn_id("claude", &Value::Null, &messages).unwrap();
        assert_eq!(id.len(), 16);
    }

    #[test]
    fn cache_control_does_not_change_id() {
        // The same conversation with vs without a moved cache breakpoint must
        // hash identically (the whole point of stripping annotations).
        let plain = vec![json!({"role": "user", "content": "hello"})];
        let annotated = vec![json!({
            "role": "user",
            "content": "hello",
            "cache_control": {"type": "ephemeral"}
        })];
        assert_eq!(
            compute_turn_id("claude", &Value::Null, &plain),
            compute_turn_id("claude", &Value::Null, &annotated),
        );
    }

    #[test]
    fn id_is_deterministic() {
        let messages = vec![
            json!({"role": "user", "content": "do the thing"}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "name": "x"}]}),
        ];
        let system = json!("you are helpful");
        let a = compute_turn_id("claude", &system, &messages).unwrap();
        let b = compute_turn_id("claude", &system, &messages).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn turn_rolls_over_on_new_prompt() {
        let turn1 = vec![json!({"role": "user", "content": "first"})];
        let turn2 = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": "answer"}),
            json!({"role": "user", "content": "second"}),
        ];
        assert_ne!(
            compute_turn_id("claude", &Value::Null, &turn1),
            compute_turn_id("claude", &Value::Null, &turn2),
        );
    }

    #[test]
    fn system_array_normalized() {
        // System as blocks with cache_control should still hash stably and
        // equal the same blocks without cache_control.
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let sys_plain = json!([{"type": "text", "text": "sys"}]);
        let sys_annotated =
            json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}]);
        assert_eq!(
            compute_turn_id("claude", &sys_plain, &messages),
            compute_turn_id("claude", &sys_annotated, &messages),
        );
    }
}

//! Protocol, registry, and pipeline for tool_result interceptors.
//!
//! Mirrors Python's `headroom.proxy.interceptors.base`.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

// ─── Failure tracking ────────────────────────────────────────────────────

static FAILURES: LazyLock<Mutex<HashMap<String, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn record_failure(name: &str) {
    let mut map = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    *map.entry(name.to_string()).or_insert(0) += 1;
}

pub fn interceptor_failure_counts() -> HashMap<String, usize> {
    FAILURES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn reset_interceptor_failure_counts() {
    FAILURES.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ─── ToolResultInterceptor trait ─────────────────────────────────────────

/// A stateless rewriter for a single tool_result's text content.
///
/// Implementations MUST be idempotent and MUST return either a strictly
/// smaller string (measured in tokens) or None to pass through.
pub trait ToolResultInterceptor: Send + Sync {
    fn name(&self) -> &str;

    fn matches(&self, tool_name: Option<&str>, tool_input: &Value, tool_output: &str) -> bool;

    fn transform(
        &self,
        tool_name: Option<&str>,
        tool_input: &Value,
        tool_output: &str,
    ) -> Option<String>;

    /// Optional: return a stable content key (e.g. file path).
    /// If a key is returned and the same (interceptor.name, key) pair was
    /// already successfully rewritten earlier, subsequent occurrences pass
    /// through unchanged.
    fn progressive_disclosure_key(
        &self,
        _tool_name: Option<&str>,
        _tool_input: &Value,
    ) -> Option<String> {
        None
    }
}

// ─── Types ───────────────────────────────────────────────────────────────

/// Per-interceptor measurement for metrics.
#[derive(Debug, Clone)]
pub struct TransformSpan {
    pub tool: String,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

impl TransformSpan {
    pub fn tokens_saved(&self) -> usize {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}

/// Result of running interceptors on messages.
#[derive(Debug)]
pub struct InterceptionResult {
    pub messages: Vec<Value>,
    pub spans: Vec<TransformSpan>,
}

// ─── Registry ────────────────────────────────────────────────────────────

pub static INTERCEPTORS: LazyLock<Mutex<Vec<Box<dyn ToolResultInterceptor>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

pub fn register(interceptor: Box<dyn ToolResultInterceptor>) {
    let mut list = INTERCEPTORS.lock().unwrap_or_else(|e| e.into_inner());
    let name = interceptor.name().to_string();
    if list.iter().any(|i| i.name() == name) {
        return;
    }
    list.push(interceptor);
}

// ─── Message helpers ─────────────────────────────────────────────────────

/// Check if a message is a tool result (Anthropic or OpenAI format).
pub fn is_tool_result_message(msg: &Value) -> bool {
    // OpenAI format: role="tool"
    if msg.get("role").and_then(Value::as_str) == Some("tool") {
        return true;
    }
    // Anthropic format: role="user" with content list containing tool_result blocks
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        return content
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
    }
    false
}

/// Extract text content from a tool result message.
pub fn extract_tool_result_content(msg: &Value) -> Option<String> {
    // OpenAI format
    if msg.get("role").and_then(Value::as_str) == Some("tool") {
        return msg.get("content").and_then(Value::as_str).map(String::from);
    }
    // Anthropic format
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                if let Some(inner) = block.get("content").and_then(Value::as_str) {
                    return Some(inner.to_string());
                }
            }
        }
    }
    None
}

/// Replace tool result content in a message (deep copy).
pub fn swap_tool_result_content(msg: &Value, new_content: &str) -> Value {
    let mut new_msg = msg.clone();

    // OpenAI format
    if new_msg.get("role").and_then(Value::as_str) == Some("tool") {
        new_msg["content"] = Value::String(new_content.to_string());
        return new_msg;
    }

    // Anthropic format
    if let Some(content) = new_msg.get_mut("content").and_then(Value::as_array_mut) {
        for block in content.iter_mut() {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                block["content"] = Value::String(new_content.to_string());
                break;
            }
        }
    }

    new_msg
}

// ─── Tool use index ──────────────────────────────────────────────────────

/// Build O(1) tool_use lookup: {tool_use_id: (tool_name, tool_input)}.
pub fn build_tool_use_index(messages: &[Value]) -> HashMap<String, (Option<String>, Value)> {
    let mut index = HashMap::new();

    for msg in messages {
        // Anthropic: content blocks with type=tool_use
        if let Some(content) = msg.get("content").and_then(Value::as_array) {
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    let name = block.get("name").and_then(Value::as_str).map(String::from);
                    let input = block
                        .get("input")
                        .cloned()
                        .unwrap_or(Value::Object(serde_json::Map::new()));
                    index.insert(id.to_string(), (name, input));
                }
            }
        }

        // OpenAI: assistant message with tool_calls list
        if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    let fn_obj = call.get("function");
                    let name = fn_obj
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let args = fn_obj
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| match a {
                            Value::String(s) => serde_json::from_str(s).ok(),
                            Value::Object(_) => Some(a.clone()),
                            _ => None,
                        })
                        .unwrap_or(Value::Object(serde_json::Map::new()));
                    index.insert(id.to_string(), (name, args));
                }
            }
        }
    }

    index
}

/// Extract tool_use_id from a tool result message.
pub fn tool_use_id_for_message(msg: &Value) -> Option<String> {
    // Anthropic format
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                    return Some(id.to_string());
                }
            }
        }
    }
    // OpenAI format
    if msg.get("role").and_then(Value::as_str) == Some("tool") {
        if let Some(id) = msg.get("tool_call_id").and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }
    None
}

// ─── Token counting (simple approximation) ───────────────────────────────

/// Simple token count approximation: split on whitespace.
/// For production use, this should be replaced with a real tokenizer.
pub fn count_tokens(text: &str) -> usize {
    text.split_whitespace().count()
}

// ─── Pipeline ────────────────────────────────────────────────────────────

/// Run every registered interceptor against every tool_result in messages.
///
/// `frozen_count`: leading messages that must be passed through verbatim.
/// Their tool_uses are still scanned for progressive disclosure state.
pub fn apply_to_messages(messages: Vec<Value>, frozen_count: usize) -> InterceptionResult {
    let interceptors = INTERCEPTORS.lock().unwrap_or_else(|e| e.into_inner());

    if interceptors.is_empty() {
        return InterceptionResult {
            messages,
            spans: vec![],
        };
    }

    let mut spans = Vec::new();
    // Progressive disclosure: per-interceptor set of keys already rewritten
    let mut fired: HashMap<String, HashSet<String>> = HashMap::new();

    // Build O(1) tool_use lookup index
    let tool_use_index = build_tool_use_index(&messages);

    // Pre-seed fired from frozen prefix
    for msg in messages.iter().take(frozen_count) {
        if !is_tool_result_message(msg) {
            continue;
        }
        if let Some(tuid) = tool_use_id_for_message(msg) {
            let (tool_name, tool_input) = tool_use_index
                .get(&tuid)
                .map(|(n, i)| (n.clone(), i.clone()))
                .unwrap_or_else(|| (None, Value::Object(serde_json::Map::new())));
            for interceptor in interceptors.iter() {
                if let Some(key) =
                    interceptor.progressive_disclosure_key(tool_name.as_deref(), &tool_input)
                {
                    fired
                        .entry(interceptor.name().to_string())
                        .or_default()
                        .insert(key);
                }
            }
        }
    }

    let mut new_messages: Vec<Value> = messages[..frozen_count].to_vec();

    for msg in messages[frozen_count..].iter() {
        if !is_tool_result_message(msg) {
            new_messages.push(msg.clone());
            continue;
        }

        let original = match extract_tool_result_content(msg) {
            Some(c) if !c.is_empty() => c,
            _ => {
                new_messages.push(msg.clone());
                continue;
            }
        };

        let tuid = tool_use_id_for_message(msg);
        let (tool_name, tool_input) = match tuid.as_ref().and_then(|id| tool_use_index.get(id)) {
            Some((n, i)) => (n.clone(), i.clone()),
            None => (None, Value::Object(serde_json::Map::new())),
        };

        let mut current = original.clone();

        for interceptor in interceptors.iter() {
            let interceptor_name = interceptor.name().to_string();

            // Progressive disclosure check (with panic guard)
            let key = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                interceptor.progressive_disclosure_key(tool_name.as_deref(), &tool_input)
            }))
            .unwrap_or_else(|e| {
                record_failure(&interceptor_name);
                eprintln!("interceptor {} key() panicked: {:?}", interceptor_name, e);
                None
            });

            if let Some(ref k) = key {
                if fired
                    .get(&interceptor_name)
                    .map(|s| s.contains(k))
                    .unwrap_or(false)
                {
                    continue;
                }
            }

            // matches() with panic guard
            let matched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                interceptor.matches(tool_name.as_deref(), &tool_input, &current)
            }))
            .unwrap_or_else(|e| {
                record_failure(&interceptor_name);
                eprintln!(
                    "interceptor {} matches() panicked: {:?}",
                    interceptor_name, e
                );
                false
            });

            if !matched {
                continue;
            }

            // transform() with panic guard
            let transformed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                interceptor.transform(tool_name.as_deref(), &tool_input, &current)
            }))
            .unwrap_or_else(|e| {
                record_failure(&interceptor_name);
                eprintln!(
                    "interceptor {} transform() panicked: {:?}",
                    interceptor_name, e
                );
                None
            });

            if let Some(rewritten) = transformed {
                if rewritten != current {
                    let before = count_tokens(&current);
                    let after = count_tokens(&rewritten);
                    if after >= before {
                        continue;
                    }
                    spans.push(TransformSpan {
                        tool: interceptor_name.clone(),
                        tokens_before: before,
                        tokens_after: after,
                    });
                    current = rewritten;
                    if let Some(k) = key {
                        fired.entry(interceptor_name).or_default().insert(k);
                    }
                }
            }
        }

        if current != original {
            new_messages.push(swap_tool_result_content(msg, &current));
        } else {
            new_messages.push(msg.clone());
        }
    }

    InterceptionResult {
        messages: new_messages,
        spans,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- is_tool_result_message ---

    #[test]
    fn is_tool_result_openai() {
        let msg = json!({"role": "tool", "content": "result"});
        assert!(is_tool_result_message(&msg));
    }

    #[test]
    fn is_tool_result_anthropic() {
        let msg = json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "tu_1", "content": "result"}]
        });
        assert!(is_tool_result_message(&msg));
    }

    #[test]
    fn is_tool_result_false_for_text() {
        let msg = json!({"role": "user", "content": "hello"});
        assert!(!is_tool_result_message(&msg));
    }

    #[test]
    fn is_tool_result_false_for_assistant() {
        let msg = json!({"role": "assistant", "content": "hi"});
        assert!(!is_tool_result_message(&msg));
    }

    // --- extract_tool_result_content ---

    #[test]
    fn extract_openai() {
        let msg = json!({"role": "tool", "content": "the result"});
        assert_eq!(
            extract_tool_result_content(&msg).as_deref(),
            Some("the result")
        );
    }

    #[test]
    fn extract_anthropic() {
        let msg = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "here"},
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "tool output"}
            ]
        });
        assert_eq!(
            extract_tool_result_content(&msg).as_deref(),
            Some("tool output")
        );
    }

    #[test]
    fn extract_none_for_text_message() {
        let msg = json!({"role": "user", "content": "hello"});
        assert!(extract_tool_result_content(&msg).is_none());
    }

    // --- swap_tool_result_content ---

    #[test]
    fn swap_openai() {
        let msg = json!({"role": "tool", "content": "old"});
        let swapped = swap_tool_result_content(&msg, "new");
        assert_eq!(swapped["content"], "new");
        assert_eq!(swapped["role"], "tool");
    }

    #[test]
    fn swap_anthropic() {
        let msg = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "keep"},
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "old"}
            ]
        });
        let swapped = swap_tool_result_content(&msg, "new");
        let content = swapped["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "keep");
        assert_eq!(content[1]["content"], "new");
    }

    #[test]
    fn swap_preserves_original() {
        let msg = json!({"role": "tool", "content": "original"});
        let _swapped = swap_tool_result_content(&msg, "changed");
        assert_eq!(msg["content"], "original");
    }

    // --- build_tool_use_index ---

    #[test]
    fn index_anthropic() {
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": "Read", "input": {"file_path": "/foo"}}
            ]
        })];
        let index = build_tool_use_index(&messages);
        assert_eq!(index.len(), 1);
        let (name, input) = index.get("tu_1").unwrap();
        assert_eq!(name.as_deref(), Some("Read"));
        assert_eq!(input["file_path"], "/foo");
    }

    #[test]
    fn index_openai() {
        let messages = vec![json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "tc_1",
                "type": "function",
                "function": {"name": "grep", "arguments": "{\"pattern\":\"foo\"}"}
            }]
        })];
        let index = build_tool_use_index(&messages);
        assert_eq!(index.len(), 1);
        let (name, input) = index.get("tc_1").unwrap();
        assert_eq!(name.as_deref(), Some("grep"));
        assert_eq!(input["pattern"], "foo");
    }

    #[test]
    fn index_empty() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let index = build_tool_use_index(&messages);
        assert!(index.is_empty());
    }

    // --- tool_use_id_for_message ---

    #[test]
    fn tuid_anthropic() {
        let msg = json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "tu_42", "content": "x"}]
        });
        assert_eq!(tool_use_id_for_message(&msg).as_deref(), Some("tu_42"));
    }

    #[test]
    fn tuid_openai() {
        let msg = json!({"role": "tool", "tool_call_id": "tc_7", "content": "x"});
        assert_eq!(tool_use_id_for_message(&msg).as_deref(), Some("tc_7"));
    }

    #[test]
    fn tuid_none_for_text() {
        let msg = json!({"role": "user", "content": "hello"});
        assert!(tool_use_id_for_message(&msg).is_none());
    }

    // --- count_tokens ---

    #[test]
    fn count_tokens_basic() {
        assert_eq!(count_tokens("hello world"), 2);
        assert_eq!(count_tokens(""), 0);
        assert_eq!(count_tokens("  spaced  out  "), 2);
    }

    // --- apply_to_messages (basic) ---

    #[test]
    fn apply_empty_interceptors() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let result = apply_to_messages(messages.clone(), 0);
        assert_eq!(result.messages.len(), 1);
        assert!(result.spans.is_empty());
    }

    // --- Failure tracking ---

    #[test]
    fn failure_counts() {
        reset_interceptor_failure_counts();
        record_failure("test-interceptor");
        record_failure("test-interceptor");
        record_failure("other");
        let counts = interceptor_failure_counts();
        assert_eq!(counts.get("test-interceptor"), Some(&2));
        assert_eq!(counts.get("other"), Some(&1));
        reset_interceptor_failure_counts();
        assert!(interceptor_failure_counts().is_empty());
    }

    // --- TransformSpan ---

    #[test]
    fn span_tokens_saved() {
        let span = TransformSpan {
            tool: "test".to_string(),
            tokens_before: 100,
            tokens_after: 30,
        };
        assert_eq!(span.tokens_saved(), 70);
    }

    #[test]
    fn span_tokens_saved_floor_at_zero() {
        let span = TransformSpan {
            tool: "test".to_string(),
            tokens_before: 30,
            tokens_after: 100,
        };
        assert_eq!(span.tokens_saved(), 0);
    }
}

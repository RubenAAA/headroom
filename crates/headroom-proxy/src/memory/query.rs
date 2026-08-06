//! ``MemoryQuery``: multi-source, full-fidelity retrieval query.
//!
//! Captures user text, recent tool outputs, and recent assistant turns
//! at full fidelity (no truncation). The embedding model handles its
//! own context window.
//!
//! Mirrors Python's `headroom.proxy.memory_query`.

use serde_json::Value;

const USER_DELIM: &str = "### USER ###\n";
const ASSISTANT_DELIM: &str = "\n### PRIOR_ASSISTANT ###\n";
const TOOL_DELIM: &str = "\n### TOOL_OUTPUT ###\n";

/// Frozen multi-source query for memory retrieval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQuery {
    pub user_text: String,
    pub recent_tool_outputs: Vec<String>,
    pub recent_assistant_turns: Vec<String>,
    pub conversation_id: Option<String>,
}

impl MemoryQuery {
    /// Concatenate sources into a delimited embedding input.
    /// Order: assistant turns → tool outputs → user text.
    pub fn to_embedding_input(&self) -> String {
        let mut parts = Vec::new();
        for asst in &self.recent_assistant_turns {
            if !asst.is_empty() {
                parts.push(format!("{}{}", ASSISTANT_DELIM, asst));
            }
        }
        for tool_out in &self.recent_tool_outputs {
            if !tool_out.is_empty() {
                parts.push(format!("{}{}", TOOL_DELIM, tool_out));
            }
        }
        if !self.user_text.is_empty() {
            parts.push(format!("{}{}", USER_DELIM, self.user_text));
        }
        parts.join("")
    }

    /// Construct from a chat-style messages list.
    ///
    /// Walks backward, extracts latest user text, recent assistant turns,
    /// and recent tool outputs. Handles both OpenAI and Anthropic formats.
    pub fn from_messages(
        messages: &[Value],
        lookback_assistant: usize,
        lookback_tools: usize,
        conversation_id: Option<String>,
    ) -> Self {
        let mut user_text = String::new();
        let mut assistant_turns: Vec<String> = Vec::new();
        let mut tool_outputs: Vec<String> = Vec::new();

        for msg in messages.iter().rev() {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            let content = msg.get("content");

            match role {
                "user" => {
                    if let Some(Value::Array(blocks)) = content {
                        // Anthropic tool_result masquerading as user message
                        for block in blocks {
                            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                                let tool_text = extract_tool_result_text(block);
                                if !tool_text.is_empty() && tool_outputs.len() < lookback_tools {
                                    tool_outputs.push(tool_text);
                                }
                            }
                        }
                        // NOT a real user turn — continue walking
                    } else if let Some(Value::String(text)) = content {
                        if user_text.is_empty() {
                            user_text = text.clone();
                        }
                    }
                }
                "assistant" => match content {
                    Some(Value::String(text)) if !text.is_empty() => {
                        if assistant_turns.len() < lookback_assistant {
                            assistant_turns.push(text.clone());
                        }
                    }
                    Some(Value::Array(blocks)) => {
                        let joined: String = blocks
                            .iter()
                            .filter_map(|b| {
                                if b.get("type").and_then(Value::as_str) == Some("text") {
                                    b.get("text").and_then(Value::as_str).map(String::from)
                                } else {
                                    None
                                }
                            })
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !joined.is_empty() && assistant_turns.len() < lookback_assistant {
                            assistant_turns.push(joined);
                        }
                    }
                    _ => {}
                },
                "tool" => {
                    if let Some(Value::String(text)) = content {
                        if !text.is_empty() && tool_outputs.len() < lookback_tools {
                            tool_outputs.push(text.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        // Reverse to restore chronological order
        assistant_turns.reverse();
        tool_outputs.reverse();

        Self {
            user_text,
            recent_tool_outputs: tool_outputs,
            recent_assistant_turns: assistant_turns,
            conversation_id,
        }
    }
}

/// Extract text from an Anthropic tool_result block.
fn extract_tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    b.get("text").and_then(Value::as_str).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_embedding_input_empty() {
        let q = MemoryQuery {
            user_text: String::new(),
            recent_tool_outputs: vec![],
            recent_assistant_turns: vec![],
            conversation_id: None,
        };
        assert!(q.to_embedding_input().is_empty());
    }

    #[test]
    fn to_embedding_input_with_all_sources() {
        let q = MemoryQuery {
            user_text: "hello".to_string(),
            recent_tool_outputs: vec!["tool result".to_string()],
            recent_assistant_turns: vec!["assistant text".to_string()],
            conversation_id: None,
        };
        let input = q.to_embedding_input();
        assert!(input.contains("### USER ###"));
        assert!(input.contains("### TOOL_OUTPUT ###"));
        assert!(input.contains("### PRIOR_ASSISTANT ###"));
        // User text last
        let user_pos = input.find("### USER ###").unwrap();
        let tool_pos = input.find("### TOOL_OUTPUT ###").unwrap();
        let asst_pos = input.find("### PRIOR_ASSISTANT ###").unwrap();
        assert!(asst_pos < tool_pos);
        assert!(tool_pos < user_pos);
    }

    #[test]
    fn from_messages_empty() {
        let q = MemoryQuery::from_messages(&[], 2, 3, None);
        assert!(q.user_text.is_empty());
        assert!(q.recent_tool_outputs.is_empty());
        assert!(q.recent_assistant_turns.is_empty());
    }

    #[test]
    fn from_messages_extracts_user_text() {
        let msgs = vec![
            json!({"role": "assistant", "content": "hi"}),
            json!({"role": "user", "content": "hello"}),
        ];
        let q = MemoryQuery::from_messages(&msgs, 2, 3, None);
        assert_eq!(q.user_text, "hello");
    }

    #[test]
    fn from_messages_extracts_assistant_turns() {
        let msgs = vec![
            json!({"role": "assistant", "content": "first"}),
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": "second"}),
        ];
        let q = MemoryQuery::from_messages(&msgs, 2, 3, None);
        assert_eq!(q.recent_assistant_turns.len(), 2);
        assert_eq!(q.recent_assistant_turns[0], "first");
        assert_eq!(q.recent_assistant_turns[1], "second");
    }

    #[test]
    fn from_messages_assistant_lookback() {
        let msgs = vec![
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "assistant", "content": "a2"}),
            json!({"role": "assistant", "content": "a3"}),
        ];
        let q = MemoryQuery::from_messages(&msgs, 2, 3, None);
        assert_eq!(q.recent_assistant_turns.len(), 2);
        // Chronological: a2, a3 (most recent 2)
        assert_eq!(q.recent_assistant_turns[0], "a2");
        assert_eq!(q.recent_assistant_turns[1], "a3");
    }

    #[test]
    fn from_messages_openai_tool_format() {
        let msgs = vec![
            json!({"role": "tool", "content": "tool result here"}),
            json!({"role": "user", "content": "what next"}),
        ];
        let q = MemoryQuery::from_messages(&msgs, 2, 3, None);
        assert_eq!(q.user_text, "what next");
        assert_eq!(q.recent_tool_outputs, vec!["tool result here"]);
    }

    #[test]
    fn from_messages_anthropic_tool_result() {
        let msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "result text"}
            ]
        })];
        let q = MemoryQuery::from_messages(&msgs, 2, 3, None);
        // Anthropic tool_result is NOT a real user turn
        assert!(q.user_text.is_empty());
        assert_eq!(q.recent_tool_outputs, vec!["result text"]);
    }

    #[test]
    fn from_messages_anthropic_text_blocks() {
        let msgs = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "part1"},
                {"type": "text", "text": "part2"}
            ]
        })];
        let q = MemoryQuery::from_messages(&msgs, 2, 3, None);
        assert_eq!(q.recent_assistant_turns, vec!["part1\npart2"]);
    }

    #[test]
    fn from_messages_tool_lookback() {
        let msgs = vec![
            json!({"role": "tool", "content": "t1"}),
            json!({"role": "tool", "content": "t2"}),
            json!({"role": "tool", "content": "t3"}),
        ];
        let q = MemoryQuery::from_messages(&msgs, 2, 2, None);
        assert_eq!(q.recent_tool_outputs.len(), 2);
        assert_eq!(q.recent_tool_outputs[0], "t2");
        assert_eq!(q.recent_tool_outputs[1], "t3");
    }

    #[test]
    fn from_messages_conversation_id() {
        let q = MemoryQuery::from_messages(&[], 2, 3, Some("conv-1".to_string()));
        assert_eq!(q.conversation_id.as_deref(), Some("conv-1"));
    }
}

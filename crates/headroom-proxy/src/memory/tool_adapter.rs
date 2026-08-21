//! Memory tool adapter for multi-provider support.
//!
//! Handles provider detection, tool injection, tool call extraction,
//! and result formatting for Anthropic, OpenAI, and Gemini providers.
//! Backend execution is delegated to a trait — the adapter itself is
//! pure logic, no I/O.
//!
//! Mirrors Python's `headroom.proxy.memory_tool_adapter`.

use std::collections::HashSet;

use serde_json::Value;

// ─── Constants ───────────────────────────────────────────────────────────

pub const MEMORY_TOOL_NAMES: &[&str] = &[
    "memory_save",
    "memory_search",
    "memory_update",
    "memory_delete",
    "memory_list",
];

pub const NATIVE_MEMORY_TOOL_NAME: &str = "memory";
pub const NATIVE_MEMORY_TOOL_TYPE: &str = "memory_20250818";
pub const ANTHROPIC_BETA_HEADER: &str = "context-management-2025-06-27";

// ─── Provider type ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Anthropic,
    Openai,
    Gemini,
    Generic,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::Anthropic => write!(f, "anthropic"),
            Provider::Openai => write!(f, "openai"),
            Provider::Gemini => write!(f, "gemini"),
            Provider::Generic => write!(f, "generic"),
        }
    }
}

// ─── Tool schemas ────────────────────────────────────────────────────────

/// Anthropic native memory tool definition.
pub fn anthropic_native_tool() -> Value {
    serde_json::json!({
        "type": NATIVE_MEMORY_TOOL_TYPE,
        "name": NATIVE_MEMORY_TOOL_NAME,
    })
}

/// Anthropic custom memory tools.
pub fn anthropic_custom_tools() -> Vec<Value> {
    vec![
        serde_json::json!({
            "name": "memory_save",
            "description": "Save important information to long-term memory for future reference.\n\nUse this tool when you encounter information that should be remembered across conversations:\n- User preferences, personal facts, project context, decisions, relationships\n\nDO NOT save: transient info, sensitive data (passwords, keys), redundant info.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "content": {"type": "string", "description": "The information to remember. Be specific and self-contained."},
                    "scope": {"type": "string", "enum": ["project", "global"], "description": "Where this belongs. 'project' (the default) files it under the current repository. Use 'global' for facts about the user, their preferences, or their tools, which are true whatever they are working on — filing those under one repository hides them everywhere else."},
                    "importance": {"type": "number", "minimum": 0.0, "maximum": 1.0, "description": "Importance score from 0.0 (low) to 1.0 (critical)."},
                    "facts": {"type": "array", "items": {"type": "string"}, "description": "Pre-extracted discrete facts."},
                    "entities": {"type": "array", "items": {"type": "string"}, "description": "Entity names referenced in this memory."},
                    "extracted_entities": {"type": "array", "items": {"type": "object", "properties": {"entity": {"type": "string"}, "entity_type": {"type": "string"}}, "required": ["entity", "entity_type"]}, "description": "Pre-extracted entities with types."},
                    "extracted_relationships": {"type": "array", "items": {"type": "object", "properties": {"source": {"type": "string"}, "relationship": {"type": "string"}, "destination": {"type": "string"}}, "required": ["source", "relationship", "destination"]}, "description": "Pre-extracted relationships for graph storage."},
                },
                "required": ["content", "importance"],
            },
        }),
        serde_json::json!({
            "name": "memory_search",
            "description": "Search stored memories to recall relevant information.\n\nUse this tool to retrieve previously saved information before responding to questions about user preferences, personal context, previously discussed topics, or relationships.\n\nSearch BEFORE saving to avoid duplicates.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Natural language search query."},
                    "entities": {"type": "array", "items": {"type": "string"}, "description": "Filter to memories mentioning these entities."},
                    "include_related": {"type": "boolean", "description": "Also retrieve connected memories."},
                    "top_k": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Maximum number of memories to retrieve (default 10)."},
                },
                "required": ["query"],
            },
        }),
        serde_json::json!({
            "name": "memory_update",
            "description": "Update an existing memory with corrected or evolved information.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string", "description": "The unique ID of the memory to update."},
                    "new_content": {"type": "string", "description": "The updated content."},
                    "reason": {"type": "string", "description": "Explanation for the update."},
                },
                "required": ["memory_id", "new_content"],
            },
        }),
        serde_json::json!({
            "name": "memory_delete",
            "description": "Delete a memory that is no longer relevant or was stored in error.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string", "description": "The unique ID of the memory to delete."},
                    "reason": {"type": "string", "description": "Explanation for the deletion."},
                },
                "required": ["memory_id"],
            },
        }),
        serde_json::json!({
            "name": "memory_list",
            "description": "Browse memories without a semantic query — list recent or all memories with their IDs.\n\nUse this when you want to see what's stored without a specific search term, or need a memory ID for memory_update/memory_delete but don't have a good search query.\n\nReturns memories in reverse chronological order (newest first).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "Maximum number of memories to return (default 10, max 100).", "minimum": 1, "maximum": 100},
                },
                "required": [],
            },
        }),
    ]
}

/// OpenAI function-calling memory tools.
pub fn openai_tools() -> Vec<Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "memory_save",
                "description": "Save important information to long-term memory for future reference.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "The information to remember."},
                        "scope": {"type": "string", "enum": ["project", "global"], "description": "'project' (default) files it under the current repository; 'global' for facts about the user or their tools, true whatever they are working on."},
                        "importance": {"type": "number", "minimum": 0.0, "maximum": 1.0, "description": "Importance score."},
                        "facts": {"type": "array", "items": {"type": "string"}, "description": "Pre-extracted discrete facts."},
                        "entities": {"type": "array", "items": {"type": "string"}, "description": "Entity names."},
                        "extracted_entities": {"type": "array", "items": {"type": "object", "properties": {"entity": {"type": "string"}, "entity_type": {"type": "string"}}, "required": ["entity", "entity_type"]}},
                        "extracted_relationships": {"type": "array", "items": {"type": "object", "properties": {"source": {"type": "string"}, "relationship": {"type": "string"}, "destination": {"type": "string"}}, "required": ["source", "relationship", "destination"]}},
                    },
                    "required": ["content", "importance"],
                },
            },
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "memory_search",
                "description": "Search stored memories to recall relevant information.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Natural language search query."},
                        "entities": {"type": "array", "items": {"type": "string"}},
                        "include_related": {"type": "boolean"},
                        "top_k": {"type": "integer", "minimum": 1, "maximum": 50},
                    },
                    "required": ["query"],
                },
            },
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "memory_update",
                "description": "Update an existing memory with corrected or evolved information.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "The unique ID of the memory to update."},
                        "new_content": {"type": "string", "description": "The updated content."},
                        "reason": {"type": "string"},
                    },
                    "required": ["memory_id", "new_content"],
                },
            },
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "memory_delete",
                "description": "Delete a memory that is no longer relevant.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "The unique ID of the memory to delete."},
                        "reason": {"type": "string"},
                    },
                    "required": ["memory_id"],
                },
            },
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "memory_list",
                "description": "Browse memories without a semantic query — list recent memories with IDs.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "description": "Max memories to return (default 10, max 100).", "minimum": 1, "maximum": 100},
                    },
                    "required": [],
                },
            },
        }),
    ]
}

/// Gemini function-calling memory tools.
pub fn gemini_tools() -> Vec<Value> {
    vec![
        serde_json::json!({
            "name": "memory_save",
            "description": "Save important information to long-term memory for future reference.",
            "parameters": {
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "importance": {"type": "number"},
                    "facts": {"type": "array", "items": {"type": "string"}},
                    "entities": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["content", "importance"],
            },
        }),
        serde_json::json!({
            "name": "memory_search",
            "description": "Search stored memories to recall relevant information.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "entities": {"type": "array", "items": {"type": "string"}},
                    "include_related": {"type": "boolean"},
                    "top_k": {"type": "integer"},
                },
                "required": ["query"],
            },
        }),
        serde_json::json!({
            "name": "memory_update",
            "description": "Update an existing memory.",
            "parameters": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "new_content": {"type": "string"},
                    "reason": {"type": "string"},
                },
                "required": ["memory_id", "new_content"],
            },
        }),
        serde_json::json!({
            "name": "memory_delete",
            "description": "Delete a memory that is no longer relevant.",
            "parameters": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "reason": {"type": "string"},
                },
                "required": ["memory_id"],
            },
        }),
        serde_json::json!({
            "name": "memory_list",
            "description": "Browse memories without a semantic query — list recent memories with IDs.",
            "parameters": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "Max memories to return (default 10, max 100)."},
                },
                "required": [],
            },
        }),
    ]
}

// ─── Configuration ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MemoryToolAdapterConfig {
    pub enabled: bool,
    pub use_native_tool: bool,
    pub inject_tools: bool,
    pub inject_context: bool,
}

impl Default for MemoryToolAdapterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            use_native_tool: true,
            inject_tools: true,
            inject_context: true,
        }
    }
}

// ─── Provider detection ──────────────────────────────────────────────────

/// Detect the LLM provider from request headers and model name.
pub fn detect_provider(headers: &Value, model_name: &str) -> Provider {
    let model = model_name.to_lowercase();

    // Check headers for provider hints
    if headers.get("x-api-key").is_some() || headers.get("anthropic-version").is_some() {
        return Provider::Anthropic;
    }
    if let Some(auth) = headers.get("authorization").and_then(Value::as_str) {
        if auth.starts_with("Bearer sk-") {
            return Provider::Openai;
        }
    }

    // Check model name patterns
    if model.starts_with("claude") {
        return Provider::Anthropic;
    }
    if model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3") {
        return Provider::Openai;
    }
    if model.starts_with("gemini") || model.contains("gemma") {
        return Provider::Gemini;
    }

    Provider::Generic
}

// ─── Tool injection ──────────────────────────────────────────────────────

/// Inject memory tools into the tools list for the given provider.
/// Returns (updated_tools, beta_headers).
pub fn inject_tools(
    tools: &Value,
    provider: Provider,
    config: &MemoryToolAdapterConfig,
) -> (Value, Value) {
    if !config.inject_tools {
        return (tools.clone(), serde_json::json!({}));
    }

    let mut tools_arr = tools.as_array().cloned().unwrap_or_default();
    let existing_names = get_existing_tool_names(&tools_arr);
    let mut beta_headers = serde_json::json!({});

    // Anthropic native tool
    if provider == Provider::Anthropic && config.use_native_tool {
        if !existing_names.contains(NATIVE_MEMORY_TOOL_NAME) {
            tools_arr.push(anthropic_native_tool());
            beta_headers["anthropic-beta"] = serde_json::json!(ANTHROPIC_BETA_HEADER);
        }
        return (Value::Array(tools_arr), beta_headers);
    }

    let schemas = match provider {
        Provider::Anthropic => anthropic_custom_tools(),
        Provider::Openai => openai_tools(),
        Provider::Gemini => gemini_tools(),
        Provider::Generic => openai_tools(),
    };

    for schema in schemas {
        let name = get_tool_name_from_schema(&schema, provider);
        if !existing_names.contains(name.as_str()) {
            tools_arr.push(schema);
        }
    }

    (Value::Array(tools_arr), beta_headers)
}

/// Extract existing tool names from a tools array.
fn get_existing_tool_names(tools: &[Value]) -> HashSet<String> {
    let mut names = HashSet::new();
    for tool in tools {
        // Anthropic format
        if let Some(name) = tool.get("name").and_then(Value::as_str) {
            names.insert(name.to_string());
        }
        // OpenAI format
        if let Some(name) = tool
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        {
            names.insert(name.to_string());
        }
    }
    names
}

/// Get tool name from a schema definition.
fn get_tool_name_from_schema(schema: &Value, provider: Provider) -> String {
    match provider {
        Provider::Openai => schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => schema
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

// ─── Tool call detection ─────────────────────────────────────────────────

/// Check if a response contains memory tool calls.
pub fn has_memory_tool_calls(response: &Value, provider: Provider) -> bool {
    let tool_calls = extract_tool_calls(response, provider);
    for tc in tool_calls {
        let name = get_tool_name(&tc, provider);
        if MEMORY_TOOL_NAMES.contains(&name.as_str()) || name == NATIVE_MEMORY_TOOL_NAME {
            return true;
        }
    }
    false
}

/// Extract tool calls from a response based on provider format.
pub fn extract_tool_calls(response: &Value, provider: Provider) -> Vec<Value> {
    match provider {
        Provider::Anthropic => response
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),

        Provider::Openai => {
            // Chat Completions: choices[0].message.tool_calls
            if let Some(tc) = response
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("tool_calls"))
                .and_then(Value::as_array)
            {
                return tc.clone();
            }
            // Responses API: output[] with type=function_call
            if let Some(output) = response.get("output").and_then(Value::as_array) {
                return output
                    .iter()
                    .filter(|item| {
                        item.get("type").and_then(Value::as_str) == Some("function_call")
                    })
                    .cloned()
                    .collect();
            }
            vec![]
        }

        Provider::Gemini => response
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p.get("functionCall").is_some())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),

        Provider::Generic => {
            let mut calls = Vec::new();
            // Try Anthropic format
            if let Some(blocks) = response.get("content").and_then(Value::as_array) {
                calls.extend(
                    blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                        .cloned(),
                );
            }
            // Try OpenAI format
            if let Some(tc) = response
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("tool_calls"))
                .and_then(Value::as_array)
            {
                calls.extend(tc.iter().cloned());
            }
            calls
        }
    }
}

/// Get the tool name from a tool call.
pub fn get_tool_name(tool_call: &Value, provider: Provider) -> String {
    match provider {
        Provider::Anthropic => tool_call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Provider::Openai => tool_call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Provider::Gemini => tool_call
            .get("functionCall")
            .and_then(|fc| fc.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Provider::Generic => {
            let name = tool_call.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                tool_call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            } else {
                name.to_string()
            }
        }
    }
}

/// Get the tool call ID.
pub fn get_tool_id(tool_call: &Value, provider: Provider) -> String {
    match provider {
        Provider::Gemini => tool_call
            .get("functionCall")
            .and_then(|fc| fc.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => tool_call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

/// Get tool input/arguments from a tool call.
pub fn get_tool_input(tool_call: &Value, provider: Provider) -> Value {
    match provider {
        Provider::Anthropic => tool_call
            .get("input")
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new())),
        Provider::Openai => {
            let args_str = tool_call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            serde_json::from_str(args_str).unwrap_or(Value::Object(serde_json::Map::new()))
        }
        Provider::Gemini => tool_call
            .get("functionCall")
            .and_then(|fc| fc.get("args"))
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new())),
        Provider::Generic => {
            if let Some(input) = tool_call.get("input") {
                return input.clone();
            }
            let args_str = tool_call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            serde_json::from_str(args_str).unwrap_or(Value::Object(serde_json::Map::new()))
        }
    }
}

// ─── Result formatting ───────────────────────────────────────────────────

/// Format a tool result for the given provider.
pub fn format_tool_result(tool_id: &str, content: &str, provider: Provider) -> Value {
    match provider {
        Provider::Anthropic => serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_id,
            "content": content,
        }),
        Provider::Openai | Provider::Generic => serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_id,
            "content": content,
        }),
        Provider::Gemini => serde_json::json!({
            "functionResponse": {
                "name": tool_id,
                "response": {"result": content},
            },
        }),
    }
}

/// Get beta headers required for the provider.
pub fn get_beta_headers(provider: Provider, config: &MemoryToolAdapterConfig) -> Value {
    if provider == Provider::Anthropic && config.use_native_tool {
        serde_json::json!({"anthropic-beta": ANTHROPIC_BETA_HEADER})
    } else {
        serde_json::json!({})
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- detect_provider ---

    #[test]
    fn detect_anthropic_by_header() {
        let h = json!({"x-api-key": "sk-ant-..."});
        assert_eq!(detect_provider(&h, ""), Provider::Anthropic);
    }

    #[test]
    fn detect_anthropic_by_version_header() {
        let h = json!({"anthropic-version": "2023-06-01"});
        assert_eq!(detect_provider(&h, ""), Provider::Anthropic);
    }

    #[test]
    fn detect_anthropic_by_model() {
        assert_eq!(
            detect_provider(&json!({}), "claude-3-opus"),
            Provider::Anthropic
        );
    }

    #[test]
    fn detect_openai_by_bearer() {
        let h = json!({"authorization": "Bearer sk-abc123"});
        assert_eq!(detect_provider(&h, ""), Provider::Openai);
    }

    #[test]
    fn detect_openai_by_model() {
        assert_eq!(detect_provider(&json!({}), "gpt-4"), Provider::Openai);
    }

    #[test]
    fn detect_openai_by_o1_model() {
        assert_eq!(detect_provider(&json!({}), "o1-preview"), Provider::Openai);
    }

    #[test]
    fn detect_gemini_by_model() {
        assert_eq!(detect_provider(&json!({}), "gemini-pro"), Provider::Gemini);
    }

    #[test]
    fn detect_gemini_by_gemma() {
        assert_eq!(detect_provider(&json!({}), "gemma-2b"), Provider::Gemini);
    }

    #[test]
    fn detect_generic_fallback() {
        assert_eq!(
            detect_provider(&json!({}), "unknown-model"),
            Provider::Generic
        );
    }

    // --- has_memory_tool_calls ---

    #[test]
    fn has_memory_tool_calls_anthropic() {
        let r = json!({
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": "memory_search", "input": {"query": "test"}}
            ]
        });
        assert!(has_memory_tool_calls(&r, Provider::Anthropic));
    }

    #[test]
    fn has_memory_tool_calls_openai() {
        let r = json!({
            "choices": [{"message": {
                "tool_calls": [{"id": "tc_1", "type": "function", "function": {"name": "memory_save", "arguments": "{}"}}]
            }}]
        });
        assert!(has_memory_tool_calls(&r, Provider::Openai));
    }

    #[test]
    fn has_memory_tool_calls_gemini() {
        let r = json!({
            "candidates": [{"content": {"parts": [
                {"functionCall": {"name": "memory_delete", "args": {"memory_id": "123"}}}
            ]}}]
        });
        assert!(has_memory_tool_calls(&r, Provider::Gemini));
    }

    #[test]
    fn has_memory_tool_calls_native() {
        let r = json!({
            "content": [{"type": "tool_use", "id": "tu_1", "name": "memory", "input": {}}]
        });
        assert!(has_memory_tool_calls(&r, Provider::Anthropic));
    }

    #[test]
    fn has_memory_tool_calls_false() {
        let r = json!({
            "content": [{"type": "tool_use", "id": "tu_1", "name": "grep", "input": {}}]
        });
        assert!(!has_memory_tool_calls(&r, Provider::Anthropic));
    }

    // --- extract_tool_calls ---

    #[test]
    fn extract_openai_responses_api() {
        let r = json!({
            "output": [
                {"type": "function_call", "name": "memory_save", "arguments": "{}"}
            ]
        });
        let calls = extract_tool_calls(&r, Provider::Openai);
        assert_eq!(calls.len(), 1);
    }

    // --- get_tool_name ---

    #[test]
    fn get_tool_name_anthropic() {
        let tc = json!({"name": "memory_search", "input": {}});
        assert_eq!(get_tool_name(&tc, Provider::Anthropic), "memory_search");
    }

    #[test]
    fn get_tool_name_openai() {
        let tc = json!({"function": {"name": "memory_save"}});
        assert_eq!(get_tool_name(&tc, Provider::Openai), "memory_save");
    }

    #[test]
    fn get_tool_name_gemini() {
        let tc = json!({"functionCall": {"name": "memory_delete"}});
        assert_eq!(get_tool_name(&tc, Provider::Gemini), "memory_delete");
    }

    // --- get_tool_id ---

    #[test]
    fn get_tool_id_anthropic() {
        let tc = json!({"id": "tu_123", "name": "memory_search"});
        assert_eq!(get_tool_id(&tc, Provider::Anthropic), "tu_123");
    }

    #[test]
    fn get_tool_id_gemini_uses_name() {
        let tc = json!({"functionCall": {"name": "memory_delete"}});
        assert_eq!(get_tool_id(&tc, Provider::Gemini), "memory_delete");
    }

    // --- get_tool_input ---

    #[test]
    fn get_tool_input_anthropic() {
        let tc = json!({"input": {"query": "hello"}});
        assert_eq!(get_tool_input(&tc, Provider::Anthropic)["query"], "hello");
    }

    #[test]
    fn get_tool_input_openai() {
        let tc = json!({"function": {"arguments": "{\"query\":\"hello\"}"}});
        assert_eq!(get_tool_input(&tc, Provider::Openai)["query"], "hello");
    }

    #[test]
    fn get_tool_input_openai_invalid_json() {
        let tc = json!({"function": {"arguments": "not json"}});
        assert!(get_tool_input(&tc, Provider::Openai).is_object());
    }

    // --- format_tool_result ---

    #[test]
    fn format_result_anthropic() {
        let r = format_tool_result("tu_1", "content", Provider::Anthropic);
        assert_eq!(r["type"], "tool_result");
        assert_eq!(r["tool_use_id"], "tu_1");
    }

    #[test]
    fn format_result_openai() {
        let r = format_tool_result("tc_1", "content", Provider::Openai);
        assert_eq!(r["role"], "tool");
        assert_eq!(r["tool_call_id"], "tc_1");
    }

    #[test]
    fn format_result_gemini() {
        let r = format_tool_result("memory_save", "content", Provider::Gemini);
        assert_eq!(r["functionResponse"]["name"], "memory_save");
    }

    // --- inject_tools ---

    #[test]
    fn inject_tools_disabled() {
        let config = MemoryToolAdapterConfig {
            inject_tools: false,
            ..Default::default()
        };
        let (tools, _) = inject_tools(&json!([]), Provider::Anthropic, &config);
        assert_eq!(tools.as_array().unwrap().len(), 0);
    }

    #[test]
    fn inject_tools_anthropic_native() {
        let config = MemoryToolAdapterConfig {
            use_native_tool: true,
            ..Default::default()
        };
        let (tools, headers) = inject_tools(&json!([]), Provider::Anthropic, &config);
        assert_eq!(tools.as_array().unwrap().len(), 1);
        assert_eq!(tools[0]["type"], NATIVE_MEMORY_TOOL_TYPE);
        assert_eq!(headers["anthropic-beta"], ANTHROPIC_BETA_HEADER);
    }

    #[test]
    fn inject_tools_anthropic_custom() {
        let config = MemoryToolAdapterConfig {
            use_native_tool: false,
            ..Default::default()
        };
        let (tools, _) = inject_tools(&json!([]), Provider::Anthropic, &config);
        assert_eq!(tools.as_array().unwrap().len(), 5);
    }

    #[test]
    fn inject_tools_openai() {
        let config = MemoryToolAdapterConfig::default();
        let (tools, _) = inject_tools(&json!([]), Provider::Openai, &config);
        assert_eq!(tools.as_array().unwrap().len(), 5);
        // OpenAI format has "type": "function"
        assert_eq!(tools[0]["type"], "function");
    }

    #[test]
    fn inject_tools_gemini() {
        let config = MemoryToolAdapterConfig::default();
        let (tools, _) = inject_tools(&json!([]), Provider::Gemini, &config);
        assert_eq!(tools.as_array().unwrap().len(), 5);
        // Gemini has top-level "name"
        assert!(tools[0].get("name").is_some());
    }

    #[test]
    fn inject_tools_skips_existing() {
        let config = MemoryToolAdapterConfig {
            use_native_tool: false,
            ..Default::default()
        };
        let existing = json!([{"name": "memory_save"}]);
        let (tools, _) = inject_tools(&existing, Provider::Anthropic, &config);
        // 1 existing + 4 new (memory_save skipped) = 5 total
        assert_eq!(tools.as_array().unwrap().len(), 5);
    }

    // --- get_beta_headers ---

    #[test]
    fn beta_headers_anthropic_native() {
        let config = MemoryToolAdapterConfig {
            use_native_tool: true,
            ..Default::default()
        };
        let h = get_beta_headers(Provider::Anthropic, &config);
        assert_eq!(h["anthropic-beta"], ANTHROPIC_BETA_HEADER);
    }

    #[test]
    fn beta_headers_openai_empty() {
        let config = MemoryToolAdapterConfig::default();
        let h = get_beta_headers(Provider::Openai, &config);
        assert!(h.as_object().unwrap().is_empty());
    }

    // --- tool schema constants ---

    #[test]
    fn anthropic_custom_tools_count() {
        assert_eq!(anthropic_custom_tools().len(), 5);
    }

    #[test]
    fn openai_tools_count() {
        assert_eq!(openai_tools().len(), 5);
    }

    #[test]
    fn gemini_tools_count() {
        assert_eq!(gemini_tools().len(), 5);
    }
}

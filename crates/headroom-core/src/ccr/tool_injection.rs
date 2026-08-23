//! Tool injection for CCR (Compress-Cache-Retrieve).
//!
//! Provides the retrieval tool definition that gets injected into LLM
//! requests when compression occurs. The tool allows the LLM to retrieve
//! original uncompressed content if needed.
//!
//! Two injection modes:
//! 1. Tool Definition Injection: Adds a function tool to the tools array
//! 2. System Message Injection: Adds instructions to the system message

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

// ─── Constants ───────────────────────────────────────────────────────────

/// Tool name constant - used for matching tool calls.
pub const CCR_TOOL_NAME: &str = "headroom_retrieve";

// ─── Marker patterns ─────────────────────────────────────────────────────

fn marker_standard() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[(\d+) \w+ compressed to (\d+)\. Retrieve more: hash=([a-f0-9]{24})\]")
            .expect("MARKER_STANDARD is valid")
    })
}

fn marker_legacy() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[(\d+) \w+ compressed\. hash=([a-f0-9]{24})\]")
            .expect("MARKER_LEGACY is valid")
    })
}

fn marker_generic() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\[.*?compressed.*?hash=([a-f0-9]{24})\]").expect("MARKER_GENERIC is valid")
    })
}

fn marker_smartcrusher() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"<<ccr:([a-f0-9]{12,24})\b").expect("MARKER_SMARTCRUSHER is valid")
    })
}

// ─── Tool definition creation ────────────────────────────────────────────

/// Create the CCR retrieval tool definition.
///
/// Returns a JSON Value in the appropriate format for the given provider.
pub fn create_ccr_tool_definition(provider: &str) -> Value {
    let description = "Retrieve original uncompressed content that was compressed to save tokens. \
                       Use this when you need more data than what's shown in compressed tool results. \
                       The hash is provided in compression markers like [N items compressed... hash=abc123].";

    let hash_param = json!({
        "type": "string",
        "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
    });

    match provider {
        "openai" => json!({
            "type": "function",
            "function": {
                "name": CCR_TOOL_NAME,
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "hash": hash_param
                    },
                    "required": ["hash"]
                }
            }
        }),
        "anthropic" => json!({
            "name": CCR_TOOL_NAME,
            "description": description,
            "input_schema": {
                "type": "object",
                "properties": {
                    "hash": hash_param
                },
                "required": ["hash"]
            }
        }),
        "google" => json!({
            "name": CCR_TOOL_NAME,
            "description": "Retrieve original uncompressed content that was compressed to save tokens. \
                Use this when you need more data than what's shown in compressed tool results.",
            "parameters": {
                "type": "object",
                "properties": {
                    "hash": {
                        "type": "string",
                        "description": "Hash key from the compression marker"
                    }
                },
                "required": ["hash"]
            }
        }),
        _ => json!({
            "type": "function",
            "function": {
                "name": CCR_TOOL_NAME,
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "hash": hash_param
                    },
                    "required": ["hash"]
                }
            }
        }),
    }
}

/// Create system message instructions for CCR retrieval.
pub fn create_system_instructions(hashes: &[String], _retrieval_endpoint: &str) -> String {
    let hash_list = if hashes.len() <= 5 {
        hashes.join(", ")
    } else {
        format!("{} ...", hashes[..5].join(", "))
    };

    format!(
        r#"
## Compressed Context Available

Some tool outputs have been compressed to reduce context size. If you need
the full uncompressed data, you can retrieve it using the `{CCR_TOOL_NAME}` tool.

**How to retrieve:**
- Call `{CCR_TOOL_NAME}(hash="<hash>")` to get the full original content back

**Available hashes:** {hash_list}

Look for markers like `[N items compressed to M. Retrieve more: hash=abc123]`
in tool results to find the hash for each compressed output.
"#,
        hash_list = hash_list
    )
}

// ─── Marker scanning ─────────────────────────────────────────────────────

/// Scan text for compression markers and extract hashes.
pub fn scan_text_for_markers(text: &str) -> Vec<String> {
    let mut hashes: Vec<String> = Vec::new();

    let mut scan = |pattern: &Regex, text: &str| {
        for cap in pattern.captures_iter(text) {
            // Last capture group is always the hash
            if let Some(hash_match) = cap.get(cap.len() - 1) {
                let hash = hash_match.as_str().to_string();
                if !hashes.contains(&hash) {
                    hashes.push(hash);
                }
            }
        }
    };

    scan(marker_standard(), text);
    scan(marker_legacy(), text);
    scan(marker_generic(), text);
    scan(marker_smartcrusher(), text);

    hashes
}

/// Scan a JSON value (message content) for compression markers.
pub fn scan_content_for_markers(content: &Value) -> Vec<String> {
    let mut all_hashes: Vec<String> = Vec::new();

    match content {
        Value::String(text) => {
            all_hashes.extend(scan_text_for_markers(text));
        }
        Value::Array(blocks) => {
            for block in blocks {
                if let Some(obj) = block.as_object() {
                    // Text blocks
                    if obj.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = obj.get("text").and_then(Value::as_str) {
                            all_hashes.extend(scan_text_for_markers(text));
                        }
                    }
                    // Tool result blocks
                    if obj.get("type").and_then(Value::as_str) == Some("tool_result") {
                        if let Some(tool_content) = obj.get("content") {
                            match tool_content {
                                Value::String(text) => {
                                    all_hashes.extend(scan_text_for_markers(text));
                                }
                                Value::Array(items) => {
                                    for item in items {
                                        if let Some(item_obj) = item.as_object() {
                                            if item_obj.get("type").and_then(Value::as_str)
                                                == Some("text")
                                            {
                                                if let Some(text) =
                                                    item_obj.get("text").and_then(Value::as_str)
                                                {
                                                    all_hashes.extend(scan_text_for_markers(text));
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }

    // Deduplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    all_hashes.retain(|h| seen.insert(h.clone()));
    all_hashes
}

/// Scan a full message (including Google/Gemini `parts` field) for compression markers.
///
/// Google/Gemini messages put content under `parts` as a sibling to `content`,
/// not nested within it. This function handles both formats.
pub fn scan_message_for_markers(message: &Value) -> Vec<String> {
    let mut all_hashes: Vec<String> = Vec::new();

    // Scan standard content field
    if let Some(content) = message.get("content") {
        all_hashes.extend(scan_content_for_markers(content));
    }

    // Scan Google/Gemini parts field
    if let Some(parts) = message.get("parts").and_then(Value::as_array) {
        for part in parts {
            if let Some(obj) = part.as_object() {
                // Text parts
                if let Some(text) = obj.get("text").and_then(Value::as_str) {
                    all_hashes.extend(scan_text_for_markers(text));
                }
                // Function response parts (tool results)
                if let Some(func_response) = obj.get("functionResponse") {
                    if let Some(response) = func_response.get("response") {
                        match response {
                            Value::String(text) => {
                                all_hashes.extend(scan_text_for_markers(text));
                            }
                            Value::Object(map) => {
                                for value in map.values() {
                                    if let Some(text) = value.as_str() {
                                        all_hashes.extend(scan_text_for_markers(text));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Deduplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    all_hashes.retain(|h| seen.insert(h.clone()));
    all_hashes
}

// ─── Tool call parsing ───────────────────────────────────────────────────

/// Parse a CCR tool call to extract the content hash.
///
/// Returns the hash key, or None if this is not a valid CCR tool call.
/// A rejected hash, trimmed to something safe to put in a log line. The value
/// is whatever the model emitted, so it is capped and stripped of control
/// characters rather than logged raw.
fn loggable_hash(hash: &str) -> String {
    const MAX: usize = 64;
    let cleaned: String = hash
        .chars()
        .take(MAX)
        .map(|c| if c.is_control() { '.' } else { c })
        .collect();
    if hash.chars().count() > MAX {
        format!("{cleaned}…")
    } else {
        cleaned
    }
}

/// Split a provider-shaped tool call into its name and input object. Shared by
/// the strict parse and the raw-hash probe so the two agree on what counts as a
/// CCR call.
fn name_and_input<'a>(tool_call: &'a Value, provider: &str) -> Option<(&'a str, Value)> {
    let pair = match provider {
        "anthropic" => {
            let name = tool_call.get("name")?.as_str()?;
            let input = tool_call.get("input").cloned().unwrap_or(json!({}));
            (name, input)
        }
        "openai" => {
            let function = tool_call.get("function")?;
            let name = function.get("name")?.as_str()?;
            let args_str = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            (name, input)
        }
        "openai_responses" => {
            // Responses API: flat `function_call` item — name and arguments
            // live directly on it, not nested under "function" like chat
            // completions tool_calls.
            let name = tool_call.get("name")?.as_str()?;
            let args_str = tool_call
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            (name, input)
        }
        "google" => {
            let function_call = tool_call.get("functionCall")?;
            let name = function_call.get("name")?.as_str()?;
            let input = function_call.get("args").cloned().unwrap_or(json!({}));
            (name, input)
        }
        _ => {
            let name = tool_call.get("name")?.as_str()?;
            let input = tool_call
                .get("input")
                .or_else(|| tool_call.get("args"))
                .cloned()
                .unwrap_or(json!({}));
            (name, input)
        }
    };

    Some(pair)
}

/// The `hash` argument of a `headroom_retrieve` call exactly as the model sent
/// it, with no validation. `None` means the call is not CCR at all.
///
/// Callers need this to tell a malformed retrieval apart from an ordinary
/// client tool call. The client never declared `headroom_retrieve`, so handing
/// one back unanswered leaves an orphaned `tool_use` that nothing resolves.
pub fn raw_ccr_hash(tool_call: &Value, provider: &str) -> Option<String> {
    let (name, input_data) = name_and_input(tool_call, provider)?;
    if name != CCR_TOOL_NAME {
        return None;
    }
    Some(
        input_data
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    )
}

/// Parse a CCR tool call to extract the content hash.
///
/// Returns the hash key, or None if this is not a valid CCR tool call.
pub fn parse_tool_call(tool_call: &Value, provider: &str) -> Option<String> {
    let (name, input_data) = name_and_input(tool_call, provider)?;

    if name != CCR_TOOL_NAME {
        return None;
    }

    let Some(hash_key) = input_data.get("hash").and_then(Value::as_str) else {
        tracing::warn!(provider, "CCR tool call is missing a string hash argument");
        return None;
    };

    // Validate hash format: 12 or 24 hex chars
    if hash_key.len() != 12 && hash_key.len() != 24 {
        tracing::warn!(
            provider,
            hash_len = hash_key.len(),
            hash = %loggable_hash(hash_key),
            "CCR tool call has an invalid hash length"
        );
        return None;
    }
    if !hash_key.chars().all(|c| c.is_ascii_hexdigit()) {
        tracing::warn!(
            provider,
            hash_len = hash_key.len(),
            hash = %loggable_hash(hash_key),
            "CCR tool call has a non-hex hash"
        );
        return None;
    }

    Some(hash_key.to_string())
}

// ─── PR-B7 Sticky-on ───────────────────────────────────────────────────

/// Determine whether the CCR tool definition should be injected into
/// the request. Returns `(should_inject, hashes)`.
///
/// PR-B7 sticky-on: once a session has done CCR at least once
/// (`session_has_done_ccr == true`), keep the tool definition present
/// even when no fresh markers are found in the current turn. This
/// prevents the tool definition from toggling on/off between turns,
/// which would bust the provider's prompt cache.
pub fn should_inject_tool(
    messages: &[serde_json::Value],
    session_has_done_ccr: bool,
) -> (bool, Vec<String>) {
    let hashes = scan_messages_for_hashes(messages);
    if !hashes.is_empty() {
        return (true, hashes);
    }
    // Sticky-on: if the session has previously done CCR, keep the tool
    // definition even without fresh markers.
    if session_has_done_ccr {
        return (true, Vec::new());
    }
    (false, Vec::new())
}

/// Scan all messages for compression marker hashes.
fn scan_messages_for_hashes(messages: &[serde_json::Value]) -> Vec<String> {
    let mut all_hashes: Vec<String> = Vec::new();
    for msg in messages {
        let hashes = scan_message_for_markers(msg);
        all_hashes.extend(hashes);
    }
    // Deduplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    all_hashes.retain(|h| seen.insert(h.clone()));
    all_hashes
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- create_ccr_tool_definition ---

    #[test]
    fn create_tool_definition_anthropic_format() {
        let def = create_ccr_tool_definition("anthropic");
        assert_eq!(def["name"], CCR_TOOL_NAME);
        assert!(def.get("description").is_some());
        assert!(def.get("input_schema").is_some());
        assert!(def.get("function").is_none()); // No function wrapper
    }

    #[test]
    fn create_tool_definition_openai_format() {
        let def = create_ccr_tool_definition("openai");
        assert_eq!(def["type"], "function");
        assert_eq!(def["function"]["name"], CCR_TOOL_NAME);
        assert!(def.get("input_schema").is_none()); // No input_schema
    }

    #[test]
    fn create_tool_definition_google_format() {
        let def = create_ccr_tool_definition("google");
        assert_eq!(def["name"], CCR_TOOL_NAME);
        assert!(def.get("parameters").is_some());
        assert!(def.get("function").is_none());
    }

    #[test]
    fn create_tool_definition_unknown_provider_defaults_to_openai() {
        let def = create_ccr_tool_definition("unknown");
        assert_eq!(def["type"], "function");
        assert_eq!(def["function"]["name"], CCR_TOOL_NAME);
    }

    // --- create_system_instructions ---

    #[test]
    fn create_system_instructions_few_hashes() {
        let hashes = vec!["abc123".to_string(), "def456".to_string()];
        let instructions = create_system_instructions(&hashes, "/v1/retrieve");
        assert!(instructions.contains("abc123, def456"));
        assert!(instructions.contains(CCR_TOOL_NAME));
    }

    #[test]
    fn create_system_instructions_many_hashes() {
        let hashes: Vec<String> = (0..10).map(|i| format!("hash{}", i)).collect();
        let instructions = create_system_instructions(&hashes, "/v1/retrieve");
        assert!(instructions.contains("hash0, hash1, hash2, hash3, hash4 ..."));
    }

    // --- scan_text_for_markers ---

    #[test]
    fn scan_text_standard_marker() {
        let text = "[100 items compressed to 10. Retrieve more: hash=abc123def456abc123def456]";
        let hashes = scan_text_for_markers(text);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "abc123def456abc123def456");
    }

    #[test]
    fn scan_text_legacy_marker() {
        let text = "[50 items compressed. hash=abc123def456abc123def456]";
        let hashes = scan_text_for_markers(text);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "abc123def456abc123def456");
    }

    #[test]
    fn scan_text_smartcrusher_marker() {
        let text = "<<ccr:abc123def456>>";
        let hashes = scan_text_for_markers(text);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "abc123def456");
    }

    #[test]
    fn scan_text_smartcrusher_marker_with_suffix() {
        let text = "<<ccr:abc123def456 10_rows_offloaded>>";
        let hashes = scan_text_for_markers(text);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "abc123def456");
    }

    #[test]
    fn scan_text_no_markers() {
        let text = "This is just plain text with no compression markers.";
        let hashes = scan_text_for_markers(text);
        assert!(hashes.is_empty());
    }

    #[test]
    fn scan_text_multiple_markers_deduped() {
        let text = "[100 items compressed to 10. Retrieve more: hash=abc123def456abc123def456] \
                    some text \
                    [50 items compressed to 5. Retrieve more: hash=abc123def456abc123def456]";
        let hashes = scan_text_for_markers(text);
        assert_eq!(hashes.len(), 1); // Deduplicated
    }

    #[test]
    fn scan_text_different_hashes() {
        let text = "[100 items compressed to 10. Retrieve more: hash=111111111111111111111111] \
                    [50 items compressed to 5. Retrieve more: hash=222222222222222222222222]";
        let hashes = scan_text_for_markers(text);
        assert_eq!(hashes.len(), 2);
    }

    // --- parse_tool_call ---

    #[test]
    fn parse_tool_call_anthropic_valid() {
        let tool_call = json!({
            "name": CCR_TOOL_NAME,
            "input": {"hash": "abc123def456abc123def456"}
        });
        let hash = parse_tool_call(&tool_call, "anthropic");
        assert_eq!(hash, Some("abc123def456abc123def456".to_string()));
    }

    #[test]
    fn parse_tool_call_openai_valid() {
        let tool_call = json!({
            "function": {
                "name": CCR_TOOL_NAME,
                "arguments": "{\"hash\":\"abc123def456abc123def456\"}"
            }
        });
        let hash = parse_tool_call(&tool_call, "openai");
        assert_eq!(hash, Some("abc123def456abc123def456".to_string()));
    }

    #[test]
    fn parse_tool_call_openai_responses_valid() {
        // Responses API: flat function_call item, arguments is a JSON string.
        let tool_call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": CCR_TOOL_NAME,
            "arguments": "{\"hash\":\"abc123def456abc123def456\"}"
        });
        let hash = parse_tool_call(&tool_call, "openai_responses");
        assert_eq!(hash, Some("abc123def456abc123def456".to_string()));
    }

    #[test]
    fn parse_tool_call_openai_responses_invalid_json() {
        let tool_call = json!({
            "type": "function_call",
            "name": CCR_TOOL_NAME,
            "arguments": "not json"
        });
        let hash = parse_tool_call(&tool_call, "openai_responses");
        assert!(hash.is_none());
    }

    #[test]
    fn parse_tool_call_google_valid() {
        let tool_call = json!({
            "functionCall": {
                "name": CCR_TOOL_NAME,
                "args": {"hash": "abc123def456abc123def456"}
            }
        });
        let hash = parse_tool_call(&tool_call, "google");
        assert_eq!(hash, Some("abc123def456abc123def456".to_string()));
    }

    #[test]
    fn parse_tool_call_wrong_name() {
        let tool_call = json!({
            "name": "other_tool",
            "input": {"hash": "abc123def456abc123def456"}
        });
        let hash = parse_tool_call(&tool_call, "anthropic");
        assert!(hash.is_none());
    }

    #[test]
    fn parse_tool_call_missing_hash() {
        let tool_call = json!({
            "name": CCR_TOOL_NAME,
            "input": {}
        });
        let hash = parse_tool_call(&tool_call, "anthropic");
        assert!(hash.is_none());
    }

    #[test]
    fn parse_tool_call_invalid_hash_length() {
        let tool_call = json!({
            "name": CCR_TOOL_NAME,
            "input": {"hash": "abc123"} // Only 6 chars
        });
        let hash = parse_tool_call(&tool_call, "anthropic");
        assert!(hash.is_none());
    }

    #[test]
    fn parse_tool_call_invalid_hash_hex() {
        let tool_call = json!({
            "name": CCR_TOOL_NAME,
            "input": {"hash": "xyz123def456xyz123def456"} // Contains non-hex
        });
        let hash = parse_tool_call(&tool_call, "anthropic");
        assert!(hash.is_none());
    }

    #[test]
    fn parse_tool_call_12_char_hash() {
        let tool_call = json!({
            "name": CCR_TOOL_NAME,
            "input": {"hash": "abc123def456"}
        });
        let hash = parse_tool_call(&tool_call, "anthropic");
        assert_eq!(hash, Some("abc123def456".to_string()));
    }

    #[test]
    fn parse_tool_call_openai_invalid_json() {
        let tool_call = json!({
            "function": {
                "name": CCR_TOOL_NAME,
                "arguments": "not json"
            }
        });
        let hash = parse_tool_call(&tool_call, "openai");
        assert!(hash.is_none());
    }

    #[test]
    fn parse_tool_call_16_char_hash_rejected() {
        // 16-char hash is neither 12 nor 24, should be rejected
        let tool_call = json!({
            "name": CCR_TOOL_NAME,
            "input": {"hash": "abc123def4567890"} // 16 chars
        });
        let hash = parse_tool_call(&tool_call, "anthropic");
        assert!(hash.is_none());
    }

    // --- scan_message_for_markers ---

    #[test]
    fn scan_message_google_parts_text() {
        let message = json!({
            "parts": [
                {"text": "[100 items compressed to 10. Retrieve more: hash=abc123def456abc123def456]"}
            ]
        });
        let hashes = scan_message_for_markers(&message);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "abc123def456abc123def456");
    }

    #[test]
    fn scan_message_google_parts_function_response() {
        let message = json!({
            "parts": [
                {
                    "functionResponse": {
                        "response": "[50 items compressed to 5. Retrieve more: hash=111111111111111111111111]"
                    }
                }
            ]
        });
        let hashes = scan_message_for_markers(&message);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "111111111111111111111111");
    }

    #[test]
    fn scan_message_google_parts_function_response_dict() {
        let message = json!({
            "parts": [
                {
                    "functionResponse": {
                        "response": {
                            "output": "[50 items compressed to 5. Retrieve more: hash=222222222222222222222222]"
                        }
                    }
                }
            ]
        });
        let hashes = scan_message_for_markers(&message);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "222222222222222222222222");
    }

    #[test]
    fn scan_message_mixed_content_and_parts() {
        let message = json!({
            "content": "[100 items compressed to 10. Retrieve more: hash=abc123def456abc123def456]",
            "parts": [
                {"text": "[50 items compressed to 5. Retrieve more: hash=111111111111111111111111]"}
            ]
        });
        let hashes = scan_message_for_markers(&message);
        assert_eq!(hashes.len(), 2);
    }

    // --- scan_content_for_markers ---

    #[test]
    fn scan_content_anthropic_blocks() {
        let content = json!([
            {"type": "text", "text": "[100 items compressed to 10. Retrieve more: hash=abc123def456abc123def456]"},
            {"type": "text", "text": "regular content"}
        ]);
        let hashes = scan_content_for_markers(&content);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "abc123def456abc123def456");
    }

    #[test]
    fn scan_content_string() {
        let content =
            json!("[100 items compressed to 10. Retrieve more: hash=aabbccddeeffaabbccddeeff]");
        let hashes = scan_content_for_markers(&content);
        assert_eq!(hashes.len(), 1);
    }

    #[test]
    fn scan_content_no_markers() {
        let content = json!("Just regular text with no compression markers.");
        let hashes = scan_content_for_markers(&content);
        assert!(hashes.is_empty());
    }

    #[test]
    fn scan_content_multiple_hashes_deduped() {
        // Two text blocks with the same hash
        let content = json!([
            {"type": "text", "text": "[100 items compressed to 10. Retrieve more: hash=abc123def456abc123def456]"},
            {"type": "text", "text": "[50 items compressed to 5. Retrieve more: hash=abc123def456abc123def456]"}
        ]);
        let hashes = scan_content_for_markers(&content);
        // Same hash should be deduplicated
        assert_eq!(hashes.len(), 1);
    }

    // --- parse_tool_call provider coverage ---

    #[test]
    fn parse_tool_call_wrong_name_ignored() {
        let tool_call = json!({
            "name": "some_other_tool",
            "input": {"hash": "abc123def456"}
        });
        let hash = parse_tool_call(&tool_call, "anthropic");
        assert!(hash.is_none());
    }

    #[test]
    fn create_system_instructions_empty_hashes() {
        let instructions = create_system_instructions(&[], "/v1/retrieve");
        assert!(instructions.contains(CCR_TOOL_NAME));
    }

    #[test]
    fn create_system_instructions_includes_tool_name() {
        let hashes = vec!["abc123".to_string()];
        let instructions = create_system_instructions(&hashes, "/custom/endpoint");
        assert!(instructions.contains(CCR_TOOL_NAME));
        assert!(instructions.contains("abc123"));
    }

    // ─── Integration: end-to-end flow ─────────────────────────────────

    #[test]
    fn e2e_scan_inject_tool_anthropic() {
        // Simulate a real Anthropic request with compressed content in tool result
        let messages = json!([
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "Show me the auth code"},
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "toolu_01", "name": "Grep", "input": {"pattern": "auth"}}]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_01",
                        "content": "tool result text"
                    }
                ]
            },
            {
                "role": "user",
                "content": "[500 items compressed to 25. Retrieve more: hash=abc123def456abc123def456]"
            }
        ]);

        // Step 1: Scan messages for compression markers
        let mut all_hashes: Vec<String> = Vec::new();
        for msg in messages.as_array().unwrap() {
            if let Some(content) = msg.get("content") {
                let hashes = scan_content_for_markers(content);
                all_hashes.extend(hashes);
            }
        }
        all_hashes.sort();
        all_hashes.dedup();

        assert_eq!(all_hashes.len(), 1, "expected 1 hash, got {:?}", all_hashes);
        assert_eq!(all_hashes[0], "abc123def456abc123def456");

        // Step 2: Create tool definition for Anthropic
        let tool_def = create_ccr_tool_definition("anthropic");
        assert_eq!(tool_def["name"], CCR_TOOL_NAME);
        assert!(tool_def.get("input_schema").is_some());

        // Step 3: Create system instructions with hashes
        let instructions = create_system_instructions(&all_hashes, "/v1/retrieve");
        assert!(instructions.contains("Compressed Context Available"));
        assert!(instructions.contains(&all_hashes[0]));
        assert!(instructions.contains(CCR_TOOL_NAME));
    }

    #[test]
    fn e2e_scan_inject_tool_openai() {
        // Simulate an OpenAI request with compressed content in tool message
        let messages = json!([
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Find files"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "call_01", "type": "function", "function": {"name": "Glob", "arguments": "{}"}}]
            },
            {
                "role": "tool",
                "tool_call_id": "call_01",
                "content": "[100 items compressed to 10. Retrieve more: hash=aaaa1111bbbb2222cccc3333]"
            }
        ]);

        // Step 1: Scan for markers across all messages
        let mut all_hashes: Vec<String> = Vec::new();
        for msg in messages.as_array().unwrap() {
            if let Some(content) = msg.get("content") {
                let hashes = scan_content_for_markers(content);
                all_hashes.extend(hashes);
            }
            if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                all_hashes.extend(scan_text_for_markers(text));
            }
        }
        all_hashes.sort();
        all_hashes.dedup();

        assert_eq!(all_hashes.len(), 1);

        // Step 2: Create tool definition for OpenAI
        let tool_def = create_ccr_tool_definition("openai");
        assert_eq!(tool_def["type"], "function");
        assert_eq!(tool_def["function"]["name"], CCR_TOOL_NAME);

        // Step 3: Create system instructions
        let instructions = create_system_instructions(&all_hashes, "/v1/retrieve");
        assert!(instructions.contains(CCR_TOOL_NAME));
    }

    #[test]
    fn e2e_full_cycle_with_marker_parsing() {
        // End-to-end: scan → detect → inject tool → LLM calls tool → parse response
        let tool_result_text =
            "[200 items compressed to 15. Retrieve more: hash=deadbeef1234deadbeef1234]";
        let hashes = scan_text_for_markers(tool_result_text);
        assert_eq!(hashes.len(), 1);
        let hash = &hashes[0];

        // Inject tool definition
        let _tool_def = create_ccr_tool_definition("anthropic");

        // Simulate LLM calling the retrieve tool
        let tool_call = json!({
            "name": CCR_TOOL_NAME,
            "input": {"hash": hash}
        });
        let parsed_hash = parse_tool_call(&tool_call, "anthropic");
        assert_eq!(parsed_hash.as_deref(), Some(hash.as_str()));

        // Verify the hash matches the original
        assert_eq!(hash, "deadbeef1234deadbeef1234");
    }

    // --- should_inject_tool (PR-B7 sticky-on) ---

    #[test]
    fn should_inject_tool_with_markers() {
        let messages = json!([
            {"role": "tool", "content": "[100 items compressed to 10. Retrieve more: hash=abc123def456abc123def456]"}
        ]);
        let (inject, hashes) = should_inject_tool(messages.as_array().unwrap(), false);
        assert!(inject);
        assert_eq!(hashes.len(), 1);
    }

    #[test]
    fn should_inject_tool_sticky_on_no_markers() {
        let messages = json!([
            {"role": "user", "content": "hello"}
        ]);
        // No markers, but session has done CCR before → sticky-on
        let (inject, hashes) = should_inject_tool(messages.as_array().unwrap(), true);
        assert!(inject);
        assert!(hashes.is_empty());
    }

    #[test]
    fn should_not_inject_tool_no_markers_no_session() {
        let messages = json!([
            {"role": "user", "content": "hello"}
        ]);
        // No markers, no previous CCR → don't inject
        let (inject, hashes) = should_inject_tool(messages.as_array().unwrap(), false);
        assert!(!inject);
        assert!(hashes.is_empty());
    }
}

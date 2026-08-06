//! Response handling for CCR (Compress-Cache-Retrieve).
//!
//! Detects CCR tool calls in LLM responses, retrieves original content
//! from the compression store, and continues the conversation until the
//! LLM produces a response without CCR tool calls.
//!
//! Mirrors Python's `headroom.ccr.response_handler`.

use std::collections::HashMap;

use serde_json::Value;

use super::tool_injection::{parse_tool_call, CCR_TOOL_NAME};

// ─── Residual-CCR classification ─────────────────────────────────────────
//
// Why CCR tool calls are still present after handling. Kept as three distinct
// values rather than a bool because "skipped on purpose" and "failed" need
// different operator responses.

/// No `headroom_retrieve` tool calls remain — fully handled.
pub const RESIDUAL_CCR_RESOLVED: &str = "resolved";
/// `headroom_retrieve` remains alongside a non-CCR client tool call. An
/// intentional skip (#839): the proxy cannot synthesize the client's
/// tool_result, so the turn is handed back to the client unchanged.
pub const RESIDUAL_CCR_SKIPPED_MIXED: &str = "skipped_mixed_tools";
/// `headroom_retrieve` remains with no accompanying client tool call — a
/// genuine handling/conversion failure.
pub const RESIDUAL_CCR_ERROR: &str = "error";

// ─── Types ───────────────────────────────────────────────────────────────

/// A detected CCR tool call.
#[derive(Debug, Clone)]
pub struct CcrToolCall {
    pub tool_call_id: String,
    pub hash_key: String,
}

/// Result of handling a CCR tool call.
#[derive(Debug, Clone)]
pub struct CcrToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub success: bool,
    pub items_retrieved: usize,
}

/// Configuration for CCR response handling.
#[derive(Debug, Clone)]
pub struct ResponseHandlerConfig {
    pub enabled: bool,
    pub max_retrieval_rounds: usize,
    pub strip_ccr_from_response: bool,
}

impl Default for ResponseHandlerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retrieval_rounds: 3,
            strip_ccr_from_response: true,
        }
    }
}

/// Statistics about the response handler.
#[derive(Debug, Clone, Default)]
pub struct ResponseHandlerStats {
    pub total_retrievals: usize,
}

// ─── CCRResponseHandler ─────────────────────────────────────────────────

/// Handles CCR tool calls in LLM responses.
pub struct CCRResponseHandler {
    pub config: ResponseHandlerConfig,
    retrieval_count: usize,
}

impl CCRResponseHandler {
    pub fn new(config: Option<ResponseHandlerConfig>) -> Self {
        Self {
            config: config.unwrap_or_default(),
            retrieval_count: 0,
        }
    }

    /// Check if response contains CCR tool calls.
    pub fn has_ccr_tool_calls(&self, response: &Value, provider: &str) -> bool {
        let tool_calls = self.extract_tool_calls(response, provider);
        tool_calls.iter().any(|tc| {
            tc.get("name").and_then(Value::as_str) == Some(CCR_TOOL_NAME)
                || tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some(CCR_TOOL_NAME)
                || tc
                    .get("functionCall")
                    .and_then(|fc| fc.get("name"))
                    .and_then(Value::as_str)
                    == Some(CCR_TOOL_NAME)
        })
    }

    /// Extract tool calls from response based on provider format.
    pub fn extract_tool_calls(&self, response: &Value, provider: &str) -> Vec<Value> {
        match provider {
            "anthropic" => {
                // content blocks with type=tool_use
                response
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "openai" => {
                // message.tool_calls array
                response
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("tool_calls"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            }
            "openai_responses" => {
                // OpenAI Responses API: top-level `output[]` array with flat
                // `function_call` items (no nested "function" object, no
                // `choices[].message.tool_calls` wrapper like chat completions).
                response
                    .get("output")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter(|item| {
                                item.get("type").and_then(Value::as_str) == Some("function_call")
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "google" => {
                // candidates[0].content.parts with functionCall
                response
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
                    .unwrap_or_default()
            }
            _ => vec![],
        }
    }

    /// Classify why (if at all) CCR tool calls remain in a handled response.
    ///
    /// Stateless and provider-generic, derived from the same parsing
    /// [`Self::handle_response`] uses, so it stays correct under concurrency and
    /// behaves identically for every provider/harness.
    ///
    /// The distinction that matters: a residual `headroom_retrieve` alongside a
    /// non-CCR client tool call is an INTENTIONAL skip (#839) — the proxy can't
    /// synthesize the client's tool_result, so the turn goes back to the client
    /// unchanged. Collapsing that into the error bucket would make a correct
    /// hand-back look like a handling failure.
    pub fn residual_ccr_status(&self, response: &Value, provider: &str) -> &'static str {
        let (ccr_calls, other_calls) = self.parse_ccr_tool_calls(response, provider);
        if ccr_calls.is_empty() {
            RESIDUAL_CCR_RESOLVED
        } else if !other_calls.is_empty() {
            RESIDUAL_CCR_SKIPPED_MIXED
        } else {
            RESIDUAL_CCR_ERROR
        }
    }

    /// Parse CCR tool calls, separating them from other tool calls.
    pub fn parse_ccr_tool_calls(
        &self,
        response: &Value,
        provider: &str,
    ) -> (Vec<CcrToolCall>, Vec<Value>) {
        let all_tool_calls = self.extract_tool_calls(response, provider);
        let mut ccr_calls = Vec::new();
        let mut other_calls = Vec::new();

        for tc in all_tool_calls {
            if let Some(hash_key) = parse_tool_call(&tc, provider) {
                let tool_call_id = if provider == "google" {
                    tc.get("functionCall")
                        .and_then(|fc| fc.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or(CCR_TOOL_NAME)
                        .to_string()
                } else if provider == "openai_responses" {
                    // Responses API function_call items key off `call_id`,
                    // which is what the matching `function_call_output` item
                    // must echo back (its own `id` is a separate item id).
                    tc.get("call_id")
                        .and_then(Value::as_str)
                        .or_else(|| tc.get("id").and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string()
                } else {
                    tc.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                };
                ccr_calls.push(CcrToolCall {
                    tool_call_id,
                    hash_key,
                });
            } else {
                other_calls.push(tc);
            }
        }

        (ccr_calls, other_calls)
    }

    /// Create a tool result message from CCR results.
    pub fn create_tool_result_message(&self, results: &[CcrToolResult], provider: &str) -> Value {
        match provider {
            "anthropic" => {
                let content_blocks: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": r.tool_call_id,
                            "content": r.content,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "role": "user",
                    "content": content_blocks,
                })
            }
            "openai" => {
                let tool_msgs: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "role": "tool",
                            "tool_call_id": r.tool_call_id,
                            "content": r.content,
                        })
                    })
                    .collect();
                // Return a special marker so callers can expand into multiple messages
                serde_json::json!({
                    "_openai_tool_results": tool_msgs,
                })
            }
            "openai_responses" => {
                // Responses API: `function_call_output` items, echoed back into
                // `input[]` alongside (not nested under) the preceding
                // function_call items. Sentinel key mirrors the "openai"
                // multi-message pattern above — the continuation loop extends
                // rather than appends when it sees this key.
                let items: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "type": "function_call_output",
                            "call_id": r.tool_call_id,
                            "output": r.content,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "_openai_responses_tool_results": items,
                })
            }
            "google" => {
                let parts: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        let response_data: Value =
                            serde_json::from_str(&r.content).unwrap_or(serde_json::json!({
                                "content": r.content
                            }));
                        serde_json::json!({
                            "functionResponse": {
                                "name": r.tool_call_id,
                                "response": response_data,
                            }
                        })
                    })
                    .collect();
                serde_json::json!({
                    "role": "user",
                    "parts": parts,
                })
            }
            _ => {
                let items: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "tool_call_id": r.tool_call_id,
                            "result": r.content,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "role": "tool",
                    "content": serde_json::to_string(&items).unwrap_or_default(),
                })
            }
        }
    }

    /// Extract the assistant message from an API response.
    pub fn extract_assistant_message(&self, response: &Value, provider: &str) -> Value {
        match provider {
            "anthropic" => serde_json::json!({
                "role": "assistant",
                "content": response.get("content").cloned().unwrap_or(Value::Array(vec![])),
            }),
            "openai" => {
                let message = response
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("message"));
                match message {
                    Some(msg) => serde_json::json!({
                        "role": "assistant",
                        "content": msg.get("content").cloned(),
                        "tool_calls": msg.get("tool_calls").cloned(),
                    }),
                    None => serde_json::json!({
                        "role": "assistant",
                        "content": Value::Null,
                    }),
                }
            }
            "openai_responses" => {
                // Responses API: the model's turn is the full `output[]` array
                // (function_call items, message items, reasoning items, ...),
                // echoed back verbatim as `input[]` items — not a single
                // role/content dict like chat completions. Sentinel key mirrors
                // `_openai_tool_results`; the continuation loop extends on it.
                serde_json::json!({
                    "_openai_responses_output_items":
                        response.get("output").cloned().unwrap_or(Value::Array(vec![])),
                })
            }
            "google" => {
                let parts = response
                    .get("candidates")
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("content"))
                    .and_then(|c| c.get("parts"))
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                serde_json::json!({
                    "role": "model",
                    "parts": parts,
                })
            }
            _ => serde_json::json!({
                "role": "assistant",
                "content": response.get("content").cloned().unwrap_or(Value::String(String::new())),
            }),
        }
    }

    /// Get handler statistics.
    pub fn get_stats(&self) -> ResponseHandlerStats {
        ResponseHandlerStats {
            total_retrievals: self.retrieval_count,
        }
    }

    /// Increment retrieval count (called by batch_processor or proxy layer).
    pub fn add_retrievals(&mut self, count: usize) {
        self.retrieval_count += count;
    }
}

// ─── StreamingCCRBuffer ─────────────────────────────────────────────────

/// Buffer for detecting CCR tool calls in streaming responses.
pub struct StreamingCcrBuffer {
    chunks: Vec<Vec<u8>>,
    detected_ccr: bool,
}

impl StreamingCcrBuffer {
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            detected_ccr: false,
        }
    }

    /// Add a chunk and check for CCR tool calls.
    /// Returns true if CCR tool call detected.
    pub fn add_chunk(&mut self, chunk: Vec<u8>) -> bool {
        self.chunks.push(chunk);

        if self.detected_ccr {
            return true;
        }

        // Quick check: does accumulated content contain CCR tool?
        let accumulated = self.get_accumulated();
        let tool_use_marker = b"\"type\":\"tool_use\"";
        let ccr_pattern = format!("\"{}\"", CCR_TOOL_NAME).into_bytes();

        if accumulated
            .windows(tool_use_marker.len())
            .any(|w| w == tool_use_marker)
            && accumulated
                .windows(ccr_pattern.len())
                .any(|w| w == &ccr_pattern[..])
        {
            self.detected_ccr = true;
            return true;
        }

        false
    }

    /// Get all accumulated chunks.
    pub fn get_accumulated(&self) -> Vec<u8> {
        let total_len: usize = self.chunks.iter().map(|c| c.len()).sum();
        let mut result = Vec::with_capacity(total_len);
        for chunk in &self.chunks {
            result.extend_from_slice(chunk);
        }
        result
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.detected_ccr = false;
    }

    pub fn is_detected(&self) -> bool {
        self.detected_ccr
    }
}

// ─── StreamingCCRHandler ────────────────────────────────────────────────

/// Handle CCR tool calls in streaming responses.
///
/// Two modes:
/// 1. Pass-through: No CCR detected, stream chunks directly
/// 2. Buffered: CCR detected, buffer response, handle, then re-stream
pub struct StreamingCcrHandler {
    pub response_handler: CCRResponseHandler,
    pub provider: String,
    pub buffer: StreamingCcrBuffer,
}

impl StreamingCcrHandler {
    pub fn new(response_handler: CCRResponseHandler, provider: String) -> Self {
        Self {
            response_handler,
            provider,
            buffer: StreamingCcrBuffer::new(),
        }
    }

    /// Parse SSE stream data into a response dict.
    ///
    /// Handles the `data: {...}\n\n` SSE format. Each complete event is
    /// decoded as UTF-8 after the `\n\n` boundary.
    pub fn parse_sse_stream(&self, data: &[u8]) -> Value {
        let events = self.parse_sse_events(data);

        match self.provider.as_str() {
            "anthropic" => self.reconstruct_anthropic_response(&events),
            _ => self.reconstruct_openai_response(&events),
        }
    }

    /// Split raw SSE bytes into individual event JSON objects.
    fn parse_sse_events(&self, data: &[u8]) -> Vec<Value> {
        let mut events = Vec::new();
        // SSE format: "data: {...}\n\n" — split on double newline
        let text = String::from_utf8_lossy(data);
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with("data: ") {
                let payload = &line[6..];
                if payload == "[DONE]" || payload.is_empty() {
                    continue;
                }
                if let Ok(evt) = serde_json::from_str::<Value>(payload) {
                    events.push(evt);
                }
            }
        }
        events
    }

    /// Reconstruct a full Anthropic response from stream events.
    fn reconstruct_anthropic_response(&self, events: &[Value]) -> Value {
        let mut content: Vec<Value> = Vec::new();
        let mut stop_reason: Option<Value> = None;
        let mut current_text = String::new();
        let mut current_tool: Option<Value> = None;

        for event in events {
            let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");

            match event_type {
                "content_block_start" => {
                    if let Some(block) = event.get("content_block") {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                current_text = block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                            }
                            Some("tool_use") => {
                                current_tool = Some(serde_json::json!({
                                    "type": "tool_use",
                                    "id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                                    "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                                    "input": {},
                                    "_partial_json": "",
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = event.get("delta") {
                        match delta.get("type").and_then(Value::as_str) {
                            Some("text_delta") => {
                                current_text.push_str(
                                    delta.get("text").and_then(Value::as_str).unwrap_or(""),
                                );
                            }
                            Some("input_json_delta") => {
                                if let Some(ref mut tool) = current_tool {
                                    let partial = delta
                                        .get("partial_json")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    if let Some(existing) =
                                        tool.get("_partial_json").and_then(Value::as_str)
                                    {
                                        let new_val = format!("{}{}", existing, partial);
                                        tool["_partial_json"] = serde_json::json!(new_val);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    if !current_text.is_empty() {
                        content.push(serde_json::json!({
                            "type": "text",
                            "text": current_text.clone(),
                        }));
                        current_text.clear();
                    }
                    if let Some(ref mut tool) = current_tool.take() {
                        let partial = tool
                            .get("_partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        tool.as_object_mut().unwrap().remove("_partial_json");
                        if !partial.is_empty() {
                            if let Ok(input) = serde_json::from_str::<Value>(&partial) {
                                tool["input"] = input;
                            }
                        }
                        content.push(tool.clone());
                    }
                }
                "message_delta" => {
                    if let Some(delta) = event.get("delta") {
                        if let Some(sr) = delta.get("stop_reason") {
                            stop_reason = Some(sr.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        serde_json::json!({
            "content": content,
            "stop_reason": stop_reason,
        })
    }

    /// Reconstruct a full OpenAI response from stream events.
    fn reconstruct_openai_response(&self, events: &[Value]) -> Value {
        let mut content = String::new();
        let mut tool_calls_map: std::collections::HashMap<usize, Value> =
            std::collections::HashMap::new();

        for event in events {
            let choices = event.get("choices").and_then(Value::as_array);
            let choices = match choices {
                Some(c) if !c.is_empty() => c,
                _ => continue,
            };

            let delta = choices[0].get("delta");
            if let Some(delta) = delta {
                if let Some(c) = delta.get("content").and_then(Value::as_str) {
                    content.push_str(c);
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tc_delta in tcs {
                        let idx =
                            tc_delta.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let entry = tool_calls_map.entry(idx).or_insert_with(|| {
                            serde_json::json!({
                                "id": "",
                                "type": "function",
                                "function": {"name": "", "arguments": ""}
                            })
                        });
                        if let Some(id) = tc_delta.get("id").and_then(Value::as_str) {
                            entry["id"] = serde_json::json!(id);
                        }
                        if let Some(func) = tc_delta.get("function") {
                            if let Some(name) = func.get("name").and_then(Value::as_str) {
                                entry["function"]["name"] = serde_json::json!(name);
                            }
                            if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                                let existing = entry["function"]["arguments"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                entry["function"]["arguments"] =
                                    serde_json::json!(format!("{}{}", existing, args));
                            }
                        }
                    }
                }
            }
        }

        let mut tool_calls: Vec<(usize, Value)> = tool_calls_map.into_iter().collect();
        tool_calls.sort_by_key(|(k, _)| *k);
        let tool_calls: Vec<Value> = tool_calls.into_iter().map(|(_, v)| v).collect();

        let message = if tool_calls.is_empty() {
            serde_json::json!({
                "role": "assistant",
                "content": if content.is_empty() { Value::Null } else { serde_json::json!(content) },
            })
        } else {
            serde_json::json!({
                "role": "assistant",
                "content": if content.is_empty() { Value::Null } else { serde_json::json!(content) },
                "tool_calls": tool_calls,
            })
        };

        serde_json::json!({
            "choices": [{"message": message, "finish_reason": "stop"}],
        })
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- has_ccr_tool_calls ---

    #[test]
    fn has_ccr_tool_calls_anthropic_true() {
        let response = serde_json::json!({
            "content": [
                {"type": "text", "text": "Let me retrieve that."},
                {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME, "input": {"hash": "abc123"}}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        assert!(handler.has_ccr_tool_calls(&response, "anthropic"));
    }

    #[test]
    fn has_ccr_tool_calls_anthropic_false() {
        let response = serde_json::json!({
            "content": [
                {"type": "text", "text": "Here is the answer."},
                {"type": "tool_use", "id": "tu_1", "name": "other_tool", "input": {}}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        assert!(!handler.has_ccr_tool_calls(&response, "anthropic"));
    }

    #[test]
    fn has_ccr_tool_calls_openai_true() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "tc_1",
                        "type": "function",
                        "function": {"name": CCR_TOOL_NAME, "arguments": "{\"hash\":\"abc123\"}"}
                    }]
                }
            }]
        });
        let handler = CCRResponseHandler::new(None);
        assert!(handler.has_ccr_tool_calls(&response, "openai"));
    }

    #[test]
    fn has_ccr_tool_calls_openai_false() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "tc_1",
                        "type": "function",
                        "function": {"name": "search", "arguments": "{}"}
                    }]
                }
            }]
        });
        let handler = CCRResponseHandler::new(None);
        assert!(!handler.has_ccr_tool_calls(&response, "openai"));
    }

    #[test]
    fn has_ccr_tool_calls_google_true() {
        let response = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "Let me check."},
                        {"functionCall": {"name": CCR_TOOL_NAME, "args": {"hash": "abc123"}}}
                    ]
                }
            }]
        });
        let handler = CCRResponseHandler::new(None);
        assert!(handler.has_ccr_tool_calls(&response, "google"));
    }

    #[test]
    fn has_ccr_tool_calls_google_false() {
        let response = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "Here is the answer."},
                        {"functionCall": {"name": "search", "args": {}}}
                    ]
                }
            }]
        });
        let handler = CCRResponseHandler::new(None);
        assert!(!handler.has_ccr_tool_calls(&response, "google"));
    }

    // --- extract_tool_calls ---

    #[test]
    fn extract_tool_calls_anthropic() {
        let response = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME, "input": {}},
                {"type": "tool_use", "id": "tu_2", "name": "other", "input": {}}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        let calls = handler.extract_tool_calls(&response, "anthropic");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn extract_tool_calls_openai() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [
                        {"id": "tc_1", "function": {"name": CCR_TOOL_NAME, "arguments": "{}"}},
                        {"id": "tc_2", "function": {"name": "other", "arguments": "{}"}}
                    ]
                }
            }]
        });
        let handler = CCRResponseHandler::new(None);
        let calls = handler.extract_tool_calls(&response, "openai");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn extract_tool_calls_google() {
        let response = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "hello"},
                        {"functionCall": {"name": CCR_TOOL_NAME, "args": {}}},
                        {"functionCall": {"name": "other", "args": {}}}
                    ]
                }
            }]
        });
        let handler = CCRResponseHandler::new(None);
        let calls = handler.extract_tool_calls(&response, "google");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn extract_tool_calls_openai_responses() {
        let response = serde_json::json!({
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME, "arguments": "{}"},
                {"type": "function_call", "call_id": "call_2", "name": "other", "arguments": "{}"}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        let calls = handler.extract_tool_calls(&response, "openai_responses");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn has_ccr_tool_calls_openai_responses_true() {
        let response = serde_json::json!({
            "output": [
                {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME, "arguments": "{\"hash\":\"abc123def456abc123def456\"}"}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        assert!(handler.has_ccr_tool_calls(&response, "openai_responses"));
    }

    #[test]
    fn parse_ccr_tool_calls_openai_responses() {
        let response = serde_json::json!({
            "output": [
                {"type": "function_call", "call_id": "call_ccr", "name": CCR_TOOL_NAME, "arguments": "{\"hash\":\"abc123def456abc123def456\"}"},
                {"type": "function_call", "call_id": "call_other", "name": "search", "arguments": "{}"}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        let (ccr, other) = handler.parse_ccr_tool_calls(&response, "openai_responses");
        assert_eq!(ccr.len(), 1);
        assert_eq!(ccr[0].tool_call_id, "call_ccr");
        assert_eq!(ccr[0].hash_key, "abc123def456abc123def456");
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn create_tool_result_openai_responses() {
        let results = vec![CcrToolResult {
            tool_call_id: "call_1".to_string(),
            content: "original data".to_string(),
            success: true,
            items_retrieved: 1,
        }];
        let handler = CCRResponseHandler::new(None);
        let msg = handler.create_tool_result_message(&results, "openai_responses");
        let items = msg["_openai_responses_tool_results"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["output"], "original data");
    }

    #[test]
    fn extract_assistant_openai_responses() {
        let response = serde_json::json!({
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME, "arguments": "{}"}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        let msg = handler.extract_assistant_message(&response, "openai_responses");
        let items = msg["_openai_responses_output_items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn extract_tool_calls_empty() {
        let response = serde_json::json!({"content": [{"type": "text", "text": "hello"}]});
        let handler = CCRResponseHandler::new(None);
        let calls = handler.extract_tool_calls(&response, "anthropic");
        assert!(calls.is_empty());
    }

    // --- parse_ccr_tool_calls ---

    #[test]
    fn parse_ccr_tool_calls_separates() {
        let response = serde_json::json!({
            "content": [
                {"type": "tool_use", "id": "tu_ccr", "name": CCR_TOOL_NAME, "input": {"hash": "abc123def456abc123def456"}},
                {"type": "tool_use", "id": "tu_other", "name": "search", "input": {}}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        let (ccr, other) = handler.parse_ccr_tool_calls(&response, "anthropic");
        assert_eq!(ccr.len(), 1);
        assert_eq!(ccr[0].hash_key, "abc123def456abc123def456");
        assert_eq!(ccr[0].tool_call_id, "tu_ccr");
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn parse_ccr_tool_calls_all_ccr() {
        let response = serde_json::json!({
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME, "input": {"hash": "aaaa1111bbbb2222cccc3333"}},
                {"type": "tool_use", "id": "tu_2", "name": CCR_TOOL_NAME, "input": {"hash": "dddd4444eeee5555ffff6666"}}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        let (ccr, other) = handler.parse_ccr_tool_calls(&response, "anthropic");
        assert_eq!(ccr.len(), 2);
        assert!(other.is_empty());
    }

    #[test]
    fn parse_ccr_tool_calls_none() {
        let response = serde_json::json!({
            "content": [{"type": "text", "text": "no tools here"}]
        });
        let handler = CCRResponseHandler::new(None);
        let (ccr, other) = handler.parse_ccr_tool_calls(&response, "anthropic");
        assert!(ccr.is_empty());
        assert!(other.is_empty());
    }

    // --- create_tool_result_message ---

    #[test]
    fn create_tool_result_anthropic() {
        let results = vec![CcrToolResult {
            tool_call_id: "tu_1".to_string(),
            content: r#"{"hash": "abc123"}"#.to_string(),
            success: true,
            items_retrieved: 10,
        }];
        let handler = CCRResponseHandler::new(None);
        let msg = handler.create_tool_result_message(&results, "anthropic");
        assert_eq!(msg["role"], "user");
        let content = msg["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "tu_1");
    }

    #[test]
    fn create_tool_result_openai() {
        let results = vec![CcrToolResult {
            tool_call_id: "tc_1".to_string(),
            content: "result".to_string(),
            success: true,
            items_retrieved: 0,
        }];
        let handler = CCRResponseHandler::new(None);
        let msg = handler.create_tool_result_message(&results, "openai");
        let tool_results = msg["_openai_tool_results"].as_array().unwrap();
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_results[0]["role"], "tool");
    }

    #[test]
    fn create_tool_result_google() {
        let results = vec![CcrToolResult {
            tool_call_id: CCR_TOOL_NAME.to_string(),
            content: r#"{"hash": "abc123", "data": "original"}"#.to_string(),
            success: true,
            items_retrieved: 5,
        }];
        let handler = CCRResponseHandler::new(None);
        let msg = handler.create_tool_result_message(&results, "google");
        assert_eq!(msg["role"], "user");
        let parts = msg["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert!(parts[0].get("functionResponse").is_some());
    }

    // --- extract_assistant_message ---

    #[test]
    fn extract_assistant_anthropic() {
        let response = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME, "input": {}}
            ]
        });
        let handler = CCRResponseHandler::new(None);
        let msg = handler.extract_assistant_message(&response, "anthropic");
        assert_eq!(msg["role"], "assistant");
        assert!(msg["content"].is_array());
    }

    #[test]
    fn extract_assistant_openai() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hello",
                    "tool_calls": [{"id": "tc_1"}]
                }
            }]
        });
        let handler = CCRResponseHandler::new(None);
        let msg = handler.extract_assistant_message(&response, "openai");
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "hello");
    }

    #[test]
    fn extract_assistant_google() {
        let response = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "hello"}, {"functionCall": {"name": CCR_TOOL_NAME}}]
                }
            }]
        });
        let handler = CCRResponseHandler::new(None);
        let msg = handler.extract_assistant_message(&response, "google");
        assert_eq!(msg["role"], "model");
        assert!(msg["parts"].is_array());
    }

    // --- StreamingCcrBuffer ---

    #[test]
    fn streaming_buffer_no_ccr() {
        let mut buf = StreamingCcrBuffer::new();
        assert!(!buf.add_chunk(b"some text".to_vec()));
        assert!(!buf.is_detected());
    }

    #[test]
    fn streaming_buffer_detects_ccr() {
        let mut buf = StreamingCcrBuffer::new();
        // First chunk: no CCR
        assert!(!buf.add_chunk(b"text content".to_vec()));
        // Second chunk: contains tool_use + CCR tool name
        let chunk = format!(
            r#"{{"type":"tool_use","name":"{}","input":{{}}}}"#,
            CCR_TOOL_NAME
        );
        assert!(buf.add_chunk(chunk.into_bytes()));
        assert!(buf.is_detected());
    }

    #[test]
    fn streaming_buffer_clear() {
        let mut buf = StreamingCcrBuffer::new();
        buf.add_chunk(b"hello".to_vec());
        buf.clear();
        assert!(!buf.is_detected());
        assert!(buf.get_accumulated().is_empty());
    }

    #[test]
    fn streaming_buffer_accumulated() {
        let mut buf = StreamingCcrBuffer::new();
        buf.add_chunk(b"hello ".to_vec());
        buf.add_chunk(b"world".to_vec());
        assert_eq!(buf.get_accumulated(), b"hello world");
    }

    // --- ResponseHandlerConfig ---

    #[test]
    fn default_config() {
        let config = ResponseHandlerConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_retrieval_rounds, 3);
        assert!(config.strip_ccr_from_response);
    }

    // --- Stats ---

    #[test]
    fn stats_default() {
        let handler = CCRResponseHandler::new(None);
        let stats = handler.get_stats();
        assert_eq!(stats.total_retrievals, 0);
    }

    #[test]
    fn stats_after_add_retrievals() {
        let mut handler = CCRResponseHandler::new(None);
        handler.add_retrievals(5);
        assert_eq!(handler.get_stats().total_retrievals, 5);
    }

    // --- Edge cases ---

    #[test]
    fn has_ccr_no_content_field() {
        let response = serde_json::json!({});
        let handler = CCRResponseHandler::new(None);
        assert!(!handler.has_ccr_tool_calls(&response, "anthropic"));
    }

    #[test]
    fn has_ccr_no_choices() {
        let response = serde_json::json!({});
        let handler = CCRResponseHandler::new(None);
        assert!(!handler.has_ccr_tool_calls(&response, "openai"));
    }

    #[test]
    fn has_ccr_no_candidates() {
        let response = serde_json::json!({});
        let handler = CCRResponseHandler::new(None);
        assert!(!handler.has_ccr_tool_calls(&response, "google"));
    }

    #[test]
    fn extract_assistant_no_choices() {
        let response = serde_json::json!({});
        let handler = CCRResponseHandler::new(None);
        let msg = handler.extract_assistant_message(&response, "openai");
        assert_eq!(msg["role"], "assistant");
        assert!(msg["content"].is_null());
    }

    // --- StreamingCCRHandler ---

    #[test]
    fn streaming_handler_reconstruct_anthropic() {
        let handler = CCRResponseHandler::new(None);
        let stream_handler = StreamingCcrHandler::new(handler, "anthropic".to_string());

        let sse_data = b"event: content_block_start\n\
            data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
            event: content_block_delta\n\
            data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello world\"}}\n\n\
            event: content_block_stop\n\
            data: {\"type\":\"content_block_stop\"}\n\n\
            event: message_delta\n\
            data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";

        let response = stream_handler.parse_sse_stream(sse_data);
        assert_eq!(response["content"][0]["type"], "text");
        assert_eq!(response["content"][0]["text"], "Hello world");
        assert_eq!(response["stop_reason"], "end_turn");
    }

    #[test]
    fn streaming_handler_reconstruct_anthropic_with_tool() {
        let handler = CCRResponseHandler::new(None);
        let stream_handler = StreamingCcrHandler::new(handler, "anthropic".to_string());

        let sse_data = format!(
            "event: content_block_start\n\
             data: {{\"type\":\"content_block_start\",\"content_block\":{{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"{}\"}}}}\n\n\
             event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"hash\\\":\\\"abc123\\\"}}\"}}}}\n\n\
             event: content_block_stop\n\
             data: {{\"type\":\"content_block_stop\"}}\n\n",
            CCR_TOOL_NAME
        );

        let response = stream_handler.parse_sse_stream(sse_data.as_bytes());
        let tool_block = &response["content"][0];
        assert_eq!(tool_block["type"], "tool_use");
        assert_eq!(tool_block["name"], CCR_TOOL_NAME);
        assert_eq!(tool_block["input"]["hash"], "abc123");
    }

    #[test]
    fn streaming_handler_reconstruct_openai() {
        let handler = CCRResponseHandler::new(None);
        let stream_handler = StreamingCcrHandler::new(handler, "openai".to_string());

        let sse_data =
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"}}]}\n\n\
            data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n\
            data: [DONE]\n\n";

        let response = stream_handler.parse_sse_stream(sse_data);
        let message = &response["choices"][0]["message"];
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["content"], "Hi there");
    }

    #[test]
    fn streaming_handler_reconstruct_openai_with_tool() {
        let handler = CCRResponseHandler::new(None);
        let stream_handler = StreamingCcrHandler::new(handler, "openai".to_string());

        let sse_data = b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"tc_1\",\"type\":\"function\",\"function\":{\"name\":\"search\",\"arguments\":\"\"}}]}}]}\n\n\
            data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"q\\\":\\\"test\\\"}\"}}]}}]}\n\n\
            data: [DONE]\n\n";

        let response = stream_handler.parse_sse_stream(sse_data);
        let tc = &response["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["function"]["name"], "search");
        assert_eq!(tc["function"]["arguments"], "{\"q\":\"test\"}");
    }

    #[test]
    fn streaming_handler_parse_sse_empty() {
        let handler = CCRResponseHandler::new(None);
        let stream_handler = StreamingCcrHandler::new(handler, "anthropic".to_string());
        let response = stream_handler.parse_sse_stream(b"");
        // Empty SSE should produce empty content
        assert!(response["content"].as_array().unwrap().is_empty());
    }

    // ─── Residual-CCR classification (upstream addition) ─────────────────
    //
    // Values verified against Python's `CCRResponseHandler.residual_ccr_status`
    // on these exact payloads.

    #[test]
    fn residual_ccr_status_matches_python() {
        let h = CCRResponseHandler::new(None);
        let resolved = serde_json::json!({"content": [{"type": "text", "text": "done"}]});
        assert_eq!(h.residual_ccr_status(&resolved, "anthropic"), "resolved");

        // CCR call with nothing else -> a real handling failure.
        let ccr_only = serde_json::json!({"content": [
            {"type":"tool_use","id":"t1","name":"headroom_retrieve","input":{"hash":"abc123def456"}}
        ]});
        assert_eq!(h.residual_ccr_status(&ccr_only, "anthropic"), "error");

        // CCR call ALONGSIDE a client tool call -> intentional skip, not an
        // error: the proxy can't synthesize the client's tool_result.
        let mixed = serde_json::json!({"content": [
            {"type":"tool_use","id":"t1","name":"headroom_retrieve","input":{"hash":"abc123def456"}},
            {"type":"tool_use","id":"t2","name":"Read","input":{"path":"/x"}}
        ]});
        assert_eq!(
            h.residual_ccr_status(&mixed, "anthropic"),
            "skipped_mixed_tools",
            "a mixed turn must not be reported as a failure"
        );
    }

    #[test]
    fn residual_ccr_status_constants_match_python_values() {
        assert_eq!(RESIDUAL_CCR_RESOLVED, "resolved");
        assert_eq!(RESIDUAL_CCR_SKIPPED_MIXED, "skipped_mixed_tools");
        assert_eq!(RESIDUAL_CCR_ERROR, "error");
    }
}

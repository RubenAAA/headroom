//! Batch result post-processor for CCR tool call handling.
//!
//! When batch results are retrieved, this processor detects CCR tool calls
//! in each result and extracts the data needed for continuation calls.
//!
//! Mirrors Python's `headroom.ccr.batch_processor`, but the actual async
//! HTTP continuation calls are delegated to the proxy layer.

use super::batch_store::BatchRequestContext;
use super::response_handler::{CCRResponseHandler, ResponseHandlerConfig};
use super::tool_injection::CCR_TOOL_NAME;

use serde_json::Value;

// ─── Types ───────────────────────────────────────────────────────────────

/// Configuration for batch result processing.
#[derive(Debug, Clone)]
pub struct BatchResultProcessorConfig {
    pub enabled: bool,
    pub continuation_timeout_secs: u64,
    pub max_continuation_rounds: usize,
}

impl Default for BatchResultProcessorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            continuation_timeout_secs: 120,
            max_continuation_rounds: 3,
        }
    }
}

/// A processed batch result.
#[derive(Debug, Clone)]
pub struct ProcessedBatchResult {
    pub custom_id: String,
    pub result: Value,
    pub was_processed: bool,
    pub continuation_rounds: usize,
    pub error: Option<String>,
}

/// Information needed to make a continuation API call.
#[derive(Debug, Clone)]
pub struct ContinuationRequest {
    pub custom_id: String,
    pub messages: Vec<Value>,
    pub tools: Option<Vec<Value>>,
    pub model: String,
    pub system_instruction: Option<String>,
    pub api_key: Option<String>,
    pub provider: String,
}

// ─── BatchResultProcessor ───────────────────────────────────────────────

/// Processes batch results to handle CCR tool calls.
pub struct BatchResultProcessor {
    pub config: BatchResultProcessorConfig,
    pub response_handler: CCRResponseHandler,
}

impl BatchResultProcessor {
    pub fn new(config: Option<BatchResultProcessorConfig>) -> Self {
        let config = config.unwrap_or_default();
        let response_handler = CCRResponseHandler::new(Some(ResponseHandlerConfig {
            enabled: true,
            max_retrieval_rounds: config.max_continuation_rounds,
            ..Default::default()
        }));
        Self {
            config,
            response_handler,
        }
    }

    /// Extract the custom_id from a batch result based on provider.
    pub fn get_custom_id(result: &Value, provider: &str) -> String {
        match provider {
            "google" => result
                .get("metadata")
                .and_then(|m| m.get("key"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            _ => result
                .get("custom_id")
                .or_else(|| result.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }
    }

    /// Extract the actual response from a batch result.
    /// Returns None if the extracted value is not a JSON object (matches
    /// Python's `isinstance(response, dict)` guard).
    pub fn extract_response(result: &Value, provider: &str) -> Option<Value> {
        let raw = match provider {
            "anthropic" => result.get("result").and_then(|r| r.get("message")).cloned(),
            "openai" => result.get("response").and_then(|r| r.get("body")).cloned(),
            "google" => result.get("response").cloned(),
            _ => result.get("response").cloned(),
        };
        // Only return object responses (matches Python's isinstance dict check)
        raw.filter(|v| v.is_object())
    }

    /// Process batch results, identifying which ones contain CCR tool calls.
    ///
    /// Returns a list of `(index, ContinuationRequest)` for results that
    /// need continuation calls. The caller (proxy layer) is responsible
    /// for making the actual HTTP calls.
    pub fn analyze_results(
        &self,
        results: &[Value],
        provider: &str,
        request_contexts: &std::collections::HashMap<String, BatchRequestContext>,
    ) -> Vec<(usize, ContinuationRequest)> {
        if !self.config.enabled {
            return vec![];
        }

        let mut continuations = Vec::new();

        for (idx, result) in results.iter().enumerate() {
            let custom_id = Self::get_custom_id(result, provider);

            let request_context = match request_contexts.get(&custom_id) {
                Some(ctx) => ctx,
                None => continue,
            };

            let response = match Self::extract_response(result, provider) {
                Some(r) => r,
                None => continue,
            };

            if self
                .response_handler
                .has_ccr_tool_calls(&response, provider)
            {
                let (ccr_calls, _other_calls) = self
                    .response_handler
                    .parse_ccr_tool_calls(&response, provider);

                if !ccr_calls.is_empty() {
                    continuations.push((
                        idx,
                        ContinuationRequest {
                            custom_id: custom_id.clone(),
                            messages: request_context.messages.clone(),
                            tools: request_context.tools.clone(),
                            model: request_context.model.clone(),
                            system_instruction: request_context.system_instruction.clone(),
                            api_key: None, // Filled by caller from BatchContext
                            provider: provider.to_string(),
                        },
                    ));
                }
            }
        }

        continuations
    }

    /// Update a batch result with a final processed response.
    pub fn update_result(original: &Value, final_response: &Value, provider: &str) -> Value {
        let mut result = original.clone();

        match provider {
            "anthropic" => {
                if !result.get("result").is_some() {
                    result["result"] = serde_json::json!({});
                }
                result["result"]["message"] = final_response.clone();
                result["result"]["type"] = serde_json::json!("succeeded");
            }
            "openai" => {
                if !result.get("response").is_some() {
                    result["response"] = serde_json::json!({});
                }
                result["response"]["body"] = final_response.clone();
            }
            "google" => {
                result["response"] = final_response.clone();
            }
            _ => {
                result["response"] = final_response.clone();
            }
        }

        result
    }

    /// Convert standard messages to Google/Gemini contents format.
    pub fn messages_to_google_contents(messages: &[Value]) -> Vec<Value> {
        let mut contents = Vec::new();

        for msg in messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            let content = msg.get("content");

            // Handle Google format messages (already have parts)
            if msg.get("parts").is_some() {
                let google_role = if role == "assistant" || role == "model" {
                    "model"
                } else {
                    "user"
                };
                contents.push(serde_json::json!({
                    "role": google_role,
                    "parts": msg["parts"],
                }));
                continue;
            }

            // Skip system messages (handled separately)
            if role == "system" {
                continue;
            }

            let google_role = if role == "assistant" { "model" } else { "user" };

            match content {
                Some(Value::String(text)) => {
                    contents.push(serde_json::json!({
                        "role": google_role,
                        "parts": [{"text": text}],
                    }));
                }
                Some(Value::Array(blocks)) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| {
                            let obj = block.as_object()?;
                            match obj.get("type").and_then(Value::as_str) {
                                Some("text") => Some(serde_json::json!({
                                    "text": obj.get("text").and_then(Value::as_str).unwrap_or("")
                                })),
                                Some("tool_result") => Some(serde_json::json!({
                                    "functionResponse": {
                                        "name": obj.get("tool_use_id").and_then(Value::as_str).unwrap_or(CCR_TOOL_NAME),
                                        "response": {"content": obj.get("content").and_then(Value::as_str).unwrap_or("")},
                                    }
                                })),
                                Some("tool_use") => Some(serde_json::json!({
                                    "functionCall": {
                                        "name": obj.get("name").and_then(Value::as_str).unwrap_or(""),
                                        "args": obj.get("input").cloned().unwrap_or(serde_json::json!({})),
                                    }
                                })),
                                _ => None,
                            }
                        })
                        .collect();

                    if !parts.is_empty() {
                        contents.push(serde_json::json!({
                            "role": google_role,
                            "parts": parts,
                        }));
                    }
                }
                _ => {}
            }
        }

        contents
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_request(custom_id: &str, model: &str) -> BatchRequestContext {
        BatchRequestContext {
            custom_id: custom_id.to_string(),
            messages: vec![serde_json::json!({"role": "user", "content": "hello"})],
            tools: None,
            model: model.to_string(),
            system_instruction: None,
            extras: HashMap::new(),
        }
    }

    // --- get_custom_id ---

    #[test]
    fn get_custom_id_anthropic() {
        let result = serde_json::json!({"custom_id": "req_1"});
        assert_eq!(
            BatchResultProcessor::get_custom_id(&result, "anthropic"),
            "req_1"
        );
    }

    #[test]
    fn get_custom_id_openai() {
        let result = serde_json::json!({"custom_id": "req_2"});
        assert_eq!(
            BatchResultProcessor::get_custom_id(&result, "openai"),
            "req_2"
        );
    }

    #[test]
    fn get_custom_id_google() {
        let result = serde_json::json!({"metadata": {"key": "req_3"}});
        assert_eq!(
            BatchResultProcessor::get_custom_id(&result, "google"),
            "req_3"
        );
    }

    #[test]
    fn get_custom_id_missing() {
        let result = serde_json::json!({});
        assert_eq!(
            BatchResultProcessor::get_custom_id(&result, "anthropic"),
            ""
        );
    }

    // --- extract_response ---

    #[test]
    fn extract_response_anthropic() {
        let result = serde_json::json!({
            "result": {"message": {"content": [{"type": "text", "text": "hello"}]}}
        });
        let resp = BatchResultProcessor::extract_response(&result, "anthropic");
        assert!(resp.is_some());
        assert_eq!(resp.unwrap()["content"][0]["text"], "hello");
    }

    #[test]
    fn extract_response_openai() {
        let result = serde_json::json!({
            "response": {"body": {"choices": [{"message": {"content": "hi"}}]}}
        });
        let resp = BatchResultProcessor::extract_response(&result, "openai");
        assert!(resp.is_some());
    }

    #[test]
    fn extract_response_google() {
        let result = serde_json::json!({"response": {"candidates": []}});
        let resp = BatchResultProcessor::extract_response(&result, "google");
        assert!(resp.is_some());
    }

    #[test]
    fn extract_response_missing() {
        let result = serde_json::json!({});
        assert!(BatchResultProcessor::extract_response(&result, "anthropic").is_none());
    }

    // --- analyze_results ---

    #[test]
    fn analyze_results_disabled() {
        let config = BatchResultProcessorConfig {
            enabled: false,
            ..Default::default()
        };
        let processor = BatchResultProcessor::new(Some(config));
        let results = vec![serde_json::json!({"custom_id": "r1"})];
        let ctxs = HashMap::new();
        assert!(processor
            .analyze_results(&results, "anthropic", &ctxs)
            .is_empty());
    }

    #[test]
    fn analyze_results_no_ccr_tool_calls() {
        let processor = BatchResultProcessor::new(None);
        let results = vec![serde_json::json!({
            "custom_id": "r1",
            "result": {
                "message": {
                    "content": [{"type": "text", "text": "no tools here"}]
                }
            }
        })];
        let mut ctxs = HashMap::new();
        ctxs.insert(
            "r1".to_string(),
            make_request("r1", "claude-sonnet-4-5-20250929"),
        );

        let conts = processor.analyze_results(&results, "anthropic", &ctxs);
        assert!(conts.is_empty());
    }

    #[test]
    fn analyze_results_with_ccr_tool_call() {
        let processor = BatchResultProcessor::new(None);
        let results = vec![serde_json::json!({
            "custom_id": "r1",
            "result": {
                "message": {
                    "content": [
                        {"type": "text", "text": "Let me retrieve that."},
                        {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME, "input": {"hash": "abc123def456abc123def456"}}
                    ]
                }
            }
        })];
        let mut ctxs = HashMap::new();
        ctxs.insert(
            "r1".to_string(),
            make_request("r1", "claude-sonnet-4-5-20250929"),
        );

        let conts = processor.analyze_results(&results, "anthropic", &ctxs);
        assert_eq!(conts.len(), 1);
        assert_eq!(conts[0].0, 0); // index
        assert_eq!(conts[0].1.custom_id, "r1");
        assert_eq!(conts[0].1.model, "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn analyze_results_missing_context_skips() {
        let processor = BatchResultProcessor::new(None);
        let results = vec![serde_json::json!({
            "custom_id": "r1",
            "result": {
                "message": {
                    "content": [
                        {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME, "input": {"hash": "abc123def456abc123def456"}}
                    ]
                }
            }
        })];
        let ctxs = HashMap::new(); // No context for r1

        let conts = processor.analyze_results(&results, "anthropic", &ctxs);
        assert!(conts.is_empty());
    }

    // --- update_result ---

    #[test]
    fn update_result_anthropic() {
        let original = serde_json::json!({
            "custom_id": "r1",
            "result": {"message": {"content": [{"type": "tool_use"}]}, "type": "tool_use"}
        });
        let final_response = serde_json::json!({
            "content": [{"type": "text", "text": "final answer"}]
        });

        let updated = BatchResultProcessor::update_result(&original, &final_response, "anthropic");
        assert_eq!(
            updated["result"]["message"]["content"][0]["text"],
            "final answer"
        );
        assert_eq!(updated["result"]["type"], "succeeded");
        assert_eq!(updated["custom_id"], "r1");
    }

    #[test]
    fn update_result_openai() {
        let original = serde_json::json!({"custom_id": "r1", "response": {"body": {}}});
        let final_response = serde_json::json!({"choices": [{"message": {"content": "done"}}]});

        let updated = BatchResultProcessor::update_result(&original, &final_response, "openai");
        assert_eq!(
            updated["response"]["body"]["choices"][0]["message"]["content"],
            "done"
        );
    }

    #[test]
    fn update_result_google() {
        let original = serde_json::json!({"response": {"candidates": []}});
        let final_response =
            serde_json::json!({"candidates": [{"content": {"parts": [{"text": "done"}]}}]});

        let updated = BatchResultProcessor::update_result(&original, &final_response, "google");
        assert_eq!(
            updated["response"]["candidates"][0]["content"]["parts"][0]["text"],
            "done"
        );
    }

    // --- messages_to_google_contents ---

    #[test]
    fn google_contents_string_content() {
        let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let contents = BatchResultProcessor::messages_to_google_contents(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hello");
    }

    #[test]
    fn google_contents_assistant_role() {
        let messages = vec![serde_json::json!({"role": "assistant", "content": "hi"})];
        let contents = BatchResultProcessor::messages_to_google_contents(&messages);
        assert_eq!(contents[0]["role"], "model");
    }

    #[test]
    fn google_contents_system_skipped() {
        let messages = vec![serde_json::json!({"role": "system", "content": "instructions"})];
        let contents = BatchResultProcessor::messages_to_google_contents(&messages);
        assert!(contents.is_empty());
    }

    #[test]
    fn google_contents_tool_use_block() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "tool_use", "name": "search", "input": {"query": "test"}}
            ]
        })];
        let contents = BatchResultProcessor::messages_to_google_contents(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["parts"][0]["functionCall"]["name"], "search");
    }

    #[test]
    fn google_contents_tool_result_block() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "result data"}
            ]
        })];
        let contents = BatchResultProcessor::messages_to_google_contents(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["parts"][0]["functionResponse"]["name"], "tu_1");
    }

    #[test]
    fn google_contents_already_parts_format() {
        let messages = vec![serde_json::json!({
            "role": "model",
            "parts": [{"text": "already in google format"}]
        })];
        let contents = BatchResultProcessor::messages_to_google_contents(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "model");
    }

    // --- Config ---

    #[test]
    fn default_config() {
        let config = BatchResultProcessorConfig::default();
        assert!(config.enabled);
        assert_eq!(config.continuation_timeout_secs, 120);
        assert_eq!(config.max_continuation_rounds, 3);
    }

    // --- Multiple results ---

    #[test]
    fn analyze_results_mixed() {
        let processor = BatchResultProcessor::new(None);
        let results = vec![
            serde_json::json!({
                "custom_id": "r1",
                "result": {
                    "message": {
                        "content": [{"type": "text", "text": "no CCR"}]
                    }
                }
            }),
            serde_json::json!({
                "custom_id": "r2",
                "result": {
                    "message": {
                        "content": [
                            {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME, "input": {"hash": "abc123def456abc123def456"}}
                        ]
                    }
                }
            }),
        ];
        let mut ctxs = HashMap::new();
        ctxs.insert("r1".to_string(), make_request("r1", "model"));
        ctxs.insert("r2".to_string(), make_request("r2", "model"));

        let conts = processor.analyze_results(&results, "anthropic", &ctxs);
        assert_eq!(conts.len(), 1);
        assert_eq!(conts[0].0, 1); // index of r2
    }
}

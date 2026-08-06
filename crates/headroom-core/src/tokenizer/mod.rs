//! Token counting for Headroom transforms.
//!
//! Mirrors the public surface of the Python `headroom.tokenizers` package:
//! a `Tokenizer` trait, a tiktoken-backed counter for OpenAI / o-series models
//! (via the `tiktoken-rs` crate, which uses the same BPE data files as Python's
//! `tiktoken` and therefore returns byte-identical token IDs), and an estimation
//! fallback for everything else.
//!
//! # Why this exists
//! Counting tokens currently round-trips into Python's `tiktoken` (itself a
//! Rust extension under the hood). For Rust transforms running on the proxy
//! hot path, counting natively avoids the Python-Rust FFI cost and keeps the
//! Rust binary self-contained.
//!
//! # What this is NOT
//! - Not used by `headroom-proxy` yet (Stage 2 is library-only; no production
//!   wiring).
//! - Not a real Anthropic Claude tokenizer (Anthropic doesn't publish theirs;
//!   estimation matches what the Python implementation does).
//! - SentencePiece — Gemini's tokenizer is SP-based but Google doesn't
//!   publish the model. Falls through to estimation.
//!
//! # What is here
//! - [`TiktokenCounter`] — OpenAI / o-series, byte-equal to Python `tiktoken`.
//! - [`HfTokenizer`] — any model with a public `tokenizer.json` (Cohere
//!   `command-*`, Llama-3.x, Mistral, Qwen, BERT, T5, …). Construct it from
//!   bytes or a file path; register it via [`register_hf`] to make it the
//!   default for a given model-name prefix.
//! - [`EstimatingCounter`] — last-resort `chars / cpt` fallback for Anthropic
//!   and Gemini, calibrated to match the Python implementation.

mod estimator;
mod hf_impl;
mod mistral;
mod registry;
mod tiktoken_impl;

pub use estimator::EstimatingCounter;
pub use hf_impl::{HfTokenizer, HfTokenizerError};
pub use mistral::{mistral_hf_repo, mistral_tokenizer_version, try_register_default_mistral};
pub use registry::{
    clear_hf_registrations, detect_backend, get_tokenizer, register_hf, try_register_hf, Backend,
};
pub(crate) use registry::name_candidates;
pub use tiktoken_impl::{TiktokenCounter, TiktokenError};

/// Token overhead per message (role, formatting, etc.).
/// Matches Python `BaseTokenizer.MESSAGE_OVERHEAD`.
const MESSAGE_OVERHEAD: usize = 4;

/// Assistant reply start tokens.
/// Matches Python `BaseTokenizer.REPLY_OVERHEAD`.
const REPLY_OVERHEAD: usize = 3;

/// Above this serialized size, sample instead of full count.
/// Matches Python `BaseTokenizer.LARGE_BLOB_CHARS`.
const LARGE_BLOB_CHARS: usize = 50_000;

/// Total characters fed to count_text for an oversized blob.
/// Matches Python `BaseTokenizer.SAMPLE_CHARS`.
const SAMPLE_CHARS: usize = 20_000;

/// Size of each evenly-spaced chunk in that sample.
/// Matches Python `BaseTokenizer.SAMPLE_CHUNK`.
const SAMPLE_CHUNK: usize = 2_000;

/// Image token estimate (max after auto-resize).
/// Matches Python `_count_content_parts` image cost.
pub(crate) const IMAGE_TOKENS: usize = 1600;

/// Audio token estimate.
/// Matches Python `_count_content_parts` audio cost.
pub(crate) const AUDIO_TOKENS: usize = 200;

/// Counts tokens. Implementations must be thread-safe (`Send + Sync`).
///
/// # Conventions (preserved across all built-in implementations)
/// - `count_text("")` returns `0`.
/// - Counts are deterministic for a given input and instance.
/// - For non-empty input, counts are `>= 1`.
pub trait Tokenizer: Send + Sync + std::fmt::Debug {
    /// Number of tokens that this tokenizer assigns to `text`.
    fn count_text(&self, text: &str) -> usize;

    /// Which backend produced the count. Useful for logs and metrics.
    fn backend(&self) -> Backend;

    /// Count tokens in a list of chat messages.
    ///
    /// Uses OpenAI-style message counting as the baseline, which works well
    /// for most models. This is a default method that calls `count_text`
    /// for text content, matching the Python `BaseTokenizer.count_messages`
    /// implementation.
    fn count_messages(&self, messages: &[serde_json::Value]) -> usize
    where
        Self: Sized,
    {
        count_messages_default(self, messages)
    }
}

/// Default implementation of `count_messages`, matching Python
/// `BaseTokenizer.count_messages` exactly.
fn count_messages_default(tokenizer: &dyn Tokenizer, messages: &[serde_json::Value]) -> usize {
    let mut total: usize = 0;

    for message in messages {
        total += MESSAGE_OVERHEAD;

        // Count role
        if let Some(role) = message.get("role").and_then(|r| r.as_str()) {
            total += tokenizer.count_text(role);
        }

        // Count content
        if let Some(content) = message.get("content") {
            match content {
                serde_json::Value::String(s) => {
                    total += tokenizer.count_text(s);
                }
                serde_json::Value::Array(parts) => {
                    total += count_content_parts(tokenizer, parts);
                }
                _ => {}
            }
        }

        // Count tool_calls
        if let Some(tool_calls) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
            total += count_tool_calls(tokenizer, tool_calls);
        }

        // Count function_call (legacy)
        if let Some(function_call) = message.get("function_call") {
            total += count_function_call(tokenizer, function_call);
        }

        // Count name field
        if let Some(name) = message.get("name").and_then(|n| n.as_str()) {
            total += tokenizer.count_text(name);
            total += 1; // Name field overhead
        }
    }

    // Reply start overhead
    total += REPLY_OVERHEAD;

    total
}

/// Count tokens in multi-part content.
/// Matches Python `_count_content_parts`.
fn count_content_parts(tokenizer: &dyn Tokenizer, parts: &[serde_json::Value]) -> usize {
    let mut total = 0;

    for part in parts {
        if let Some(obj) = part.as_object() {
            let part_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

            if part_type == "text" {
                let text = obj.get("text").and_then(|t| t.as_str()).unwrap_or("");
                total += tokenizer.count_text(text);
            } else if part_type == "image_url" || part_type == "image" || part_type == "input_image"
            {
                total += IMAGE_TOKENS;
            } else if part_type == "input_audio" || part_type == "audio" {
                total += AUDIO_TOKENS;
            } else if part_type == "tool_result" {
                if let Some(content) = obj.get("content") {
                    match content {
                        serde_json::Value::String(s) => {
                            total += tokenizer.count_text(s);
                        }
                        _ => {
                            total += count_serialized(tokenizer, content);
                        }
                    }
                }
            } else if part_type == "tool_use" {
                let name = obj.get("name").and_then(|n| n.as_str()).unwrap_or("");
                total += tokenizer.count_text(name);
                let default_input = serde_json::Value::Object(serde_json::Map::new());
                let input = obj.get("input").unwrap_or(&default_input);
                total += count_serialized(tokenizer, input);
            } else if part_type.is_empty() {
                // No type field — check for Strands SDK format
                if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                    // Strands SDK: {"text": "..."}
                    total += tokenizer.count_text(text);
                } else if let Some(tool_use) = obj.get("toolUse") {
                    // Strands SDK: {"toolUse": {"name": ..., "input": ...}}
                    let name = tool_use.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    total += tokenizer.count_text(name);
                    let default_input = serde_json::Value::Object(serde_json::Map::new());
                    let input = tool_use.get("input").unwrap_or(&default_input);
                    total += count_serialized(tokenizer, input);
                } else if let Some(tool_result) = obj.get("toolResult") {
                    // Strands SDK: {"toolResult": {"content": [...]}}
                    if let Some(tr_content) = tool_result.get("content") {
                        match tr_content {
                            serde_json::Value::String(s) => {
                                total += tokenizer.count_text(s);
                            }
                            serde_json::Value::Array(arr) => {
                                total += count_content_parts(tokenizer, arr);
                            }
                            _ => {
                                total += count_serialized(tokenizer, tr_content);
                            }
                        }
                    }
                } else if let Some(reasoning) = obj.get("reasoningContent") {
                    // Strands SDK: {"reasoningContent": {"reasoningText": {"text": "..."}}}
                    if let Some(reasoning_text) = reasoning.get("reasoningText") {
                        match reasoning_text {
                            serde_json::Value::String(s) => {
                                total += tokenizer.count_text(s);
                            }
                            serde_json::Value::Object(m) => {
                                let text = m.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                total += tokenizer.count_text(text);
                            }
                            _ => {}
                        }
                    }
                } else if let Some(doc) = obj.get("document") {
                    // Strands SDK: {"document": {"source": {"bytes": ...}}}
                    let estimated_pages = doc
                        .get("source")
                        .and_then(|s| s.get("bytes"))
                        .and_then(|b| b.as_array())
                        .map(|arr| arr.len() / 3000)
                        .unwrap_or(0);
                    total += estimated_pages.max(1) * 1500;
                } else if obj.get("image").is_some() {
                    // Strands SDK image — conservative estimate
                    total += IMAGE_TOKENS;
                } else if obj.get("video").is_some() {
                    // Strands SDK video — conservative estimate
                    total += 3200;
                } else {
                    // Unknown type — estimate from JSON
                    total += count_serialized(tokenizer, part);
                }
            } else {
                // Unknown type — estimate from JSON
                total += count_serialized(tokenizer, part);
            }
        } else if let Some(s) = part.as_str() {
            total += tokenizer.count_text(s);
        }
    }

    total
}

/// Count tokens for a non-string content blob.
/// Matches Python `_count_serialized`.
fn count_serialized(tokenizer: &dyn Tokenizer, obj: &serde_json::Value) -> usize {
    let s = match serde_json::to_string(obj) {
        Ok(s) => s,
        Err(_) => return LARGE_BLOB_CHARS / 4,
    };

    if s.len() <= LARGE_BLOB_CHARS {
        return tokenizer.count_text(&s);
    }

    // Oversized blob: sample evenly-spaced chunks and scale
    let chunks = (SAMPLE_CHARS / SAMPLE_CHUNK).max(1);
    let step = s.len() as f64 / chunks as f64;
    // Snap byte offsets down to char boundaries — `s.get` on a mid-codepoint
    // offset returns None and would silently drop the chunk, skewing the
    // estimate for non-ASCII payloads.
    let floor_boundary = |mut i: usize| {
        i = i.min(s.len());
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let sample: String = (0..chunks)
        .filter_map(|i| {
            let start = floor_boundary((i as f64 * step) as usize);
            let end = floor_boundary(((i as f64 * step) as usize + SAMPLE_CHUNK).min(s.len()));
            s.get(start..end)
        })
        .collect();

    if sample.is_empty() {
        return tokenizer.count_text(&s);
    }

    let sample_tokens = tokenizer.count_text(&sample);
    (sample_tokens as f64 * s.len() as f64 / sample.len() as f64) as usize
}

/// Return `value` as text safe to count. Matches Python
/// `coerce_countable_text`.
///
/// Tool-call fields (`function.name`, `function.arguments`, `id`) are strings
/// per the OpenAI spec, but OpenAI-compatible upstreams do emit `null` or a
/// raw object there. Treating those as `""` prices a whole tool call at the
/// bare overhead, so a request carrying an object `arguments` looks far
/// cheaper than it is — and the malformed message stays in conversation
/// history, so every later request undercounts too.
///
/// Objects and arrays are JSON-serialized (priced roughly like the JSON
/// string they should have been); scalars render as themselves; `null` and
/// missing count as nothing.
fn coerce_countable_text(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
            serde_json::to_string(v).unwrap_or_default()
        }
        Some(v) => v.to_string(),
    }
}

/// Count tokens in tool calls.
/// Matches Python `_count_tool_calls`.
fn count_tool_calls(tokenizer: &dyn Tokenizer, tool_calls: &[serde_json::Value]) -> usize {
    let mut total = 0;

    for call in tool_calls {
        total += 4; // Tool call overhead

        if let Some(func) = call.get("function") {
            total += tokenizer.count_text(&coerce_countable_text(func.get("name")));
            total += tokenizer.count_text(&coerce_countable_text(func.get("arguments")));
        }

        total += tokenizer.count_text(&coerce_countable_text(call.get("id")));
    }

    total
}

/// Count tokens in legacy function call.
/// Matches Python `_count_function_call`.
fn count_function_call(tokenizer: &dyn Tokenizer, function_call: &serde_json::Value) -> usize {
    let mut total = 4; // Function call overhead
    total += tokenizer.count_text(&coerce_countable_text(function_call.get("name")));
    total += tokenizer.count_text(&coerce_countable_text(function_call.get("arguments")));
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::estimator::EstimatingCounter;

    fn estimator() -> EstimatingCounter {
        EstimatingCounter::new(4.0)
    }

    #[test]
    fn count_messages_empty() {
        let tok = estimator();
        assert_eq!(tok.count_messages(&[]), REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_single_text() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let count = tok.count_messages(&msgs);
        // MESSAGE_OVERHEAD + role("user") + content("hello") + REPLY_OVERHEAD
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_string_content() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": "Hello, how can I help you today?"
        })];
        let count = tok.count_messages(&msgs);
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_array_content_text_block() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Here is the result:"}
            ]
        })];
        let count = tok.count_messages(&msgs);
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_image_tokens() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
            ]
        })];
        let count = tok.count_messages(&msgs);
        // Should include IMAGE_TOKENS for the image part
        assert!(count >= MESSAGE_OVERHEAD + IMAGE_TOKENS + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_audio_tokens() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "input_audio", "input_audio": {"data": "..."}}
            ]
        })];
        let count = tok.count_messages(&msgs);
        assert!(count >= MESSAGE_OVERHEAD + AUDIO_TOKENS + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_tool_calls() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_123",
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "arguments": "{\"city\": \"NYC\"}"
                }
            }]
        })];
        let count = tok.count_messages(&msgs);
        // Should include tool call overhead (4) + name + arguments + id
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD + 4);
    }

    #[test]
    fn count_messages_legacy_function_call() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": null,
            "function_call": {
                "name": "get_weather",
                "arguments": "{\"city\": \"NYC\"}"
            }
        })];
        let count = tok.count_messages(&msgs);
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD + 4);
    }

    #[test]
    fn count_messages_name_field() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "user",
            "content": "hello",
            "name": "ruben"
        })];
        let count = tok.count_messages(&msgs);
        let msgs_no_name = vec![serde_json::json!({
            "role": "user",
            "content": "hello"
        })];
        let count_no_name = tok.count_messages(&msgs_no_name);
        // Name field adds count_text("ruben") + 1
        assert!(count > count_no_name);
    }

    #[test]
    fn count_messages_multiple() {
        let tok = estimator();
        let msgs = vec![
            serde_json::json!({"role": "system", "content": "You are helpful."}),
            serde_json::json!({"role": "user", "content": "Hi"}),
            serde_json::json!({"role": "assistant", "content": "Hello!"}),
        ];
        let count = tok.count_messages(&msgs);
        // 3 messages × MESSAGE_OVERHEAD + REPLY_OVERHEAD + content tokens
        assert!(count > 3 * MESSAGE_OVERHEAD + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_tool_result_content() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "content": "The weather is sunny"
                }
            ]
        })];
        let count = tok.count_messages(&msgs);
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_striands_text() {
        let tok = estimator();
        // Strands SDK format: {"text": "..."} without "type" field
        let msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [{"text": "Hello from Strands"}]
        })];
        let count = tok.count_messages(&msgs);
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_striands_tool_use() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [{"toolUse": {"name": "search", "input": {"q": "test"}}}]
        })];
        let count = tok.count_messages(&msgs);
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD);
    }

    #[test]
    fn count_messages_striands_reasoning() {
        let tok = estimator();
        let msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [{"reasoningContent": {"reasoningText": {"text": "Let me think..."}}}]
        })];
        let count = tok.count_messages(&msgs);
        assert!(count > MESSAGE_OVERHEAD + REPLY_OVERHEAD);
    }

    /// Tool-call fields are strings per the OpenAI spec, but upstreams do send
    /// objects and nulls. Those must be priced, not silently zeroed.
    #[test]
    fn tool_call_object_arguments_are_counted() {
        let t = estimator();
        let object_args = serde_json::json!([{
            "id": "call_1",
            "function": {"name": "search", "arguments": {"query": "a rather long search string"}},
        }]);
        let string_args = serde_json::json!([{
            "id": "call_1",
            "function": {"name": "search", "arguments": "{\"query\":\"a rather long search string\"}"},
        }]);
        let object_total = count_tool_calls(&t, object_args.as_array().unwrap());
        let string_total = count_tool_calls(&t, string_args.as_array().unwrap());
        // Before the coercion the object form counted as bare overhead only.
        assert!(
            object_total > 4 + t.count_text("search"),
            "object arguments must contribute tokens, got {object_total}"
        );
        // And it should land close to the JSON string it stands in for.
        let delta = (object_total as i64 - string_total as i64).abs();
        assert!(delta <= 4, "object vs string arguments diverged by {delta}");
    }

    #[test]
    fn null_and_missing_tool_call_fields_count_as_nothing() {
        let t = estimator();
        let calls = serde_json::json!([{"id": null, "function": {"name": null}}]);
        assert_eq!(count_tool_calls(&t, calls.as_array().unwrap()), 4);
    }

}

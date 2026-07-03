# Local Qwen Model Routing Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route requests targeting a local Qwen model through the Headroom proxy by translating between Anthropic Messages API format and OpenAI Chat Completions format, enabling Claude Code to switch between Anthropic and local models via `/model`.

**Architecture:** Add an explicit `/v1/messages` handler (like the existing `/v1/chat/completions` handler) that intercepts requests whose `model` field matches a configured local model name. When matched, the handler translates the Anthropic request body to OpenAI Chat Completions format, forwards to the local upstream (e.g., `http://localhost:8080`), translates the OpenAI response back to Anthropic format, and streams it to the client. Unmatched requests fall through to the existing `forward_http()` transparent proxy path.

**Tech Stack:** Rust, Axum, serde_json, reqwest, tokio

---

## File Structure

| File | Responsibility |
|------|---------------|
| `crates/headroom-proxy/src/config.rs` | Add `local_model` and `local_upstream` CLI args + Config fields |
| `crates/headroom-proxy/src/handlers/local_model.rs` | **NEW** — Translation logic + handler for local model routing |
| `crates/headroom-proxy/src/handlers/mod.rs` | Register the new `local_model` module |
| `crates/headroom-proxy/src/proxy.rs` | Add `/v1/messages` route pointing to the local model handler |

---

## Chunk 1: Config + Translation Functions

### Task 1: Add local model config fields

**Files:**
- Modify: `crates/headroom-proxy/src/config.rs:192-530` (CliArgs)
- Modify: `crates/headroom-proxy/src/config.rs:564-648` (Config struct)
- Modify: `crates/headroom-proxy/src/config.rs:650-694` (Config::from_cli)
- Modify: `crates/headroom-proxy/src/config.rs:698-758` (Config::for_test)

- [ ] **Step 1: Add CLI args to CliArgs struct**

Add after the `vertex_adc_scope` field (~line 529):

```rust
    /// Route requests for a local model through a local upstream with
    /// Anthropic↔OpenAI format translation. When set, any `/v1/messages`
    /// request whose `model` field matches this value is translated to
    /// OpenAI Chat Completions format and forwarded to `--local-upstream`.
    /// All other requests pass through transparently.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_LOCAL_MODEL` env var →
    /// default (None = disabled).
    #[arg(long = "local-model", env = "HEADROOM_PROXY_LOCAL_MODEL")]
    pub local_model: Option<String>,

    /// Upstream URL for the local model (e.g. http://localhost:8080).
    /// Required when `--local-model` is set; the proxy appends
    /// `/v1/chat/completions` to this base. Ignored when `--local-model`
    /// is unset.
    ///
    /// Source priority: CLI flag → `HEADROOM_PROXY_LOCAL_UPSTREAM` env var →
    /// default (None).
    #[arg(long = "local-upstream", env = "HEADROOM_PROXY_LOCAL_UPSTREAM")]
    pub local_upstream: Option<Url>,
```

- [ ] **Step 2: Add fields to Config struct**

Add after the `vertex_adc_scope` field (~line 647):

```rust
    /// Local model routing: when `Some`, requests whose `model` field
    /// matches this string are translated to OpenAI format and forwarded
    /// to `local_upstream`.
    pub local_model: Option<String>,
    /// Local model upstream URL. Required when `local_model` is `Some`.
    pub local_upstream: Option<Url>,
```

- [ ] **Step 3: Wire into Config::from_cli**

Add to the `Config::from_cli()` return struct (~line 693):

```rust
            local_model: args.local_model,
            local_upstream: args.local_upstream,
```

- [ ] **Step 4: Wire into Config::for_test**

Add to the `Config::for_test()` return struct (~line 757):

```rust
            local_model: None,
            local_upstream: None,
```

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p headroom-proxy`
Expected: compiles without errors

- [ ] **Step 6: Commit**

```bash
git add crates/headroom-proxy/src/config.rs
git commit -m "feat: add local model routing config fields"
```

---

### Task 2: Anthropic-to-OpenAI request translation

**Files:**
- Create: `crates/headroom-proxy/src/handlers/local_model.rs`

- [ ] **Step 1: Create the module with request translation**

```rust
//! Local model routing handler.
//!
//! Intercepts `/v1/messages` requests whose `model` field matches the
//! configured local model name. Translates Anthropic Messages API format
//! to OpenAI Chat Completions format, forwards to the local upstream,
//! and translates the response back.

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{json, Value};
use std::net::SocketAddr;

use crate::proxy::{forward_http, AppState};

/// Handle POST /v1/messages with local model routing.
///
/// 1. Buffer the body
/// 2. Check if `model` matches the configured local model
/// 3. If yes: translate request → forward to local upstream → translate response
/// 4. If no: delegate to `forward_http()` (transparent passthrough)
pub async fn handle_messages(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Check if local model routing is enabled and the body matches.
    let should_route = match (&state.config.local_model, &state.config.local_upstream) {
        (Some(model), Some(upstream)) => {
            // Quick check: is this JSON with a matching model field?
            if let Ok(parsed) = serde_json::from_slice::<Value>(&body) {
                let body_model = parsed
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                body_model == model
            } else {
                false
            }
        }
        _ => false,
    };

    if !should_route {
        // Not a local model request — delegate to the standard forwarder.
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(hs) = builder.headers_mut() {
            *hs = headers;
        }
        let req = match builder.body(Body::from(body)) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    event = "handler_error",
                    handler = "messages_local_model",
                    error = %e,
                    "failed to reconstruct request"
                );
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("internal handler error"))
                    .expect("static response");
            }
        };
        return forward_http(state, client_addr, req)
            .await
            .unwrap_or_else(|e| e.into_response());
    }

    // Local model path: translate and forward.
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "local_model_parse_error",
                error = %e,
                "failed to parse request body for local model translation"
            );
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("invalid JSON body"))
                .expect("static response");
        }
    };

    let openai_body = match anthropic_to_openai_request(&parsed) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "local_model_translate_error",
                error = %e,
                "failed to translate Anthropic request to OpenAI format"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("translation error"))
                .expect("static response");
        }
    };

    let is_stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let upstream_url = format!(
        "{}/v1/chat/completions",
        state.config.local_upstream.as_ref().unwrap().as_str().trim_end_matches('/')
    );

    tracing::info!(
        event = "local_model_route",
        model = %parsed.get("model").and_then(|v| v.as_str()).unwrap_or(""),
        upstream = %upstream_url,
        stream = is_stream,
        "routing to local model with format translation"
    );

    // Build upstream request with OpenAI headers.
    let mut upstream_headers = HeaderMap::new();
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        "application/json".parse().expect("valid header"),
    );

    let openai_body_bytes = match serde_json::to_vec(&openai_body) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::warn!(event = "local_model_serialize_error", error = %e, "failed to serialize OpenAI request");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };

    let upstream_resp = match state
        .client
        .post(&upstream_url)
        .headers(upstream_headers)
        .body(openai_body_bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                event = "local_model_upstream_error",
                error = %e,
                upstream = %upstream_url,
                "failed to connect to local model upstream"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("local upstream error: {e}")))
                .expect("static response");
        }
    };

    let upstream_status = upstream_resp.status();

    if is_stream {
        handle_streaming_response(state, upstream_resp, &parsed).await
    } else {
        handle_buffered_response(upstream_resp, &parsed, upstream_status).await
    }
}

/// Translate an Anthropic Messages API request body to OpenAI Chat Completions.
fn anthropic_to_openai_request(anthropic: &Value) -> Result<Value, String> {
    let model = anthropic
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("missing model field")?;

    let max_tokens = anthropic.get("max_tokens").and_then(|v| v.as_u64());

    let stream = anthropic
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let temperature = anthropic.get("temperature");

    // Build OpenAI messages array.
    let mut messages: Vec<Value> = Vec::new();

    // System prompt → system message.
    if let Some(system) = anthropic.get("system") {
        match system {
            Value::String(s) => {
                messages.push(json!({"role": "system", "content": s}));
            }
            Value::Array(arr) => {
                // Anthropic allows array of content blocks for system.
                let text: String = arr
                    .iter()
                    .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(json!({"role": "system", "content": text}));
            }
            _ => {}
        }
    }

    // Convert messages.
    if let Some(msgs) = anthropic.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            match role {
                "user" => {
                    // User messages: content can be string or array of blocks.
                    translate_user_message(msg, &mut messages);
                }
                "assistant" => {
                    // Assistant messages: content can be string, array of blocks
                    // (text, tool_use), or null.
                    translate_assistant_message(msg, &mut messages);
                }
                _ => {}
            }
        }
    }

    // Build OpenAI tools array.
    let tools = anthropic
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|anthropic_tools| {
            anthropic_tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name")?.as_str()?;
                    let description = tool.get("description").and_then(|d| d.as_str());
                    let input_schema = tool.get("input_schema");
                    Some(json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description.unwrap_or(""),
                            "parameters": input_schema.unwrap_or(&json!({}))
                        }
                    }))
                })
                .collect::<Vec<_>>()
        });

    let mut openai = json!({
        "model": "qwen36-uncensored",
        "messages": messages,
        "stream": stream,
    });

    if let Some(mt) = max_tokens {
        openai["max_tokens"] = json!(mt);
    }
    if let Some(t) = temperature {
        openai["temperature"] = t.clone();
    }
    if let Some(tools) = tools {
        if !tools.is_empty() {
            openai["tools"] = json!(tools);
        }
    }

    Ok(openai)
}

/// Translate an Anthropic user message to OpenAI format.
fn translate_user_message(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            // Simple string content.
            out.push(json!({"role": "user", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            out.push(json!({"role": "user", "content": ""}));
            return;
        }
    };

    // Check if this is a tool_result-only message (single tool_result block).
    if content.len() == 1 {
        if let Some(block) = content.first() {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let result_content = block
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let is_error = block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": if is_error { format!("Error: {result_content}") } else { result_content.to_string() }
                }));
                return;
            }
        }
    }

    // Mixed content: may contain text blocks and tool_result blocks.
    // Group consecutive tool_results into one "tool" message.
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_results: Vec<&Value> = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_result") => {
                tool_results.push(block);
            }
            _ => {
                // image, etc. — skip for now (Qwen likely can't handle)
            }
        }
    }

    // Emit text content first.
    if !text_parts.is_empty() {
        out.push(json!({
            "role": "user",
            "content": text_parts.join("\n")
        }));
    }

    // Emit tool results as separate tool messages.
    for tr in tool_results {
        let tool_use_id = tr.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
        let result_content = tr.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let is_error = tr.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
        out.push(json!({
            "role": "tool",
            "tool_call_id": tool_use_id,
            "content": if is_error { format!("Error: {result_content}") } else { result_content.to_string() }
        }));
    }
}

/// Translate an Anthropic assistant message to OpenAI format.
fn translate_assistant_message(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"role": "assistant", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        Some(Value::Null) | None => {
            out.push(json!({"role": "assistant", "content": null}));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block.get("input").unwrap_or(&json!({}));
                let arguments = serde_json::to_string(input).unwrap_or_default();
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }));
            }
            _ => {}
        }
    }

    let mut assistant_msg = json!({"role": "assistant"});
    if text_parts.is_empty() && tool_calls.is_empty() {
        assistant_msg["content"] = json!(null);
    } else if text_parts.is_empty() {
        assistant_msg["content"] = json!(null);
    } else {
        assistant_msg["content"] = json!(text_parts.join("\n"));
    }
    if !tool_calls.is_empty() {
        assistant_msg["tool_calls"] = json!(tool_calls);
    }

    out.push(assistant_msg);
}

/// Handle a non-streaming (buffered) response from the local model.
async fn handle_buffered_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
) -> Response {
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if status != StatusCode::OK {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        tracing::warn!(
            event = "local_model_upstream_error",
            status = status.as_u16(),
            body = %body_text,
            "local model upstream returned error"
        );
        return Response::builder()
            .status(status)
            .body(Body::from(body_text))
            .expect("static response");
    }

    let openai_body: Value = match upstream_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(event = "local_model_response_parse_error", error = %e, "failed to parse OpenAI response");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("failed to parse upstream response"))
                .expect("static response");
        }
    };

    let anthropic_response = openai_to_anthropic_response(&openai_body, original);

    let body_bytes = match serde_json::to_vec(&anthropic_response) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(event = "local_model_serialize_error", error = %e, "failed to serialize Anthropic response");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        http::header::CONTENT_TYPE,
        "application/json".parse().expect("valid header"),
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .expect("static response")
}

/// Translate an OpenAI Chat Completions response to Anthropic Messages format.
fn openai_to_anthropic_response(openai: &Value, original: &Value) -> Value {
    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let choice = openai
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first());

    let message = choice
        .and_then(|c| c.get("message"));

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|r| r.as_str())
        .unwrap_or("stop");

    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        _ => "end_turn",
    };

    let mut content: Vec<Value> = Vec::new();

    if let Some(msg) = message {
        // Text content.
        if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
        }

        // Tool calls.
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let arguments = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let input: Value =
                    serde_json::from_str(arguments).unwrap_or(json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input
                }));
            }
        }
    }

    // Usage mapping.
    let usage = openai.get("usage").unwrap_or(&json!({}));
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let raw_id = uuid::Uuid::new_v4().to_string().replace('-', "");
    let msg_id = format!("msg_{}", &raw_id[..raw_id.len().min(24)]);

    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": original_model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    })
}

/// Handle a streaming response from the local model.
async fn handle_streaming_response(
    state: AppState,
    upstream_resp: reqwest::Response,
    original: &Value,
) -> Response {
    use futures_util::StreamExt;

    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let stream = upstream_resp.bytes_stream();
    let translated_stream = translate_openai_stream_to_anthropic(stream, original_model);

    let body = axum::body::Body::from_stream(translated_stream);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .expect("static response")
}

/// State machine for translating OpenAI SSE chunks to Anthropic SSE events.
struct StreamTranslator {
    model: String,
    content_block_index: usize,
    started: bool,
    in_text_block: bool,
    in_tool_block: bool,
    text_block_started: bool,
    tool_block_started: bool,
    current_tool_id: String,
    current_tool_name: String,
    total_output_tokens: u64,
}

impl StreamTranslator {
    fn new(model: String) -> Self {
        Self {
            model,
            content_block_index: 0,
            started: false,
            in_text_block: false,
            in_tool_block: false,
            text_block_started: false,
            tool_block_started: false,
            current_tool_id: String::new(),
            current_tool_name: String::new(),
            total_output_tokens: 0,
        }
    }

    /// Process an OpenAI SSE data line and emit Anthropic SSE events.
    fn process_line(&mut self, line: &str) -> Vec<String> {
        let mut events = Vec::new();

        // Skip empty lines and [DONE].
        if line.trim().is_empty() || line.trim() == "[DONE]" {
            if line.trim() == "[DONE]" && (self.in_text_block || self.in_tool_block) {
                // Close any open block.
                if self.in_text_block {
                    events.push(self.emit_content_block_stop());
                    self.in_text_block = false;
                }
                if self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                }
                events.push(self.emit_message_delta("end_turn"));
                events.push(self.emit_message_stop());
            }
            return events;
        }

        // Parse the JSON data.
        let chunk: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return events,
        };

        // Emit message_start on first chunk.
        if !self.started {
            events.push(self.emit_message_start());
            self.started = true;
        }

        // Extract the first choice's delta.
        let delta = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"));

        let finish_reason = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("finish_reason"))
            .and_then(|r| r.as_str());

        // Extract usage from the chunk (OpenAI sends it on the last chunk
        // when stream_options.include_usage is set).
        if let Some(usage) = chunk.get("usage") {
            if let Some(tokens) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                self.total_output_tokens = tokens;
            }
        }

        if let Some(delta) = delta {
            // Text content.
            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                if !self.in_text_block && !self.in_tool_block {
                    events.push(self.emit_content_block_start_text());
                    self.in_text_block = true;
                }
                if self.in_tool_block {
                    // Close tool block before starting text block.
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                    self.content_block_index += 1;
                    events.push(self.emit_content_block_start_text());
                    self.in_text_block = true;
                }
                events.push(self.emit_text_delta(text));
            }

            // Tool calls.
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);

                    // New tool call start.
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        // Close any open text block.
                        if self.in_text_block {
                            events.push(self.emit_content_block_stop());
                            self.in_text_block = false;
                            self.content_block_index += 1;
                        }
                        if self.in_tool_block {
                            events.push(self.emit_content_block_stop());
                            self.in_tool_block = false;
                            self.content_block_index += 1;
                        }

                        self.current_tool_id = id.to_string();
                        self.current_tool_name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();

                        events.push(self.emit_content_block_start_tool(
                            &self.current_tool_id.clone(),
                            &self.current_tool_name.clone(),
                        ));
                        self.in_tool_block = true;
                    }

                    // Tool call arguments (incremental).
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        if !args.is_empty() {
                            if !self.in_tool_block {
                                // Shouldn't happen, but handle gracefully.
                                events.push(self.emit_content_block_start_tool(
                                    &self.current_tool_id.clone(),
                                    &self.current_tool_name.clone(),
                                ));
                                self.in_tool_block = true;
                            }
                            events.push(self.emit_input_json_delta(args));
                        }
                    }
                }
            }
        }

        // Handle finish_reason.
        if let Some(reason) = finish_reason {
            if self.in_text_block {
                events.push(self.emit_content_block_stop());
                self.in_text_block = false;
            }
            if self.in_tool_block {
                events.push(self.emit_content_block_stop());
                self.in_tool_block = false;
            }

            let stop_reason = match reason {
                "stop" => "end_turn",
                "tool_calls" => "tool_use",
                "length" => "max_tokens",
                _ => "end_turn",
            };
            events.push(self.emit_message_delta(stop_reason));
            events.push(self.emit_message_stop());
        }

        events
    }

    fn emit_message_start(&self) -> String {
        let raw = uuid::Uuid::new_v4().to_string().replace('-', "");
        let msg_id = format!("msg_{}", &raw[..raw.len().min(24)]);
        let event = json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        format!("event: message_start\ndata: {}\n\n", event)
    }

    fn emit_content_block_start_text(&mut self) -> String {
        let event = json!({
            "type": "content_block_start",
            "index": self.content_block_index,
            "content_block": {"type": "text", "text": ""}
        });
        self.text_block_started = true;
        format!("event: content_block_start\ndata: {}\n\n", event)
    }

    fn emit_content_block_start_tool(&mut self, id: &str, name: &str) -> String {
        let event = json!({
            "type": "content_block_start",
            "index": self.content_block_index,
            "content_block": {"type": "tool_use", "id": id, "name": name}
        });
        self.tool_block_started = true;
        format!("event: content_block_start\ndata: {}\n\n", event)
    }

    fn emit_text_delta(&self, text: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "text_delta", "text": text}
        });
        format!("event: content_block_delta\ndata: {}\n\n", event)
    }

    fn emit_input_json_delta(&self, json_str: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "input_json_delta", "partial_json": json_str}
        });
        format!("event: content_block_delta\ndata: {}\n\n", event)
    }

    fn emit_content_block_stop(&self) -> String {
        let event = json!({
            "type": "content_block_stop",
            "index": self.content_block_index
        });
        format!("event: content_block_stop\ndata: {}\n\n", event)
    }

    fn emit_message_delta(&self, stop_reason: &str) -> String {
        let event = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": self.total_output_tokens}
        });
        format!("event: message_delta\ndata: {}\n\n", event)
    }

    fn emit_message_stop(&self) -> String {
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string()
    }
}

/// Translate an OpenAI SSE stream to Anthropic SSE events.
async fn translate_openai_stream_to_anthropic(
    stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::StreamExt;

    let mut translator = StreamTranslator::new(model);
    let mut buffer = String::new();

    stream.filter_map(move |chunk| {
        let translated = match chunk {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                buffer.push_str(&text);

                let mut output = Vec::new();
                // Process complete lines.
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim().to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    // SSE lines start with "data: ".
                    if let Some(data) = line.strip_prefix("data: ") {
                        let events = translator.process_line(data);
                        for event in events {
                            output.extend_from_slice(event.as_bytes());
                        }
                    }
                }

                if output.is_empty() {
                    None
                } else {
                    Some(Ok(bytes::Bytes::from(output)))
                }
            }
            Err(e) => Some(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))),
        };
        async { translated }
    })
}
```

- [ ] **Step 2: Add uuid dependency check**

The `uuid` crate is already a dependency (used in `proxy.rs:1746`). Verify in `Cargo.toml`.

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p headroom-proxy`
Expected: compiles without errors

- [ ] **Step 4: Write unit tests for request translation**

Add `#[cfg(test)] mod tests` at the bottom of `local_model.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_to_openai_simple_text() {
        let input = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "stream": false,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let output = anthropic_to_openai_request(&input).unwrap();
        assert_eq!(output["messages"][0]["role"], "system");
        assert_eq!(output["messages"][0]["content"], "You are helpful.");
        assert_eq!(output["messages"][1]["role"], "user");
        assert_eq!(output["messages"][1]["content"], "Hello");
        assert_eq!(output["max_tokens"], 1024);
        assert_eq!(output["stream"], false);
    }

    #[test]
    fn anthropic_to_openai_tool_use() {
        let input = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Run ls"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "file1\nfile2"}
                ]}
            ],
            "tools": [
                {"name": "bash", "description": "Run bash", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}}
            ]
        });
        let output = anthropic_to_openai_request(&input).unwrap();
        // Assistant message should have tool_calls.
        let assistant = &output["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["tool_calls"].is_array());
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "bash");
        // Tool result should be a tool message.
        let tool_msg = &output["messages"][2];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
        assert_eq!(tool_msg["content"], "file1\nfile2");
    }

    #[test]
    fn openai_to_anthropic_text_response() {
        let openai = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["type"], "message");
        assert_eq!(output["role"], "assistant");
        assert_eq!(output["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(output["stop_reason"], "end_turn");
        assert_eq!(output["content"][0]["type"], "text");
        assert_eq!(output["content"][0]["text"], "Hello!");
        assert_eq!(output["usage"]["input_tokens"], 10);
        assert_eq!(output["usage"]["output_tokens"], 5);
    }

    #[test]
    fn openai_to_anthropic_tool_calls() {
        let openai = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["stop_reason"], "tool_use");
        assert_eq!(output["content"][0]["type"], "tool_use");
        assert_eq!(output["content"][0]["id"], "call_123");
        assert_eq!(output["content"][0]["name"], "bash");
        assert_eq!(output["content"][0]["input"]["command"], "ls");
    }

    #[test]
    fn stream_translator_text_only() {
        let mut translator = StreamTranslator::new("test-model".to_string());

        let chunk1 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        let chunk2 = r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk3 = r#"{"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#;
        let chunk4 = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;

        let mut all_events = Vec::new();
        all_events.extend(translator.process_line(chunk1));
        all_events.extend(translator.process_line(chunk2));
        all_events.extend(translator.process_line(chunk3));
        all_events.extend(translator.process_line(chunk4));

        let output = all_events.join("");
        assert!(output.contains("event: message_start"));
        assert!(output.contains("event: content_block_start"));
        assert!(output.contains("\"text_delta\""));
        assert!(output.contains("Hello"));
        assert!(output.contains(" world"));
        assert!(output.contains("event: content_block_stop"));
        assert!(output.contains("event: message_delta"));
        assert!(output.contains("event: message_stop"));
    }

    #[test]
    fn stream_translator_tool_calls() {
        let mut translator = StreamTranslator::new("test-model".to_string());

        let chunk1 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        let chunk2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}"#;
        let chunk3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"co"}}]},"finish_reason":null}]}"#;
        let chunk4 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"mmand\"}"}}]},"finish_reason":null}]}"#;
        let chunk5 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut all_events = Vec::new();
        all_events.extend(translator.process_line(chunk1));
        all_events.extend(translator.process_line(chunk2));
        all_events.extend(translator.process_line(chunk3));
        all_events.extend(translator.process_line(chunk4));
        all_events.extend(translator.process_line(chunk5));

        let output = all_events.join("");
        assert!(output.contains("event: message_start"));
        assert!(output.contains("\"tool_use\""));
        assert!(output.contains("bash"));
        assert!(output.contains("\"input_json_delta\""));
        assert!(output.contains("\"stop_reason\":\"tool_use\""));
        assert!(output.contains("event: message_stop"));
    }
}
```

- [ ] **Step 5: Run unit tests**

Run: `cargo test -p headroom-proxy -- handlers::local_model`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/headroom-proxy/src/handlers/local_model.rs
git commit -m "feat: add local model translation logic with unit tests"
```

---

## Chunk 2: Route Registration + Integration

### Task 3: Register the `/v1/messages` route

**Files:**
- Modify: `crates/headroom-proxy/src/handlers/mod.rs`
- Modify: `crates/headroom-proxy/src/proxy.rs:140-293` (build_app)

- [ ] **Step 1: Add module declaration to handlers/mod.rs**

```rust
pub mod chat_completions;
pub mod conversations;
pub mod local_model;
pub mod responses;
```

- [ ] **Step 2: Add /v1/messages route in build_app() (conditional)**

In `proxy.rs`, just before the `router.fallback(any(catch_all)).with_state(state)` line (~line 293), add:

```rust
    // Local model routing: intercept /v1/messages only when a local
    // model is configured. When disabled, /v1/messages falls through
    // to the catch-all and streams normally (zero overhead).
    if state.config.local_model.is_some() {
        router = router.route(
            "/v1/messages",
            post(crate::handlers::local_model::handle_messages),
        );
    }
```

**Why conditional:** When `--local-model` is not set, we don't want to buffer every `/v1/messages` request. The catch-all path streams requests without buffering when compression is off. Registering the route unconditionally would force buffering on all `/v1/messages` requests (the `Bytes` axum extractor always buffers), which is a behavioral regression for non-local-model deployments.

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p headroom-proxy`
Expected: compiles without errors

- [ ] **Step 4: Run existing tests to verify no regression**

Run: `cargo test -p headroom-proxy`
Expected: all existing tests pass (the new handler falls through to `forward_http()` when local model routing is disabled)

- [ ] **Step 5: Commit**

```bash
git add crates/headroom-proxy/src/handlers/mod.rs crates/headroom-proxy/src/proxy.rs
git commit -m "feat: register /v1/messages route for local model routing"
```

---

### Task 4: Integration test

**Files:**
- Create: `crates/headroom-proxy/tests/local_model_routing.rs` (or add to existing integration test file)

- [ ] **Step 1: Write integration test for passthrough (local model disabled)**

When `local_model` is `None`, `/v1/messages` should fall through to `forward_http()` and behave exactly as before.

```rust
//! Integration tests for local model routing.

use headroom_proxy::config::{CliArgs, Config};
use headroom_proxy::{build_app, AppState};
use std::net::SocketAddr;

/// When local_model is not configured, /v1/messages passes through
/// to the upstream transparently.
#[tokio::test]
async fn passthrough_when_local_model_disabled() {
    // Set up a mock upstream.
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
            serde_json::json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "hello"}],
                "model": "claude-3-5-sonnet-20241022",
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
        ))
        .expect(1)
        .mount(&mock)
        .await;

    let config = Config::for_test(mock.uri().parse().unwrap());
    let state = AppState::new(config).unwrap();
    let app = build_app(state).into_make_service_with_connect_info::<SocketAddr>();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", mock.uri()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "hello");
}
```

- [ ] **Step 2: Run integration test**

Run: `cargo test -p headroom-proxy --test local_model_routing`
Expected: test passes

- [ ] **Step 3: Commit**

```bash
git add crates/headroom-proxy/tests/local_model_routing.rs
git commit -m "test: add integration test for local model routing passthrough"
```

---

## Configuration Summary

### CLI flags

```bash
headroom-proxy \
  --upstream http://anthropic-api:443 \
  --local-model qwen36-uncensored \
  --local-upstream http://localhost:8080
```

### Environment variables

```bash
export HEADROOM_PROXY_LOCAL_MODEL=qwen36-uncensored
export HEADROOM_PROXY_LOCAL_UPSTREAM=http://localhost:8080
```

### Behavior

| Scenario | Behavior |
|----------|----------|
| `model = "claude-3-5-sonnet-20241022"` | Transparent passthrough to `--upstream` (existing behavior) |
| `model = "qwen36-uncensored"` | Translate to OpenAI format, forward to `--local-upstream/v1/chat/completions`, translate response back |
| `model = "qwen36-uncensored"` with `stream: true` | Same as above, but with SSE streaming translation |
| `--local-model` not set | All requests pass through transparently (zero overhead) |

---

## Streaming Translation Spec

### OpenAI SSE → Anthropic SSE mapping

| OpenAI event | Anthropic events emitted |
|-------------|------------------------|
| First chunk with `delta.role` | `message_start` |
| First `delta.content` | `content_block_start` (text) |
| Each `delta.content` | `content_block_delta` (text_delta) |
| First `delta.tool_calls[].id` | Close text block if open, `content_block_start` (tool_use) |
| Each `delta.tool_calls[].function.arguments` | `content_block_delta` (input_json_delta) |
| `finish_reason` | Close open block, `content_block_stop`, `message_delta`, `message_stop` |

### State machine

```
IDLE → (first chunk) → TEXT_BLOCK or TOOL_BLOCK
TEXT_BLOCK → (tool_calls delta) → close text, TOOL_BLOCK
TOOL_BLOCK → (content delta) → close tool, TEXT_BLOCK
TEXT_BLOCK/TOOL_BLOCK → (finish_reason) → close block → EMIT message_delta + message_stop → DONE
```

---

## Verification Plan

1. **Unit tests** (Task 2, Step 5): Request translation, response translation, streaming state machine
2. **Integration test** (Task 4): Passthrough when disabled
3. **Manual test** (after all tasks):
   - Start Qwen: `~/start-qwen.sh`
   - Start Headroom: `cargo run -- --upstream http://localhost:8788 --local-model qwen36-uncensored --local-upstream http://localhost:8080`
   - Configure Claude Code to use Headroom as upstream
   - Use `/model` to switch to `qwen36-uncensored`
   - Send a simple message → verify text response
   - Send a message requiring tool use → verify tool call response
   - Test streaming: verify SSE events arrive in correct Anthropic format
   - Switch back to Claude model → verify transparent passthrough still works

---

## Known Limitations (MVP)

- **`stop_sequences`**: Not translated. Anthropic's `stop_sequences` field is dropped; OpenAI's `stop` field is not set. Qwen will run until max_tokens or natural stop. (Low priority — Claude Code rarely uses stop sequences.)
- **`thinking` / `extended_thinking`**: Not translated. Anthropic thinking blocks are dropped. (Qwen doesn't support extended thinking.)
- **Image / multi-modal content**: Anthropic image blocks are skipped in translation. (Qwen via llama-server may support vision, but not in this MVP.)
- **`top_p`, `top_k`**: Not translated. Only `temperature` and `max_tokens` are forwarded.
- **Non-JSON system prompts**: Only string and text-block array system prompts are handled. Complex Anthropic system prompt structures are dropped.
- **Tool `cache_control`**: Anthropic tool definitions may have `cache_control` fields; these are stripped during translation (OpenAI doesn't have this concept).
- **`metadata`**: Anthropic request metadata is dropped.
- **Token counting**: Output token counts come from OpenAI's `usage.completion_tokens`, which may differ from Anthropic's counting methodology.

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

/// Read the Codex access token from the auth JSON file.
/// Returns the token string, or None if the file doesn't exist or is invalid.
/// Re-reads on every call so Codex's token refresh cycle is picked up.
fn read_codex_access_token(path: &str) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&data).ok()?;
    parsed
        .get("tokens")?
        .get("access_token")?
        .as_str()
        .map(String::from)
}

/// Handle POST `/v1/messages` with local model routing.
///
/// 1. Buffer the body
/// 2. Check if `model` matches the configured local model
/// 3. If yes: translate request -> forward to local upstream -> translate response
/// 4. If no: delegate to `forward_http()` (transparent passthrough)
pub async fn handle_messages(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Parse body to extract model name.
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            // Not JSON — can't be a model-routed request, delegate to forward_http.
            let mut builder = Request::builder().method(method).uri(uri);
            if let Some(hs) = builder.headers_mut() {
                *hs = headers;
            }
            let req = builder.body(Body::from(body)).expect("valid request");
            return forward_http(state, client_addr, req)
                .await
                .unwrap_or_else(|e| e.into_response());
        }
    };

    let body_model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Find a matching route: first check local_model (backward compat),
    // then check model_routes table.
    // Check for mimo_run route first (highest priority)
    let mimo_run_model = state.config.model_routes.iter()
        .find(|r| r.matches(body_model))
        .and_then(|r| r.mimo_run.clone());

    if let Some(ref mimo_model) = mimo_run_model {
        return handle_mimo_run(state, &parsed, body_model, mimo_model).await;
    }

    let matched = if let (Some(model), Some(upstream)) =
        (&state.config.local_model, &state.config.local_upstream)
    {
        if body_model == model.as_str() {
            Some((upstream.clone(), true))
        } else {
            None
        }
    } else {
        None
    };

    let matched = matched.or_else(|| {
        state
            .config
            .model_routes
            .iter()
            .find(|r| r.matches(body_model))
            .filter(|r| r.mimo_run.is_none())
            .and_then(|r| Some((r.upstream.clone()?, r.translate)))
    });

    let (upstream, translate) = match matched {
        Some((u, t)) => (u.clone(), t),
        None => {
            // No route matched — delegate to standard forwarder.
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
    };

    if !translate {
        // No translation needed — forward Anthropic format directly to the upstream.
        let upstream_url = format!(
            "{}{}",
            upstream.as_str().trim_end_matches('/'),
            uri.path()
        );
        let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        let full_url = format!("{upstream_url}{query}");

        tracing::info!(
            event = "model_route_passthrough",
            model = %body_model,
            upstream = %full_url,
            "routing to upstream without translation"
        );

        let mut upstream_headers = HeaderMap::new();
        upstream_headers.insert(
            http::header::CONTENT_TYPE,
            "application/json"
                .parse()
                .expect("valid header"),
        );
        // For OpenAI upstreams, use the Codex access token if available.
        // For other upstreams, forward the original Authorization header.
        let is_openai_upstream = upstream.host_str() == Some("api.openai.com");
        if is_openai_upstream {
            if let Some(ref auth_file) = state.config.codex_auth_file {
                if let Some(token) = read_codex_access_token(auth_file) {
                    if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
                        upstream_headers.insert(http::header::AUTHORIZATION, val);
                    }
                }
            }
        } else if let Some(auth) = headers.get(http::header::AUTHORIZATION) {
            upstream_headers.insert(http::header::AUTHORIZATION, auth.clone());
        }

        let resp = match state
            .client
            .post(&full_url)
            .headers(upstream_headers)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    event = "model_route_upstream_error",
                    error = %e,
                    upstream = %full_url,
                    "failed to connect to upstream"
                );
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!("upstream error: {e}")))
                    .expect("static response");
            }
        };

        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let resp_headers = resp.headers().clone();
        let body_bytes = resp.bytes().await.unwrap_or_default();

        let mut response = Response::builder().status(status);
        for (name, value) in resp_headers.iter() {
            if !crate::headers::is_response_drop(name) {
                response = response.header(name.clone(), value.clone());
            }
        }
        return response
            .body(Body::from(body_bytes))
            .expect("static response");
    }

    // Translation path: Anthropic → OpenAI.
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
        upstream.as_str().trim_end_matches('/')
    );

    tracing::info!(
        event = "model_route_translate",
        model = %body_model,
        upstream = %upstream_url,
        stream = is_stream,
        "routing to upstream with format translation"
    );

    let mut upstream_headers = HeaderMap::new();
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        "application/json"
            .parse()
            .expect("valid header"),
    );

    let openai_body_bytes = match serde_json::to_vec(&openai_body) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize OpenAI request"
            );
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
        handle_streaming_response(upstream_resp, &parsed).await
    } else {
        handle_buffered_response(upstream_resp, &parsed, upstream_status).await
    }
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic → OpenAI
// ---------------------------------------------------------------------------

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

    let mut messages: Vec<Value> = Vec::new();

    // System prompt → system message.
    if let Some(system) = anthropic.get("system") {
        match system {
            Value::String(s) => {
                messages.push(json!({"role": "system", "content": s}));
            }
            Value::Array(arr) => {
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

    if let Some(msgs) = anthropic.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            match role {
                "user" => translate_user_message(msg, &mut messages),
                "assistant" => translate_assistant_message(msg, &mut messages),
                _ => {}
            }
        }
    }

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
                    let default_schema = json!({});
                    let params = input_schema.unwrap_or(&default_schema);
                    Some(json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description.unwrap_or(""),
                            "parameters": params
                        }
                    }))
                })
                .collect::<Vec<_>>()
        });

    let mut openai = json!({
        "model": model,
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

fn translate_user_message(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"role": "user", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            out.push(json!({"role": "user", "content": ""}));
            return;
        }
    };

    // Single tool_result block → tool message.
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

    // Mixed content: text blocks + tool_result blocks.
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
            _ => {}
        }
    }

    if !text_parts.is_empty() {
        out.push(json!({
            "role": "user",
            "content": text_parts.join("\n")
        }));
    }

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

fn translate_assistant_message(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"role": "assistant", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
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
                let default_input = json!({});
                let input = block.get("input").unwrap_or(&default_input);
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
    if text_parts.is_empty() {
        assistant_msg["content"] = json!(null);
    } else {
        assistant_msg["content"] = json!(text_parts.join("\n"));
    }
    if !tool_calls.is_empty() {
        assistant_msg["tool_calls"] = json!(tool_calls);
    }

    out.push(assistant_msg);
}

// ---------------------------------------------------------------------------
// Response translation: OpenAI → Anthropic (non-streaming)
// ---------------------------------------------------------------------------

async fn handle_buffered_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
) -> Response {
    let status = StatusCode::from_u16(upstream_status.as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);

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

    let openai_text = match upstream_resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                event = "local_model_response_parse_error",
                error = %e,
                "failed to read upstream response body"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("failed to read upstream response"))
                .expect("static response");
        }
    };
    let openai_body: Value = match serde_json::from_str(&openai_text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "local_model_response_parse_error",
                error = %e,
                "failed to parse OpenAI response JSON"
            );
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
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize Anthropic response"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .expect("static response")
}

/// Extract a single text message from Anthropic request body for mimo run.
fn extract_text_message(body: &Value) -> String {
    let messages = match body.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return String::new(),
    };

    // Build context from system prompt
    let mut parts: Vec<String> = Vec::new();
    if let Some(system) = body.get("system") {
        match system {
            Value::String(s) => parts.push(format!("[System: {s}]")),
            Value::Array(arr) => {
                let text: String = arr
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    parts.push(format!("[System: {text}]"));
                }
            }
            _ => {}
        }
    }

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        if !content.is_empty() {
            parts.push(format!("[{role}]: {content}"));
        }
    }

    parts.join("\n")
}

/// Handle a request by routing through `mimo run` subprocess.
async fn handle_mimo_run(
    state: AppState,
    original: &Value,
    model_name: &str,
    mimo_model: &str,
) -> Response {
    let is_stream = original
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let message = extract_text_message(original);
    if message.is_empty() {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("no message content to send"))
            .expect("static response");
    }

    tracing::info!(
        event = "mimo_run_route",
        model = %model_name,
        mimo_model = %mimo_model,
        stream = is_stream,
        message_len = message.len(),
        "routing through mimo run"
    );

    let _max_tokens = original
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096);

    // Build mimo run command
    let mut cmd = std::process::Command::new("mimo");
    cmd.arg("run")
        .arg("-m")
        .arg(mimo_model)
        .arg("--format")
        .arg("json")
        .arg(&message);

    // Run in a blocking thread to avoid blocking the async runtime
    let output = match tokio::task::spawn_blocking(move || cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(
                event = "mimo_run_error",
                error = %e,
                "failed to execute mimo run"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("mimo run failed: {e}")))
                .expect("static response");
        }
        Err(e) => {
            tracing::warn!(
                event = "mimo_run_error",
                error = %e,
                "mimo run task failed"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("mimo run task failed"))
                .expect("static response");
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            event = "mimo_run_error",
            status = %output.status,
            stderr = %stderr,
            "mimo run exited with error"
        );
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from(format!("mimo run failed: {stderr}")))
            .expect("static response");
    }

    // Parse JSON lines output
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut text_parts: Vec<String> = Vec::new();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<Value>(line) {
            match event.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = event.get("part").and_then(|p| p.get("text")).and_then(|t| t.as_str()) {
                        text_parts.push(text.to_string());
                    }
                }
                Some("step_finish") => {
                    if let Some(tokens) = event.get("part").and_then(|p| p.get("tokens")) {
                        input_tokens = tokens.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
                        output_tokens = tokens.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                }
                _ => {}
            }
        }
    }

    let response_text = text_parts.join("");

    if is_stream {
        // For streaming, emit a single SSE message with the full response
        let raw_id = uuid::Uuid::new_v4().to_string().replace('-', "");
        let msg_id = format!("msg_{}", &raw_id[..raw_id.len().min(24)]);

        let events = format!(
            "event: message_start\ndata: {}\n\n\
             event: content_block_start\ndata: {}\n\n\
             event: content_block_delta\ndata: {}\n\n\
             event: content_block_stop\ndata: {}\n\n\
             event: message_delta\ndata: {}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            json!({"type":"message_start","message":{"id":msg_id,"type":"message","role":"assistant","content":[],"model":model_name,"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":input_tokens,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":response_text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":output_tokens}}),
        );

        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from(events))
            .expect("static response")
    } else {
        // Non-streaming: return full Anthropic response
        let raw_id = uuid::Uuid::new_v4().to_string().replace('-', "");
        let msg_id = format!("msg_{}", &raw_id[..raw_id.len().min(24)]);

        let mut content: Vec<Value> = Vec::new();
        if !response_text.is_empty() {
            content.push(json!({"type":"text","text":response_text}));
        }

        let response = json!({
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "content": content,
            "model": model_name,
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens
            }
        });

        let body_bytes = serde_json::to_vec(&response).unwrap_or_default();
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(body_bytes))
            .expect("static response")
    }
}

fn openai_to_anthropic_response(openai: &Value, original: &Value) -> Value {
    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let choice = openai
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first());

    let message = choice.and_then(|c| c.get("message"));

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
        // Handle reasoning_content (thinking tokens from models like Qwen).
        if let Some(reasoning) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
            if !reasoning.is_empty() {
                content.push(json!({"type": "thinking", "thinking": reasoning}));
            }
        }

        if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
        }

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

    let default_usage = json!({});
    let usage = openai.get("usage").unwrap_or(&default_usage);
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

// ---------------------------------------------------------------------------
// Streaming response translation: OpenAI SSE → Anthropic SSE
// ---------------------------------------------------------------------------

async fn handle_streaming_response(
    upstream_resp: reqwest::Response,
    original: &Value,
) -> Response {
    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let stream = upstream_resp.bytes_stream();
    let translated_stream =
        translate_openai_stream_to_anthropic(stream, original_model);

    let body = axum::body::Body::from_stream(translated_stream);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .expect("static response")
}

struct StreamTranslator {
    model: String,
    content_block_index: usize,
    started: bool,
    in_text_block: bool,
    in_tool_block: bool,
    in_thinking_block: bool,
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
            in_thinking_block: false,
            current_tool_id: String::new(),
            current_tool_name: String::new(),
            total_output_tokens: 0,
        }
    }

    fn process_line(&mut self, line: &str) -> Vec<String> {
        let mut events = Vec::new();

        if line.trim().is_empty() || line.trim() == "[DONE]" {
            if line.trim() == "[DONE]"
                && (self.in_text_block || self.in_tool_block || self.in_thinking_block)
            {
                if self.in_thinking_block {
                    events.push(self.emit_content_block_stop());
                    self.in_thinking_block = false;
                }
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

        let chunk: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return events,
        };

        if !self.started {
            events.push(self.emit_message_start());
            self.started = true;
        }

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

        if let Some(usage) = chunk.get("usage") {
            if let Some(tokens) =
                usage.get("completion_tokens").and_then(|v| v.as_u64())
            {
                self.total_output_tokens = tokens;
            }
        }

        if let Some(delta) = delta {
            // Handle reasoning_content (thinking tokens from models like Qwen).
            if let Some(thinking) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                if !self.in_thinking_block && !self.in_text_block && !self.in_tool_block {
                    events.push(self.emit_content_block_start_thinking());
                    self.in_thinking_block = true;
                }
                if self.in_text_block {
                    events.push(self.emit_content_block_stop());
                    self.in_text_block = false;
                    self.content_block_index += 1;
                    events.push(self.emit_content_block_start_thinking());
                    self.in_thinking_block = true;
                }
                if self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                    self.content_block_index += 1;
                    events.push(self.emit_content_block_start_thinking());
                    self.in_thinking_block = true;
                }
                events.push(self.emit_thinking_delta(thinking));
            }

            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                if !self.in_text_block && !self.in_tool_block {
                    if self.in_thinking_block {
                        events.push(self.emit_content_block_stop());
                        self.in_thinking_block = false;
                        self.content_block_index += 1;
                    }
                    events.push(self.emit_content_block_start_text());
                    self.in_text_block = true;
                }
                if self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                    self.content_block_index += 1;
                    events.push(self.emit_content_block_start_text());
                    self.in_text_block = true;
                }
                events.push(self.emit_text_delta(text));
            }

            if let Some(tool_calls) =
                delta.get("tool_calls").and_then(|v| v.as_array())
            {
                for tc in tool_calls {
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        if self.in_thinking_block {
                            events.push(self.emit_content_block_stop());
                            self.in_thinking_block = false;
                            self.content_block_index += 1;
                        }
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

                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        if !args.is_empty() {
                            if !self.in_tool_block {
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

        if let Some(reason) = finish_reason {
            if self.in_thinking_block {
                events.push(self.emit_content_block_stop());
                self.in_thinking_block = false;
            }
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
        format!("event: message_start\ndata: {event}\n\n")
    }

    fn emit_content_block_start_text(&mut self) -> String {
        let event = json!({
            "type": "content_block_start",
            "index": self.content_block_index,
            "content_block": {"type": "text", "text": ""}
        });
        format!("event: content_block_start\ndata: {event}\n\n")
    }

    fn emit_content_block_start_thinking(&mut self) -> String {
        let event = json!({
            "type": "content_block_start",
            "index": self.content_block_index,
            "content_block": {"type": "thinking", "thinking": ""}
        });
        format!("event: content_block_start\ndata: {event}\n\n")
    }

    fn emit_content_block_start_tool(&mut self, id: &str, name: &str) -> String {
        let event = json!({
            "type": "content_block_start",
            "index": self.content_block_index,
            "content_block": {"type": "tool_use", "id": id, "name": name}
        });
        format!("event: content_block_start\ndata: {event}\n\n")
    }

    fn emit_text_delta(&self, text: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "text_delta", "text": text}
        });
        format!("event: content_block_delta\ndata: {event}\n\n")
    }

    fn emit_thinking_delta(&self, thinking: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "thinking_delta", "thinking": thinking}
        });
        format!("event: content_block_delta\ndata: {event}\n\n")
    }

    fn emit_input_json_delta(&self, json_str: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "input_json_delta", "partial_json": json_str}
        });
        format!("event: content_block_delta\ndata: {event}\n\n")
    }

    fn emit_content_block_stop(&self) -> String {
        let event = json!({
            "type": "content_block_stop",
            "index": self.content_block_index
        });
        format!("event: content_block_stop\ndata: {event}\n\n")
    }

    fn emit_message_delta(&self, stop_reason: &str) -> String {
        let event = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": self.total_output_tokens}
        });
        format!("event: message_delta\ndata: {event}\n\n")
    }

    fn emit_message_stop(&self) -> String {
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string()
    }
}

fn translate_openai_stream_to_anthropic(
    stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>>
        + Unpin,
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
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim().to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        let assistant = &output["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["tool_calls"].is_array());
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "bash");
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
        let chunk2 =
            r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk3 = r#"{"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#;
        let chunk4 =
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;

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
        let chunk5 =
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

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

//! Gemini-native request handlers with compression.
//!
//! Ports `headroom/proxy/handlers/gemini.py`. Gemini's native API uses
//! `contents[]` with `parts[]` instead of OpenAI's `messages[]`, and
//! `systemInstruction` instead of a system message. The handler converts
//! to OpenAI format for the compression pipeline, then converts back.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use headroom_core::auth_mode::{classify as classify_auth_mode, AuthMode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

use crate::compression::live_zone_openai::compress_openai_chat_request;
use crate::compression::Outcome;
use crate::config::CompressionMode;
use crate::error::ProxyError;
use crate::proxy::AppState;

/// Output-token count for a Gemini `usageMetadata`, including thinking tokens.
///
/// Gemini reports `candidatesTokenCount` sometimes inclusive of the
/// `thoughtsTokenCount` (2.5-family reasoning) and sometimes exclusive of it.
/// When `promptTokenCount + candidatesTokenCount != totalTokenCount` the
/// thinking tokens are a separate bucket and must be added, or the output cost
/// (billed at the output rate) is undercounted. Mirrors litellm's
/// `is_candidate_token_count_inclusive` rule. Robust to missing/null fields.
pub(crate) fn gemini_output_tokens(usage_meta: &Value) -> u64 {
    let field = |key: &str| usage_meta.get(key).and_then(Value::as_u64).unwrap_or(0);

    let candidates = field("candidatesTokenCount");
    let thoughts = field("thoughtsTokenCount");
    if thoughts == 0 {
        return candidates;
    }
    // Inclusive iff prompt + candidates already equals total; otherwise the
    // thinking tokens are a separate bucket that belongs in the output count.
    if field("promptTokenCount") + candidates == field("totalTokenCount") {
        candidates
    } else {
        candidates + thoughts
    }
}

/// Parse a `model:action` path segment (e.g. `"gemini-2.0-flash:generateContent"`)
/// into `(model, action)`. Returns `None` if there is no `:` separator.
pub(crate) fn split_model_action(model_action: &str) -> Option<(&str, &str)> {
    let (model, action) = model_action.rsplit_once(':')?;
    Some((action, model)) // rsplit_once returns (before, after) so action is first
}

/// Parse `model:action` where the action is the part AFTER the last colon.
/// For `"gemini-2.0-flash:generateContent"` returns `("gemini-2.0-flash", "generateContent")`.
pub(crate) fn parse_model_and_action(model_action: &str) -> Option<(&str, &str)> {
    model_action
        .rsplit_once(':')
        .map(|(action, model)| (model, action))
}

// ─── Query params ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct GeminiQuery {
    pub alt: Option<String>,
    pub key: Option<String>,
}

// ─── Gemini format conversion utilities ───────────────────────────────────

/// Check if a Gemini content entry has non-text parts.
pub fn has_non_text_parts(content: &Value) -> bool {
    content
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts.iter().any(|part| {
                part.get("inlineData").is_some()
                    || part.get("fileData").is_some()
                    || part.get("functionCall").is_some()
                    || part.get("functionResponse").is_some()
            })
        })
        .unwrap_or(false)
}

/// Extract text parts from a Gemini content entry.
fn text_parts(content: &Value) -> Vec<String> {
    content
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Convert Gemini contents[] format to OpenAI messages[] format for compression.
pub fn gemini_contents_to_messages(
    contents: &[Value],
    system_instruction: Option<&Value>,
) -> (Vec<Value>, HashSet<usize>) {
    let mut messages = Vec::new();
    let mut preserved_indices = HashSet::new();

    if let Some(system_instruction) = system_instruction {
        let parts = text_parts(system_instruction);
        if !parts.is_empty() {
            messages.push(json!({
                "role": "system",
                "content": parts.join("\n"),
            }));
        }
    }

    for (idx, content) in contents.iter().enumerate() {
        if has_non_text_parts(content) {
            preserved_indices.insert(idx);
        }

        let role = match content
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
        {
            "model" => "assistant",
            other => other,
        };
        let parts = text_parts(content);
        if !parts.is_empty() {
            messages.push(json!({
                "role": role,
                "content": parts.join("\n"),
            }));
        }
    }

    (messages, preserved_indices)
}

/// Convert OpenAI messages[] format back to Gemini contents[] plus systemInstruction.
pub fn messages_to_gemini_contents(messages: &[Value]) -> (Vec<Value>, Option<Value>) {
    let mut contents = Vec::new();
    let mut system_instruction = None;

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };

        if role == "system" {
            system_instruction = Some(json!({"parts": [{"text": content}]}));
            continue;
        }

        let gemini_role = if role == "assistant" { "model" } else { "user" };
        contents.push(json!({
            "role": gemini_role,
            "parts": [{"text": content}],
        }));
    }

    (contents, system_instruction)
}

/// Interleave preserved (non-text) entries back into optimized_contents at their
/// original positions.
pub fn rebuild_gemini_contents(
    original_contents: &[Value],
    preserved_indices: &HashSet<usize>,
    preserved_contents: &HashMap<usize, Value>,
    optimized_contents: Vec<Value>,
) -> Vec<Value> {
    let mut optimized_iter = optimized_contents.into_iter();
    let mut result = Vec::new();

    for (idx, original_content) in original_contents.iter().enumerate() {
        let had_text = !text_parts(original_content).is_empty();
        if preserved_indices.contains(&idx) {
            if let Some(preserved) = preserved_contents.get(&idx) {
                result.push(preserved.clone());
            }
            if had_text {
                let _ = optimized_iter.next();
            }
        } else if let Some(optimized) = optimized_iter.next() {
            result.push(optimized);
        }
    }

    result
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn strip_headroom_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        let lower = name_str.to_ascii_lowercase();
        if lower.starts_with("x-headroom-")
            || lower == "host"
            || lower == "content-length"
            || lower == "accept-encoding"
        {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.insert(lower, v.to_string());
        }
    }
    out
}

fn error_response(status: StatusCode, message: impl Into<String>, code: i32) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message.into(),
                "code": code,
            }
        })),
    )
        .into_response()
}

fn compress_messages(
    messages: &[Value],
    model: &str,
    mode: CompressionMode,
    auth_mode: AuthMode,
    request_id: &str,
) -> Option<(Vec<Value>, usize, usize)> {
    let wrapper = json!({
        "model": model,
        "messages": messages,
    });
    let body = Bytes::from(serde_json::to_vec(&wrapper).ok()?);
    match compress_openai_chat_request(&body, mode, auth_mode, request_id) {
        Outcome::Compressed {
            body,
            tokens_before,
            tokens_after,
            ..
        } if tokens_after <= tokens_before => {
            let compressed: Value = serde_json::from_slice(&body).ok()?;
            let compressed_messages = compressed
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| messages.to_vec());
            Some((compressed_messages, tokens_before, tokens_after))
        }
        Outcome::Compressed { tokens_before, .. } => {
            Some((messages.to_vec(), tokens_before, tokens_before))
        }
        _ => None,
    }
}

fn auth_mode_for_gemini(headers: &HeaderMap) -> AuthMode {
    let mode = classify_auth_mode(headers);
    if matches!(mode, AuthMode::Payg) {
        AuthMode::Payg
    } else {
        mode
    }
}

// ─── Token counting (simple heuristic) ────────────────────────────────────

/// Rough token count: split on whitespace/punctuation, ~4 chars per token.
fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.split(|c: char| c.is_whitespace() || c == ',' || c == '.' || c == ';' || c == ':')
        .filter(|s| !s.is_empty())
        .count()
}

fn count_message_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .map(|s| estimate_tokens(s))
        .sum()
}

// ─── Unified dispatcher ────────────────────────────────────────────────────

/// Unified dispatcher for `POST /v1beta/models/*model_action`.
/// Routes to the appropriate handler based on the action suffix.
pub async fn handle_gemini_action(
    state: State<AppState>,
    Path(model_action): Path<String>,
    query: Query<GeminiQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (model, action) = match parse_model_and_action(&model_action) {
        Some(pair) => pair,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("Invalid path: expected model:action, got '{model_action}'"),
                400,
            );
        }
    };
    let model = model.to_string();
    match action {
        "generateContent" => {
            handle_generate_content_inner(state, model, query, headers, body).await
        }
        "streamGenerateContent" => {
            handle_stream_generate_content_inner(state, model, query, headers, body).await
        }
        "countTokens" => handle_count_tokens_inner(state, model, query, headers, body).await,
        "batchGenerateContent" => {
            if state.config.enable_batch_api {
                // Delegate to the batch handler.
                crate::handlers::batch::google_batch_create(
                    state,
                    Path(model_action),
                    headers,
                    body,
                )
                .await
            } else {
                // Batch API disabled: forward to upstream unchanged,
                // preserving the flag's documented passthrough semantics.
                let base = state.config.upstream.as_str().trim_end_matches('/');
                let mut url = format!("{base}/v1beta/models/{model_action}");
                if let Some(ref key) = query.key {
                    url.push_str(&format!("?key={key}"));
                }
                let upstream_headers = strip_headroom_headers(&headers);
                let body_json: Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            format!("Invalid request body: {e}"),
                            400,
                        );
                    }
                };
                match forward_to_upstream(&state, "POST", &url, upstream_headers, &body_json).await
                {
                    Ok(r) => r,
                    Err(e) => {
                        error_response(StatusCode::BAD_GATEWAY, format!("upstream error: {e}"), 502)
                    }
                }
            }
        }
        _ => error_response(
            StatusCode::NOT_FOUND,
            format!("Unknown Gemini action '{action}'"),
            404,
        ),
    }
}

// ─── Route: :generateContent ──────────────────────────────────────────────

/// Handle `POST /v1beta/models/*model_action` where model_action is `model:generateContent`.
async fn handle_generate_content_inner(
    State(state): State<AppState>,
    model: String,
    Query(query): Query<GeminiQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = crate::proxy::ensure_request_id(&headers);
    let start = std::time::Instant::now();

    // Parse body
    let body_json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("Invalid request body: {e}"),
                400,
            );
        }
    };

    let contents = body_json
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let system_instruction = body_json.get("systemInstruction");

    // Strip internal headers
    let upstream_headers = strip_headroom_headers(&headers);

    // Convert to OpenAI messages for compression
    let (messages, preserved_indices) = gemini_contents_to_messages(&contents, system_instruction);
    let preserved_contents: HashMap<usize, Value> = preserved_indices
        .iter()
        .map(|&idx| (idx, contents[idx].clone()))
        .collect();

    let original_tokens = count_message_tokens(&messages);

    // Early exit: all content has non-text parts
    if preserved_indices.len() == contents.len() {
        return forward_to_gemini_upstream(&state, &model, &upstream_headers, &body_json, &query)
            .await;
    }

    // Compression decision
    let auth_mode = auth_mode_for_gemini(&headers);
    let has_messages = !messages.is_empty();
    let decision = crate::compression_decision::CompressionDecision::decide(
        &headers,
        state.config.compression,
        true,
        has_messages,
    );

    let mut optimized_messages = messages.clone();
    let mut tokens_saved = 0usize;
    let mut transforms_applied: Vec<String> = Vec::new();

    if decision.should_compress {
        if let Some((compressed, before, after)) = compress_messages(
            &messages,
            &model,
            state.config.compression_mode,
            auth_mode,
            &request_id,
        ) {
            optimized_messages = compressed;
            tokens_saved = before.saturating_sub(after);
            transforms_applied.push("gemini_openai_compression".to_string());
        }
    }

    // Convert back to Gemini format if optimized
    let mut output_body = body_json.clone();
    if optimized_messages != messages {
        let (opt_contents, opt_sys) = messages_to_gemini_contents(&optimized_messages);
        let rebuilt = rebuild_gemini_contents(
            &contents,
            &preserved_indices,
            &preserved_contents,
            opt_contents,
        );
        output_body["contents"] = Value::Array(rebuilt);
        if let Some(sys) = opt_sys {
            output_body["systemInstruction"] = sys;
        } else if output_body.get("systemInstruction").is_some() {
            output_body
                .as_object_mut()
                .unwrap()
                .remove("systemInstruction");
        }
    }

    // Forward to upstream
    let is_streaming = query.alt.as_deref() == Some("sse")
        || headers
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.contains("text/event-stream"))
            .unwrap_or(false);

    if is_streaming {
        return forward_streaming(
            &state,
            &model,
            &upstream_headers,
            &output_body,
            &query,
            &request_id,
            original_tokens,
            original_tokens.saturating_sub(tokens_saved),
            tokens_saved,
            &transforms_applied,
        )
        .await;
    }

    // Non-streaming forward
    let upstream_url = build_gemini_upstream_url(&state, &model, &query, false);
    let resp = forward_to_upstream(
        &state,
        "POST",
        &upstream_url,
        upstream_headers,
        &output_body,
    )
    .await;

    let latency = start.elapsed().as_millis() as f64;
    let response = match resp {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                event = "gemini_upstream_error",
                request_id = %request_id,
                model = %model,
                error = %e,
                "Gemini upstream request failed"
            );
            return error_response(StatusCode::BAD_GATEWAY, "Upstream request failed", 502);
        }
    };

    // Extract usage from response
    let resp_status = response.status();
    let resp_headers_map = response.headers().clone();
    let resp_body = response.into_body();
    let resp_bytes = axum::body::to_bytes(resp_body, 10 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let resp_json: Value = serde_json::from_slice(&resp_bytes).unwrap_or_default();
    let usage = resp_json.get("usageMetadata").cloned().unwrap_or_default();
    let upstream_input_tokens = usage
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(original_tokens as u64) as usize;
    let output_tokens = gemini_output_tokens(&usage) as usize;
    let cache_read_tokens = usage
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;

    let optimized_tokens = upstream_input_tokens;
    let actual_saved = original_tokens.saturating_sub(optimized_tokens);

    // Build response with metrics headers
    let mut resp_headers: HeaderMap = resp_headers_map;
    resp_headers.remove("content-encoding");
    resp_headers.remove("content-length");
    resp_headers.insert(
        "x-headroom-tokens-before",
        HeaderValue::from_str(&original_tokens.to_string()).unwrap(),
    );
    resp_headers.insert(
        "x-headroom-tokens-after",
        HeaderValue::from_str(&optimized_tokens.to_string()).unwrap(),
    );
    resp_headers.insert(
        "x-headroom-tokens-saved",
        HeaderValue::from_str(&actual_saved.to_string()).unwrap(),
    );
    // `model` comes from the URL path — invalid header bytes must not panic.
    if let Ok(hv) = HeaderValue::from_str(&model) {
        resp_headers.insert("x-headroom-model", hv);
    }
    if !transforms_applied.is_empty() {
        resp_headers.insert(
            "x-headroom-transforms",
            HeaderValue::from_str(&transforms_applied.join(",")).unwrap(),
        );
    }
    if cache_read_tokens > 0 {
        resp_headers.insert("x-headroom-cached", HeaderValue::from_str("true").unwrap());
    }

    // Log outcome
    tracing::info!(
        event = "gemini_request",
        request_id = %request_id,
        model = %model,
        original_tokens = original_tokens,
        optimized_tokens = optimized_tokens,
        tokens_saved = actual_saved,
        output_tokens = output_tokens,
        cache_read_tokens = cache_read_tokens,
        latency_ms = latency,
        "Gemini generateContent completed"
    );

    (resp_status, resp_headers, resp_bytes).into_response()
}

// ─── Route: :streamGenerateContent ────────────────────────────────────────

/// Handle `POST /v1beta/models/*model_action` where model_action is `model:streamGenerateContent`.
async fn handle_stream_generate_content_inner(
    State(state): State<AppState>,
    model: String,
    Query(query): Query<GeminiQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let upstream_headers = strip_headroom_headers(&headers);
    let request_id = crate::proxy::ensure_request_id(&headers);

    let body_json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("Invalid request body: {e}"),
                400,
            );
        }
    };

    let contents = body_json
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let system_instruction = body_json.get("systemInstruction");

    let (messages, preserved_indices) = gemini_contents_to_messages(&contents, system_instruction);
    let preserved_contents: HashMap<usize, Value> = preserved_indices
        .iter()
        .map(|&idx| (idx, contents[idx].clone()))
        .collect();

    let original_tokens = count_message_tokens(&messages);

    // Compression
    let auth_mode = auth_mode_for_gemini(&headers);
    let decision = crate::compression_decision::CompressionDecision::decide(
        &headers,
        state.config.compression,
        true,
        !messages.is_empty(),
    );

    let mut optimized_messages = messages.clone();
    let mut tokens_saved = 0usize;
    let mut transforms_applied: Vec<String> = Vec::new();

    if decision.should_compress {
        if let Some((compressed, before, after)) = compress_messages(
            &messages,
            &model,
            state.config.compression_mode,
            auth_mode,
            &request_id,
        ) {
            optimized_messages = compressed;
            tokens_saved = before.saturating_sub(after);
            transforms_applied.push("gemini_openai_compression".to_string());
        }
    }

    let mut output_body = body_json.clone();
    if optimized_messages != messages {
        let (opt_contents, opt_sys) = messages_to_gemini_contents(&optimized_messages);
        let rebuilt = rebuild_gemini_contents(
            &contents,
            &preserved_indices,
            &preserved_contents,
            opt_contents,
        );
        output_body["contents"] = Value::Array(rebuilt);
        if let Some(sys) = opt_sys {
            output_body["systemInstruction"] = sys;
        } else if output_body.get("systemInstruction").is_some() {
            output_body
                .as_object_mut()
                .unwrap()
                .remove("systemInstruction");
        }
    }

    // Forward as streaming
    forward_streaming(
        &state,
        &model,
        &upstream_headers,
        &output_body,
        &query,
        &request_id,
        original_tokens,
        original_tokens.saturating_sub(tokens_saved),
        tokens_saved,
        &transforms_applied,
    )
    .await
}

// ─── Route: :countTokens ──────────────────────────────────────────────────

/// Handle `POST /v1beta/models/*model_action` where model_action is `model:countTokens`.
async fn handle_count_tokens_inner(
    State(state): State<AppState>,
    model: String,
    Query(query): Query<GeminiQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = crate::proxy::ensure_request_id(&headers);
    let start = std::time::Instant::now();

    let body_json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("Invalid request body: {e}"),
                400,
            );
        }
    };

    let contents = body_json
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let system_instruction = body_json.get("systemInstruction");

    let upstream_headers = strip_headroom_headers(&headers);

    let (messages, preserved_indices) = gemini_contents_to_messages(&contents, system_instruction);
    let preserved_contents: HashMap<usize, Value> = preserved_indices
        .iter()
        .map(|&idx| (idx, contents[idx].clone()))
        .collect();

    let original_tokens = count_message_tokens(&messages);

    // Early exit: all non-text
    if preserved_indices.len() == contents.len() {
        return forward_to_gemini_upstream(&state, &model, &upstream_headers, &body_json, &query)
            .await;
    }

    // Compression for countTokens
    let auth_mode = auth_mode_for_gemini(&headers);
    let decision = crate::compression_decision::CompressionDecision::decide(
        &headers,
        state.config.compression,
        true,
        !messages.is_empty(),
    );

    let mut optimized_messages = messages.clone();
    let mut transforms_applied: Vec<String> = Vec::new();

    if decision.should_compress {
        if let Some((compressed, _, _)) = compress_messages(
            &messages,
            &model,
            state.config.compression_mode,
            auth_mode,
            &request_id,
        ) {
            optimized_messages = compressed;
            transforms_applied.push("gemini_openai_compression".to_string());
        }
    }

    let mut output_body = body_json.clone();
    if optimized_messages != messages {
        let (opt_contents, opt_sys) = messages_to_gemini_contents(&optimized_messages);
        let rebuilt = rebuild_gemini_contents(
            &contents,
            &preserved_indices,
            &preserved_contents,
            opt_contents,
        );
        output_body["contents"] = Value::Array(rebuilt);
        if let Some(sys) = opt_sys {
            output_body["systemInstruction"] = sys;
        } else if output_body.get("systemInstruction").is_some() {
            output_body
                .as_object_mut()
                .unwrap()
                .remove("systemInstruction");
        }
    }

    // Forward to upstream countTokens
    let upstream_url = format!(
        "{}/v1beta/models/{}:countTokens",
        state.config.upstream.as_str().trim_end_matches('/'),
        model
    );
    let resp = forward_to_upstream(
        &state,
        "POST",
        &upstream_url,
        upstream_headers,
        &output_body,
    )
    .await;

    let latency = start.elapsed().as_millis() as f64;

    match resp {
        Ok(r) => {
            let resp_status = r.status();
            let resp_headers_map = r.headers().clone();
            let resp_body = r.into_body();
            let resp_bytes = axum::body::to_bytes(resp_body, 10 * 1024 * 1024)
                .await
                .unwrap_or_default();
            let resp_json: Value = serde_json::from_slice(&resp_bytes).unwrap_or_default();
            let compressed_tokens = resp_json
                .get("totalTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let tokens_saved = original_tokens.saturating_sub(compressed_tokens);

            tracing::info!(
                event = "gemini_count_tokens",
                request_id = %request_id,
                model = %model,
                original_tokens = original_tokens,
                compressed_tokens = compressed_tokens,
                tokens_saved = tokens_saved,
                latency_ms = latency,
                "Gemini countTokens completed"
            );

            let mut resp_headers: HeaderMap = resp_headers_map;
            resp_headers.remove("content-encoding");
            resp_headers.remove("content-length");

            (resp_status, resp_headers, resp_bytes).into_response()
        }
        Err(e) => {
            tracing::error!(
                event = "gemini_upstream_error",
                request_id = %request_id,
                model = %model,
                error = %e,
                "Gemini countTokens upstream request failed"
            );
            error_response(StatusCode::BAD_GATEWAY, "Upstream request failed", 502)
        }
    }
}

// ─── Upstream forwarding ──────────────────────────────────────────────────

fn build_gemini_upstream_url(
    state: &AppState,
    model: &str,
    query: &GeminiQuery,
    streaming: bool,
) -> String {
    let base = state.config.upstream.as_str().trim_end_matches('/');
    let action = if streaming {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let mut url = format!("{}/v1beta/models/{}:{}", base, model, action);
    if streaming {
        url.push_str("?alt=sse");
    }
    if let Some(ref key) = query.key {
        let sep = if url.contains('?') { '&' } else { '?' };
        url.push_str(&format!("{}key={}", sep, key));
    }
    url
}

async fn forward_to_upstream(
    state: &AppState,
    method: &str,
    url: &str,
    headers: HashMap<String, String>,
    body: &Value,
) -> Result<axum::response::Response<Body>, ProxyError> {
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|e| ProxyError::InvalidUpstream(format!("invalid method: {e}")))?;
    let body_bytes = Bytes::from(serde_json::to_vec(body).unwrap_or_default());
    let mut req_builder = state.client.request(method, url);
    for (k, v) in &headers {
        if let Ok(hv) = HeaderValue::from_str(v) {
            req_builder = req_builder.header(k.as_str(), hv);
        }
    }
    let resp = req_builder
        .body(body_bytes)
        .send()
        .await
        .map_err(ProxyError::Upstream)?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = resp.headers().clone();
    let resp_body = resp.bytes().await.map_err(ProxyError::Upstream)?;
    let mut response = Response::builder().status(status);
    {
        let h = response.headers_mut().expect("builder has headers");
        for (name, value) in resp_headers.iter() {
            if name.as_str().eq_ignore_ascii_case("content-length")
                || name.as_str().eq_ignore_ascii_case("content-encoding")
            {
                continue;
            }
            h.insert(name.clone(), value.clone());
        }
    }
    response
        .body(Body::from(resp_body))
        .map_err(|e| ProxyError::InvalidUpstream(format!("response build: {e}")))
}

async fn forward_to_gemini_upstream(
    state: &AppState,
    model: &str,
    headers: &HashMap<String, String>,
    body: &Value,
    query: &GeminiQuery,
) -> Response {
    let url = build_gemini_upstream_url(state, model, query, false);
    match forward_to_upstream(state, "POST", &url, headers.clone(), body).await {
        Ok(r) => {
            let status = r.status();
            let resp_headers = r.headers().clone();
            let resp_body = axum::body::to_bytes(r.into_body(), 10 * 1024 * 1024)
                .await
                .unwrap_or_default();
            let mut builder = Response::builder().status(status);
            {
                let h = builder.headers_mut().expect("builder has headers");
                for (name, value) in resp_headers.iter() {
                    if name.as_str().eq_ignore_ascii_case("content-length")
                        || name.as_str().eq_ignore_ascii_case("content-encoding")
                    {
                        continue;
                    }
                    h.insert(name.clone(), value.clone());
                }
            }
            builder.body(Body::from(resp_body)).unwrap_or_else(|e| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("response build: {e}"),
                    500,
                )
            })
        }
        Err(e) => {
            tracing::error!(error = %e, "Gemini upstream request failed");
            error_response(StatusCode::BAD_GATEWAY, "Upstream request failed", 502)
        }
    }
}

async fn forward_streaming(
    state: &AppState,
    model: &str,
    headers: &HashMap<String, String>,
    body: &Value,
    query: &GeminiQuery,
    request_id: &str,
    original_tokens: usize,
    optimized_tokens: usize,
    tokens_saved: usize,
    transforms_applied: &[String],
) -> Response {
    let url = build_gemini_upstream_url(state, model, query, true);

    let method = Method::POST;
    let body_bytes = Bytes::from(serde_json::to_vec(body).unwrap_or_default());
    let mut req_builder = state.client.request(method, &url);
    for (k, v) in headers {
        if let Ok(hv) = HeaderValue::from_str(v) {
            req_builder = req_builder.header(k.as_str(), hv);
        }
    }
    // Request SSE encoding
    req_builder = req_builder.header("accept", "text/event-stream");

    let resp = match req_builder.body(body_bytes).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                event = "gemini_stream_error",
                request_id = %request_id,
                model = %model,
                error = %e,
                "Gemini streaming upstream request failed"
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "Upstream streaming request failed",
                502,
            );
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut resp_headers: HeaderMap = resp.headers().clone();
    resp_headers.remove("content-length");
    resp_headers.insert(
        "x-headroom-tokens-before",
        HeaderValue::from_str(&original_tokens.to_string()).unwrap(),
    );
    resp_headers.insert(
        "x-headroom-tokens-after",
        HeaderValue::from_str(&optimized_tokens.to_string()).unwrap(),
    );
    resp_headers.insert(
        "x-headroom-tokens-saved",
        HeaderValue::from_str(&tokens_saved.to_string()).unwrap(),
    );
    // `model` comes from the URL path — invalid header bytes must not panic.
    if let Ok(hv) = HeaderValue::from_str(model) {
        resp_headers.insert("x-headroom-model", hv);
    }
    if !transforms_applied.is_empty() {
        resp_headers.insert(
            "x-headroom-transforms",
            HeaderValue::from_str(&transforms_applied.join(",")).unwrap(),
        );
    }

    // Stream the response body
    let stream = resp.bytes_stream();
    let body = Body::from_stream(stream);

    let mut builder = Response::builder().status(status);
    {
        let h = builder.headers_mut().expect("builder has headers");
        for (name, value) in resp_headers.iter() {
            h.insert(name.clone(), value.clone());
        }
    }
    builder.body(body).unwrap_or_else(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("stream response build: {e}"),
            500,
        )
    })
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn has_non_text_parts_inline_data() {
        let content = json!({
            "parts": [{"inlineData": {"mimeType": "image/png", "data": "abc"}}]
        });
        assert!(has_non_text_parts(&content));
    }

    #[test]
    fn has_non_text_parts_function_call() {
        let content = json!({
            "parts": [{"functionCall": {"name": "get_weather", "args": {}}}]
        });
        assert!(has_non_text_parts(&content));
    }

    #[test]
    fn has_non_text_parts_text_only() {
        let content = json!({
            "parts": [{"text": "hello"}]
        });
        assert!(!has_non_text_parts(&content));
    }

    #[test]
    fn gemini_to_messages_basic() {
        let contents = vec![
            json!({"role": "user", "parts": [{"text": "Hi"}]}),
            json!({"role": "model", "parts": [{"text": "Hello!"}]}),
        ];
        let (messages, preserved) = gemini_contents_to_messages(&contents, None);
        assert_eq!(messages.len(), 2);
        assert!(preserved.is_empty());
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hi");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "Hello!");
    }

    #[test]
    fn gemini_to_messages_with_system() {
        let contents = vec![json!({"role": "user", "parts": [{"text": "Hi"}]})];
        let sys = json!({"parts": [{"text": "You are helpful"}]});
        let (messages, _) = gemini_contents_to_messages(&contents, Some(&sys));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are helpful");
    }

    #[test]
    fn gemini_to_messages_preserves_non_text() {
        let contents = vec![
            json!({"role": "user", "parts": [{"text": "Hi"}]}),
            json!({"role": "user", "parts": [{"inlineData": {"data": "abc"}}]}),
            json!({"role": "model", "parts": [{"text": "Response"}]}),
        ];
        let (messages, preserved) = gemini_contents_to_messages(&contents, None);
        assert_eq!(messages.len(), 2); // text-only entries
        assert!(preserved.contains(&1));
    }

    #[test]
    fn messages_to_gemini_roundtrip() {
        let messages = vec![
            json!({"role": "system", "content": "Be helpful"}),
            json!({"role": "user", "content": "Hi"}),
            json!({"role": "assistant", "content": "Hello!"}),
        ];
        let (contents, sys) = messages_to_gemini_contents(&messages);
        assert_eq!(contents.len(), 2);
        assert!(sys.is_some());
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
    }

    #[test]
    fn rebuild_gemini_contents_interleaves() {
        let original = vec![
            json!({"role": "user", "parts": [{"text": "msg1"}]}),
            json!({"role": "user", "parts": [{"inlineData": {"data": "x"}}]}),
            json!({"role": "user", "parts": [{"text": "msg2"}]}),
        ];
        let preserved_indices: HashSet<usize> = [1].into();
        let preserved_contents: HashMap<usize, Value> = [(1, original[1].clone())].into();
        let optimized = vec![
            json!({"role": "user", "parts": [{"text": "compressed1"}]}),
            json!({"role": "user", "parts": [{"text": "compressed2"}]}),
        ];
        let result = rebuild_gemini_contents(
            &original,
            &preserved_indices,
            &preserved_contents,
            optimized,
        );
        assert_eq!(result.len(), 3);
        assert_eq!(result[0]["parts"][0]["text"], "compressed1");
        assert!(result[1]["parts"][0].get("inlineData").is_some());
        assert_eq!(result[2]["parts"][0]["text"], "compressed2");
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_basic() {
        let count = estimate_tokens("hello world");
        assert!(count >= 1 && count <= 3);
    }

    #[test]
    fn strip_headroom_headers_removes_internal() {
        let mut headers = HeaderMap::new();
        headers.insert("x-headroom-project-id", HeaderValue::from_static("test"));
        headers.insert("authorization", HeaderValue::from_static("Bearer tok"));
        headers.insert("host", HeaderValue::from_static("example.com"));
        let result = strip_headroom_headers(&headers);
        assert!(!result.contains_key("x-headroom-project-id"));
        assert!(result.contains_key("authorization"));
        assert!(!result.contains_key("host"));
    }

    /// Gemini 2.5 reasoning: thinking tokens are billed at the output rate but
    /// are sometimes a separate bucket from `candidatesTokenCount`.
    #[test]
    fn gemini_thinking_tokens_counted_when_exclusive() {
        // prompt + candidates != total → thoughts are separate, so add them.
        let usage = serde_json::json!({
            "promptTokenCount": 1000,
            "candidatesTokenCount": 200,
            "thoughtsTokenCount": 500,
            "totalTokenCount": 1700,
        });
        assert_eq!(gemini_output_tokens(&usage), 700);
    }

    #[test]
    fn gemini_thinking_tokens_not_double_counted_when_inclusive() {
        // prompt + candidates == total → candidates already includes thoughts.
        let usage = serde_json::json!({
            "promptTokenCount": 1000,
            "candidatesTokenCount": 700,
            "thoughtsTokenCount": 500,
            "totalTokenCount": 1700,
        });
        assert_eq!(gemini_output_tokens(&usage), 700);
    }

    #[test]
    fn gemini_output_tokens_without_thinking_or_fields() {
        let plain = serde_json::json!({"candidatesTokenCount": 42});
        assert_eq!(gemini_output_tokens(&plain), 42);
        assert_eq!(gemini_output_tokens(&serde_json::json!({})), 0);
        // Null / wrong-typed fields must not panic or poison the count.
        let junk = serde_json::json!({"candidatesTokenCount": null, "thoughtsTokenCount": "x"});
        assert_eq!(gemini_output_tokens(&junk), 0);
    }
}

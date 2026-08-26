//! Batch API handlers for Google and OpenAI batch operations.
//!
//! Port of `headroom/proxy/handlers/batch.py`. The Python mixin uses implicit
//! `self` state; every `self.X` maps to an explicit `AppState` field here.
//!
//! Routes are mounted behind the `enable_batch_api` config flag (default off).

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use headroom_core::auth_mode::{classify as classify_auth_mode, AuthMode};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

// The Gemini content-shape conversions live with the Gemini handler; the
// batch path is a second caller, not a second owner. They were copied here
// once and the two copies had already drifted apart in local variable names.
use crate::handlers::gemini::{
    gemini_contents_to_messages, messages_to_gemini_contents, rebuild_gemini_contents,
};

use crate::compression::live_zone_openai::compress_openai_chat_request;
use crate::compression::Outcome;
use crate::config::CompressionMode;
use crate::error::ProxyError;
use crate::proxy::AppState;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchJsonlStats {
    pub total_requests: usize,
    pub total_original_tokens: usize,
    pub total_compressed_tokens: usize,
    pub total_tokens_saved: usize,
    pub savings_percent: f64,
    pub errors: usize,
}

// -- helpers --

/// Strip internal `x-headroom-*` headers and hop/body-specific headers.
pub(crate) fn strip_headroom_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        let lower = name_str.to_ascii_lowercase();
        if lower.starts_with("x-headroom-") || lower == "host" || lower == "content-length" {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.insert(lower, v.to_string());
        }
    }
    out
}

pub(crate) fn upstream_url(state: &AppState, path: &str) -> String {
    format!(
        "{}/{}",
        state.config.upstream.as_str().trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn error_response(status: StatusCode, message: impl Into<String>, code: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message.into(),
                "type": if status.is_server_error() { "server_error" } else { "invalid_request_error" },
                "code": code,
            }
        })),
    )
        .into_response()
}

fn response_from_upstream(
    status: StatusCode,
    resp_headers: HeaderMap,
    resp_body: Bytes,
) -> Result<Response<Body>, ProxyError> {
    let mut response = Response::builder().status(status);
    {
        let h = response.headers_mut().expect("builder has headers");
        for (name, value) in resp_headers.iter() {
            if name.as_str().eq_ignore_ascii_case("content-length") {
                continue;
            }
            h.insert(name.clone(), value.clone());
        }
    }
    response
        .body(Body::from(resp_body))
        .map_err(|e| ProxyError::InvalidUpstream(format!("response build: {e}")))
}

/// Forward a request to upstream and relay the response.
pub(crate) async fn forward_to_upstream(
    state: &AppState,
    method: &str,
    url: &str,
    headers: HashMap<String, String>,
    body: Bytes,
) -> Result<Response<Body>, ProxyError> {
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|e| ProxyError::InvalidUpstream(format!("invalid method: {e}")))?;
    let mut req_builder = state.client.request(method, url);
    for (k, v) in &headers {
        if let Ok(hv) = HeaderValue::from_str(v) {
            req_builder = req_builder.header(k.as_str(), hv);
        }
    }
    let resp = req_builder
        .body(body)
        .send()
        .await
        .map_err(ProxyError::Upstream)?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = resp.headers().clone();
    let resp_body = resp.bytes().await.map_err(ProxyError::Upstream)?;
    response_from_upstream(status, resp_headers, resp_body)
}

pub(crate) fn compression_auth_mode(headers: &HeaderMap) -> AuthMode {
    let auth_mode = classify_auth_mode(headers);
    if matches!(auth_mode, AuthMode::Payg) {
        AuthMode::Payg
    } else {
        // Batch compression is explicitly opt-in and rewrites uploaded files.
        // Non-PAYG modes keep the compressor's cache-safety gates intact.
        auth_mode
    }
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
        Outcome::Compressed {
            tokens_before,
            tokens_after,
            ..
        } => Some((
            messages.to_vec(),
            tokens_before,
            tokens_before.min(tokens_after),
        )),
        _ => None,
    }
}

fn compress_openai_body_value(
    body: &mut Value,
    mode: CompressionMode,
    auth_mode: AuthMode,
    request_id: &str,
) -> Option<(usize, usize)> {
    let messages = body.get("messages").and_then(Value::as_array)?.clone();
    let model = body.get("model").and_then(Value::as_str).unwrap_or("gpt-4");
    let (compressed_messages, before, after) =
        compress_messages(&messages, model, mode, auth_mode, request_id)?;
    body.as_object_mut()?
        .insert("messages".to_string(), Value::Array(compressed_messages));
    Some((before, after))
}

fn multipart_file_upload_body(content: String, filename: &str) -> (Bytes, String) {
    let boundary = format!("headroom-{}", uuid::Uuid::new_v4());
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"purpose\"\r\n\r\n");
    body.extend_from_slice(b"batch\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/jsonl\r\n\r\n");
    body.extend_from_slice(content.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (Bytes::from(body), boundary)
}

fn compress_batch_jsonl_with_options(
    content: &str,
    mode: CompressionMode,
    auth_mode: AuthMode,
    request_id: &str,
) -> (Vec<String>, BatchJsonlStats) {
    let mut stats = BatchJsonlStats::default();
    let mut compressed_lines = Vec::new();

    for (idx, line) in content.trim().split('\n').enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        let mut request_obj: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    event = "batch_jsonl_invalid_line",
                    request_id = %request_id,
                    line = idx,
                    error = %e,
                    "invalid JSONL batch line; preserving original line"
                );
                stats.errors += 1;
                stats.total_requests += 1;
                compressed_lines.push(line.to_string());
                continue;
            }
        };

        let Some(body) = request_obj.get_mut("body") else {
            stats.total_requests += 1;
            compressed_lines.push(line.to_string());
            continue;
        };

        if body.get("messages").and_then(Value::as_array).is_none() {
            stats.total_requests += 1;
            compressed_lines.push(line.to_string());
            continue;
        }

        let line_request_id = format!("{request_id}:line:{idx}");
        match compress_openai_body_value(body, mode, auth_mode, &line_request_id) {
            Some((before, after)) if after <= before => {
                stats.total_original_tokens += before;
                stats.total_compressed_tokens += after;
                compressed_lines
                    .push(serde_json::to_string(&request_obj).unwrap_or_else(|_| line.to_string()));
            }
            Some((before, _after)) => {
                stats.total_original_tokens += before;
                stats.total_compressed_tokens += before;
                compressed_lines.push(line.to_string());
            }
            None => {
                compressed_lines.push(line.to_string());
            }
        }
        stats.total_requests += 1;
    }

    stats.total_tokens_saved = stats
        .total_original_tokens
        .saturating_sub(stats.total_compressed_tokens);
    stats.savings_percent = if stats.total_original_tokens > 0 {
        stats.total_tokens_saved as f64 / stats.total_original_tokens as f64 * 100.0
    } else {
        0.0
    };

    (compressed_lines, stats)
}

/// Compress messages in each line of a batch JSONL file.
pub fn compress_batch_jsonl(content: &str) -> (Vec<String>, BatchJsonlStats) {
    compress_batch_jsonl_with_options(
        content,
        CompressionMode::LiveZone,
        AuthMode::Payg,
        "batch-jsonl",
    )
}

fn serialize_json_body(value: &Value) -> Result<Bytes, Response> {
    serde_json::to_vec(value).map(Bytes::from).map_err(|e| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
            "invalid_json",
        )
    })
}

// -- Google batch --

/// `POST /v1beta/models/*model_action` where model_action is `model:batchGenerateContent`
pub async fn google_batch_create(
    State(state): State<AppState>,
    Path(model_action): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (model, action) = match model_action.rsplit_once(':') {
        Some((model, action)) => (model, action),
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("Invalid path: expected model:action, got '{model_action}'"),
                "invalid_path",
            );
        }
    };
    if action != "batchGenerateContent" {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("Unknown action '{action}'"),
            "unknown_action",
        );
    }
    let mut parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("Invalid request body: {e}"),
                "invalid_json",
            );
        }
    };

    let requests_opt = parsed
        .pointer_mut("/batch/input_config/requests/requests")
        .and_then(Value::as_array_mut);

    let Some(requests) = requests_opt else {
        return google_batch_passthrough(State(state), Path(model.to_string()), headers, body)
            .await;
    };
    if requests.is_empty() {
        return google_batch_passthrough(State(state), Path(model.to_string()), headers, body)
            .await;
    }

    let auth_mode = compression_auth_mode(&headers);
    let mut compressed_requests = Vec::with_capacity(requests.len());
    let mut total_before = 0usize;
    let mut total_after = 0usize;

    for (idx, batch_req) in requests.iter().enumerate() {
        let req_content = batch_req
            .get("request")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let Some(contents) = req_content.get("contents").and_then(Value::as_array) else {
            compressed_requests.push(batch_req.clone());
            continue;
        };
        if contents.is_empty() || matches!(state.config.compression_mode, CompressionMode::Off) {
            compressed_requests.push(batch_req.clone());
            continue;
        }

        let system_instruction = req_content.get("systemInstruction");
        let (messages, preserved_indices) =
            gemini_contents_to_messages(contents, system_instruction);
        let preserved_contents: HashMap<usize, Value> = preserved_indices
            .iter()
            .filter_map(|idx| contents.get(*idx).cloned().map(|v| (*idx, v)))
            .collect();

        if !contents.is_empty() && preserved_indices.len() == contents.len() {
            compressed_requests.push(batch_req.clone());
            continue;
        }

        let item_request_id = format!("google-batch:{idx}");
        let Some((compressed_messages, before, after)) = compress_messages(
            &messages,
            &model,
            state.config.compression_mode,
            auth_mode,
            &item_request_id,
        ) else {
            compressed_requests.push(batch_req.clone());
            continue;
        };

        total_before += before;
        total_after += after;

        let (optimized_contents, optimized_sys_inst) =
            messages_to_gemini_contents(&compressed_messages);
        let rebuilt_contents = rebuild_gemini_contents(
            contents,
            &preserved_indices,
            &preserved_contents,
            optimized_contents,
        );

        let mut compressed_req_content = req_content;
        if let Some(obj) = compressed_req_content.as_object_mut() {
            obj.insert("contents".to_string(), Value::Array(rebuilt_contents));
            if let Some(sys_inst) = optimized_sys_inst {
                obj.insert("systemInstruction".to_string(), sys_inst);
            }
        }

        let mut compressed_req = Map::new();
        compressed_req.insert("request".to_string(), compressed_req_content);
        if let Some(metadata) = batch_req.get("metadata") {
            compressed_req.insert("metadata".to_string(), metadata.clone());
        }
        compressed_requests.push(Value::Object(compressed_req));
    }

    if let Some(requests_slot) = parsed
        .pointer_mut("/batch/input_config/requests/requests")
        .and_then(Value::as_array_mut)
    {
        *requests_slot = compressed_requests;
    }

    if total_before > 0 {
        tracing::info!(
            event = "google_batch_compression",
            model = %model,
            tokens_before = total_before,
            tokens_after = total_after,
            tokens_saved = total_before.saturating_sub(total_after),
            "compressed Google batch inline requests"
        );
    }

    let outbound_body = match serialize_json_body(&parsed) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let upstream_headers = strip_headroom_headers(&headers);
    let url = upstream_url(
        &state,
        &format!("/v1beta/models/{model}:batchGenerateContent"),
    );
    match forward_to_upstream(&state, "POST", &url, upstream_headers, outbound_body).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("Failed to forward batch request: {e}"),
            "upstream_error",
        ),
    }
}

/// `POST /v1beta/models/{model}:batchGenerateContent` -- file-input passthrough.
pub async fn google_batch_passthrough(
    State(state): State<AppState>,
    Path(model): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let upstream_headers = strip_headroom_headers(&headers);
    let url = upstream_url(
        &state,
        &format!("/v1beta/models/{model}:batchGenerateContent"),
    );
    match forward_to_upstream(&state, "POST", &url, upstream_headers, body).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("Failed to forward batch request: {e}"),
            "upstream_error",
        ),
    }
}

/// Google batch results passthrough. CCR result processing stays on the Python side for now.
pub async fn google_batch_results(
    State(state): State<AppState>,
    Path(batch_name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let upstream_headers = strip_headroom_headers(&headers);
    let url = upstream_url(&state, &format!("/v1beta/{batch_name}"));
    match forward_to_upstream(&state, "GET", &url, upstream_headers, body).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("Failed to forward batch results request: {e}"),
            "upstream_error",
        ),
    }
}

// -- OpenAI batch --

/// `POST /v1/batches`
pub async fn openai_batch_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("Invalid request body: {e}"),
                "invalid_json",
            );
        }
    };

    let Some(input_file_id) = parsed.get("input_file_id").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "input_file_id is required",
            "missing_parameter",
        );
    };
    let Some(endpoint) = parsed.get("endpoint").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "endpoint is required",
            "missing_parameter",
        );
    };

    if endpoint != "/v1/chat/completions" {
        let upstream_headers = strip_headroom_headers(&headers);
        let url = upstream_url(&state, "/v1/batches");
        return match forward_to_upstream(&state, "POST", &url, upstream_headers, body).await {
            Ok(resp) => resp,
            Err(e) => error_response(
                StatusCode::BAD_GATEWAY,
                format!("Failed to forward batch request: {e}"),
                "upstream_error",
            ),
        };
    }

    let upstream_headers = strip_headroom_headers(&headers);
    let auth_mode = compression_auth_mode(&headers);

    let file_url = upstream_url(&state, &format!("/v1/files/{input_file_id}/content"));
    let mut download = state.client.get(&file_url);
    for (k, v) in &upstream_headers {
        if let Ok(hv) = HeaderValue::from_str(v) {
            download = download.header(k.as_str(), hv);
        }
    }
    let file_resp = match download.send().await {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("Failed to download file {input_file_id}: {e}"),
                "file_download_failed",
            );
        }
    };
    if !file_resp.status().is_success() {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("Failed to download file {input_file_id}"),
            "file_not_found",
        );
    }
    let file_content = match file_resp.text().await {
        Ok(content) => content,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("Failed to read file {input_file_id}: {e}"),
                "file_download_failed",
            );
        }
    };

    let (compressed_lines, stats) = compress_batch_jsonl_with_options(
        &file_content,
        state.config.compression_mode,
        auth_mode,
        "openai-batch",
    );
    if stats.total_requests == 0 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "No valid requests found in input file",
            "empty_file",
        );
    }

    let compressed_content = compressed_lines.join("\n");
    let filename = format!("compressed_{input_file_id}.jsonl");
    let upload_url = upstream_url(&state, "/v1/files");
    let (upload_body, boundary) = multipart_file_upload_body(compressed_content, &filename);

    let mut upload = state
        .client
        .post(&upload_url)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(upload_body);
    for (k, v) in &upstream_headers {
        if k.eq_ignore_ascii_case("content-type") {
            continue;
        }
        if let Ok(hv) = HeaderValue::from_str(v) {
            upload = upload.header(k.as_str(), hv);
        }
    }
    let upload_resp = match upload.send().await {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("Failed to upload compressed file: {e}"),
                "upload_failed",
            );
        }
    };
    if !upload_resp.status().is_success() {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "Failed to upload compressed file",
            "upload_failed",
        );
    }
    let upload_json: Value = match upload_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("Failed to parse upload response: {e}"),
                "upload_failed",
            );
        }
    };
    let Some(new_file_id) = upload_json.get("id").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "Upload response did not include file id",
            "upload_failed",
        );
    };

    let mut metadata = parsed
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert("headroom_compressed".to_string(), json!("true"));
    metadata.insert(
        "headroom_original_file_id".to_string(),
        json!(input_file_id),
    );
    metadata.insert(
        "headroom_total_requests".to_string(),
        json!(stats.total_requests.to_string()),
    );
    metadata.insert(
        "headroom_tokens_saved".to_string(),
        json!(stats.total_tokens_saved.to_string()),
    );
    metadata.insert(
        "headroom_original_tokens".to_string(),
        json!(stats.total_original_tokens.to_string()),
    );
    metadata.insert(
        "headroom_compressed_tokens".to_string(),
        json!(stats.total_compressed_tokens.to_string()),
    );
    metadata.insert(
        "headroom_savings_percent".to_string(),
        json!(format!("{:.1}", stats.savings_percent)),
    );

    let batch_body = json!({
        "input_file_id": new_file_id,
        "endpoint": endpoint,
        "completion_window": parsed
            .get("completion_window")
            .cloned()
            .unwrap_or_else(|| json!("24h")),
        "metadata": metadata,
    });
    let outbound_body = match serialize_json_body(&batch_body) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let mut create_headers = upstream_headers;
    create_headers.insert("content-type".to_string(), "application/json".to_string());
    let batch_url = upstream_url(&state, "/v1/batches");
    match forward_to_upstream(&state, "POST", &batch_url, create_headers, outbound_body).await {
        Ok(mut resp) => {
            if let Ok(value) = HeaderValue::from_str(&stats.total_tokens_saved.to_string()) {
                resp.headers_mut().insert("x-headroom-tokens-saved", value);
            }
            if let Ok(value) = HeaderValue::from_str(&format!("{:.1}", stats.savings_percent)) {
                resp.headers_mut()
                    .insert("x-headroom-savings-percent", value);
            }
            resp
        }
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("Failed to create batch: {e}"),
            "upstream_error",
        ),
    }
}

/// `GET /v1/batches` -- list.
pub async fn openai_batch_list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let upstream_headers = strip_headroom_headers(&headers);
    let url = upstream_url(&state, "/v1/batches");
    match forward_to_upstream(&state, "GET", &url, upstream_headers, Bytes::new()).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("Failed to forward batch list request: {e}"),
            "upstream_error",
        ),
    }
}

/// `GET /v1/batches/:batch_id` -- get.
pub async fn openai_batch_get(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let upstream_headers = strip_headroom_headers(&headers);
    let url = upstream_url(&state, &format!("/v1/batches/{batch_id}"));
    match forward_to_upstream(&state, "GET", &url, upstream_headers, Bytes::new()).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("Failed to forward batch get request: {e}"),
            "upstream_error",
        ),
    }
}

/// `POST /v1/batches/:batch_id/cancel` -- cancel.
pub async fn openai_batch_cancel(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let upstream_headers = strip_headroom_headers(&headers);
    let url = upstream_url(&state, &format!("/v1/batches/{batch_id}/cancel"));
    match forward_to_upstream(&state, "POST", &url, upstream_headers, Bytes::new()).await {
        Ok(resp) => resp,
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("Failed to forward batch cancel request: {e}"),
            "upstream_error",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_gemini_to_messages_basic() {
        let contents = vec![
            json!({"role": "user", "parts": [{"text": "hello"}]}),
            json!({"role": "model", "parts": [{"text": "world"}]}),
        ];
        let system = json!({"parts": [{"text": "sys"}]});

        let (messages, preserved) = gemini_contents_to_messages(&contents, Some(&system));

        assert!(preserved.is_empty());
        assert_eq!(
            messages,
            vec![
                json!({"role": "system", "content": "sys"}),
                json!({"role": "user", "content": "hello"}),
                json!({"role": "assistant", "content": "world"}),
            ]
        );
    }

    #[test]
    fn test_messages_to_gemini_roundtrip() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "world"}),
        ];

        let (contents, system) = messages_to_gemini_contents(&messages);

        assert_eq!(system, Some(json!({"parts": [{"text": "sys"}]})));
        assert_eq!(
            contents,
            vec![
                json!({"role": "user", "parts": [{"text": "hello"}]}),
                json!({"role": "model", "parts": [{"text": "world"}]}),
            ]
        );
    }

    #[test]
    fn test_compress_batch_jsonl_passthrough_when_no_messages() {
        let line = r#"{"custom_id":"1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4"}}"#;

        let (lines, stats) = compress_batch_jsonl(line);

        assert_eq!(lines, vec![line.to_string()]);
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.total_original_tokens, 0);
        assert_eq!(stats.total_compressed_tokens, 0);
        assert_eq!(stats.total_tokens_saved, 0);
    }

    #[test]
    fn test_compress_batch_jsonl_empty() {
        let (lines, stats) = compress_batch_jsonl("");

        assert!(lines.is_empty());
        assert_eq!(stats, BatchJsonlStats::default());
    }

}

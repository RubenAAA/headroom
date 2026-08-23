//! Anthropic Batch API handlers (`/v1/messages/batches*`).
//!
//! Port of the Anthropic batch endpoints in
//! `headroom/proxy/handlers/anthropic.py` (`handle_anthropic_batch_create`,
//! `handle_anthropic_batch_passthrough`, `handle_anthropic_batch_results`).
//!
//! Shares the helper conventions established by the Google/OpenAI batch
//! handlers in [`crate::handlers::batch`] (header stripping, upstream URL
//! building, upstream forwarding) but produces the Anthropic error
//! envelope (`{"type":"error","error":{...}}`) rather than the OpenAI-style
//! one.
//!
//! Routes mount behind the `enable_batch_api` config flag.
//!
//! # Divergences from Python (intentional)
//!
//! - No retry wrapper. Python routes create/results forwards through
//!   `_retry_request`; here we forward once via the shared batch helpers
//!   (same as the Rust Google/OpenAI batch handlers).
//! - Continuation rounds are counted **per result** (Python accumulates
//!   `_retrieval_count` across every result in the batch — a bug; each
//!   result's loop here starts at `rounds = 0`).
//! - No `RequestOutcome` recording (the outcome sink is not reachable from
//!   handler modules without an invasive refactor; the existing Rust
//!   Google/OpenAI batch handlers record none either).

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use headroom_core::auth_mode::AuthMode;
use headroom_core::ccr::response_handler::{
    CCRResponseHandler, CcrToolResult, ResponseHandlerConfig,
};
use headroom_core::ccr::tool_injection::create_ccr_tool_definition;
use headroom_core::ccr::{BatchContext, BatchRequestContext, BatchResultProcessor, CcrStore};
use serde_json::{json, Map, Value};

use crate::cache_stabilization::tool_def_normalize::sort_tools_deterministically;
use crate::compression::live_zone_anthropic::{compress_anthropic_request, Outcome};
use crate::config::CompressionMode;
use crate::error::ProxyError;
use crate::handlers::batch::{
    compression_auth_mode, forward_to_upstream, strip_headroom_headers, upstream_url,
};
use crate::proxy::AppState;

const CCR_TOOL_NAME: &str = "headroom_retrieve";

// ─── Response helpers ──────────────────────────────────────────────────

/// Build the Anthropic error envelope response.
fn anthropic_error_response(
    status: StatusCode,
    error_type: &str,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message.into(),
            }
        })),
    )
        .into_response()
}

/// Build a client-facing response from upstream parts, stripping
/// `content-encoding` and `content-length`.
///
/// reqwest auto-decompresses the body, so a surviving `content-encoding`
/// header would mislabel the (already-decoded) bytes. The shared
/// [`crate::handlers::batch`] response builder strips only
/// `content-length`; this path additionally strips `content-encoding`
/// to match Python. (The OpenAI/Google Rust batch handlers share the
/// same latent `content-encoding` gap; not touched here per scope.)
fn build_response(status: StatusCode, resp_headers: &HeaderMap, body: Bytes) -> Response {
    let mut builder = Response::builder().status(status);
    if let Some(h) = builder.headers_mut() {
        for (name, value) in resp_headers.iter() {
            if name == header::CONTENT_ENCODING || name == header::CONTENT_LENGTH {
                continue;
            }
            h.insert(name.clone(), value.clone());
        }
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// Forward to upstream and collect the full response (status, headers,
/// body). Used where we need to inspect the body (create → extract batch
/// id; results → parse JSONL).
async fn forward_collect(
    state: &AppState,
    method: reqwest::Method,
    url: &str,
    headers: HashMap<String, String>,
    body: Bytes,
) -> Result<(StatusCode, HeaderMap, Bytes), ProxyError> {
    let mut req = state.client.request(method, url);
    for (k, v) in &headers {
        if let Ok(hv) = axum::http::HeaderValue::from_str(v) {
            req = req.header(k.as_str(), hv);
        }
    }
    let resp = req.body(body).send().await.map_err(ProxyError::Upstream)?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = resp.headers().clone();
    let resp_body = resp.bytes().await.map_err(ProxyError::Upstream)?;
    Ok((status, resp_headers, resp_body))
}

// ─── Create ────────────────────────────────────────────────────────────

/// `POST /v1/messages/batches` — compress each request's messages, forward,
/// and store CCR batch context for later result post-processing.
pub async fn anthropic_batch_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Body-size guard (Content-Length vs configured max).
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        if len > state.config.max_body_bytes {
            return anthropic_error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                format!(
                    "Request body too large. Maximum size is {}MB",
                    state.config.max_body_bytes / (1024 * 1024)
                ),
            );
        }
    }

    let mut parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return anthropic_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("Invalid request body: {e}"),
            );
        }
    };

    let requests_list = match parsed.get("requests").and_then(Value::as_array) {
        Some(r) if !r.is_empty() => r.clone(),
        _ => {
            return anthropic_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Missing or empty 'requests' field in batch request",
            );
        }
    };

    let upstream_headers = strip_headroom_headers(&headers);
    let auth_mode = compression_auth_mode(&headers);
    let request_id = uuid::Uuid::new_v4().to_string();

    // Per-item compression. Any per-item failure isolates: the original
    // unmodified item is forwarded (mirrors Python's try/except append).
    let mut compressed_requests = Vec::with_capacity(requests_list.len());
    for (idx, batch_req) in requests_list.iter().enumerate() {
        compressed_requests.push(compress_batch_item(
            &state,
            batch_req,
            idx,
            auth_mode,
            &request_id,
        ));
    }
    if let Some(obj) = parsed.as_object_mut() {
        obj.insert("requests".to_string(), Value::Array(compressed_requests));
    }

    let outbound = match serde_json::to_vec(&parsed) {
        Ok(b) => Bytes::from(b),
        Err(_) => {
            return anthropic_error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "An error occurred while processing your batch request. Please try again.",
            );
        }
    };

    let mut fwd_headers = upstream_headers.clone();
    fwd_headers.insert("content-type".to_string(), "application/json".to_string());
    let url = upstream_url(&state, "/v1/messages/batches");

    match forward_collect(&state, reqwest::Method::POST, &url, fwd_headers, outbound).await {
        Ok((status, resp_headers, resp_body)) => {
            // Store batch context (with the ORIGINAL pre-compression
            // requests) for CCR result post-processing.
            if status == StatusCode::OK && state.config.ccr_inject_tool {
                if let Ok(rd) = serde_json::from_slice::<Value>(&resp_body) {
                    if let Some(batch_id) = rd.get("id").and_then(Value::as_str) {
                        store_batch_context(
                            &state,
                            batch_id,
                            &requests_list,
                            upstream_headers.get("x-api-key").cloned(),
                        );
                    }
                }
            }
            build_response(status, &resp_headers, resp_body)
        }
        Err(_) => anthropic_error_response(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "An error occurred while processing your batch request. Please try again.",
        ),
    }
}

/// Compress a single batch request item. Returns the (possibly rewritten)
/// `{custom_id, params}` object. System and tools pass through verbatim;
/// only messages are compressed. Tools are sorted deterministically and,
/// when compression saved tokens and `ccr_inject_tool` is on, the
/// retrieval tool is injected.
fn compress_batch_item(
    state: &AppState,
    batch_req: &Value,
    idx: usize,
    auth_mode: AuthMode,
    request_id: &str,
) -> Value {
    let custom_id = batch_req.get("custom_id").cloned().unwrap_or(json!(""));
    let params = batch_req.get("params").cloned().unwrap_or(json!({}));

    // Per-item isolation: a non-object params (malformed item) is
    // forwarded unchanged.
    let Some(params_obj) = params.as_object() else {
        return batch_req.clone();
    };

    let mode = state.config.compression_mode;
    let messages = params_obj.get("messages").and_then(Value::as_array);
    let has_messages = messages.map(|m| !m.is_empty()).unwrap_or(false);

    if !has_messages || matches!(mode, CompressionMode::Off) {
        // No messages or optimization disabled → pass through unchanged.
        return json!({ "custom_id": custom_id, "params": params });
    }
    let messages = messages.expect("has_messages implies Some");

    let model = params_obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let system = params_obj.get("system");
    let original_tools = params_obj.get("tools").and_then(Value::as_array).cloned();

    let item_request_id = format!("{request_id}:item:{idx}");
    let batch_ccr_store = state.ccr_store();
    let (optimized_messages, tokens_saved) = match compress_item_messages(
        messages,
        model,
        system,
        original_tools.as_deref(),
        mode,
        state.config.cache_control_auto_frozen,
        auth_mode,
        &item_request_id,
        &state.config.exclude_tools,
        // The batch results path already resolves headroom_retrieve calls
        // against this store, so a marker here is actionable.
        batch_ccr_store.as_deref(),
    ) {
        Some((msgs, before, after)) => (msgs, before.saturating_sub(after)),
        None => (messages.to_vec(), 0),
    };

    // Build compressed params from the ORIGINAL params, overriding
    // messages and (conditionally) tools — mirrors Python's
    // `{**params, "messages": ...}` + conditional tools override.
    let mut new_params = params_obj.clone();
    new_params.insert("messages".to_string(), Value::Array(optimized_messages));

    let mut final_tools = original_tools.clone().unwrap_or_default();
    sort_tools_deterministically(&mut final_tools);
    if state.config.ccr_inject_tool && tokens_saved > 0 {
        let already_has = final_tools
            .iter()
            .any(|t| t.get("name").and_then(Value::as_str) == Some(CCR_TOOL_NAME));
        if !already_has {
            final_tools.push(create_ccr_tool_definition("anthropic"));
        }
    }
    sort_tools_deterministically(&mut final_tools);

    let tools_changed = match &original_tools {
        Some(orig) => &final_tools != orig,
        None => !final_tools.is_empty(),
    };
    if tools_changed {
        new_params.insert("tools".to_string(), Value::Array(final_tools));
    }

    json!({ "custom_id": custom_id, "params": Value::Object(new_params) })
}

/// Compress the messages of a single item via the Anthropic live-zone
/// compressor. Builds a synthetic per-item request body, runs the
/// compressor, and extracts only the rewritten messages (system/tools are
/// discarded — they pass through verbatim in the caller).
///
/// Returns `(messages, tokens_before, tokens_after)`. On token inflation
/// the original messages are returned with `after == before` (saved = 0).
#[allow(clippy::too_many_arguments)]
fn compress_item_messages(
    messages: &[Value],
    model: &str,
    system: Option<&Value>,
    tools: Option<&[Value]>,
    mode: CompressionMode,
    cache_control_policy: crate::config::CacheControlAutoFrozen,
    auth_mode: AuthMode,
    request_id: &str,
    exclude_tools: &[String],
    ccr_store: Option<&dyn CcrStore>,
) -> Option<(Vec<Value>, usize, usize)> {
    let mut synth = Map::new();
    synth.insert("model".to_string(), json!(model));
    synth.insert("messages".to_string(), Value::Array(messages.to_vec()));
    if let Some(s) = system {
        synth.insert("system".to_string(), s.clone());
    }
    if let Some(t) = tools {
        synth.insert("tools".to_string(), Value::Array(t.to_vec()));
    }
    let bytes = Bytes::from(serde_json::to_vec(&Value::Object(synth)).ok()?);

    match compress_anthropic_request(
        &bytes,
        mode,
        cache_control_policy,
        auth_mode,
        request_id,
        exclude_tools,
        ccr_store,
    ) {
        Outcome::Compressed {
            body,
            tokens_before,
            tokens_after,
            ..
        } if tokens_after <= tokens_before => {
            let parsed: Value = serde_json::from_slice(&body).ok()?;
            let msgs = parsed
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| messages.to_vec());
            Some((msgs, tokens_before, tokens_after))
        }
        // Token inflation: revert to original messages, saved = 0.
        Outcome::Compressed { tokens_before, .. } => {
            Some((messages.to_vec(), tokens_before, tokens_before))
        }
        // NoCompression / Passthrough → item unchanged.
        _ => None,
    }
}

/// Store the CCR batch context keyed by `batch_id`, using the ORIGINAL
/// (pre-compression) request list.
fn store_batch_context(
    state: &AppState,
    batch_id: &str,
    requests_list: &[Value],
    api_key: Option<String>,
) {
    let ttl = Duration::from_secs(crate::proxy::BATCH_CONTEXT_TTL_SECS);
    let mut ctx = BatchContext::new(batch_id.to_string(), "anthropic".to_string(), ttl);
    ctx.api_key = api_key;
    ctx.api_base_url = Some(state.config.upstream.to_string());

    for batch_req in requests_list {
        let custom_id = batch_req
            .get("custom_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = batch_req.get("params");
        let messages = params
            .and_then(|p| p.get("messages"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tools = params
            .and_then(|p| p.get("tools"))
            .and_then(Value::as_array)
            .cloned();
        let model = params
            .and_then(|p| p.get("model"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let mut extras = HashMap::new();
        extras.insert(
            "max_tokens".to_string(),
            params
                .and_then(|p| p.get("max_tokens"))
                .cloned()
                .unwrap_or(json!(4096)),
        );
        if let Some(sys) = params.and_then(|p| p.get("system")) {
            extras.insert("system".to_string(), sys.clone());
        }

        ctx.add_request(BatchRequestContext {
            custom_id,
            messages,
            tools,
            model,
            system_instruction: None,
            extras,
        });
    }

    state.batch_context_store.store(ctx);
}

// ─── Passthrough (list / get / cancel) ─────────────────────────────────

/// Forward method + path + query verbatim to upstream, no compression.
async fn passthrough(
    state: &AppState,
    method: &str,
    path: String,
    query: Option<String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let upstream_headers = strip_headroom_headers(headers);
    let mut url = upstream_url(state, &path);
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url = format!("{url}?{q}");
    }
    match forward_to_upstream(state, method, &url, upstream_headers, body).await {
        Ok(mut resp) => {
            resp.headers_mut().remove(header::CONTENT_ENCODING);
            resp.headers_mut().remove(header::CONTENT_LENGTH);
            resp
        }
        Err(_) => anthropic_error_response(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "Failed to forward batch request.",
        ),
    }
}

/// `GET /v1/messages/batches` — list.
pub async fn anthropic_batch_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    passthrough(
        &state,
        "GET",
        "/v1/messages/batches".to_string(),
        query,
        &headers,
        Bytes::new(),
    )
    .await
}

/// `GET /v1/messages/batches/:batch_id` — get.
pub async fn anthropic_batch_get(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    passthrough(
        &state,
        "GET",
        format!("/v1/messages/batches/{batch_id}"),
        query,
        &headers,
        Bytes::new(),
    )
    .await
}

/// `POST /v1/messages/batches/:batch_id/cancel` — cancel.
pub async fn anthropic_batch_cancel(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    passthrough(
        &state,
        "POST",
        format!("/v1/messages/batches/{batch_id}/cancel"),
        query,
        &headers,
        body,
    )
    .await
}

// ─── Results (with CCR post-processing) ────────────────────────────────

/// `GET /v1/messages/batches/:batch_id/results` — fetch results and run
/// CCR post-processing (retrieval + continuation) when a batch context
/// exists and `ccr_inject_tool` is on.
pub async fn anthropic_batch_results(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let upstream_headers = strip_headroom_headers(&headers);
    let mut url = upstream_url(&state, &format!("/v1/messages/batches/{batch_id}/results"));
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url = format!("{url}?{q}");
    }

    let (status, resp_headers, resp_body) = match forward_collect(
        &state,
        reqwest::Method::GET,
        &url,
        upstream_headers,
        Bytes::new(),
    )
    .await
    {
        Ok(t) => t,
        Err(_) => {
            return anthropic_error_response(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Failed to fetch batch results.",
            );
        }
    };

    // Non-200 → passthrough verbatim.
    if status != StatusCode::OK {
        return build_response(status, &resp_headers, resp_body);
    }

    // Parse JSONL, skipping unparseable lines.
    let raw = String::from_utf8_lossy(&resp_body);
    let mut results: Vec<Value> = Vec::new();
    for line in raw.trim().split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            results.push(v);
        }
    }
    if results.is_empty() {
        return build_response(status, &resp_headers, resp_body);
    }

    // Need a stored context + CCR enabled to post-process.
    let batch_context = match state.batch_context_store.get(&batch_id) {
        Some(c) if state.config.ccr_inject_tool => c,
        _ => return build_response(status, &resp_headers, resp_body),
    };

    let ccr_store = state.ctx_offload.as_ref().map(|r| r.store.ccr());
    let processed =
        process_batch_results_ccr(&state, results, &batch_context, ccr_store.as_deref()).await;

    // Re-serialize JSONL (no trailing newline); fresh response, upstream
    // headers dropped, media type application/jsonl.
    let lines: Vec<String> = processed
        .iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .collect();
    let content = lines.join("\n");

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/jsonl")],
        content,
    )
        .into_response()
}

/// Run CCR post-processing across all batch results.
async fn process_batch_results_ccr(
    state: &AppState,
    results: Vec<Value>,
    batch_context: &BatchContext,
    ccr_store: Option<&dyn CcrStore>,
) -> Vec<Value> {
    let handler = CCRResponseHandler::new(Some(ResponseHandlerConfig {
        enabled: true,
        max_retrieval_rounds: state.config.ccr_max_retrieval_rounds,
        strip_ccr_from_response: false,
    }));
    let max_rounds = state.config.ccr_max_retrieval_rounds;

    let mut out = Vec::with_capacity(results.len());
    for result in results {
        let custom_id = BatchResultProcessor::get_custom_id(&result, "anthropic");
        let Some(request_context) = batch_context.get_request(&custom_id) else {
            out.push(result);
            continue;
        };
        let Some(response) = BatchResultProcessor::extract_response(&result, "anthropic") else {
            out.push(result);
            continue;
        };
        if !handler.has_ccr_tool_calls(&response, "anthropic") {
            out.push(result);
            continue;
        }

        let final_response = run_anthropic_continuation(
            state,
            &handler,
            response,
            request_context,
            batch_context,
            ccr_store,
            max_rounds,
        )
        .await;
        out.push(BatchResultProcessor::update_result(
            &result,
            &final_response,
            "anthropic",
        ));
    }
    out
}

/// Drive the CCR retrieval + continuation loop for one result. Rounds are
/// counted per-result (see module docs for the Python deviation).
#[allow(clippy::too_many_arguments)]
async fn run_anthropic_continuation(
    state: &AppState,
    handler: &CCRResponseHandler,
    response: Value,
    request_context: &BatchRequestContext,
    batch_context: &BatchContext,
    ccr_store: Option<&dyn CcrStore>,
    max_rounds: usize,
) -> Value {
    let mut current_response = response;
    let mut current_messages = request_context.messages.clone();
    let max_tokens = request_context
        .extras
        .get("max_tokens")
        .cloned()
        .unwrap_or(json!(4096));
    let url = upstream_url(state, "/v1/messages");

    let mut rounds = 0usize;
    loop {
        if rounds >= max_rounds {
            break;
        }
        let (ccr_calls, other_calls) = handler.parse_ccr_tool_calls(&current_response, "anthropic");
        if ccr_calls.is_empty() {
            break;
        }
        // Mixed CCR + real tool calls: cannot fabricate results for the
        // real tools — stop and return the response as-is.
        if !other_calls.is_empty() {
            break;
        }

        let mut results_vec = Vec::with_capacity(ccr_calls.len());
        for call in &ccr_calls {
            let tool_result = match ccr_store.and_then(|s| s.get(&call.hash_key)) {
                Some(content) => CcrToolResult {
                    tool_call_id: call.tool_call_id.clone(),
                    content,
                    success: true,
                    items_retrieved: 1,
                },
                None => CcrToolResult {
                    tool_call_id: call.tool_call_id.clone(),
                    content: format!(
                        "Error: CCR content not found for hash '{}'. The compressed data may have been evicted.",
                        call.hash_key
                    ),
                    success: false,
                    items_retrieved: 0,
                },
            };
            results_vec.push(tool_result);
        }

        let assistant_msg = handler.extract_assistant_message(&current_response, "anthropic");
        let tool_result_msg = handler.create_tool_result_message(&results_vec, "anthropic");
        current_messages.push(assistant_msg);
        current_messages.push(tool_result_msg);

        let mut body = Map::new();
        body.insert("model".to_string(), json!(request_context.model));
        body.insert(
            "messages".to_string(),
            Value::Array(current_messages.clone()),
        );
        body.insert("max_tokens".to_string(), max_tokens.clone());
        if let Some(tools) = &request_context.tools {
            if !tools.is_empty() {
                let mut sorted = tools.clone();
                sort_tools_deterministically(&mut sorted);
                body.insert("tools".to_string(), Value::Array(sorted));
            }
        }
        let continuation_body = match serde_json::to_vec(&Value::Object(body)) {
            Ok(b) => b,
            Err(_) => break,
        };

        let mut req = state
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(continuation_body);
        if let Some(key) = &batch_context.api_key {
            req = req.header("x-api-key", key);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) => break,
        };
        if !resp.status().is_success() {
            break;
        }
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(_) => break,
        };
        current_response = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => break,
        };

        rounds += 1;
    }

    current_response
}

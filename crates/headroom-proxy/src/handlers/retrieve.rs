//! POST /v1/retrieve + POST /v1/retrieve/tool_call: the "Retrieve" half of
//! CCR (Compress-Cache-Retrieve).
//!
//! When compression offloads a block, the original bytes land in the CCR
//! store keyed by the hash that ends up in the prompt. These endpoints hand
//! the originals back: by hash directly, or by resolving a
//! `headroom_retrieve` tool call in Anthropic/OpenAI/Gemini/provider wire
//! format into a ready-to-splice tool result.
//!
//! Both are loopback-only (invisible 404 otherwise, mirroring `/debug/*`),
//! and both require same-origin when browser provenance headers are present.
//! Port of upstream `ccr_retrieve` + `ccr_handle_tool_call`, including the
//! same-origin check #3955 added to `tool_call` after #3060 covered the
//! other write routes.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use headroom_core::ccr::tool_injection::{CCR_TOOL_NAME, parse_tool_call};
use headroom_core::ccr::{CcrEntryState, CcrEntryStatus, format_retrieval_miss_detail};

use crate::forwarded_headers::has_same_origin_or_no_provenance;
use crate::loopback_guard::{is_loopback_host, is_loopback_host_header};
use crate::proxy::AppState;

/// Shared gate for both retrieve endpoints: loopback callers only, and
/// same-origin when `Origin`/`Referer` provenance is present. Returns the
/// rejection response, or `None` when the caller may proceed. Mirrors
/// `handle_stats_reset`'s gate exactly.
fn gate(addr: SocketAddr, headers: &HeaderMap) -> Option<Response> {
    let peer = addr.ip().to_string();
    let host = headers.get("host").and_then(|v| v.to_str().ok());
    if !is_loopback_host(Some(peer.as_str())) || !is_loopback_host_header(host) {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if !has_same_origin_or_no_provenance(
        host.unwrap_or_default(),
        header_str("origin"),
        header_str("referer"),
        "http",
    ) {
        return Some(StatusCode::FORBIDDEN.into_response());
    }
    None
}

fn err(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, axum::Json(json!({ "detail": detail.into() }))).into_response()
}

/// Parse the request body as JSON. Done by hand after [`gate`] rather than
/// by the `Json` extractor, which runs first and would answer a remote
/// caller's bad body with 400/415 instead of the invisible 404.
fn json_body(body: &[u8]) -> Result<Value, Box<Response>> {
    serde_json::from_slice(body)
        .map_err(|_| Box::new(err(StatusCode::BAD_REQUEST, "invalid JSON body")))
}

// Boxed error: a bare `Response` Err variant trips clippy::result_large_err.
fn ccr_store(state: &AppState) -> Result<Arc<dyn headroom_core::ccr::CcrStore>, Box<Response>> {
    match state.ccr_store() {
        Some(store) => Ok(store),
        None => Err(Box::new(err(
            StatusCode::NOT_FOUND,
            "Entry not found (CCR is not enabled on this proxy)",
        ))),
    }
}

/// POST /v1/retrieve — original content for a compression hash.
///
/// Request: `{"hash": "<hex>"}`. Hit returns the stored payload; a missing
/// or expired hash is a 404 whose detail names the TTL and, for expired
/// entries, the age — the same strings statuslines and agents already
/// parse from upstream.
///
/// Narrowing vs upstream: the Rust store keeps payload bytes only (no
/// per-entry token counts, item counts, tool name, or retrieval counter),
/// so the hit body carries `hash` + `original_content` and not the
/// metadata fields upstream returns.
pub async fn handle_retrieve(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(rejection) = gate(addr, &headers) {
        return rejection;
    }
    let body = match json_body(&body) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let store = match ccr_store(&state) {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let Some(hash_key) = body
        .get("hash")
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
    else {
        return err(StatusCode::BAD_REQUEST, "hash required");
    };
    let status = store.entry_status(hash_key);
    if !matches!(status.state, CcrEntryState::Available) {
        return err(StatusCode::NOT_FOUND, format_retrieval_miss_detail(&status));
    }
    match store.get(hash_key) {
        Some(original_content) => axum::Json(json!({
            "status": "retrieved",
            "hash": hash_key,
            "original_content": original_content,
        }))
        .into_response(),
        None => err(
            StatusCode::NOT_FOUND,
            format_retrieval_miss_detail(&store.entry_status(hash_key)),
        ),
    }
}

/// POST /v1/retrieve/tool_call — resolve a retrieval tool call.
///
/// Request: `{"provider": "anthropic", "tool_call": {...}}`, `provider`
/// defaulting to `anthropic` as upstream does. It selects the wire shape
/// (`anthropic` / `openai` / anything else). A call that is not a
/// well-formed `headroom_retrieve` invocation is a 400. A miss is still a
/// 200: `success: false` and `data` carrying the TTL detail as `error`, so
/// the envelope splices into the conversation either way (upstream
/// `ccr_handle_tool_call`). Hit `data` is `hash` + `original_content`,
/// narrowed like `/v1/retrieve` (no per-entry counts in the Rust store).
pub async fn handle_retrieve_tool_call(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(rejection) = gate(addr, &headers) {
        return rejection;
    }
    let body = match json_body(&body) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let store = match ccr_store(&state) {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("anthropic");
    let tool_call = body.get("tool_call").unwrap_or(&Value::Null);
    let Some(hash_key) = parse_tool_call(tool_call, provider) else {
        return err(
            StatusCode::BAD_REQUEST,
            format!("Invalid tool call or not a {CCR_TOOL_NAME} call"),
        );
    };
    let miss = |status: CcrEntryStatus| {
        json!({
            "error": format_retrieval_miss_detail(&status),
            "hash": hash_key,
            "status": match status.state {
                CcrEntryState::Available => "available",
                CcrEntryState::Expired => "expired",
                CcrEntryState::Missing => "missing",
            },
            "ttl_seconds": match status.state {
                CcrEntryState::Missing => status.default_ttl_seconds,
                _ => status.ttl_seconds,
            },
        })
    };
    let status = store.entry_status(&hash_key);
    let retrieval_data = if !matches!(status.state, CcrEntryState::Available) {
        miss(status)
    } else {
        match store.get(&hash_key) {
            Some(content) => json!({ "hash": hash_key, "original_content": content }),
            None => miss(store.entry_status(&hash_key)),
        }
    };

    let tool_call_id = tool_call.get("id").and_then(Value::as_str).unwrap_or("");
    // `json.dumps(..., indent=2)`, as upstream.
    let result_content = serde_json::to_string_pretty(&retrieval_data).unwrap_or_default();
    let tool_result = if provider == "anthropic" {
        json!({
            "type": "tool_result",
            "tool_use_id": tool_call_id,
            "content": result_content,
        })
    } else if provider == "openai" {
        json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": result_content,
        })
    } else {
        json!({
            "tool_call_id": tool_call_id,
            "content": result_content,
        })
    };
    axum::Json(json!({
        "tool_result": tool_result,
        "success": retrieval_data.get("error").is_none(),
        "data": retrieval_data,
    }))
    .into_response()
}

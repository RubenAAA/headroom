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

use headroom_core::ccr::tool_injection::parse_tool_call;
use headroom_core::ccr::{CcrEntryState, format_retrieval_miss_detail};

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
    body: axum::Json<Value>,
) -> Response {
    if let Some(rejection) = gate(addr, &headers) {
        return rejection;
    }
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
/// Request: `{"provider": "anthropic", "tool_call": {...}}`. The provider
/// selects the wire shape (`anthropic` / `openai` / `gemini` / anything
/// else); a call that is not a well-formed `headroom_retrieve` invocation
/// is a 404, and a hash that misses is a 404 with the same TTL detail as
/// `/v1/retrieve`. The returned `tool_result` envelope matches the
/// provider shape so it splices straight into the conversation.
pub async fn handle_retrieve_tool_call(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::Json<Value>,
) -> Response {
    if let Some(rejection) = gate(addr, &headers) {
        return rejection;
    }
    let store = match ccr_store(&state) {
        Ok(store) => store,
        Err(response) => return *response,
    };
    let provider = body.get("provider").and_then(Value::as_str).unwrap_or("");
    let tool_call = body.get("tool_call");
    if provider.is_empty() || tool_call.is_none() {
        return err(StatusCode::BAD_REQUEST, "provider and tool_call required");
    }
    let tool_call = tool_call.unwrap_or(&Value::Null);
    let tool_call_id = tool_call.get("id").and_then(Value::as_str).unwrap_or("");
    let Some(hash_key) = parse_tool_call(tool_call, provider) else {
        return err(StatusCode::NOT_FOUND, "not a CCR tool call");
    };
    let status = store.entry_status(&hash_key);
    if !matches!(status.state, CcrEntryState::Available) {
        return err(StatusCode::NOT_FOUND, format_retrieval_miss_detail(&status));
    }
    let Some(content) = store.get(&hash_key) else {
        return err(
            StatusCode::NOT_FOUND,
            format_retrieval_miss_detail(&store.entry_status(&hash_key)),
        );
    };

    let retrieval_data = if provider == "anthropic" || provider == "openai" {
        json!({ "status": "retrieved", "content": content })
    } else {
        json!({
            "status": "retrieved",
            "response": {
                "TextResponse": {
                    "parts": [{ "text": content }],
                },
            },
        })
    };
    // `json.dumps(..., indent=2)`: borrowed content owned for the envelope.
    let result_content =
        serde_json::to_string_pretty(&retrieval_data).unwrap_or_else(|_| content.clone());
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
        "success": true,
        "data": retrieval_data,
    }))
    .into_response()
}

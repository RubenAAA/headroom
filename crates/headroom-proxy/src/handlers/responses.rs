//! POST `/v1/responses` handler — Phase C PR-C3 + PR-C4.
//!
//! # Why an explicit handler?
//!
//! The Python proxy currently flattens Responses-shape items into
//! Chat-Completions-shape via
//! `headroom/proxy/responses_converter.py` — a fragile shim that
//! silently breaks every time OpenAI lands a new item type. C3 ports
//! this path to Rust with first-class per-item-type handling.
//!
//! The handler buffers the request body (so the live-zone dispatcher
//! can inspect it) and re-injects it into [`crate::proxy::forward_http`].
//! `forward_http`'s compression gate dispatches on the path
//! classification (`CompressibleEndpoint::OpenAiResponses`) added by
//! C3.
//!
//! # Streaming (PR-C4)
//!
//! When the request carries `Accept: text/event-stream`, the response
//! tee in [`crate::proxy::forward_http`] flips on the
//! [`crate::sse::openai_responses::ResponseState`] state machine
//! (PR-C1) and frames bytes through [`crate::sse::framing::SseFramer`]
//! — never via naive `\n\n` splits. Decoded events update telemetry
//! in a spawned task that can never block the byte path.
//!
//! Per-item-type request-side compression (PR-C3) runs **regardless**
//! of `Accept`: a streaming `/v1/responses` request gets the same
//! request-body compression as a non-streaming one. C4 closes the
//! loop by confirming the full pipeline is active (no more
//! `responses_streaming_passthrough_until_c4` fallback). The
//! pipeline gate is `Config::enable_responses_streaming` (default
//! `true`) — toggle off only as an emergency rollback.
//!
//! Compression of streaming **response** events is NOT performed.
//! Output items are rendered live token-by-token; mid-stream
//! rewriting would corrupt the user-visible UX and is not part of
//! the live-zone-only contract (the live zone is **request**-side).
//!
//! # Per-item-type behaviour
//!
//! See [`crate::responses_items`] for the typed enum. Briefly:
//!
//! - `function_call_output` / `local_shell_call_output` /
//!   `apply_patch_call_output` — output strings are eligible for
//!   live-zone compression when the latest of each kind, above the
//!   2 KiB output-item floor.
//! - `message` (user role) — text content is eligible.
//! - `reasoning.encrypted_content`, `compaction.*`, MCP / computer /
//!   web-search / file-search / code-interpreter / image-generation /
//!   tool-search / custom-tool calls — passthrough byte-equal.
//! - `function_call.arguments` is a STRING the model emitted; never
//!   parsed by the proxy.
//! - `local_shell_call.action.command` is an argv array; never
//!   joined into a string.
//! - `apply_patch_call.operation.diff` is a V4A diff payload; never
//!   re-serialized.
//! - Unknown `type` values log
//!   `event = responses_unknown_item_type` at warn level and pass
//!   through verbatim.

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, Uri};
use axum::response::Response;
use bytes::Bytes;
use std::net::SocketAddr;

use crate::observability;
use crate::proxy::{forward_http, AppState};

const CODEX_ADDITIONAL_TOOLS_LIFT_ENV: &str = "HEADROOM_CODEX_ADDITIONAL_TOOLS_LIFT";

#[derive(Debug, Clone)]
struct AdditionalToolsCarrier {
    kept_index: usize,
    item: serde_json::Map<String, serde_json::Value>,
    tools: Vec<serde_json::Value>,
}

/// Enough information to put Codex's transcript item back after the internal
/// tools consumers have run. The carrier index is relative to the items that
/// survived the lift, so it remains valid when compression rewrites input.
#[derive(Debug, Clone)]
pub(crate) struct AdditionalToolsRestorePlan {
    carriers: Vec<AdditionalToolsCarrier>,
}

fn env_value_enabled(value: Option<&str>) -> bool {
    !matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

fn codex_additional_tools_lift_enabled() -> bool {
    env_value_enabled(
        std::env::var(CODEX_ADDITIONAL_TOOLS_LIFT_ENV)
            .ok()
            .as_deref(),
    )
}

fn value_is_truthy(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::String(value)) => !value.is_empty(),
        Some(serde_json::Value::Array(value)) => !value.is_empty(),
        Some(serde_json::Value::Object(value)) => !value.is_empty(),
        Some(serde_json::Value::Number(_)) => true,
    }
}

/// Lift Codex `additional_tools` transcript items into top-level `tools` for
/// Headroom's internal normalizers, shapers, and accounting. This is an
/// internal representation only; callers must restore the returned plan
/// before forwarding.
pub(crate) fn lift_codex_additional_tools(
    payload: &mut serde_json::Value,
    request_id: &str,
) -> Option<AdditionalToolsRestorePlan> {
    let object = payload.as_object_mut()?;
    if value_is_truthy(object.get("tools")) {
        return None;
    }
    let items = object.get("input")?.as_array()?;
    // Read the opt-out only after finding a carrier. This keeps an environment
    // lookup off the overwhelmingly common request shape.
    if !items.iter().any(|item| {
        item.get("type").and_then(serde_json::Value::as_str) == Some("additional_tools")
    }) || !codex_additional_tools_lift_enabled()
    {
        return None;
    }

    let mut lifted = Vec::new();
    let mut kept = Vec::with_capacity(items.len());
    let mut carriers = Vec::new();
    for item in items {
        let carrier_tools = item
            .as_object()
            .filter(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("additional_tools")
            })
            .and_then(|item| item.get("tools"))
            .and_then(serde_json::Value::as_array)
            .filter(|tools| !tools.is_empty());
        if let Some(tools) = carrier_tools {
            let mut metadata = item.as_object().expect("carrier checked above").clone();
            metadata.remove("tools");
            carriers.push(AdditionalToolsCarrier {
                kept_index: kept.len(),
                item: metadata,
                tools: tools.clone(),
            });
            lifted.extend(tools.iter().cloned());
        } else {
            kept.push(item.clone());
        }
    }
    if lifted.is_empty() {
        return None;
    }

    let count = lifted.len();
    object.insert("tools".to_string(), serde_json::Value::Array(lifted));
    object.insert("input".to_string(), serde_json::Value::Array(kept));
    tracing::info!(
        event = "codex_additional_tools_lifted",
        %request_id,
        tool_count = count,
        "lifted Codex additional_tools for internal request processing"
    );
    Some(AdditionalToolsRestorePlan { carriers })
}

/// Restore a lifted payload to the transcript shape Codex sent. Idempotent:
/// if a carrier already owns tools, a second restore is a no-op.
pub(crate) fn restore_codex_additional_tools(
    payload: &mut serde_json::Value,
    plan: &AdditionalToolsRestorePlan,
) -> Result<usize, &'static str> {
    let object = payload.as_object_mut().ok_or("payload is not an object")?;
    let items = object
        .get("input")
        .and_then(serde_json::Value::as_array)
        .ok_or("payload input is not an array")?;
    if items.iter().any(|item| {
        item.get("type").and_then(serde_json::Value::as_str) == Some("additional_tools")
            && value_is_truthy(item.get("tools"))
    }) {
        return Ok(0);
    }

    let tools = object
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let original_total: usize = plan.carriers.iter().map(|entry| entry.tools.len()).sum();
    let slices: Vec<Vec<serde_json::Value>> = if tools.is_empty() {
        // A consumer emptied the array. Restoring the original definitions is
        // safer than forwarding a stateful session with no tool transcript.
        plan.carriers
            .iter()
            .map(|entry| entry.tools.clone())
            .collect()
    } else if tools.len() == original_total {
        let mut offset = 0;
        plan.carriers
            .iter()
            .map(|entry| {
                let end = offset + entry.tools.len();
                let slice = tools[offset..end].to_vec();
                offset = end;
                slice
            })
            .collect()
    } else {
        // Deferral or injection changed the count, so the original carrier
        // boundaries are stale. Put the whole set in the first carrier.
        let mut changed = vec![Vec::new(); plan.carriers.len()];
        if let Some(first) = changed.first_mut() {
            *first = tools;
        }
        changed
    };

    let mut restored_items = items.clone();
    let mut restored = 0;
    let mut shift = 0;
    for (entry, tool_slice) in plan.carriers.iter().zip(slices) {
        if tool_slice.is_empty() {
            continue;
        }
        let mut carrier = entry.item.clone();
        carrier.insert(
            "type".to_string(),
            serde_json::Value::String("additional_tools".to_string()),
        );
        restored += tool_slice.len();
        carrier.insert("tools".to_string(), serde_json::Value::Array(tool_slice));
        let position = (entry.kept_index + shift).min(restored_items.len());
        restored_items.insert(position, serde_json::Value::Object(carrier));
        shift += 1;
    }
    if restored == 0 {
        return Err("restore plan produced no tool definitions");
    }
    object.insert(
        "input".to_string(),
        serde_json::Value::Array(restored_items),
    );
    object.remove("tools");
    Ok(restored)
}

pub(crate) fn lift_codex_additional_tools_body(
    body: Bytes,
    request_id: &str,
) -> (Bytes, Option<AdditionalToolsRestorePlan>) {
    let Ok(mut payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (body, None);
    };
    let Some(plan) = lift_codex_additional_tools(&mut payload, request_id) else {
        return (body, None);
    };
    match serde_json::to_vec(&payload) {
        Ok(serialized) => (Bytes::from(serialized), Some(plan)),
        Err(error) => {
            tracing::warn!(
                event = "codex_additional_tools_lift_failed",
                %request_id,
                %error,
                "failed to serialize lifted Codex request; forwarding original shape"
            );
            (body, None)
        }
    }
}

pub(crate) fn restore_codex_additional_tools_body(
    body: Bytes,
    plan: Option<&AdditionalToolsRestorePlan>,
    request_id: &str,
) -> Bytes {
    let Some(plan) = plan else {
        return body;
    };
    let restored = serde_json::from_slice::<serde_json::Value>(&body)
        .map_err(|_| "forwarded payload is not JSON")
        .and_then(|mut payload| {
            restore_codex_additional_tools(&mut payload, plan)?;
            serde_json::to_vec(&payload)
                .map(Bytes::from)
                .map_err(|_| "restored payload could not be serialized")
        });
    match restored {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(
                event = "codex_additional_tools_restore_failed",
                %request_id,
                reason = error,
                "failed to restore Codex additional_tools; forwarding internally lifted shape"
            );
            body
        }
    }
}

/// Axum POST handler for `/v1/responses`. Buffers the body, stitches
/// a fresh `Request<Body>` together, and forwards via
/// [`forward_http`]. Compression dispatch + SSE telemetry is handled
/// inside `forward_http`'s shared gate (PR-C1 + PR-C2 + PR-C3).
pub async fn handle_responses(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Rate-limit gate: check before buffering the body.
    if let Some(rejected) = super::chat_completions::check_rate_limit(&state, &headers) {
        return rejected;
    }

    // PR-C4: streaming pipeline confirmation. When the client asks
    // for SSE, log a structured breadcrumb so dashboards can confirm
    // the streaming pipeline is engaged (the SSE framer +
    // ResponseState machine in `forward_http`'s tee). The
    // `enable_responses_streaming` switch is honoured here — when
    // disabled, we still forward but emit a distinct event so the
    // operator sees the rollback take effect.
    //
    // Why log INFO (not WARN)? PR-C3 used WARN as a "this path is
    // half-built" signal. PR-C4 wires the streaming state machine
    // through, so the previous WARN is no longer accurate.
    if accepts_sse(&headers) {
        if state.config.enable_responses_streaming {
            tracing::info!(
                event = "responses_streaming_pipeline_active",
                method = %method,
                path = %uri.path(),
                framer = "byte_level_sse",
                state_machine = "openai_responses",
                "responses streaming pipeline engaged: SSE framer + ResponseState telemetry tee"
            );
        } else {
            tracing::warn!(
                event = "responses_streaming_pipeline_disabled",
                method = %method,
                path = %uri.path(),
                "responses streaming pipeline disabled by --enable-responses-streaming=false; \
                 SSE bytes will pass through opaquely (emergency rollback path)"
            );
        }
    }

    // Phase G PR-G3: extract the request-side `service_tier` so we
    // can count tier distribution on the inbound shape too. The
    // response-side tier (from `response.completed`) is captured by
    // the SSE state machine at stream-close; this counter increment
    // pairs them. Body is parsed best-effort; missing/non-JSON
    // bodies do NOT fabricate a tier — per realignment build-
    // constraint "no silent fallbacks", we just skip the emit and
    // log at debug.
    //
    // C1 fix: every raw value is validated against the bounded
    // `service_tier` vocabulary BEFORE being used as a label so a
    // malicious client cannot blow up label cardinality with
    // arbitrary strings.
    if let Some(tier) = extract_request_service_tier(&body) {
        let request_id_for_metric = headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<no-request-id>");
        let bucketed = crate::observability::metric_names::service_tier::validate(&tier);
        observability::record_service_tier(bucketed, request_id_for_metric);
    } else {
        tracing::debug!(
            event = "service_tier_skipped",
            path = %uri.path(),
            reason = "absent_or_unparseable",
            "request body had no parseable service_tier; counter not emitted"
        );
    }

    // Reconstruct the Request<Body> shape forward_http expects.
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(hs) = builder.headers_mut() {
        *hs = headers;
    }
    let req = match builder.body(Body::from(body)) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                event = "handler_error",
                handler = "responses",
                error = %e,
                "failed to reconstruct request from buffered body"
            );
            return Response::builder()
                .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("internal handler error"))
                .expect("static response");
        }
    };

    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| {
            use axum::response::IntoResponse;
            e.into_response()
        })
}

/// Phase G PR-G3: best-effort parse of `service_tier` from the
/// inbound request body. Returns `None` when the body is not valid
/// JSON, not an object, or lacks the field. The spec defines the
/// field as a string ∈ {auto, default, flex, on_demand, priority,
/// scale}; the returned raw string is normalised against the
/// bounded vocabulary at the call site via
/// [`crate::observability::metric_names::service_tier::validate`]
/// so an arbitrary inbound value cannot drive metric-label
/// cardinality unbounded (C1 fix).
fn extract_request_service_tier(body: &Bytes) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("service_tier")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

/// Cheap check: is this request asking for an SSE response? Compares
/// `Accept` against `text/event-stream` (case-insensitive on the
/// media-type token, RFC 7231 §3.1.1.1). Multiple media types in
/// `Accept` are split on `,`; any match wins.
fn accepts_sse(headers: &HeaderMap) -> bool {
    let Some(v) = headers.get(http::header::ACCEPT) else {
        return false;
    };
    let Ok(s) = v.to_str() else {
        return false;
    };
    s.split(',').any(|piece| {
        let mt = piece.split(';').next().unwrap_or("").trim();
        mt.eq_ignore_ascii_case("text/event-stream")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use serde_json::json;

    #[test]
    fn accepts_sse_explicit() {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        assert!(accepts_sse(&h));
    }

    #[test]
    fn accepts_sse_case_insensitive() {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("Text/Event-Stream"),
        );
        assert!(accepts_sse(&h));
    }

    #[test]
    fn accepts_sse_among_others() {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream;q=0.9"),
        );
        assert!(accepts_sse(&h));
    }

    #[test]
    fn accepts_json_only_returns_false() {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        assert!(!accepts_sse(&h));
    }

    #[test]
    fn no_accept_header_returns_false() {
        let h = HeaderMap::new();
        assert!(!accepts_sse(&h));
    }

    #[test]
    fn additional_tools_lift_and_restore_preserve_relative_slots() {
        let mut payload = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "message", "id": "before"},
                {"type": "additional_tools", "id": "carrier-a", "tools": [{"name": "shell"}]},
                {"type": "message", "id": "middle"},
                {"type": "additional_tools", "id": "carrier-b", "tools": [{"name": "read"}]},
                {"type": "message", "id": "after"}
            ]
        });
        let plan = lift_codex_additional_tools(&mut payload, "req-test").expect("lifted");
        assert_eq!(
            payload["tools"],
            json!([{"name": "shell"}, {"name": "read"}])
        );
        assert_eq!(payload["input"].as_array().unwrap().len(), 3);

        assert_eq!(restore_codex_additional_tools(&mut payload, &plan), Ok(2));
        assert!(payload.get("tools").is_none());
        let input = payload["input"].as_array().unwrap();
        assert_eq!(input[0]["id"], "before");
        assert_eq!(input[1]["id"], "carrier-a");
        assert_eq!(input[2]["id"], "middle");
        assert_eq!(input[3]["id"], "carrier-b");
        assert_eq!(input[4]["id"], "after");
    }

    #[test]
    fn additional_tools_restore_is_idempotent() {
        let mut payload = json!({
            "input": [{"type": "additional_tools", "tools": [{"name": "shell"}]}]
        });
        let plan = AdditionalToolsRestorePlan {
            carriers: vec![AdditionalToolsCarrier {
                kept_index: 0,
                item: serde_json::Map::new(),
                tools: vec![json!({"name": "shell"})],
            }],
        };
        assert_eq!(restore_codex_additional_tools(&mut payload, &plan), Ok(0));
        assert_eq!(payload["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn additional_tools_classic_encoding_is_untouched() {
        let mut payload = json!({
            "tools": [{"name": "classic"}],
            "input": [{"type": "additional_tools", "tools": [{"name": "carrier"}]}]
        });
        let original = payload.clone();
        assert!(lift_codex_additional_tools(&mut payload, "req-test").is_none());
        assert_eq!(payload, original);
    }

    #[test]
    fn additional_tools_opt_out_values_match_python() {
        for value in ["0", "false", "FALSE", " no ", "Off"] {
            assert!(!env_value_enabled(Some(value)), "{value}");
        }
        assert!(env_value_enabled(None));
        assert!(env_value_enabled(Some("1")));
        assert!(env_value_enabled(Some("yes")));
    }
}

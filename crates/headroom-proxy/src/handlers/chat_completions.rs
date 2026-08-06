//! POST `/v1/chat/completions` handler — Phase C PR-C2.
//!
//! # Why an explicit handler?
//!
//! Most paths flow through `forward_http` via the catch-all fallback;
//! the path gate in `forward_http` runs the per-provider live-zone
//! dispatcher (added to the gate by PR-C2). Spec PR-C2 still mandates
//! an explicit route handler for `/v1/chat/completions` so future
//! Phase-D wiring (Bedrock OpenAI-shape, Vertex), Phase-E auth-mode
//! gating, and per-endpoint rate-limit shaping have an obvious
//! attachment point.
//!
//! # What this handler does
//!
//! 1. Pre-buffers the request body (Bytes) so we can inspect
//!    `n`, `stream`, `messages`, `tool_choice`, `stream_options`
//!    before forwarding.
//! 2. Reconstructs a `Request<Body>` from the buffered bytes plus
//!    the original method, URI, and headers.
//! 3. Hands off to [`crate::proxy::forward_http`] — the same single
//!    forwarder that the catch-all uses. The compression gate inside
//!    `forward_http` re-classifies the path and runs
//!    [`crate::compression::compress_openai_chat_request`].
//!
//! Re-using `forward_http` keeps the SSE state-machine wiring
//! (PR-C1), header-stripping (PR-A5), `x-headroom-*` policy, and
//! request-id plumbing single-source. The alternative — duplicating
//! the forwarder body inside this handler — would diverge over time.
//!
//! # Skip / passthrough behaviours surfaced here
//!
//! - **`n > 1`** — multiple completions imply non-determinism.
//!   `compression::should_skip_compression` (called from the gate
//!   inside `forward_http`) returns `NGreaterThanOne(n)` and the
//!   gate skips dispatch entirely. The handler does not need to
//!   touch the body.
//! - **`stream: true`** — handled by the existing SSE state-machine
//!   tee in `forward_http` (PR-C1's `ChunkState`).
//! - **`tool_choice` change** — never read, never mutated.
//!   `tools[]` definitions live in the cache hot zone and the
//!   live-zone dispatcher only walks `messages[*].content`.
//! - **`stream_options.include_usage`** — same. Round-trips byte-equal
//!   as a side effect of byte-range surgery in the dispatcher.

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, Uri};
use axum::response::Response;
use bytes::Bytes;
use std::net::SocketAddr;

use crate::proxy::{forward_http, AppState};

/// Translate the legacy OpenAI `max_tokens` field to
/// `max_completion_tokens` in a buffered chat-completions body.
///
/// GPT-5 / o-series chat models REJECT `max_tokens`
/// ("Unsupported parameter … Use 'max_completion_tokens' instead");
/// gpt-4o/4.1 accept `max_completion_tokens` too. openai-compatible
/// clients (opencode, older SDKs) still send `max_tokens`, so translate
/// it here — the proxy already owns the outbound body — and those
/// requests work unchanged. The rename is one-way and safe for current
/// OpenAI models; it is a no-op when the caller already set
/// `max_completion_tokens`, when there is no `max_tokens`, or when the
/// body is not a JSON object. On any parse/serialize failure the
/// original bytes are returned untouched so the passthrough invariant
/// holds.
pub(crate) fn normalize_openai_max_tokens(body: Bytes) -> Bytes {
    let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(obj) = parsed.as_object_mut() else {
        return body;
    };
    if !obj.contains_key("max_tokens") {
        return body;
    }
    let legacy = obj.remove("max_tokens");
    if let Some(legacy) = legacy {
        // Preserve an already-set `max_completion_tokens`; only fill it
        // from the legacy key when absent or explicitly null.
        let needs_fill = matches!(
            obj.get("max_completion_tokens"),
            None | Some(serde_json::Value::Null)
        );
        if !legacy.is_null() && needs_fill {
            obj.insert("max_completion_tokens".to_string(), legacy);
        }
    }
    match serde_json::to_vec(&parsed) {
        Ok(v) => Bytes::from(v),
        Err(_) => body,
    }
}

/// Rate-limit check: extracts the API key from `Authorization` header
/// and checks against the per-key token bucket. Returns 429 when denied.
pub(crate) fn check_rate_limit(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let limiter = state.rate_limiter.as_ref()?;
    let key = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous")
        .to_string();
    let result = limiter.check_request(&key);
    if result.allowed {
        None
    } else {
        let wait = std::time::Duration::from_secs_f64(result.wait_seconds);
        Some(
            Response::builder()
                .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
                .header("retry-after", format!("{:.0}", wait.as_secs()))
                .body(Body::from(format!(
                    "rate limit exceeded; retry after {:.1}s",
                    result.wait_seconds
                )))
                .expect("static response"),
        )
    }
}

/// Axum POST handler for `/v1/chat/completions`. Buffers the body,
/// stitches a fresh `Request<Body>` together, and forwards via
/// [`forward_http`]. Compression dispatch + SSE telemetry is handled
/// inside `forward_http`'s shared gate (PR-C1 + PR-C2).
pub async fn handle_chat_completions(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Rate-limit gate: check before buffering the body.
    if let Some(rejected) = check_rate_limit(&state, &headers) {
        return rejected;
    }

    // Compatibility shim: GPT-5 / o-series chat models reject the legacy
    // `max_tokens`; translate it to `max_completion_tokens` before
    // forwarding. Runs on the always-buffered chat body regardless of
    // whether compression later intercepts it downstream.
    let body = normalize_openai_max_tokens(body);

    // Reconstruct the Request<Body> shape forward_http expects.
    // Cloning the headers into a fresh builder keeps the original
    // method/uri/version intact. `axum::body::Body::from(Bytes)` is
    // a single-shot stream, which is exactly what the buffered
    // compression branch wants.
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(hs) = builder.headers_mut() {
        *hs = headers;
    }
    let req = match builder.body(Body::from(body)) {
        Ok(r) => r,
        Err(e) => {
            // Building the request out of pieces we already have
            // shouldn't fail; if it does it's an internal bug. Don't
            // silently swallow — log loudly and 500.
            tracing::error!(
                event = "handler_error",
                handler = "chat_completions",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn norm(v: Value) -> Value {
        let out = normalize_openai_max_tokens(Bytes::from(serde_json::to_vec(&v).unwrap()));
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn renames_legacy_max_tokens() {
        let out = norm(json!({"model": "gpt-5", "max_tokens": 256}));
        assert_eq!(out.get("max_tokens"), None);
        assert_eq!(out["max_completion_tokens"], json!(256));
    }

    #[test]
    fn keeps_existing_max_completion_tokens_and_drops_legacy() {
        let out = norm(json!({"max_tokens": 256, "max_completion_tokens": 512}));
        assert_eq!(out.get("max_tokens"), None);
        assert_eq!(out["max_completion_tokens"], json!(512));
    }

    #[test]
    fn noop_without_max_tokens() {
        let out = norm(json!({"model": "gpt-4o", "max_completion_tokens": 128}));
        assert_eq!(out.get("max_tokens"), None);
        assert_eq!(out["max_completion_tokens"], json!(128));
    }

    #[test]
    fn null_legacy_drops_without_filling() {
        let out = norm(json!({"max_tokens": Value::Null}));
        assert_eq!(out.get("max_tokens"), None);
        assert_eq!(out.get("max_completion_tokens"), None);
    }

    #[test]
    fn non_object_body_untouched() {
        let bytes = Bytes::from_static(b"[1,2,3]");
        assert_eq!(normalize_openai_max_tokens(bytes.clone()), bytes);
    }

    #[test]
    fn invalid_json_untouched() {
        let bytes = Bytes::from_static(b"not json");
        assert_eq!(normalize_openai_max_tokens(bytes.clone()), bytes);
    }
}

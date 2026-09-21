//! HTTP call creation for Codex Live (port of upstream `53982aef`).
//!
//! Codex Desktop opens a voice/video session with a multipart
//! `POST /v1/live` carrying `sdp` and JSON-encoded `session` fields. That
//! shape must be converted to the ChatGPT realtime-calls JSON payload and
//! sent to the Codex backend; the generic forwarder would relay the
//! multipart bytes to the default upstream, where call creation fails and
//! the client loses the upstream `Location` header.
//!
//! Only ChatGPT-authenticated requests take this path (resolved from
//! headers alone, before the body is touched, via the same
//! [`crate::websocket_codex::resolve_codex_routing`] the WS path uses).
//! Everything else falls through to the generic forwarder with its body
//! unread.
//!
//! Two deliberate divergences from the Python handler:
//!
//! - The Python side runs on httpx, which decodes `content-encoding` before
//!   handing over bytes, so it must drop that header when replaying. This
//!   crate's reqwest client has no decompression features: bytes pass
//!   through untouched, so `content-encoding` is KEPT and only true
//!   framing (`content-length`, which axum recomputes, plus
//!   `transfer-encoding`, `connection`, `keep-alive`) is dropped.
//! - The upstream base URL honours `HEADROOM_CODEX_REALTIME_CALLS_URL`
//!   (operator override and integration-test seam); otherwise it is the
//!   same `chatgpt.com/backend-api/codex` base [`crate::codex`] uses.

use axum::body::Body;
use axum::extract::{FromRequest, Multipart};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;

/// Paths that carry Codex Live traffic. Mirrors upstream
/// `CODEX_LIVE_ROUTE_PATHS`.
pub(crate) const LIVE_HTTP_PATHS: [&str; 4] = [
    "/v1/live",
    "/v1/codex/live",
    "/backend-api/live",
    "/backend-api/codex/live",
];

/// Default upstream call-creation endpoint (same host+prefix as
/// [`crate::codex::codex_endpoint`]). The query carries the quicksilver
/// intent, mirroring upstream `CODEX_LIVE_CALLS_QUERY`.
const DEFAULT_REALTIME_CALLS_URL: &str =
    "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";

/// Env override for the call-creation URL: operator escape hatch and the
/// seam the wiremock integration test points at a mock with.
const REALTIME_CALLS_URL_ENV: &str = "HEADROOM_CODEX_REALTIME_CALLS_URL";

/// Upstream request timeout. Mirrors upstream's `timeout=120.0`.
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// True for a path carrying Codex Live call creation. Exact match on the
/// path (no query, no trailing slash), like the registered routes.
pub(crate) fn is_live_call_path(path: &str) -> bool {
    LIVE_HTTP_PATHS.contains(&path)
}

/// Upstream call-creation URL: env override when set and non-blank,
/// otherwise the ChatGPT default.
pub(crate) fn realtime_calls_url() -> String {
    std::env::var(REALTIME_CALLS_URL_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_REALTIME_CALLS_URL.to_string())
}

/// Build the realtime-calls JSON payload from the multipart fields.
pub(crate) fn call_payload(sdp: &str, session: Value) -> Value {
    serde_json::json!({"sdp": sdp, "session": session})
}

/// Headers forwarded back downstream: everything except wire framing.
/// `content-encoding` is deliberately KEPT (see module docs): unlike the
/// Python handler we never decode the body, so dropping it would leave
/// encoded bytes the client cannot interpret. `content-length` is dropped
/// because axum recomputes it from the body we hand back.
fn map_response_headers(upstream: &HeaderMap) -> HeaderMap {
    const DROPPED: [&str; 5] = [
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "server",
    ];
    let mut out = HeaderMap::with_capacity(upstream.len());
    for (name, value) in upstream.iter() {
        if DROPPED.contains(&name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Outbound headers: the inbound set minus hop-by-hop and body-describing
/// headers (reqwest sets its own `Host`/`Content-Type`/`Content-Length`
/// for the JSON payload), minus internal `x-headroom-*` when stripping is
/// enabled. Mirrors upstream's drop list plus this proxy's hygiene.
fn outbound_headers(inbound: &HeaderMap, strip_internal: bool) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(inbound.len());
    for (name, value) in inbound.iter() {
        let lower = name.as_str();
        if matches!(
            lower,
            "host" | "accept-encoding" | "content-length" | "content-type" | "content-encoding"
        ) {
            continue;
        }
        if strip_internal && crate::headers::is_internal_header(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

fn text_response(status: StatusCode, message: &'static str) -> Response {
    (status, message).into_response()
}

/// Outcome of [`handle_live_call`]. `Fallthrough` hands the untouched
/// parts and body back so the caller forwards exactly what arrived.
pub(crate) enum LiveHttpDecision {
    Handled(Response),
    Fallthrough(http::request::Parts, Body),
}

/// Forward one Codex Live call-creation request.
///
/// The ChatGPT-auth gate runs on a header clone before the body is
/// touched: a non-ChatGPT-authenticated request comes back as
/// `Fallthrough` with its body unread, so the caller falls through to
/// the generic forwarder.
pub(crate) async fn handle_live_call(
    client: &reqwest::Client,
    strip_internal: bool,
    max_body_bytes: usize,
    request_id: &str,
    inbound_path: &str,
    parts: http::request::Parts,
    body: Body,
) -> LiveHttpDecision {
    let mut routed = parts.headers.clone();
    if !crate::websocket_codex::resolve_codex_routing(&mut routed) {
        return LiveHttpDecision::Fallthrough(parts, body);
    }
    let headers = outbound_headers(&routed, strip_internal);
    let bytes = match axum::body::to_bytes(body, max_body_bytes).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                event = "codex_live_http_body_error",
                request_id = %request_id,
                path = %inbound_path,
                error = %e,
                "Codex Live call creation: could not read request body"
            );
            return LiveHttpDecision::Handled(text_response(
                StatusCode::BAD_REQUEST,
                "Could not read request body.",
            ));
        }
    };
    let req = Request::from_parts(parts, Body::from(bytes));
    let (sdp, session) = match live_call_fields(req).await {
        Ok(fields) => fields,
        Err(resp) => return LiveHttpDecision::Handled(resp),
    };
    let url = realtime_calls_url();
    let upstream = match client
        .post(&url)
        .timeout(UPSTREAM_TIMEOUT)
        .headers(headers)
        .json(&call_payload(&sdp, session))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(
                event = "codex_live_http_upstream_error",
                request_id = %request_id,
                path = %inbound_path,
                error = %e,
                "Codex Live HTTP call creation failed"
            );
            return LiveHttpDecision::Handled(text_response(
                StatusCode::BAD_GATEWAY,
                "Upstream request failed.",
            ));
        }
    };
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let body_bytes: Bytes = upstream.bytes().await.unwrap_or_default();
    let mut builder = Response::builder().status(status);
    if let Some(map) = builder.headers_mut() {
        *map = map_response_headers(&upstream_headers);
    }
    match builder.body(Body::from(body_bytes)) {
        Ok(resp) => LiveHttpDecision::Handled(resp),
        Err(_) => LiveHttpDecision::Handled(text_response(
            StatusCode::BAD_GATEWAY,
            "Upstream request failed.",
        )),
    }
}

/// Extract the `sdp` and `session` fields from a multipart (or empty)
/// body. A non-multipart body yields the same 400 the Python handler
/// produces for absent fields rather than a 500.
async fn live_call_fields(req: Request<Body>) -> Result<(String, Value), Response> {
    let mut multipart = match Multipart::from_request(req, &()).await {
        Ok(m) => m,
        Err(_) => {
            return Err(text_response(
                StatusCode::BAD_REQUEST,
                "Missing sdp or session form field.",
            ));
        }
    };
    let mut sdp: Option<String> = None;
    let mut session: Option<String> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name().unwrap_or("") {
            "sdp" => sdp = field.text().await.ok(),
            "session" => session = field.text().await.ok(),
            _ => {}
        }
    }
    match (sdp, session) {
        (Some(sdp), Some(session)) if !sdp.is_empty() && !session.is_empty() => {
            match serde_json::from_str::<Value>(&session) {
                Ok(payload) => Ok((sdp, payload)),
                Err(_) => Err(text_response(
                    StatusCode::BAD_REQUEST,
                    "Invalid session JSON.",
                )),
            }
        }
        _ => Err(text_response(
            StatusCode::BAD_REQUEST,
            "Missing sdp or session form field.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_paths_match_upstream_set() {
        for path in [
            "/v1/live",
            "/v1/codex/live",
            "/backend-api/live",
            "/backend-api/codex/live",
        ] {
            assert!(is_live_call_path(path), "{path}");
        }
        assert!(!is_live_call_path("/v1/live/"));
        assert!(!is_live_call_path("/v1/messages"));
        assert!(!is_live_call_path("/livez"));
    }

    #[test]
    fn realtime_url_default_shape() {
        assert_eq!(
            realtime_calls_url(),
            DEFAULT_REALTIME_CALLS_URL,
            "env must be unset for this assertion"
        );
        assert!(DEFAULT_REALTIME_CALLS_URL.contains("/realtime/calls"));
        assert!(DEFAULT_REALTIME_CALLS_URL.contains("intent=quicksilver&architecture=avas"));
    }

    #[test]
    fn response_mapping_keeps_location_and_encoding_drops_framing() {
        let mut upstream = HeaderMap::new();
        upstream.insert("location", "/realtime/calls/abc".parse().unwrap());
        upstream.insert("content-type", "application/json".parse().unwrap());
        upstream.insert("content-encoding", "gzip".parse().unwrap());
        upstream.insert("content-length", "42".parse().unwrap());
        upstream.insert("transfer-encoding", "chunked".parse().unwrap());
        upstream.insert("connection", "keep-alive".parse().unwrap());
        upstream.insert("server", "x".parse().unwrap());
        let mapped = map_response_headers(&upstream);
        // The client needs these to find and decode the answer.
        assert!(mapped.contains_key("location"));
        assert!(mapped.contains_key("content-encoding"));
        assert!(mapped.contains_key("content-type"));
        // Stale framing must never ride along on recomputed bytes.
        assert!(!mapped.contains_key("content-length"));
        assert!(!mapped.contains_key("transfer-encoding"));
        assert!(!mapped.contains_key("connection"));
        assert!(!mapped.contains_key("server"));
    }

    #[test]
    fn outbound_headers_drop_hop_and_internal() {
        let mut inbound = HeaderMap::new();
        inbound.insert("host", "proxy:8787".parse().unwrap());
        inbound.insert("authorization", "Bearer chatgpt".parse().unwrap());
        inbound.insert("chatgpt-account-id", "acct_1".parse().unwrap());
        inbound.insert("content-type", "multipart/form-data".parse().unwrap());
        inbound.insert("x-headroom-mode", "token".parse().unwrap());
        let out = outbound_headers(&inbound, true);
        assert!(!out.contains_key("host"));
        assert!(!out.contains_key("content-type"));
        assert!(!out.contains_key("x-headroom-mode"));
        assert!(out.contains_key("authorization"));
        assert!(out.contains_key("chatgpt-account-id"));
    }

    #[test]
    fn jwt_derived_account_id_survives_outbound() {
        // Regression test: handle_live_call gates on a `routed` clone into
        // which resolve_codex_routing inserts ChatGPT-Account-ID when
        // derived from the Bearer JWT. Outbound must be built from `routed`,
        // not the original headers, or JWT-only clients lose routing.
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_jwt"}})
                .to_string(),
        );
        let mut inbound = HeaderMap::new();
        inbound.insert(
            "authorization",
            format!("Bearer a.{payload}.b").parse().unwrap(),
        );
        let mut routed = inbound.clone();
        assert!(crate::websocket_codex::resolve_codex_routing(&mut routed));
        let out = outbound_headers(&routed, true);
        assert_eq!(
            out.get("chatgpt-account-id").and_then(|v| v.to_str().ok()),
            Some("acct_jwt")
        );
    }

    #[test]
    fn call_payload_shape() {
        let session = serde_json::json!({"model": "gpt-5"});
        assert_eq!(
            call_payload("v=0\r\n...", session),
            serde_json::json!({"sdp": "v=0\r\n...", "session": {"model": "gpt-5"}})
        );
    }
}

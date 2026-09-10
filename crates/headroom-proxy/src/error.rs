//! Error types for the proxy.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("upstream request failed: {0}")]
    Upstream(#[from] reqwest::Error),

    #[error("invalid upstream URL: {0}")]
    InvalidUpstream(String),

    #[error("invalid header: {0}")]
    InvalidHeader(String),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// PR-A8 / P5-59: request body exceeded the configured cap. RFC 7231
    /// §6.5.11: 413 Payload Too Large. Previously surfaced as
    /// `InvalidHeader` (400) which mis-classified an oversize body as a
    /// header parse error; clients with retry-on-413 logic broke.
    #[error("request body exceeds configured limit: {0}")]
    PayloadTooLarge(String),

    /// Surfaced when `--compression` is enabled but the proxy can't
    /// build the IntelligentContextManager at startup (e.g. the
    /// embedded tokenizer asset failed to initialize). Bubbles up to
    /// `main` as a fatal startup error rather than a per-request
    /// failure — if compression is configured but the engine won't
    /// build, the operator should know immediately, not at first
    /// LLM request.
    #[error("compression engine startup failed: {0}")]
    CompressionStartup(String),

    /// A startup configuration invariant was violated (e.g. a feature that
    /// depends on another was enabled without it). Fatal at construction time
    /// — the operator is told exactly what to fix rather than degrading
    /// silently.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        // Transport-exhausted turns are transient (VPN rotation RST, wifi
        // flap, corpse-pool first-write failure): 503 + Retry-After so the
        // client's standard retry fires instead of stalling the session.
        // Non-transport failures (decode/builder/status) stay 502 — retrying
        // those cannot succeed. The `x-headroom-retryable` marker keeps
        // proxy-transient 503s distinct from provider 5xx on dashboards,
        // mirroring the `x-headroom-shed` convention on the concurrency shed.
        if let ProxyError::Upstream(e) = &self {
            if e.is_timeout() {
                let msg = format!("upstream timeout: {e}");
                tracing::warn!(error = %msg, "proxy error");
                return transient_response(msg);
            }
            if e.is_connect() || e.is_request() || e.is_body() {
                let msg = format!("upstream transient error: {e}");
                tracing::warn!(error = %msg, "proxy error");
                return transient_response(msg);
            }
        }
        let (status, msg) = match &self {
            ProxyError::Upstream(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
            ProxyError::InvalidUpstream(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
            ProxyError::InvalidHeader(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            ProxyError::PayloadTooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, self.to_string()),
            ProxyError::WebSocket(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
            ProxyError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            // CompressionStartup is a startup-time error, not a
            // per-request one — but if it ever surfaces in the
            // handler path, surface as 500 rather than panic.
            ProxyError::CompressionStartup(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
            ProxyError::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        tracing::warn!(error = %msg, "proxy error");
        (status, msg).into_response()
    }
}

/// 503 + `Retry-After` for a transient upstream failure (rotation RST, wifi
/// flap, corpse-pool first-write miss, connect timeout). 503 rather than 502
/// so stock client retry policies treat it as "try again shortly" — 502
/// without `Retry-After` stalls the agent loop until a human nudges it.
pub(crate) fn transient_response(msg: String) -> Response {
    use axum::body::Body;
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(http::header::RETRY_AFTER, "2")
        .header("x-headroom-retryable", "transport-exhausted")
        .body(Body::from(msg))
        .expect("static 503 response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// A real refused connection is the cheapest genuine `reqwest::Error`
    /// with `is_connect() == true` (no public constructor exists).
    async fn connect_refused_error() -> reqwest::Error {
        reqwest::Client::new()
            .post("http://127.0.0.1:1/no-listener")
            .body("hi")
            .send()
            .await
            .expect_err("nothing listens on port 1")
    }

    #[tokio::test]
    async fn transport_exhaustion_is_503_with_retry_after() {
        let err = connect_refused_error().await;
        assert!(
            err.is_connect(),
            "test premise: refused localhost conn must be a connect error"
        );
        let resp = ProxyError::Upstream(err).into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let headers = resp.headers();
        assert_eq!(headers.get(http::header::RETRY_AFTER).unwrap(), "2");
        assert_eq!(
            headers.get("x-headroom-retryable").unwrap(),
            "transport-exhausted"
        );
    }

    /// Non-transport failures carry no Retry-After. (A genuine non-retryable
    /// `Upstream` error — decode/builder/status — has no public constructor,
    /// so this pins the neighboring 502 arm instead; see the routed
    /// `transport_exhaustion_answers_503_with_retry_after` test for the 503.)
    #[test]
    fn invalid_upstream_stays_502_without_retry_after() {
        let resp = ProxyError::InvalidUpstream("bogus://url".to_string()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert!(resp.headers().get(http::header::RETRY_AFTER).is_none());
    }
}

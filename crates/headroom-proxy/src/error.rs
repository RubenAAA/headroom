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

    /// The request path, or a decoded route parameter, would have to be
    /// rewritten on the way upstream (dot segments, backslashes, or a
    /// segment that would gain or lose a `/` boundary). The proxy forwards
    /// paths verbatim or not at all — see `crate::upstream_path`. RFC 7231
    /// §6.5.1: 400.
    #[error("request path rejected: {0}")]
    InvalidPath(String),

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

impl From<crate::upstream_path::PathError> for ProxyError {
    fn from(e: crate::upstream_path::PathError) -> Self {
        ProxyError::InvalidPath(e.to_string())
    }
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
        //
        // Full detail is for operators only. The `Display` impls above
        // interpolate the source error (e.g. reqwest's chain), which for
        // upstream failures can embed the upstream host/port and other
        // internal wiring. We log that detail but return a generic,
        // stable body to the client so an error response can't be used
        // to fingerprint the upstream. The HTTP status still carries the
        // actionable signal for well-behaved clients.
        let detail = self.to_string();
        if let ProxyError::Upstream(e) = &self {
            if e.is_timeout() {
                tracing::warn!(event = "proxy_error", status = 503, error = %detail, cause = ?e, "proxy error");
                return transient_response("upstream timeout".to_string());
            }
            if e.is_connect() || e.is_request() || e.is_body() {
                tracing::warn!(event = "proxy_error", status = 503, error = %detail, cause = ?e, "proxy error");
                return transient_response("upstream transient error".to_string());
            }
        }
        let (status, client_msg): (StatusCode, &'static str) = match &self {
            ProxyError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream request failed"),
            ProxyError::InvalidUpstream(_) => (StatusCode::BAD_GATEWAY, "upstream request failed"),
            ProxyError::InvalidHeader(_) => (StatusCode::BAD_REQUEST, "invalid request"),
            // PayloadTooLarge stays descriptive: it leaks no internal
            // detail (the interpolated value is the client's own size vs
            // the configured cap) and clients with retry-on-413 logic
            // rely on recognizing it. See the PayloadTooLarge doc comment.
            ProxyError::PayloadTooLarge(_) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds configured limit",
            ),
            ProxyError::InvalidPath(_) => (StatusCode::BAD_REQUEST, "invalid request"),
            ProxyError::WebSocket(_) => (StatusCode::BAD_GATEWAY, "upstream request failed"),
            ProxyError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
            // CompressionStartup is a startup-time error, not a
            // per-request one — but if it ever surfaces in the
            // handler path, surface as 500 rather than panic.
            ProxyError::CompressionStartup(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
            ProxyError::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        };
        tracing::warn!(event = "proxy_error", status = %status.as_u16(), error = %detail, "proxy error");
        (status, client_msg).into_response()
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
        // A just-closed port refuses with RST: a pure connect error, fast.
        // (Port 1 blackholes instead of refusing in some sandboxes, which
        // surfaces as a timeout — the wrong arm for this test.)
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        crate::ssl_context::client_builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://127.0.0.1:{port}/no-listener"))
            .body("hi")
            .send()
            .await
            .expect_err("closed port must refuse")
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

    async fn status_and_body(err: ProxyError) -> (StatusCode, String) {
        let resp = err.into_response();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// A listener that accepts connections and never answers them, so a
    /// client with a short timeout fails with `is_timeout() == true` — the
    /// only way to build a genuine timeout `reqwest::Error` (no public
    /// constructor exists).
    async fn timeout_error() -> reqwest::Error {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Hold every accepted socket open without answering: the client
            // must time out rather than see a reset.
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    drop(stream);
                });
            }
        });
        crate::ssl_context::client_builder()
            .no_proxy()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/hangs"))
            .send()
            .await
            .expect_err("listener never responds")
    }

    // The interpolated detail (which for real upstream errors can embed
    // the upstream host/port) must NEVER reach the client body. Before
    // the hardening these bodies were `self.to_string()` and DID contain
    // the detail, so these assertions fail on old code.

    #[tokio::test]
    async fn transient_arms_keep_retry_headers_but_hide_detail() {
        let timeout = timeout_error().await;
        assert!(timeout.is_timeout(), "test premise: hang must time out");
        let (status, body) = status_and_body(ProxyError::Upstream(timeout)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "upstream timeout");

        let refused = connect_refused_error().await;
        assert!(
            refused.is_connect() && !refused.is_timeout(),
            "test premise: refused localhost conn must be a pure connect error"
        );
        let (status, body) = status_and_body(ProxyError::Upstream(refused)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "upstream transient error");
        assert!(!body.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn invalid_header_detail_is_not_leaked_to_client() {
        let (status, body) = status_and_body(ProxyError::InvalidHeader(
            "upstream-secret-host:9443".into(),
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "invalid request");
        assert!(!body.contains("upstream-secret-host"));
    }

    #[tokio::test]
    async fn invalid_upstream_detail_is_not_leaked_to_client() {
        let (status, body) = status_and_body(ProxyError::InvalidUpstream(
            "http://10.0.0.5:8788/internal".into(),
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body, "upstream request failed");
        assert!(!body.contains("10.0.0.5"));
    }

    #[tokio::test]
    async fn payload_too_large_stays_descriptive() {
        // Deliberately kept descriptive: no internal detail, and
        // retry-on-413 clients rely on recognizing it.
        let (status, body) =
            status_and_body(ProxyError::PayloadTooLarge("2097152 > 1048576".into())).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body, "request body exceeds configured limit");
    }

    /// Every remaining arm maps to a fixed string, whatever the detail held.
    #[tokio::test]
    async fn remaining_arms_return_generic_bodies() {
        let secret = "http://10.0.0.5:8788/internal".to_string();
        let cases = [
            (
                ProxyError::InvalidPath(secret.clone()),
                StatusCode::BAD_REQUEST,
                "invalid request",
            ),
            (
                ProxyError::WebSocket(secret.clone()),
                StatusCode::BAD_GATEWAY,
                "upstream request failed",
            ),
            (
                ProxyError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    secret.clone(),
                )),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ),
            (
                ProxyError::CompressionStartup(secret.clone()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ),
            (
                ProxyError::Config(secret.clone()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ),
        ];
        for (err, status, body) in cases {
            let (actual_status, actual_body) = status_and_body(err).await;
            assert_eq!(actual_status, status);
            assert_eq!(actual_body, body);
            assert!(!actual_body.contains("10.0.0.5"));
        }
    }
}

//! Inbound authentication for the data-plane routes.
//!
//! `HEADROOM_PROXY_TOKEN` was parsed into [`crate::config::Config`] and then
//! read by nothing: the flag existed, the docs described it, and every `/v1/*`
//! route stayed open. A configured-but-unenforced credential is worse than no
//! flag at all, because an operator who sets it believes the proxy is closed.
//!
//! The trust boundary is the one the admin and debug routes already use
//! (`crate::loopback_guard`): a caller on loopback is the operator, anyone else
//! must present the token. Health probes stay open so an orchestrator can
//! check a container that binds a non-loopback interface.
//!
//! One gate covers both transports. A WebSocket upgrade reaches
//! [`crate::proxy::catch_all`] as an ordinary HTTP GET and only becomes a
//! socket once `WebSocketUpgrade::from_request_parts` succeeds, so a router
//! layer sees the handshake like any other request. The Python proxy needed a
//! second, separate middleware here, because Starlette's `BaseHTTPMiddleware`
//! passes any non-`http` scope straight through and its `/v1/responses` and
//! `/v1/live` upgrades slipped past the HTTP gate — the POST on a path was
//! authenticated while the upgrade on that same path was not.

use axum::extract::{ConnectInfo, State};
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

use crate::proxy::AppState;

/// Probe endpoints that answer without a token.
///
/// An orchestrator health-checking a container that binds `0.0.0.0` is not
/// authenticated and should not have to be. These return no request data.
const AUTH_EXEMPT_PATHS: &[&str] = &["/health", "/healthz", "/livez", "/readyz"];

/// The caller's token, from either accepted carrier.
///
/// `Authorization: Bearer` is what a programmatic client sends by default;
/// `x-headroom-proxy-token` exists for callers whose `Authorization` header is
/// already spoken for by the upstream provider's own credential. Never read
/// from the query string: that lands in access logs and browser history.
fn bearer_proxy_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?;
    if auth.get(..7)?.eq_ignore_ascii_case("bearer ") {
        let token = auth[7..].trim();
        if !token.is_empty() {
            return Some(token);
        }
    }
    None
}

fn dedicated_proxy_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get("x-headroom-proxy-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

pub fn read_proxy_token(headers: &axum::http::HeaderMap) -> Option<String> {
    // Prefer the dedicated carrier because Authorization is commonly already
    // occupied by the provider's own bearer credential.
    dedicated_proxy_token(headers)
        .or_else(|| bearer_proxy_token(headers))
        .map(str::to_string)
}

/// Compare without letting the time taken reveal how much of the token matched.
///
/// A byte-by-byte `==` returns as soon as it finds a difference, which turns a
/// secret into a per-character guessing game against a remote timer. Length is
/// not hidden — it is not the secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Require `HEADROOM_PROXY_TOKEN` from non-loopback callers.
///
/// Declines to act when no token is configured, which keeps the default
/// single-user setup working exactly as before.
pub async fn proxy_auth_gate(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(expected) = state.config.proxy_token.as_deref() else {
        return next.run(req).await;
    };

    let path = req.uri().path();
    if AUTH_EXEMPT_PATHS.contains(&path) {
        return next.run(req).await;
    }

    let peer_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string());
    let client_ip = crate::forwarded_headers::resolve_client_ip(
        peer_ip.as_deref(),
        req.headers(),
        &state.trusted_gateway_cidrs,
    );
    if crate::loopback_guard::is_loopback_host(Some(&client_ip)) {
        return next.run(req).await;
    }

    // Accept either carrier independently. In particular, a provider bearer
    // token in Authorization must not shadow a correct dedicated proxy token.
    let candidates = [
        dedicated_proxy_token(req.headers()),
        bearer_proxy_token(req.headers()),
    ];
    let presented = candidates.iter().any(Option::is_some);
    let ok = candidates
        .into_iter()
        .flatten()
        .any(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));

    if !ok {
        // Reason distinguishes "sent nothing" from "sent something wrong" for
        // the operator reading logs; neither branch echoes what was sent.
        let reason = if !presented { "missing" } else { "invalid" };
        tracing::warn!(
            event = "proxy_auth_rejected",
            path = %path,
            client = %client_ip,
            reason = reason,
            "rejected an unauthenticated non-loopback request"
        );
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn bearer_token_is_read() {
        assert_eq!(
            read_proxy_token(&headers(&[("authorization", "Bearer s3cret")])).as_deref(),
            Some("s3cret")
        );
    }

    /// Header names are case-insensitive over the wire, and so is the scheme
    /// token in `Authorization` — a client sending `bearer` must not be told
    /// its credential is missing.
    #[test]
    fn bearer_scheme_is_case_insensitive() {
        assert_eq!(
            read_proxy_token(&headers(&[("authorization", "bearer s3cret")])).as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn dedicated_header_is_read() {
        assert_eq!(
            read_proxy_token(&headers(&[("x-headroom-proxy-token", "s3cret")])).as_deref(),
            Some("s3cret")
        );
    }

    /// An `Authorization` header carrying the upstream provider's own key is
    /// not a proxy token, and must not be mistaken for one.
    #[test]
    fn non_bearer_authorization_is_not_a_token() {
        assert_eq!(
            read_proxy_token(&headers(&[("authorization", "Basic abc")])),
            None
        );
    }

    #[test]
    fn empty_bearer_is_not_a_token() {
        assert_eq!(
            read_proxy_token(&headers(&[("authorization", "Bearer   ")])),
            None
        );
    }

    #[test]
    fn missing_headers_yield_nothing() {
        assert_eq!(read_proxy_token(&headers(&[])), None);
    }

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}

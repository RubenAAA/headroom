//! Gates for the read-only operator routes (`/stats-history`, `/metrics`).
//!
//! Port of upstream's `_require_operator_read_client` and
//! `_require_metrics_scrape_client` (commit `ee731d7e`, findings 01-F5/01-F9).
//! These routes carry operator data (model/project/session labels, spend
//! history) and were served to any network caller. Now: a caller that
//! presented the proxy token (marked by [`crate::proxy_auth`] before its
//! headers are scrubbed) is entitled; otherwise loopback — peer and `Host:`
//! header, the DNS-rebinding pair the `/debug/*` guard uses — or the trusted
//! dashboard chain decides, and everyone else gets 404 (invisible, not 403).
//!
//! Deviations from upstream, both deliberate:
//! - No container-gateway exception: Rust has no `is_container_host_gateway`
//!   helper, and a bridge-gateway peer serving a published dashboard still
//!   passes via the trusted-dashboard CIDR chain when configured.
//! - `/metrics` additionally honors the existing opt-in
//!   `--metrics-require-loopback` flag: with the flag off the route stays
//!   open, preserving the documented default. The flag was previously
//!   parsed but never enforced.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::forwarded_headers::can_view_dashboard_metadata;
use crate::loopback_guard::is_loopback_host_header;
use crate::proxy::AppState;
use crate::proxy_auth::ProxyTokenAuthenticated;

/// Pure operator-read decision, unit-testable without a socket.
///
/// `peer` is the TCP peer IP (never a forwarded address); `host_header` is
/// the raw `Host:` value for the rebinding check.
pub fn operator_read_allowed(
    authenticated_at_gate: bool,
    peer: Option<std::net::IpAddr>,
    headers: &axum::http::HeaderMap,
    gateway_cidrs: &[crate::forwarded_headers::IpCidr],
    dashboard_cidrs: &[crate::forwarded_headers::IpCidr],
) -> bool {
    // A caller that presented the proxy token is an entitled operator.
    if authenticated_at_gate {
        return true;
    }
    // Loopback, or a trusted dashboard client behind a gateway: the same
    // chain that guards the dashboard settings routes (it reads `Host:`
    // from `headers` itself — the DNS-rebinding defence).
    let peer_host = peer.map(|ip| ip.to_string());
    can_view_dashboard_metadata(
        peer_host.as_deref(),
        headers,
        gateway_cidrs,
        dashboard_cidrs,
    )
}

/// Pure `/metrics` scrape decision. Same shape as [`operator_read_allowed`]
/// except the flag gates enforcement: with `--metrics-require-loopback` off
/// the route stays open.
pub fn metrics_scrape_allowed(
    metrics_require_loopback: bool,
    authenticated_at_gate: bool,
    peer: Option<std::net::IpAddr>,
    host_header: Option<&str>,
) -> bool {
    if !metrics_require_loopback {
        return true;
    }
    if authenticated_at_gate {
        return true;
    }
    // Prometheus scrapes with a plain GET whose `Host:` names the server
    // address, so peer and header must both be loopback.
    peer.is_some_and(|ip| ip.is_loopback()) && is_loopback_host_header(host_header)
}

fn peer_of(req: &Request) -> Option<std::net::IpAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
}

fn host_of(req: &Request) -> Option<&str> {
    req.headers().get("host").and_then(|v| v.to_str().ok())
}

/// Route-layer gate for `/stats-history`. Denies with 404 (invisible).
pub async fn operator_read_gate(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let allowed = operator_read_allowed(
        req.extensions().get::<ProxyTokenAuthenticated>().is_some(),
        peer_of(&req),
        req.headers(),
        &state.trusted_gateway_cidrs,
        &state.trusted_dashboard_client_cidrs,
    );
    if allowed {
        return next.run(req).await;
    }
    StatusCode::NOT_FOUND.into_response()
}

/// Route-layer gate for `/metrics`. Denies with 404, like upstream.
pub async fn metrics_scrape_gate(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let allowed = metrics_scrape_allowed(
        state.config.metrics_require_loopback,
        req.extensions().get::<ProxyTokenAuthenticated>().is_some(),
        peer_of(&req),
        host_of(&req),
    );
    if allowed {
        return next.run(req).await;
    }
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forwarded_headers::load_trusted_gateway_cidrs;

    fn headers(host: &str) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        map.insert("host", host.parse().unwrap());
        map
    }

    fn gateway() -> Vec<crate::forwarded_headers::IpCidr> {
        load_trusted_gateway_cidrs("10.20.0.0/16").unwrap()
    }

    #[test]
    fn token_authenticated_operator_passes_from_anywhere() {
        let public: std::net::IpAddr = "203.0.113.9".parse().unwrap();
        let h = headers("proxy.example:8787");
        assert!(operator_read_allowed(
            true,
            Some(public),
            &h,
            &gateway(),
            &[]
        ));
        assert!(metrics_scrape_allowed(
            true,
            true,
            Some(public),
            Some("proxy.example:8787")
        ));
    }

    #[test]
    fn direct_loopback_passes_without_token() {
        let lo: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let h = headers("127.0.0.1:8787");
        assert!(operator_read_allowed(false, Some(lo), &h, &gateway(), &[]));
        assert!(metrics_scrape_allowed(
            true,
            false,
            Some(lo),
            Some("127.0.0.1:8787")
        ));
    }

    #[test]
    fn remote_unauthenticated_is_denied() {
        let public: std::net::IpAddr = "203.0.113.9".parse().unwrap();
        let h = headers("proxy.example:8787");
        assert!(!operator_read_allowed(
            false,
            Some(public),
            &h,
            &gateway(),
            &[]
        ));
        assert!(!metrics_scrape_allowed(
            true,
            false,
            Some(public),
            Some("proxy.example:8787")
        ));
    }

    #[test]
    fn loopback_peer_with_foreign_host_is_denied() {
        // DNS-rebinding shape: the peer is local but `Host:` is not.
        let lo: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        assert!(!metrics_scrape_allowed(
            true,
            false,
            Some(lo),
            Some("proxy.example:8787")
        ));
    }

    #[test]
    fn metrics_flag_off_leaves_route_open() {
        let public: std::net::IpAddr = "203.0.113.9".parse().unwrap();
        assert!(metrics_scrape_allowed(
            false,
            false,
            Some(public),
            Some("proxy.example:8787")
        ));
        assert!(metrics_scrape_allowed(false, false, None, None));
    }

    #[test]
    fn missing_peer_fails_closed() {
        let h = headers("127.0.0.1:8787");
        assert!(!operator_read_allowed(false, None, &h, &gateway(), &[]));
        assert!(!metrics_scrape_allowed(
            true,
            false,
            None,
            Some("127.0.0.1:8787")
        ));
    }
}

//! Bind-adjacent gates: GET-only health exemption and operator-route gating.
//!
//! Ports of upstream `d99779da` (only GET health probes skip the proxy
//! token) and `ee731d7e` (the `/stats-history` and `/metrics` operator
//! routes answer loopback and token-verified callers, not the network).
//! The harness always connects from loopback, which the auth gate trusts,
//! so the token tests pose as relayed remote callers instead: the proxy
//! trusts `X-Forwarded-For` from a configured gateway CIDR, and a relayed
//! request is never trusted without the token. The operator routes stay
//! reachable locally with a token configured.

mod common;

use common::{start_proxy_with, start_proxy_with_state};
use headroom_proxy::forwarded_headers::load_trusted_gateway_cidrs;

const TOKEN: &str = "operator-gate-test-token";

async fn token_proxy(upstream: &str) -> common::ProxyHandle {
    start_proxy_with(upstream, |c| {
        c.proxy_token = Some(TOKEN.to_string());
    })
    .await
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// GET probes skip the token; other methods on the same paths must prove
/// it. Posed as a relayed remote caller (see module docs): GET passes, POST
/// gets 401.
#[tokio::test]
async fn only_get_health_probes_skip_the_token() {
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:9",
        |c| {
            c.proxy_token = Some(TOKEN.to_string());
        },
        |mut s| {
            s.trusted_gateway_cidrs = load_trusted_gateway_cidrs("127.0.0.0/8").expect("test CIDR");
            s
        },
    )
    .await;

    for path in ["/healthz", "/health", "/livez", "/readyz"] {
        let get = client()
            .get(format!("{}{path}", proxy.url()))
            .header("x-forwarded-for", "203.0.113.9")
            .send()
            .await
            .unwrap();
        assert_eq!(get.status(), 200, "{path}");

        let post = client()
            .post(format!("{}{path}", proxy.url()))
            .header("x-forwarded-for", "203.0.113.9")
            .send()
            .await
            .unwrap();
        assert_eq!(post.status(), 401, "{path}");
    }

    proxy.shutdown().await;
}

/// The operator routes stay reachable from loopback with a token set.
#[tokio::test]
async fn operator_routes_answer_loopback_with_token_configured() {
    let upstream = wiremock::MockServer::start().await;
    let proxy = token_proxy(&upstream.uri()).await;

    let history = client()
        .get(format!("{}/stats-history", proxy.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(history.status(), 200);

    let metrics = client()
        .get(format!("{}/metrics", proxy.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), 200);

    proxy.shutdown().await;
}

/// A token-verified caller reaches the operator routes, even posed as a
/// relayed remote caller. Without the gate's authenticated-at-gate mark
/// this same request would be denied (the relayed address is outside the
/// dashboard chain), so the 200 pins the extension plumbing end to end.
#[tokio::test]
async fn token_verified_caller_reaches_operator_routes() {
    let upstream = wiremock::MockServer::start().await;
    let proxy = start_proxy_with_state(
        &upstream.uri(),
        |c| {
            c.proxy_token = Some(TOKEN.to_string());
        },
        |mut s| {
            s.trusted_gateway_cidrs = load_trusted_gateway_cidrs("127.0.0.0/8").expect("test CIDR");
            s
        },
    )
    .await;

    let history = client()
        .get(format!("{}/stats-history", proxy.url()))
        .header("x-forwarded-for", "203.0.113.9")
        .header("x-headroom-proxy-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(history.status(), 200);

    proxy.shutdown().await;
}

/// Without a token configured the auth gate passes everything through, so
/// the operator gate is what stands between a relayed remote caller and
/// spend history: it answers 404 (invisible, like the other operator
/// routes).
#[tokio::test]
async fn operator_route_is_invisible_to_relayed_remote_without_token() {
    let upstream = wiremock::MockServer::start().await;
    let proxy = start_proxy_with_state(
        &upstream.uri(),
        |_| {},
        |mut s| {
            s.trusted_gateway_cidrs = load_trusted_gateway_cidrs("127.0.0.0/8").expect("test CIDR");
            s
        },
    )
    .await;

    // Posed as a relayed remote caller behind a non-loopback Host: the
    // loopback-peer short-circuit no longer applies, so the operator gate
    // itself answers 404 (invisible, like the other operator routes).
    let denied = client()
        .get(format!("{}/stats-history", proxy.url()))
        .header("host", "proxy.example:8787")
        .header("x-forwarded-for", "203.0.113.9")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);

    // And direct loopback still reads it: the default single-user setup is
    // unaffected. (The loopback-peer short-circuit applies here because
    // `Host:` names loopback too, which is why the deny leg above overrides
    // it.)
    let local = client()
        .get(format!("{}/stats-history", proxy.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(local.status(), 200);

    proxy.shutdown().await;
}

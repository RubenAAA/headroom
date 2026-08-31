//! `HEADROOM_PROXY_TOKEN` is enforced on every route, from every transport.
//!
//! The flag was parsed into config and read by nothing, so these tests exist
//! to keep the wiring attached: a gate that is only unit-tested at its token
//! reader would pass while the layer sits unmounted.
//!
//! Requests are driven straight at the router with a spoofed `ConnectInfo`,
//! because the test harness binds `127.0.0.1` and every caller would otherwise
//! be exempt as loopback — which is exactly the case that must not be the only
//! one covered.

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use headroom_proxy::config::Config;
use headroom_proxy::proxy::{build_app, AppState};
use std::net::SocketAddr;
use tower::ServiceExt;

/// A caller that is not on loopback.
const REMOTE: &str = "203.0.113.7:51234";

fn app_with_token(token: Option<&str>) -> axum::Router {
    let mut config = Config::for_test("http://127.0.0.1:1/".parse().expect("url"));
    config.proxy_token = token.map(str::to_string);
    let state = AppState::new(config).expect("app state");
    build_app(state)
}

fn app_with_token_behind_loopback_gateway(token: &str) -> axum::Router {
    let mut config = Config::for_test("http://127.0.0.1:1/".parse().expect("url"));
    config.proxy_token = Some(token.to_string());
    let mut state = AppState::new(config).expect("app state");
    state.trusted_gateway_cidrs =
        headroom_proxy::forwarded_headers::load_trusted_gateway_cidrs("127.0.0.0/8").expect("CIDR");
    build_app(state)
}

fn remote_request(path: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().uri(path).method("GET");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::empty()).expect("request");
    let addr: SocketAddr = REMOTE.parse().expect("addr");
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

#[tokio::test]
async fn a_remote_caller_without_a_token_is_rejected() {
    let resp = app_with_token(Some("s3cret"))
        .oneshot(remote_request("/v1/messages", &[]))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_remote_caller_with_the_wrong_token_is_rejected() {
    let resp = app_with_token(Some("s3cret"))
        .oneshot(remote_request(
            "/v1/messages",
            &[("authorization", "Bearer wrong")],
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// The gate must let the request through to the handler. The upstream is a
/// dead port, so anything other than 401 proves the gate stopped blocking.
#[tokio::test]
async fn a_remote_caller_with_the_right_token_passes_the_gate() {
    let resp = app_with_token(Some("s3cret"))
        .oneshot(remote_request(
            "/v1/messages",
            &[("authorization", "Bearer s3cret")],
        ))
        .await
        .expect("response");
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_dedicated_header_also_authenticates() {
    let resp = app_with_token(Some("s3cret"))
        .oneshot(remote_request(
            "/v1/messages",
            &[("x-headroom-proxy-token", "s3cret")],
        ))
        .await
        .expect("response");
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Authorization often carries the provider's own bearer credential. It must
/// not shadow a correct proxy credential in the dedicated carrier.
#[tokio::test]
async fn provider_bearer_does_not_shadow_the_dedicated_proxy_token() {
    let resp = app_with_token(Some("s3cret"))
        .oneshot(remote_request(
            "/v1/messages",
            &[
                ("authorization", "Bearer provider-key"),
                ("x-headroom-proxy-token", "s3cret"),
            ],
        ))
        .await
        .expect("response");
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// An orchestrator probing a container that binds a non-loopback interface is
/// not authenticated and must not have to be.
#[tokio::test]
async fn health_probes_stay_open() {
    for path in ["/healthz", "/health", "/livez", "/readyz"] {
        let resp = app_with_token(Some("s3cret"))
            .oneshot(remote_request(path, &[]))
            .await
            .expect("response");
        assert_ne!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{path} must answer without a token"
        );
    }
}

/// The default single-user setup configures no token, and must keep working
/// exactly as it did before the gate existed.
#[tokio::test]
async fn no_configured_token_means_no_gate() {
    let resp = app_with_token(None)
        .oneshot(remote_request("/v1/messages", &[]))
        .await
        .expect("response");
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// The operator on the box is the trust boundary the admin and debug routes
/// already use; the gate must agree with them.
#[tokio::test]
async fn a_loopback_caller_is_exempt() {
    let mut req = Request::builder()
        .uri("/v1/messages")
        .method("GET")
        .body(Body::empty())
        .expect("request");
    let addr: SocketAddr = "127.0.0.1:51234".parse().expect("addr");
    req.extensions_mut().insert(ConnectInfo(addr));

    let resp = app_with_token(Some("s3cret"))
        .oneshot(req)
        .await
        .expect("response");
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// When loopback is explicitly configured as a trusted gateway, the remote
/// address it forwards is the caller. Otherwise every reverse-proxied request
/// would inherit the gateway's loopback exemption.
#[tokio::test]
async fn a_remote_caller_behind_a_trusted_loopback_gateway_is_not_exempt() {
    let mut req = Request::builder()
        .uri("/v1/messages")
        .method("GET")
        .header("x-forwarded-for", "203.0.113.7")
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:51234".parse::<SocketAddr>().expect("addr"),
    ));

    let resp = app_with_token_behind_loopback_gateway("s3cret")
        .oneshot(req)
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A WebSocket upgrade is answered by the catch-all, so it passes through the
/// same layer as any other request. Upstream's Python proxy needed a second
/// middleware for this: Starlette hands a non-`http` scope straight to the app,
/// so its `/v1/responses` POST was authenticated while the upgrade on that
/// same path was not.
#[tokio::test]
async fn a_websocket_upgrade_is_gated_too() {
    let resp = app_with_token(Some("s3cret"))
        .oneshot(remote_request(
            "/v1/live",
            &[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-version", "13"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ],
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

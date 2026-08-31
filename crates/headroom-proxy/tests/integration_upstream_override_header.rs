//! `x-headroom-base-url` per-request upstream override on the Anthropic
//! Messages route (`POST /v1/messages`) and other passthrough routes.
//!
//! Mirrors the Python fix in `headroom/providers/proxy_routes.py`
//! (commit bb2acf70, #1763): when the header is present it is trimmed and
//! a trailing `/` is stripped, then used as the upstream base; an absent,
//! empty, or whitespace-only value falls through to the configured
//! upstream. In the Rust port the override is resolved centrally in
//! `forward_http`, so it covers `/v1/messages` and the generic passthrough
//! routes alike.

mod common;

use common::{start_proxy, start_proxy_with};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn messages_body() -> serde_json::Value {
    json!({
        "model": "glm-5.2",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// Permit the loopback mock servers these tests point the override at.
///
/// WEB-01 makes the default policy reject a client-chosen upstream that
/// resolves to loopback or private space, which is what a wiremock server is.
/// A bare host allowlists every safe scheme and port for that host, so one
/// value serves every test here and setting it twice is harmless — which
/// matters because the env is process-global and these tests run in parallel.
fn allow_loopback_overrides() {
    std::env::set_var("HEADROOM_ALLOWED_BASE_URLS", "127.0.0.1,localhost");
}

fn localhost_url(server: &MockServer) -> String {
    let mut url = url::Url::parse(&server.uri()).unwrap();
    url.set_host(Some("localhost")).unwrap();
    url.to_string().trim_end_matches('/').to_string()
}

/// Header present → the request is forwarded to the overridden upstream,
/// not the configured default upstream.
#[tokio::test]
async fn header_present_routes_to_override_upstream() {
    allow_loopback_overrides();
    let default_upstream = MockServer::start().await;
    let override_upstream = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "override"})))
        .expect(1)
        .mount(&override_upstream)
        .await;
    // The default upstream must NOT be hit.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "default"})))
        .expect(0)
        .mount(&default_upstream)
        .await;

    let proxy = start_proxy(&default_upstream.uri()).await;

    // Resolve a hostname rather than handing the guard a literal address. The
    // caller transport must connect to the exact loopback addresses approved
    // under the explicit test allowlist while retaining `localhost` in the URL.
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "gateway-key")
        .header("x-headroom-base-url", localhost_url(&override_upstream))
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "override");

    proxy.shutdown().await;
}

/// Header absent → the configured default upstream is used.
#[tokio::test]
async fn header_absent_uses_default_upstream() {
    let default_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "default"})))
        .expect(1)
        .mount(&default_upstream)
        .await;

    let proxy = start_proxy(&default_upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-test")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "default");

    proxy.shutdown().await;
}

/// Empty or whitespace-only header → default upstream (must not blank the
/// upstream or error the request).
#[tokio::test]
async fn empty_or_whitespace_header_uses_default_upstream() {
    for value in ["", "   "] {
        let default_upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "default"})))
            .expect(1)
            .mount(&default_upstream)
            .await;

        let proxy = start_proxy(&default_upstream.uri()).await;

        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", proxy.url()))
            .header("x-api-key", "sk-ant-test")
            .header("x-headroom-base-url", value)
            .json(&messages_body())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "value = {value:?}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["id"], "default", "value = {value:?}");

        proxy.shutdown().await;
    }
}

/// A trailing `/` on the header value is stripped before the path is
/// joined, so the override still targets `<base>/v1/messages` exactly once
/// (no doubled slash, correct upstream).
#[tokio::test]
async fn trailing_slash_is_stripped() {
    allow_loopback_overrides();
    let override_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "override"})))
        .expect(1)
        .mount(&override_upstream)
        .await;

    let default_upstream = MockServer::start().await;
    let proxy = start_proxy(&default_upstream.uri()).await;

    // wiremock's `.uri()` has no trailing slash; append one to exercise
    // the strip logic (and surrounding whitespace for good measure).
    let header_value = format!("  {}/  ", override_upstream.uri());
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "gateway-key")
        .header("x-headroom-base-url", header_value)
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "override");

    proxy.shutdown().await;
}

/// Redirects from a caller-selected endpoint are returned to the caller rather
/// than followed by the proxy. Otherwise a public endpoint could redirect the
/// transport to a private service after the original destination was pinned.
#[tokio::test]
async fn override_redirect_is_not_followed() {
    allow_loopback_overrides();
    let override_upstream = MockServer::start().await;
    let redirect_target = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", redirect_target.uri()))
        .expect(1)
        .mount(&override_upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&redirect_target)
        .await;

    let default_upstream = MockServer::start().await;
    let proxy = start_proxy(&default_upstream.uri()).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let response = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "gateway-key")
        .header("x-headroom-base-url", override_upstream.uri())
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);

    proxy.shutdown().await;
}

/// Provider proxy configuration belongs to trusted operator destinations. A
/// caller-selected target must connect through its pinned address set instead
/// of delegating target resolution to that proxy.
#[tokio::test]
async fn override_bypasses_the_provider_http_proxy() {
    allow_loopback_overrides();
    let override_upstream = MockServer::start().await;
    let provider_proxy = MockServer::start().await;
    let default_upstream = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "override"})))
        .expect(1)
        .mount(&override_upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&provider_proxy)
        .await;

    let provider_proxy_url = provider_proxy.uri();
    let proxy = start_proxy_with(&default_upstream.uri(), |config| {
        config.http_proxy = Some(provider_proxy_url);
    })
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "gateway-key")
        .header("x-headroom-base-url", override_upstream.uri())
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["id"],
        "override"
    );

    proxy.shutdown().await;
}

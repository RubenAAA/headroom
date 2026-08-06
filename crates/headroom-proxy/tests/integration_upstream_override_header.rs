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

use common::start_proxy;
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

/// Header present → the request is forwarded to the overridden upstream,
/// not the configured default upstream.
#[tokio::test]
async fn header_present_routes_to_override_upstream() {
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

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "gateway-key")
        .header("x-headroom-base-url", override_upstream.uri())
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

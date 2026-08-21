//! WEB-01: a client-chosen upstream that resolves inward is ignored.
//!
//! `x-headroom-base-url` is client-controlled, so honouring a destination in
//! private or loopback space would let any caller read back whatever the proxy
//! can reach and it cannot — cloud metadata, an internal admin port. Python's
//! `headroom/proxy/upstream_guard.py` resolves the host and refuses; this
//! covers the Rust port of that policy on the request path.
//!
//! Its own test binary on purpose. `HEADROOM_ALLOWED_BASE_URLS` is
//! process-global, and `integration_upstream_override_header.rs` sets it to
//! permit exactly the loopback destinations this file needs refused.

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

/// An override pointing at loopback is dropped and the configured upstream
/// answers instead. The request still succeeds: refusing the override is not
/// grounds for failing the turn, and it matches the Python handler.
#[tokio::test]
async fn loopback_override_is_ignored_in_favour_of_the_configured_upstream() {
    assert!(
        std::env::var("HEADROOM_ALLOWED_BASE_URLS").is_err(),
        "the allowlist must be unset here or the guard under test is bypassed"
    );

    let default_upstream = MockServer::start().await;
    let internal_target = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "default"})))
        .expect(1)
        .mount(&default_upstream)
        .await;
    // The stand-in for an internal service. It must never be reached.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "internal"})))
        .expect(0)
        .mount(&internal_target)
        .await;

    let proxy = start_proxy(&default_upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "gateway-key")
        .header("x-headroom-base-url", internal_target.uri())
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["id"], "default",
        "a loopback override must not be followed"
    );

    proxy.shutdown().await;
}

/// The cloud-metadata address, the case the guard exists for.
#[tokio::test]
async fn metadata_address_override_is_ignored() {
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
        .header("x-api-key", "gateway-key")
        .header("x-headroom-base-url", "http://169.254.169.254")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "default");

    proxy.shutdown().await;
}

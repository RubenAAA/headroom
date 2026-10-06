//! `x-headroom-base-url` per-request upstream override on the native Gemini
//! routes (`POST /v1beta/models/*model_action`).
//!
//! Mirrors the Python fix in `headroom/providers/route_specs.py`
//! (commit 0ad996a8, #2697): plugin-originated Gemini traffic reaches the
//! tagged gateway and runs the same native transforms as direct Gemini
//! traffic, instead of falling through to a generic path. A present-but-unsafe
//! (or malformed) base stops with 400 before any handler connects to it;
//! an absent, empty, or whitespace-only value falls back to the configured
//! upstream. The batch route keeps its documented passthrough semantics.

// Edition 2024 makes std::env::set_var and remove_var unsafe. Tests call them
// to set up config; non-test code stays free of unsafe.
#![allow(unsafe_code, clippy::undocumented_unsafe_blocks)]

mod common;

use common::start_proxy;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn gemini_body() -> serde_json::Value {
    json!({
        "contents": [
            {"role": "user", "parts": [{"text": "hello there, this is a plugin test message"}]}
        ]
    })
}

/// Permit the loopback mock servers these tests point the override at.
/// Same policy note as `integration_upstream_override_header.rs`: WEB-01
/// rejects loopback caller upstreams by default, so allowlist it once per
/// process (setting it twice is harmless).
fn allow_loopback_overrides() {
    unsafe { std::env::set_var("HEADROOM_ALLOWED_BASE_URLS", "127.0.0.1,localhost") };
}

/// Plugin traffic with the override header reaches the tagged upstream
/// through the native route — and runs the native transform pipeline
/// (token accounting headers) on the way.
#[tokio::test]
async fn plugin_generate_content_reaches_tagged_upstream_through_native_route() {
    allow_loopback_overrides();
    let default_upstream = MockServer::start().await;
    let override_upstream = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{"content": {"parts": [{"text": "tagged hello"}]}}],
            "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 3}
        })))
        .expect(1)
        .mount(&override_upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "default"})))
        .expect(0)
        .mount(&default_upstream)
        .await;

    let proxy = start_proxy(&default_upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/v1beta/models/gemini-2.0-flash:generateContent?key=test-key",
            proxy.url()
        ))
        .header("x-headroom-base-url", override_upstream.uri())
        .json(&gemini_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let tokens_before = resp
        .headers()
        .get("x-headroom-tokens-before")
        .expect("native pipeline token header")
        .to_str()
        .unwrap()
        .to_string();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "tagged hello"
    );
    assert!(tokens_before.parse::<usize>().is_ok());

    // The override body survives the native round-trip untouched.
    let received = &override_upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let forwarded: serde_json::Value = received[0].body_json().unwrap();
    assert_eq!(
        forwarded["contents"][0]["parts"][0]["text"],
        "hello there, this is a plugin test message"
    );

    proxy.shutdown().await;
}

/// Same for the streaming action: SSE reaches the caller-selected base
/// through the native streaming transforms.
#[tokio::test]
async fn plugin_stream_generate_content_streams_through_native_transforms() {
    allow_loopback_overrides();
    let default_upstream = MockServer::start().await;
    let override_upstream = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"tagged chunk\"}]}}]}\n\n",
                ),
        )
        .expect(1)
        .mount(&override_upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&default_upstream)
        .await;

    let proxy = start_proxy(&default_upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/v1beta/models/gemini-2.0-flash:streamGenerateContent?key=test-key",
            proxy.url()
        ))
        .header("x-headroom-base-url", override_upstream.uri())
        .json(&gemini_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-headroom-tokens-before").is_some());
    let text = resp.text().await.unwrap();
    assert!(text.contains("tagged chunk"), "unexpected body: {text}");

    proxy.shutdown().await;
}

/// An unsafe caller-selected base stops with 400 before any handler
/// connects to it — neither the tagged base nor the default sees traffic.
#[tokio::test]
async fn unsafe_custom_base_rejected_before_handler() {
    allow_loopback_overrides();
    let default_upstream = MockServer::start().await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "default"})))
        .expect(0)
        .mount(&default_upstream)
        .await;

    let proxy = start_proxy(&default_upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/v1beta/models/gemini-2.0-flash:generateContent?key=test-key",
            proxy.url()
        ))
        .header("x-headroom-base-url", "http://169.254.169.254/")
        .json(&gemini_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    proxy.shutdown().await;
}

/// countTokens follows the same tagged upstream as the other native actions.
#[tokio::test]
async fn count_tokens_follows_the_tagged_upstream() {
    allow_loopback_overrides();
    let default_upstream = MockServer::start().await;
    let override_upstream = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:countTokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"totalTokens": 42})))
        .expect(1)
        .mount(&override_upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "default"})))
        .expect(0)
        .mount(&default_upstream)
        .await;

    let proxy = start_proxy(&default_upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/v1beta/models/gemini-2.0-flash:countTokens?key=test-key",
            proxy.url()
        ))
        .header("x-headroom-base-url", override_upstream.uri())
        .json(&gemini_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    proxy.shutdown().await;
}

/// Without the header, Gemini traffic stays on the configured default
/// upstream — the opt-in changes nothing for direct traffic.
#[tokio::test]
async fn generate_content_without_header_stays_on_default_upstream() {
    allow_loopback_overrides();
    let default_upstream = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{"content": {"parts": [{"text": "default hello"}]}}],
            "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 3}
        })))
        .expect(1)
        .mount(&default_upstream)
        .await;

    let proxy = start_proxy(&default_upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/v1beta/models/gemini-2.0-flash:generateContent?key=test-key",
            proxy.url()
        ))
        .json(&gemini_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "default hello"
    );

    proxy.shutdown().await;
}

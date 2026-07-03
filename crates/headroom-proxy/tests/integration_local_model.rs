//! Integration tests for local model routing.
//!
//! When `local_model` is not configured, `/v1/messages` passes through
//! to the upstream transparently. When configured, matching requests
//! are translated to OpenAI format and forwarded to the local upstream.

mod common;

use common::start_proxy_with;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// When local_model is not configured, /v1/messages passes through
/// to the upstream transparently (no format translation).
#[tokio::test]
async fn passthrough_when_local_model_disabled() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "hello"}],
                "model": "claude-3-5-sonnet-20241022",
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let proxy = start_proxy_with(mock.uri().as_str(), |_| {}).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "hello");

    proxy.shutdown().await;
}

/// When local_model IS configured but the model doesn't match,
/// the request falls through to the default upstream transparently.
#[tokio::test]
async fn non_matching_model_falls_through() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "from upstream"}],
                "model": "claude-3-5-sonnet-20241022",
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let proxy = start_proxy_with(mock.uri().as_str(), |cfg| {
        cfg.local_model = Some("qwen36-uncensored".to_string());
        cfg.local_upstream = Some("http://127.0.0.1:19999".parse().unwrap());
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "from upstream");

    proxy.shutdown().await;
}

//! Integration tests for local model routing.
//!
//! When `local_model` is not configured, `/v1/messages` passes through
//! to the upstream transparently. When configured, matching requests
//! are translated to OpenAI format and forwarded to the local upstream.

mod common;

use common::start_proxy_with;
use headroom_proxy::config::ModelRoute;
use serde_json::json;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// When local_model is not configured, /v1/messages passes through
/// to the upstream transparently (no format translation).
#[tokio::test]
async fn passthrough_when_local_model_disabled() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let proxy = start_proxy_with(mock.uri().as_str(), |_| {}).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-key")
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
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "from upstream"}],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
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

async fn codex_responses_sse_upstream() -> MockServer {
    let mock = MockServer::start().await;

    let response_body = [
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_test\",\"model\":\"gpt-5.5\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"hello from responses\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
    ]
    .join("");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("cache-control", "no-cache")
                .set_body_string(response_body),
        )
        .mount(&mock)
        .await;

    mock
}

/// Gateway discovery should expose only exact-match routed models with
/// discoverable IDs, so Claude Code can populate its model picker.
#[tokio::test]
async fn gateway_model_discovery_lists_discoverable_routes() {
    let mock = MockServer::start().await;

    let proxy = start_proxy_with(mock.uri().as_str(), |cfg| {
        cfg.local_model = Some("claude-local".to_string());
        cfg.model_routes = vec![
            ModelRoute {
                model_prefix: "claude-codex-5.5".to_string(),
                prefix_match: false,
                upstream: Some(Url::parse("https://api.openai.com/v1").unwrap()),
                translate: true,
                mimo_run: None,
                target_model: Some("gpt-5.5".to_string()),
            },
            ModelRoute {
                model_prefix: "codex-*".to_string(),
                prefix_match: true,
                upstream: Some(Url::parse("https://api.openai.com/v1").unwrap()),
                translate: true,
                mimo_run: None,
                target_model: Some("gpt-5.5".to_string()),
            },
            ModelRoute {
                model_prefix: "anthropic-mimo".to_string(),
                prefix_match: false,
                upstream: None,
                translate: false,
                mimo_run: Some("MiMo-V2.5".to_string()),
                target_model: None,
            },
        ];
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/v1/models", proxy.url()))
        .header("x-api-key", "test-key")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let ids: Vec<String> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect();

    assert!(ids.contains(&"claude-local".to_string()));
    assert!(ids.contains(&"claude-codex-5.5".to_string()));
    assert!(ids.contains(&"anthropic-mimo".to_string()));
    assert!(!ids.contains(&"codex-*".to_string()));

    proxy.shutdown().await;
}

/// Codex-targeted translate routes should route to OpenAI Responses
/// instead of Chat Completions, so the upstream path matches the
/// Codex/OpenAI surface.
#[tokio::test]
async fn codex_translate_route_uses_responses_endpoint() {
    let mock = codex_responses_sse_upstream().await;
    let upstream_url = Url::parse(&mock.uri()).unwrap();

    let proxy = start_proxy_with(&mock.uri(), |cfg| {
        cfg.model_routes = vec![ModelRoute {
            model_prefix: "claude-codex-5.5".to_string(),
            prefix_match: false,
            upstream: Some(upstream_url.clone()),
            translate: true,
            mimo_run: None,
            target_model: Some("gpt-5.5".to_string()),
        }];
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let body: String = resp.text().await.unwrap();
    assert!(body.contains("message_start"));
    assert!(body.contains("hello from responses"));

    let received_requests = mock.received_requests().await.unwrap();
    let upstream_body = received_requests
        .last()
        .expect("upstream request")
        .body
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&upstream_body).unwrap();
    assert_eq!(parsed["model"], "gpt-5.5");
    assert_eq!(parsed["store"], false);
    assert_eq!(parsed["stream"], true);
    assert!(parsed.get("max_output_tokens").is_none());
    assert!(parsed.get("max_tokens").is_none());
    assert!(parsed.get("temperature").is_none());
    assert!(parsed.get("messages").is_none());
    assert_eq!(parsed["input"][0]["role"], "user");

    proxy.shutdown().await;
}

/// Non-stream Codex requests still need to validate as normal Anthropic
/// JSON, even though the upstream transport is streamed Responses SSE.
#[tokio::test]
async fn codex_translate_route_buffers_non_stream_responses() {
    let mock = codex_responses_sse_upstream().await;
    let upstream_url = Url::parse(&mock.uri()).unwrap();

    let proxy = start_proxy_with(&mock.uri(), |cfg| {
        cfg.model_routes = vec![ModelRoute {
            model_prefix: "claude-codex-5.5".to_string(),
            prefix_match: false,
            upstream: Some(upstream_url.clone()),
            translate: true,
            mimo_run: None,
            target_model: Some("gpt-5.5".to_string()),
        }];
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "stream": false,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["usage"]["input_tokens"], 5);
    assert_eq!(body["usage"]["output_tokens"], 2);
    assert_eq!(body["content"][0]["text"], "hello from responses");

    proxy.shutdown().await;
}

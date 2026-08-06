//! Azure AI Foundry route: `/anthropic/v1/messages` normalization,
//! upstream override, auth-header passthrough, streaming + non-streaming.
//!
//! Mirrors the Python behavior in
//! `headroom/providers/proxy_routes.py::foundry_anthropic_messages`
//! (path normalized to `/v1/messages`, forwarded to the resolved
//! Anthropic/Foundry target) and the wiring covered by
//! `tests/test_azure_foundry_claude_compression.py`.

mod common;

use common::{start_proxy, start_proxy_with};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn messages_body() -> serde_json::Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hello foundry"}]
    })
}

/// Non-streaming: the Foundry route strips the `/anthropic` prefix,
/// joins `/v1/messages` onto the Foundry base URL (which itself ends
/// in `/anthropic`, matching the real Azure AI Services shape), and
/// forwards the body byte-equal with the Foundry `api-key` header
/// intact. The global `--upstream` is the same server WITHOUT the
/// `/anthropic` base path — only `/anthropic/v1/messages` is mounted,
/// so a hit proves the override was applied.
#[tokio::test]
async fn foundry_route_normalizes_path_and_forwards_api_key_auth() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .and(header("api-key", "azure-key-123"))
        .and(body_json(messages_body()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("anthropic-organization-id", "org-1")
                .set_body_json(json!({"id": "msg_1", "type": "message"})),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let foundry_base = format!("{}/anthropic", upstream.uri());
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.foundry_base_url = Some(foundry_base.parse().unwrap());
    })
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/anthropic/v1/messages", proxy.url()))
        .header("api-key", "azure-key-123")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("anthropic-organization-id").unwrap(),
        "org-1"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "msg_1");

    proxy.shutdown().await;
}

/// AAD auth mode: an `Authorization: Bearer <token>` header (Entra ID
/// token instead of a Foundry api-key) passes through verbatim —
/// mirrors Python, which does no Foundry-specific auth handling.
#[tokio::test]
async fn foundry_route_forwards_aad_bearer_auth() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .and(header("authorization", "Bearer aad-token-xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "msg_2"})))
        .expect(1)
        .mount(&upstream)
        .await;

    let foundry_base = format!("{}/anthropic", upstream.uri());
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.foundry_base_url = Some(foundry_base.parse().unwrap());
    })
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/anthropic/v1/messages", proxy.url()))
        .header("authorization", "Bearer aad-token-xyz")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    proxy.shutdown().await;
}

/// Streaming: an SSE response from the Foundry upstream streams back
/// through the shared `forward_http` pipeline unchanged.
#[tokio::test]
async fn foundry_route_streams_sse_response() {
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\"}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_body.as_bytes(), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let foundry_base = format!("{}/anthropic", upstream.uri());
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.foundry_base_url = Some(foundry_base.parse().unwrap());
    })
    .await;

    let mut body = messages_body();
    body["stream"] = json!(true);
    let resp = reqwest::Client::new()
        .post(format!("{}/anthropic/v1/messages", proxy.url()))
        .header("api-key", "azure-key-123")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );
    assert_eq!(resp.text().await.unwrap(), sse_body);

    proxy.shutdown().await;
}

/// Without a configured Foundry upstream, the route still normalizes
/// the path and forwards to the global `--upstream` — the same
/// fallback Python has, where `/anthropic/v1/messages` targets the
/// resolved Anthropic API target.
#[tokio::test]
async fn foundry_route_falls_back_to_global_upstream_when_unconfigured() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_json(messages_body()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "msg_3"})))
        .expect(1)
        .mount(&upstream)
        .await;

    let proxy = start_proxy(&upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/anthropic/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-test")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    proxy.shutdown().await;
}

/// The Foundry override is scoped to the Foundry route: other paths
/// keep forwarding to the global `--upstream` even when
/// `foundry_base_url` is set.
#[tokio::test]
async fn foundry_override_does_not_affect_other_routes() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "msg_4"})))
        .expect(1)
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        // Dead-end Foundry upstream: if the override leaked onto the
        // plain Anthropic route, this request would fail to connect.
        c.foundry_base_url = Some("http://127.0.0.1:9/anthropic".parse().unwrap());
    })
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-test")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    proxy.shutdown().await;
}

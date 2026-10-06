//! Semantic response cache is partitioned by inbound credential.
//!
//! Port of the upstream Python fix `ee6cfccc` (#3862): on a proxy shared
//! by several callers the cache key must include who is asking, or the
//! second caller receives the completion generated for the first caller's
//! provider key. Two `POST /v1/messages` with identical bodies but
//! different `x-api-key` values must each reach the upstream; repeating
//! the same key must serve the second turn from the cache.

mod common;

use common::start_proxy_with;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn messages_body() -> serde_json::Value {
    json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hello"}]
    })
}

/// The partition only applies on the intercept branch (the proxy keeps the
/// inbound headers only when compression is on), so these tests run with
/// compression enabled. Replay and memory stay off under `for_test`
/// defaults, leaving the body untouched.
async fn start_partition_proxy(upstream: &str) -> common::ProxyHandle {
    start_proxy_with(upstream, |c| {
        c.compression = true;
    })
    .await
}

fn upstream_reply() -> serde_json::Value {
    json!({
        "id": "msg_partition",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "hello back"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3}
    })
}

async fn post_messages(proxy_base: &str, api_key: &str) -> serde_json::Value {
    let resp = reqwest::Client::new()
        .post(format!("{proxy_base}/v1/messages"))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

/// Same body, different credential → two upstream calls (no shared entry).
#[tokio::test]
async fn different_credentials_do_not_share_cache_entry() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_reply()))
        .expect(2)
        .mount(&upstream)
        .await;

    let proxy = start_partition_proxy(&upstream.uri()).await;

    let first = post_messages(&proxy.url(), "key-A").await;
    assert_eq!(first["content"][0]["text"], "hello back");
    let second = post_messages(&proxy.url(), "key-B").await;
    assert_eq!(second["content"][0]["text"], "hello back");

    proxy.shutdown().await;
}

/// Same body, same credential → second turn served from cache (one call).
#[tokio::test]
async fn same_credential_hits_cache() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_reply()))
        .expect(1)
        .mount(&upstream)
        .await;

    let proxy = start_partition_proxy(&upstream.uri()).await;

    let first = post_messages(&proxy.url(), "key-A").await;
    assert_eq!(first["content"][0]["text"], "hello back");
    let second = post_messages(&proxy.url(), "key-A").await;
    assert_eq!(second["content"][0]["text"], "hello back");

    proxy.shutdown().await;
}

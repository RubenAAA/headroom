//! POST /v1/messages/count_tokens: routed aliases are answered locally,
//! real Anthropic models forward byte-identical (exact upstream count).

mod common;

use common::start_proxy_with;
use headroom_proxy::config::ModelRoute;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn spark_route(upstream: &str) -> ModelRoute {
    ModelRoute {
        model_prefix: "claude-muse-spark-1.3".into(),
        prefix_match: false,
        upstream: Some(upstream.parse().unwrap()),
        translate: true,
        cursor_agent: None,
        target_model: Some("muse-spark-1.3-contributor-free".into()),
        auth_env: Some("none".into()),
    }
}

fn count_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "system": "You are helpful.",
        "messages": [
            {"role": "user", "content": "hello world, this is a test message"},
        ],
    })
}

fn cursor_route() -> ModelRoute {
    ModelRoute {
        model_prefix: "claude-grok-4.6".into(),
        prefix_match: false,
        upstream: None,
        translate: false,
        cursor_agent: Some("grok-4.6".into()),
        target_model: None,
        auth_env: None,
    }
}

#[tokio::test]
async fn count_tokens_routed_alias_answered_locally() {
    let upstream = MockServer::start().await;
    let proxy = start_proxy_with(&upstream.uri(), |cfg| {
        cfg.model_routes.push(spark_route(&upstream.uri()));
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", proxy.url()))
        .json(&count_body("claude-muse-spark-1.3"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let n = body["input_tokens"].as_u64().expect("input_tokens number");
    assert!(n > 0, "a real body counts for something: {body}");

    // Answered locally: the upstream never saw it (today it would 404 there).
    assert!(
        upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "routed count must not reach upstream"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn count_tokens_cursor_alias_answered_locally() {
    // Cursor routes run a subprocess: no HTTP upstream could count them, so
    // a forward would 404 on the default upstream. Must answer locally.
    let upstream = MockServer::start().await;
    let proxy = start_proxy_with(&upstream.uri(), |cfg| {
        cfg.model_routes.push(cursor_route());
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", proxy.url()))
        .json(&count_body("claude-grok-4.6"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let n = body["input_tokens"].as_u64().expect("input_tokens number");
    assert!(n > 0, "a real body counts for something: {body}");

    assert!(
        upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "cursor count must not reach upstream"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn count_tokens_unrouted_model_forwards_byte_identical() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "input_tokens": 123,
        })))
        .mount(&upstream)
        .await;
    let proxy = start_proxy_with(&upstream.uri(), |cfg| {
        cfg.model_routes.push(spark_route(&upstream.uri()));
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", proxy.url()))
        .json(&count_body("claude-sonnet-5"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["input_tokens"], 123,
        "unrouted models keep the exact upstream count: {body}"
    );
    proxy.shutdown().await;
}

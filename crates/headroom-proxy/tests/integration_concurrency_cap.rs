//! Per-conversation concurrency cap (`--max-conversation-concurrency`).
//!
//! Overlapping turns of one conversation race the provider's cache commit,
//! so past a configured number in flight on the same conversation key the
//! proxy sheds the excess with a 429 the client retries, instead of paying
//! the race on every turn. Off at 0: with no cap every turn forwards.

mod common;

use std::time::Duration;

use common::start_proxy_with;
use headroom_proxy::config::ModelRoute;
use serde_json::{json, Value};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A clean Anthropic SSE session. `message_stop` matters: only a completed
/// turn leaves the pending map, so the held-open turns below stay in flight.
fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cap\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":2048,\"cache_read_input_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

fn turn(opener: &str) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 100,
        "stream": true,
        "messages": [{"role": "user", "content": opener}],
    })
}

async fn post(client: &reqwest::Client, proxy_url: &str, body: &Value) -> reqwest::Response {
    client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-ant-cap-test")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .expect("proxy reachable")
}

async fn drain(resp: reqwest::Response) -> reqwest::StatusCode {
    let status = resp.status();
    let _ = resp.bytes().await;
    status
}

/// Third concurrent turn on one conversation sheds with a 429 carrying the
/// retry contract, while a turn on another conversation sails through and
/// the held turns complete normally once the upstream answers.
#[tokio::test]
async fn third_concurrent_turn_on_one_conversation_sheds_with_429() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body(), "text/event-stream")
                .set_delay(Duration::from_millis(1500)),
        )
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.max_conversation_concurrency = 2;
    })
    .await;
    let client = reqwest::Client::new();
    let url = proxy.url();

    // Two turns sharing an opener (hence a conversation key), held open by
    // the delayed upstream so both are provably in flight below.
    let a = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        let body = turn("same opener");
        async move { drain(post(&client, &url, &body).await).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let b = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        let body = turn("same opener");
        async move { drain(post(&client, &url, &body).await).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Third turn, same conversation: shed, fast, with the retry contract.
    let started = std::time::Instant::now();
    let shed = post(&client, &url, &turn("same opener")).await;
    let shed_took = started.elapsed();
    assert_eq!(shed.status(), 429);
    assert_eq!(
        shed.headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("1"),
        "the client retries on the header, so it must be present"
    );
    assert_eq!(
        shed.headers()
            .get("x-headroom-shed")
            .map(|v| v.to_str().unwrap()),
        Some("conversation-concurrency"),
        "a proxy shed must not read as provider throttling"
    );
    let shed_body: Value = shed.json().await.expect("json shed body");
    assert_eq!(shed_body["type"], "error");
    assert!(
        shed_body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("concurrency cap"),
        "unexpected shed body: {shed_body}"
    );
    assert!(
        shed_took < Duration::from_millis(1000),
        "shed took {shed_took:?}: it must answer without touching upstream"
    );

    // Both held turns were still in flight when the shed fired: the shed
    // decided on a real overlap, not on bookkeeping.
    assert!(
        !a.is_finished() && !b.is_finished(),
        "a held turn finished first; the overlap above proves nothing"
    );

    // Another conversation is unaffected by the cap trip.
    let other = drain(post(&client, &url, &turn("different opener")).await).await;
    assert_eq!(other, reqwest::StatusCode::OK);

    assert_eq!(a.await.expect("turn A finished"), reqwest::StatusCode::OK);
    assert_eq!(b.await.expect("turn B finished"), reqwest::StatusCode::OK);
    proxy.shutdown().await;
}

/// Cap unset: the same overlap forwards everything, preserving the
/// long-standing behavior for operators who never opt in.
#[tokio::test]
async fn unset_cap_forwards_overlapping_turns_untouched() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body(), "text/event-stream")
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |_| {}).await;
    let client = reqwest::Client::new();
    let url = proxy.url();

    let a = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        let body = turn("same opener");
        async move { drain(post(&client, &url, &body).await).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let b = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        let body = turn("same opener");
        async move { drain(post(&client, &url, &body).await).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let c = drain(post(&client, &url, &turn("same opener")).await).await;
    assert_eq!(c, reqwest::StatusCode::OK);

    assert_eq!(a.await.expect("turn A finished"), reqwest::StatusCode::OK);
    assert_eq!(b.await.expect("turn B finished"), reqwest::StatusCode::OK);
    proxy.shutdown().await;
}

/// Same contract on the routed path: the cap is enforced where the usage
/// observer parks the turn, before translation spends work on it, and the
/// client sees the identical Anthropic-shaped 429.
#[tokio::test]
async fn routed_turn_sheds_with_429_past_the_cap() {
    let mock = MockServer::start().await;
    let response_body = [
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_cap\",\"model\":\"gpt-5.5\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"hello\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_cap\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
    ]
    .join("");
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(response_body)
                .set_delay(Duration::from_millis(1500)),
        )
        .mount(&mock)
        .await;
    let upstream_url = Url::parse(&mock.uri()).unwrap();

    let proxy = start_proxy_with(&mock.uri(), |cfg| {
        cfg.max_conversation_concurrency = 2;
        cfg.model_routes = vec![ModelRoute {
            model_prefix: "claude-codex-5.5".to_string(),
            prefix_match: false,
            upstream: Some(upstream_url.clone()),
            translate: true,
            cursor_agent: None,
            target_model: Some("gpt-5.5".to_string()),
            auth_env: None,
        }];
    })
    .await;
    let client = reqwest::Client::new();
    let url = proxy.url();
    let routed_turn = |opener: &str| {
        json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": opener}],
        })
    };

    let a = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        let body = routed_turn("same opener");
        async move { drain(post(&client, &url, &body).await).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let b = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        let body = routed_turn("same opener");
        async move { drain(post(&client, &url, &body).await).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let shed = post(&client, &url, &routed_turn("same opener")).await;
    assert_eq!(shed.status(), 429);
    assert_eq!(
        shed.headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("1")
    );
    assert_eq!(
        shed.headers()
            .get("x-headroom-shed")
            .map(|v| v.to_str().unwrap()),
        Some("conversation-concurrency")
    );
    assert!(
        !a.is_finished() && !b.is_finished(),
        "a held turn finished first; the overlap above proves nothing"
    );

    assert_eq!(a.await.expect("turn A finished"), reqwest::StatusCode::OK);
    assert_eq!(b.await.expect("turn B finished"), reqwest::StatusCode::OK);
    proxy.shutdown().await;
}

//! Non-streaming `RequestOutcome` emission (cache metrics + PERF/savings
//! funnel) for buffered (non-SSE) upstream responses.
//!
//! Mirrors the Python fix in commit 85804043 (#1271, "record cache metrics
//! for non-streaming backend paths"), but the Rust gap was broader: the
//! non-streaming response branch in `forward_http` never emitted a
//! `RequestOutcome` at all (streaming responses already did, via
//! `run_sse_state_machine`), so PERF/savings/cost/cache metrics were
//! silently dropped for every non-streaming intercepted request. These
//! tests pin that a non-streaming Anthropic response with cache usage
//! fields now reaches the request logger with `cache_hit = true` and the
//! right token counts, proving the outcome funnel fires on this path.

mod common;

use std::sync::{Arc, Mutex};

use common::start_proxy_with_state;
use headroom_proxy::request_logger::RequestLogger;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn messages_body() -> serde_json::Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// A non-streaming Anthropic response with `cache_read_input_tokens` > 0
/// results in a `RequestLogEntry` with `cache_hit = true` and the right
/// token counts — proving the outcome funnel now fires on the buffered
/// (non-SSE) response path, not just the streaming one.
#[tokio::test]
async fn nonstreaming_anthropic_response_records_cache_metrics() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet-20241022",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 11,
                "output_tokens": 7,
                "cache_read_input_tokens": 1226,
                "cache_creation_input_tokens": 0,
            }
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    let logger_holder: Arc<Mutex<Option<Arc<RequestLogger>>>> = Arc::new(Mutex::new(None));
    let logger_holder2 = logger_holder.clone();

    let proxy = start_proxy_with_state(
        &upstream.uri(),
        |cfg| {
            // Interception (and therefore `outcome_ctx` construction) only
            // happens on the compression-enabled path.
            cfg.compression = true;
        },
        move |state| {
            *logger_holder2.lock().unwrap() = Some(state.request_logger.clone());
            state
        },
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-test")
        .header("anthropic-version", "2023-06-01")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Drain the body so the buffered-response branch in `forward_http`
    // finishes running before we inspect the logger.
    let _ = resp.bytes().await.unwrap();

    let logger = logger_holder
        .lock()
        .unwrap()
        .clone()
        .expect("logger captured");
    let recent = logger.get_recent(1);
    assert_eq!(
        recent.len(),
        1,
        "outcome must be recorded for a non-streaming response"
    );
    let entry = &recent[0];
    assert!(
        entry.cache_hit,
        "cache_read_input_tokens > 0 in the upstream usage block must mark cache_hit"
    );
    assert_eq!(entry.output_tokens, 7);
    assert_eq!(entry.provider, "anthropic");

    proxy.shutdown().await;
}

/// A non-streaming response with no cache usage fields still records an
/// outcome (the funnel fires), just with `cache_hit = false` — the funnel
/// itself must not be gated on cache data being present.
#[tokio::test]
async fn nonstreaming_response_without_cache_usage_still_records_outcome() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test2",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet-20241022",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 11,
                "output_tokens": 4,
            }
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    let logger_holder: Arc<Mutex<Option<Arc<RequestLogger>>>> = Arc::new(Mutex::new(None));
    let logger_holder2 = logger_holder.clone();

    let proxy = start_proxy_with_state(
        &upstream.uri(),
        |cfg| {
            cfg.compression = true;
        },
        move |state| {
            *logger_holder2.lock().unwrap() = Some(state.request_logger.clone());
            state
        },
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-test")
        .header("anthropic-version", "2023-06-01")
        .json(&messages_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.unwrap();

    let logger = logger_holder
        .lock()
        .unwrap()
        .clone()
        .expect("logger captured");
    let recent = logger.get_recent(1);
    assert_eq!(
        recent.len(),
        1,
        "outcome must be recorded even with no cache usage"
    );
    let entry = &recent[0];
    assert!(!entry.cache_hit);
    assert_eq!(entry.output_tokens, 4);

    proxy.shutdown().await;
}

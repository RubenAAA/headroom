//! Integration tests for PR-E5 volatile-content detector.
//!
//! Boots a real Rust proxy in front of a wiremock upstream. Sends a
//! request whose system prompt embeds an ISO-8601 timestamp, then
//! asserts that:
//!
//!   1. A structured `volatile_content_suspected` INFO log was
//!      emitted (captured via a `tracing_subscriber` JSON layer
//!      with an in-memory `MakeWriter`), and that a first sighting
//!      does not claim a cache bust it has no evidence for.
//!   2. The bytes that arrived at the upstream are byte-equal to
//!      the bytes the client sent — the detector observes only,
//!      it never mutates.
//!
//! Mirrors the capture pattern from `integration_compression.rs`
//! and `integration_cache_control.rs`: install a JSON subscriber
//! once via `OnceLock`, run only one capture-driven test per
//! binary so we don't fight other tests for the global default.

use super::common;

use common::start_proxy_with;
use serde_json::json;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mount a /v1/messages handler that captures the upstream request body.
async fn mount_anthropic_capture(upstream: &MockServer) -> Arc<Mutex<Option<Vec<u8>>>> {
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            *captured_clone.lock().unwrap() = Some(req.body.clone());
            ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
        })
        .mount(upstream)
        .await;
    captured
}

#[tokio::test]
async fn volatile_timestamp_in_system_emits_warn_and_passes_through() {
    // Serialized with the other capture suites: shared buffer, see common::tracing_capture.
    let _capture = common::tracing_capture::serial().await;
    let buf = common::tracing_capture::buffer();
    buf.lock().unwrap().clear();

    let upstream = MockServer::start().await;
    let captured = mount_anthropic_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.log_level = "info".into();
    })
    .await;

    // System prompt embeds an ISO-8601 timestamp — exactly the
    // pattern that busts prompt cache hits.
    let payload = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 32,
        "system": "You are a helpful assistant. Today is 2026-05-04T14:30:00Z.",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let body = serde_json::to_vec(&payload).unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Poll the capture buffer until the event lands instead of a
    // fixed settle: the async emitter usually flushes in ~1ms.
    let flushed = common::wait_until(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_millis(5),
        || String::from_utf8_lossy(&buf.lock().unwrap()).contains("volatile_content_suspected"),
    )
    .await;
    assert!(flushed, "volatile event never flushed");

    let logs = String::from_utf8(buf.lock().unwrap().clone()).expect("logs are utf-8");
    // A first sighting is a suspicion, not a confirmed bust: nothing has
    // been seen twice yet, so nothing can be said to have changed. WARN is
    // reserved for a value observed to move (unit-tested in
    // `volatile_detector::change_suppression_tests`).
    assert!(
        logs.contains("volatile_content_suspected"),
        "expected volatile_content_suspected event in logs; got: {logs}",
    );
    assert!(
        !logs.contains("volatile_content_detected"),
        "a first sighting must not claim a cache bust; got: {logs}",
    );
    assert!(
        logs.contains("iso8601_timestamp"),
        "expected kind=iso8601_timestamp in logs; got: {logs}",
    );
    assert!(
        logs.contains(r#""location":"system""#),
        "expected location=system in logs; got: {logs}",
    );

    // Non-mutation invariant: the upstream-received body is
    // byte-equal to the client-sent body.
    let upstream_received = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream should have captured a body");
    assert_eq!(
        upstream_received, body,
        "volatile detector must not mutate the request body",
    );

    proxy.shutdown().await;
}

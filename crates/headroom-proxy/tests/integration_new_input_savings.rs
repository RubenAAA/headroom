//! Savings as a share of the input that newly entered context (upstream
//! `e92cccca`). The whole-wire ratio recounts a session's cached history every
//! turn, so it reads near 0% on a long session however well compression does.
//! The new-input rate needs every turn that billed new input in its
//! denominator — including turns that saved nothing — and the ledger and
//! `/stats` must measure the same cohort.
//!
//! Its own binary because it points the ledger at a temp file through the
//! process environment.

// Edition 2024 makes std::env::set_var and remove_var unsafe. Tests call them
// to set up config; non-test code stays free of unsafe.
#![allow(unsafe_code, clippy::undocumented_unsafe_blocks)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::start_proxy_with_state;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A large, repetitive JSON tool result the live zone compresses. The tool is
/// not named `read`, which is kept byte-exact.
fn compressible_turn() -> Value {
    let rows: Vec<Value> = (0..1500)
        .map(|i| json!({"id": i, "kind": "row", "value": format!("repeat-{}", i % 5), "status": "ok"}))
        .collect();
    json!({
        "model": "claude-sonnet-4",
        "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "list the rows"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "list_rows", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": serde_json::to_string(&rows).unwrap()}
            ]}
        ]
    })
}

fn plain_turn() -> Value {
    json!({
        "model": "claude-sonnet-4",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "say hi"}]
    })
}

#[tokio::test]
async fn a_turn_that_saved_nothing_still_counts_as_new_input() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("savings_events.jsonl");
    unsafe { std::env::set_var("HEADROOM_SAVINGS_EVENTS_PATH", &ledger) };

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4", "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 300,
                "cache_creation_input_tokens": 700,
                "cache_read_input_tokens": 5000,
                "output_tokens": 2
            }
        })))
        .mount(&upstream)
        .await;

    let tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
        Some(dir.path().join("savings.json")),
        false,
    ));
    let probe = tracker.clone();
    let proxy = start_proxy_with_state(
        &upstream.uri(),
        |c| {
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        },
        move |mut s| {
            s.savings_tracker = tracker;
            s
        },
    )
    .await;

    let client = reqwest::Client::new();
    for body in [compressible_turn(), plain_turn()] {
        let resp = client
            .post(format!("{}/v1/messages", proxy.url()))
            .header("x-api-key", "k")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = resp.bytes().await;
    }

    // The ledger append runs on the blocking pool; wait for both lines.
    let mut lines: Vec<Value> = Vec::new();
    for _ in 0..100 {
        lines = std::fs::read_to_string(&ledger)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        if lines.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(lines.len(), 2, "ledger lines: {lines:?}");
    let saved = lines[0]["saved"].as_i64().unwrap();
    assert!(saved > 0, "the tool result was not compressed: {lines:?}");
    // 300 uncached + 700 cache write; the 5,000 read tokens were not new.
    assert_eq!(lines[0]["new_input"], json!(1000));
    assert_eq!(
        (&lines[1]["saved"], &lines[1]["new_input"]),
        (&json!(0), &json!(1000)),
        "the non-saving turn must still book its new input"
    );

    let expected = (saved as f64 / (2000 + saved) as f64 * 1000.0).round() / 10.0;
    let report = headroom_core::savings_ledger::aggregate_savings(Some(&ledger), None, 30);
    assert_eq!(report.lifetime["calls"], json!(1));
    assert_eq!(report.lifetime["new_input_tokens"], json!(2000));
    assert_eq!(
        report.lifetime["new_input_savings_percent"],
        json!(expected)
    );

    // `/stats` serves the same cohort from the lifetime metrics.
    let tokens = &probe.metrics_snapshot(&json!({}))["tokens"];
    assert_eq!(tokens["new_input"], json!(2000));
    assert_eq!(tokens["new_input_saved"], json!(saved));
    let stats_pct = tokens["new_input_savings_percent"].as_f64().unwrap();
    assert_eq!((stats_pct * 10.0).round() / 10.0, expected);

    proxy.shutdown().await;
}

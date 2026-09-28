//! A CCR continuation round is charged against the savings, not hidden.
//!
//! The proxy answers a `headroom_retrieve` call itself by re-POSTing upstream,
//! and the client sees one turn. The round it replaced was billed all the
//! same, so the ledger books it as overhead and the report nets it off.
//!
//! Its own binary because it points the ledger at a temp file through the
//! process environment.

// Edition 2024 makes std::env::set_var and remove_var unsafe. Tests call them
// to set up config; non-test code stays free of unsafe.
#![allow(unsafe_code, clippy::undocumented_unsafe_blocks)]

mod common;

use std::convert::Infallible;
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with_state;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

const HASH: &str = "abcdef1234567890abcdef12";

/// Round one streams a retrieval call (500 in, 9 out); the continuation, which
/// carries more than one message, streams the answer (900 in, 12 out).
async fn ccr_upstream() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(|req: Request<hyper::body::Incoming>| async move {
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                            let continuation =
                                parsed["messages"].as_array().is_some_and(|m| m.len() > 1);
                            let sse = if continuation {
                                concat!(
                                    "event: message_start\n",
                                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"model\":\"claude\",\"usage\":{\"input_tokens\":900,\"output_tokens\":0}}}\n\n",
                                    "event: content_block_start\n",
                                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                                    "event: content_block_delta\n",
                                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ANSWER\"}}\n\n",
                                    "event: content_block_stop\n",
                                    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                                    "event: message_delta\n",
                                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":12}}\n\n",
                                    "event: message_stop\n",
                                    "data: {\"type\":\"message_stop\"}\n\n",
                                )
                                .to_string()
                            } else {
                                format!(
                                    concat!(
                                        "event: message_start\n",
                                        "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"model\":\"claude\",\"usage\":{{\"input_tokens\":500,\"output_tokens\":0}}}}}}\n\n",
                                        "event: content_block_start\n",
                                        "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"headroom_retrieve\",\"input\":{{}}}}}}\n\n",
                                        "event: content_block_delta\n",
                                        "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"hash\\\":\\\"{hash}\\\"}}\"}}}}\n\n",
                                        "event: content_block_stop\n",
                                        "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                                        "event: message_delta\n",
                                        "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":9}}}}\n\n",
                                        "event: message_stop\n",
                                        "data: {{\"type\":\"message_stop\"}}\n\n",
                                    ),
                                    hash = HASH
                                )
                            };
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(200)
                                    .header("content-type", "text/event-stream")
                                    .body(Full::new(Bytes::from(sse)))
                                    .unwrap(),
                            )
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn a_hidden_ccr_round_is_booked_as_overhead_and_netted_off() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("savings_events.jsonl");
    unsafe { std::env::set_var("HEADROOM_SAVINGS_EVENTS_PATH", &ledger) };

    let addr = ccr_upstream().await;
    let store_dir = dir.path().join("ctx");
    let proxy = start_proxy_with_state(
        &format!("http://{addr}"),
        move |c| {
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
            c.ctx_offload = true;
            c.ctx_store_dir = Some(store_dir);
            c.ccr_handle_responses = true;
        },
        |s| {
            s.ctx_offload
                .as_ref()
                .expect("ctx_offload runtime")
                .store
                .ccr()
                .put(HASH, "the original uncompressed tool result");
            s
        },
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .json(&json!({
            "model": "claude-3-haiku-20240307",
            "stream": true,
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "what did that say"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = String::from_utf8_lossy(&resp.bytes().await.unwrap()).to_string();
    assert!(text.contains("ANSWER"), "continuation did not run: {text}");

    // The ledger append runs on the blocking pool.
    let mut lines: Vec<Value> = Vec::new();
    for _ in 0..100 {
        lines = std::fs::read_to_string(&ledger)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        if !lines.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(lines.len(), 1, "ledger lines: {lines:?}");
    let row = &lines[0];
    // The continuation round the client never asked for: 900 uncached in,
    // 12 out, at claude-3-haiku's $0.25 / $1.25 per MTok.
    assert_eq!(row["overhead_tokens"], json!(912));
    let expected = (900.0 * 0.25 + 12.0 * 1.25) / 1e6;
    assert!((row["overhead_usd"].as_f64().unwrap() - expected).abs() < 1e-6);
    assert_eq!(row["cost_basis"], json!("measured_mix"));

    let report = headroom_core::savings_ledger::aggregate_savings(Some(&ledger), None, 30);
    let life = &report.lifetime;
    // Nothing was compressed, so there is no saving turn — only the charge.
    assert_eq!(life["calls"], json!(0));
    assert_eq!(life["overhead_tokens"], json!(912));
    assert!((life["net_cost_usd"].as_f64().unwrap() + expected).abs() < 1e-6);

    proxy.shutdown().await;
}

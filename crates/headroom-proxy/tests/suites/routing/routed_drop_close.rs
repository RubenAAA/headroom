//! A routed stream that dies after the client is committed is closed the way
//! the client asked: an in-band `error` by default, the marked `end_turn` tail
//! with `x-headroom-on-drop: tail`.
//!
//! The tail reads as success to a headless client. Measured 2026-10-05 on a
//! Spark review pass: `claude -p` returned `is_error: false` with
//! `[truncated: the connection to the API dropped mid-response]` as its whole
//! answer, and the pipeline took that as a finished review. So the default is
//! the error: the client resends unstreamed and gets the full answer.

use super::common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with_state;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Every POST streams one text delta larger than the early-retry hold, so the
/// client is committed, then ends with no terminal event. Records whether the
/// drop-close header reached the upstream.
async fn upstream(saw_header: Arc<Mutex<bool>>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let saw_header = Arc::clone(&saw_header);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let saw_header = Arc::clone(&saw_header);
                    async move {
                        if req.headers().contains_key("x-headroom-on-drop") {
                            *saw_header.lock().unwrap() = true;
                        }
                        let _ = req.into_body().collect().await;
                        let payload = [
                            sse(
                                "response.created",
                                json!({"type":"response.created","response":{"id":"resp_1"}}),
                            ),
                            sse(
                                "response.output_text.delta",
                                json!({"type":"response.output_text.delta",
                                       "item_id":"msg_1","output_index":0,
                                       "delta": format!("partial {}", "x".repeat(64 * 1024))}),
                            ),
                        ]
                        .concat();
                        let stream = futures_util::stream::iter(vec![Ok::<_, Infallible>(
                            Frame::data(Bytes::from(payload)),
                        )]);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header("content-type", "text/event-stream")
                                .body(StreamBody::new(stream))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, task)
}

/// Send one streaming turn through a proxy routed at a dropping upstream.
async fn dropped_turn(on_drop: Option<&str>) -> (String, bool) {
    let saw_header = Arc::new(Mutex::new(false));
    let (addr, _task) = upstream(Arc::clone(&saw_header)).await;
    let upstream_url = format!("http://{addr}");
    let route_upstream = upstream_url.clone();
    let proxy = start_proxy_with_state(
        &upstream_url,
        move |config| {
            config.model_routes = vec![headroom_proxy::config::ProviderRoute {
                model_prefix: "claude-spark-test".to_string(),
                prefix_match: false,
                upstream: Some(route_upstream.parse().expect("upstream url")),
                translate: true,
                cursor_agent: None,
                target_model: Some("spark-test-model".to_string()),
                auth_env: Some("none".to_string()),
            }];
        },
        |state| state,
    )
    .await;

    let mut req = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .json(&json!({
            "model": "claude-spark-test",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .timeout(Duration::from_secs(20));
    if let Some(v) = on_drop {
        req = req.header("x-headroom-on-drop", v);
    }
    let text = req
        .send()
        .await
        .expect("proxy answers")
        .text()
        .await
        .unwrap();
    let saw = *saw_header.lock().unwrap();
    (text, saw)
}

#[tokio::test]
async fn a_committed_drop_ends_in_an_error_by_default() {
    let (text, _) = dropped_turn(None).await;
    assert!(text.contains("partial"), "text already sent: {text:.300}");
    let tail = &text[text.len().saturating_sub(400)..];
    assert!(
        tail.contains("event: error") && tail.contains("\"api_error\""),
        "the drop must end in an in-band error: {tail}"
    );
    assert!(!text.contains("[truncated"), "no marker in error mode");
    assert!(
        !text.contains("event: message_stop"),
        "turn must not read as finished"
    );
}

#[tokio::test]
async fn a_committed_drop_ends_in_the_marked_tail_when_the_client_asks() {
    let (text, _) = dropped_turn(Some("tail")).await;
    assert!(text.contains("partial"), "text already sent: {text:.300}");
    assert!(text.contains("[truncated"), "marker missing");
    assert!(text.contains("event: message_stop"), "tail missing");
    assert!(
        !text.contains("event: error"),
        "tail mode must stay the tail"
    );
}

#[tokio::test]
async fn a_committed_drop_ends_in_an_error_when_the_client_asks() {
    let (text, saw_header) = dropped_turn(Some("error")).await;
    assert!(text.contains("partial"), "text already sent: {text:.300}");
    let tail = &text[text.len().saturating_sub(400)..];
    assert!(
        tail.contains("event: error") && tail.contains("\"api_error\""),
        "the drop must end in an in-band error: {tail}"
    );
    assert!(!text.contains("[truncated"), "no marker in error mode");
    assert!(
        !text.contains("event: message_stop"),
        "turn must not read as finished"
    );
    assert!(
        !saw_header,
        "the drop-close header is for headroom, not the upstream"
    );
}

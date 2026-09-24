//! A routed stream that dies early is re-sent while the client is
//! uncommitted, instead of reaching the client truncated.
//!
//! Zen sheds load by closing streams cleanly mid-turn (measured 2026-09-24
//! on Spark: turns closed with zero output tokens and no terminal event).
//! The direct Anthropic path retries this window via `stream_retry`; the
//! routed path had no equivalent, so every such death became a
//! `[truncated: …]` turn and a Stop-hook spin.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with_state;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tempfile::TempDir;

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// First POST dies right after the opening event (clean EOF, no terminal —
/// the Zen shed shape). Later POSTs answer a complete turn.
async fn upstream(posts: Arc<AtomicUsize>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let posts = Arc::clone(&posts);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let posts = Arc::clone(&posts);
                    async move {
                        let _ = req.into_body().collect().await;
                        let n = posts.fetch_add(1, Ordering::SeqCst);
                        let payload: String = if n == 0 {
                            // Truncated: an opening event, then EOF.
                            sse(
                                "response.created",
                                json!({"type":"response.created","response":{"id":"resp_1"}}),
                            )
                        } else {
                            [
                                sse(
                                    "response.created",
                                    json!({"type":"response.created","response":{"id":"resp_2"}}),
                                ),
                                sse(
                                    "response.output_text.delta",
                                    json!({"type":"response.output_text.delta",
                                           "item_id":"msg_1","output_index":0,
                                           "delta":"second try lands"}),
                                ),
                                sse(
                                    "response.output_text.done",
                                    json!({"type":"response.output_text.done",
                                           "item_id":"msg_1","output_index":0,
                                           "text":"second try lands"}),
                                ),
                                sse(
                                    "response.completed",
                                    json!({"type":"response.completed","response":
                                           {"id":"resp_2","status":"completed",
                                            "usage":{"input_tokens":10,"output_tokens":5}}}),
                                ),
                            ]
                            .concat()
                        };
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

#[tokio::test]
async fn a_truncated_first_attempt_is_resent_not_surfaced() {
    let _dir = TempDir::new().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let (addr, _task) = upstream(Arc::clone(&posts)).await;

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

    let body = json!({
        "model": "claude-spark-test",
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let text = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("proxy answers")
        .text()
        .await
        .unwrap();

    assert_eq!(
        posts.load(Ordering::SeqCst),
        2,
        "the dead first attempt must be re-sent"
    );
    assert!(
        text.contains("second try lands"),
        "the client sees the replacement turn: {text}"
    );
    assert!(
        !text.contains("[truncated"),
        "no truncation marker on a rescued turn: {text}"
    );
    assert!(
        text.contains("event: message_stop"),
        "the rescued turn ends well-formed: {text}"
    );
}

#[tokio::test]
async fn a_complete_small_turn_is_not_resent() {
    let _dir = TempDir::new().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    // First POST already answers completely: single small SSE body with a
    // terminal event. n == 0 arm below is bypassed by answering whole.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts_in = Arc::clone(&posts);
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let posts = Arc::clone(&posts_in);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let posts = Arc::clone(&posts);
                    async move {
                        let _ = req.into_body().collect().await;
                        posts.fetch_add(1, Ordering::SeqCst);
                        let payload = [
                            sse(
                                "response.created",
                                json!({"type":"response.created","response":{"id":"resp_1"}}),
                            ),
                            sse(
                                "response.output_text.delta",
                                json!({"type":"response.output_text.delta",
                                       "item_id":"msg_1","output_index":0,"delta":"short"}),
                            ),
                            sse(
                                "response.output_text.done",
                                json!({"type":"response.output_text.done",
                                       "item_id":"msg_1","output_index":0,"text":"short"}),
                            ),
                            sse(
                                "response.completed",
                                json!({"type":"response.completed","response":
                                       {"id":"resp_1","status":"completed",
                                        "usage":{"input_tokens":10,"output_tokens":2}}}),
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
    let _task = task;

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

    let body = json!({
        "model": "claude-spark-test",
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let text = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("proxy answers")
        .text()
        .await
        .unwrap();

    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "a complete turn must not be re-sent"
    );
    assert!(text.contains("short"), "client gets the answer: {text}");
    assert!(!text.contains("[truncated"), "no marker: {text}");
}

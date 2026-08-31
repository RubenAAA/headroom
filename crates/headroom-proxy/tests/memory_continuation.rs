//! A `memory_search` call is injected, intercepted and answered by the proxy.
//!
//! Three things had never been exercised together: that the five memory tools
//! reach the request body at all, that a call to one is caught before it can
//! reach a client which has no such tool, and that the turn is then finished
//! with what was found. The store round trip is covered in
//! `memory_round_trip.rs`; this is the wire.
//!
//! Claude Code streams, so the streaming arm is the one that matters.

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

/// Upstream that calls `memory_search` on the first request and answers with
/// text on the continuation.
async fn upstream(
    rounds: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let rounds = Arc::clone(&rounds);
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let rounds = Arc::clone(&rounds);
                    let seen = Arc::clone(&seen);
                    async move {
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        seen.lock().unwrap().push(String::from_utf8_lossy(&body).into_owned());
                        let round = rounds.fetch_add(1, Ordering::SeqCst);

                        // The continuation asks for `stream: false`, so it has
                        // to be answered with plain JSON rather than SSE.
                        let (content_type, events): (&str, Vec<String>) = if round > 0 {
                            (
                                "application/json",
                                vec![json!({
                                    "id": "m2", "type": "message", "role": "assistant",
                                    "model": "claude-opus-5", "stop_reason": "end_turn",
                                    "content": [{"type": "text", "text": "It cost 511 percent more creation."}],
                                    "usage": {"input_tokens": 20, "output_tokens": 9}
                                })
                                .to_string()],
                            )
                        } else {
                            ("text/event-stream", vec![
                                sse("message_start", json!({"type":"message_start","message":{"id":"m1","role":"assistant","content":[],"model":"claude-opus-5","usage":{"input_tokens":10,"output_tokens":0}}})),
                                sse("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"memory_search","input":{}}})),
                                sse("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"split cache TTL\"}"}})),
                                sse("content_block_stop", json!({"type":"content_block_stop","index":0})),
                                sse("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}})),
                                sse("message_stop", json!({"type":"message_stop"})),
                            ])
                        };
                        let stream = futures_util::stream::iter(
                            events.into_iter().map(|e| Ok::<_, Infallible>(Frame::data(Bytes::from(e)))),
                        );
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header("content-type", content_type)
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

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

async fn run_turn(dir: &TempDir) -> (String, Vec<String>, usize) {
    let rounds = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (addr, _task) = upstream(Arc::clone(&rounds), Arc::clone(&seen)).await;

    let store = dir.path().to_path_buf();
    let proxy = start_proxy_with_state(
        &format!("http://{addr}"),
        move |config| {
            config.memory_enabled = true;
            config.memory_inject_tools = true;
            config.memory_mode = "tool".to_string();
            config.ctx_store_dir = Some(store.clone());
            // The injector is nested inside the block gated on
            // `ctx_inject.is_some() || ctx_offload.is_some()`, so memory tools
            // silently depend on ctx offload being on. Production has it on.
            config.compression = true;
            config.ctx_offload = true;
        },
        |state| state,
    )
    .await;

    let body = json!({
        "model": "claude-opus-5",
        "stream": true,
        "messages": [{"role": "user", "content": "what did we learn about the split TTL?"}],
        "tools": [{"name": "Read", "description": "read a file", "input_schema": {"type": "object"}}]
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

    let requests = seen.lock().unwrap().clone();
    let n = rounds.load(Ordering::SeqCst);
    (text, requests, n)
}

#[tokio::test]
async fn the_memory_tools_reach_the_request_body() {
    let dir = TempDir::new().unwrap();
    let (_client_saw, upstream_saw, _) = run_turn(&dir).await;

    let first = &upstream_saw[0];
    for tool in [
        "memory_save",
        "memory_search",
        "memory_update",
        "memory_delete",
        "memory_list",
    ] {
        assert!(first.contains(tool), "{tool} must be injected into the tools array");
    }
    assert!(first.contains("\"Read\""), "the client's own tools must survive");
}

#[tokio::test]
async fn the_call_is_answered_by_the_proxy_and_never_reaches_the_client() {
    let dir = TempDir::new().unwrap();
    let (client_saw, _upstream_saw, rounds) = run_turn(&dir).await;

    assert_eq!(rounds, 2, "the proxy must have continued the turn upstream");
    assert!(
        client_saw.contains("It cost 511 percent more creation."),
        "the client must get the continued answer: {client_saw}"
    );
    assert!(
        !client_saw.contains("\"type\":\"tool_use\""),
        "the client must never see a tool it does not have: {client_saw}"
    );
}

#[tokio::test]
async fn the_answered_call_leaves_a_receipt_in_the_turn() {
    let dir = TempDir::new().unwrap();
    let (client_saw, _upstream_saw, _) = run_turn(&dir).await;

    // Without this line the call is invisible to the client, so the next
    // request — which the client rebuilds from its own transcript — shows the
    // model a claim with no tool output behind it.
    assert!(
        client_saw.contains("[headroom memory]") && client_saw.contains("memory_search("),
        "the turn must record the call the proxy answered: {client_saw}"
    );
}

#[tokio::test]
async fn the_continuation_carries_the_tool_result_upstream() {
    let dir = TempDir::new().unwrap();
    let (_client_saw, upstream_saw, _) = run_turn(&dir).await;

    assert_eq!(upstream_saw.len(), 2, "expected an original and a continuation");
    assert!(
        upstream_saw[1].contains("tool_result"),
        "the continuation must carry the memory answer: {}",
        upstream_saw[1]
    );
}


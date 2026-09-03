//! A proxy-answered memory call re-sends the turn upstream with the assistant
//! partial and the tool result appended. Each such round must be a plain
//! extension of the round before it, with the message breakpoint on its tail,
//! or every round re-pays the whole continuation uncached.
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

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Upstream that calls `memory_search` on the first two requests and answers
/// with text on the third, so the proxy runs two continuation rounds.
async fn upstream(
    rounds: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
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
                        seen.lock().unwrap().push(body.to_vec());
                        let round = rounds.fetch_add(1, Ordering::SeqCst);

                        let (content_type, events): (&str, Vec<String>) = match round {
                            0 => ("text/event-stream", vec![
                                sse("message_start", json!({"type":"message_start","message":{"id":"m1","role":"assistant","content":[],"model":"claude-opus-5","usage":{"input_tokens":10,"output_tokens":0}}})),
                                sse("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"memory_search","input":{}}})),
                                sse("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"split cache TTL\"}"}})),
                                sse("content_block_stop", json!({"type":"content_block_stop","index":0})),
                                sse("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}})),
                                sse("message_stop", json!({"type":"message_stop"})),
                            ]),
                            1 => ("application/json", vec![json!({
                                "id": "m2", "type": "message", "role": "assistant",
                                "model": "claude-opus-5", "stop_reason": "tool_use",
                                "content": [{"type": "tool_use", "id": "t2", "name": "memory_search", "input": {"query": "tail breakpoint"}}],
                                "usage": {"input_tokens": 20, "output_tokens": 9}
                            }).to_string()]),
                            _ => ("application/json", vec![json!({
                                "id": "m3", "type": "message", "role": "assistant",
                                "model": "claude-opus-5", "stop_reason": "end_turn",
                                "content": [{"type": "text", "text": "Nothing stored on that."}],
                                "usage": {"input_tokens": 20, "output_tokens": 9}
                            }).to_string()]),
                        };
                        let stream = futures_util::stream::iter(
                            events
                                .into_iter()
                                .map(|e| Ok::<_, Infallible>(Frame::data(Bytes::from(e)))),
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

/// Every `cache_control` on `messages`, as `(message, block)`.
fn message_markers(v: &Value) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (i, m) in v["messages"].as_array().unwrap().iter().enumerate() {
        if let Some(blocks) = m["content"].as_array() {
            for (j, b) in blocks.iter().enumerate() {
                if b.get("cache_control").is_some() {
                    out.push((i, j));
                }
            }
        }
    }
    out
}

/// `messages` with every `cache_control` removed. A moved marker is the one
/// change an earlier block is allowed to show between rounds: Anthropic keys
/// its cache on content, not on where the markers sit.
fn messages_without_markers(v: &Value) -> Vec<Value> {
    let mut messages = v["messages"].as_array().unwrap().clone();
    for m in &mut messages {
        if let Some(blocks) = m["content"].as_array_mut() {
            for b in blocks {
                if let Some(o) = b.as_object_mut() {
                    o.remove("cache_control");
                }
            }
        }
    }
    messages
}

/// Two tail breakpoints on `messages`, which is what the proxy sends on the
/// live setup (`--cache-tail-breakpoints 2`). The client here places them
/// itself so the test does not depend on prefix replay.
#[tokio::test]
async fn every_continuation_round_extends_the_last_and_marks_its_tail() {
    let rounds = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (addr, _task) = upstream(Arc::clone(&rounds), Arc::clone(&seen)).await;

    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().to_path_buf();
    let proxy = start_proxy_with_state(
        &format!("http://{addr}"),
        move |config| {
            config.memory_enabled = true;
            config.memory_inject_tools = true;
            config.memory_mode = "tool".to_string();
            config.ctx_store_dir = Some(store.clone());
            config.compression = true;
            config.ctx_offload = true;
            config.cache_tail_breakpoint = true;
            config.cache_tail_breakpoints = 2;
            config.ccr_max_retrieval_rounds = 4;
        },
        |state| state,
    )
    .await;

    let body = json!({
        "model": "claude-opus-5",
        "stream": true,
        "system": [{"type": "text", "text": "You are terse.", "cache_control": {"type": "ephemeral"}}],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "first question", "cache_control": {"type": "ephemeral"}}]},
            {"role": "assistant", "content": [{"type": "text", "text": "first answer"}]},
            {"role": "user", "content": [{"type": "text", "text": "what did we learn about the split TTL?", "cache_control": {"type": "ephemeral"}}]}
        ],
        "tools": [{"name": "Read", "description": "read a file", "input_schema": {"type": "object"}}]
    });
    reqwest::Client::new()
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

    let bodies: Vec<Value> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|b| serde_json::from_slice(b).expect("upstream body is JSON"))
        .collect();
    assert_eq!(
        bodies.len(),
        3,
        "two memory answers make three upstream rounds"
    );

    for (round, v) in bodies.iter().enumerate() {
        let messages = v["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        let blocks = last["content"].as_array().expect("tail message has blocks");
        let markers = message_markers(v);
        assert!(
            blocks.last().unwrap().get("cache_control").is_some(),
            "round {round}: no breakpoint on the tail block; markers at {markers:?}"
        );
        assert_eq!(
            markers.len(),
            2,
            "round {round}: marker count changed: {markers:?}"
        );

        if round == 0 {
            continue;
        }
        let prev = &bodies[round - 1];
        assert_eq!(v["system"], prev["system"], "round {round}: system changed");
        assert_eq!(v["tools"], prev["tools"], "round {round}: tools changed");
        let prev_messages = messages_without_markers(prev);
        let this_messages = messages_without_markers(v);
        assert_eq!(
            this_messages.len(),
            prev_messages.len() + 2,
            "round {round}: one assistant turn and one tool result should be appended"
        );
        assert_eq!(
            &this_messages[..prev_messages.len()],
            &prev_messages[..],
            "round {round}: messages are not an extension of the previous round"
        );
        assert_eq!(messages[messages.len() - 1]["role"], "user");
        assert_eq!(messages[messages.len() - 2]["role"], "assistant");
    }

    proxy.shutdown().await;
}

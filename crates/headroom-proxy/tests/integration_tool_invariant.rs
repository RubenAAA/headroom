//! The invariant: whatever the proxy adds to a request, that same path answers.
//!
//! Every bug in this family looked different and was the same thing. The proxy
//! injects a tool the client has never heard of — `headroom_retrieve`,
//! `memory_search` — the model calls it, and the call reaches a client that
//! answers `No such tool available`. The model gets blamed for a tool the
//! proxy invented.
//!
//! Injection is decided per-request; resolution lives on response paths with
//! their own conditions. Nothing tied the two together, so they drifted apart
//! three separate times. These tests tie them together at the only place both
//! are observable: what leaves the proxy on the wire.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tempfile::TempDir;

/// Tools the proxy injects and therefore owes an answer for. Keep in step with
/// the injection sites in `proxy.rs` and `handlers/local_model.rs`.
const PROXY_OWNED_TOOLS: &[&str] = &[
    "headroom_retrieve",
    "memory_save",
    "memory_search",
    "memory_update",
];

/// An upstream that records the request it was given and answers trivially.
async fn recording_upstream(
    seen: Arc<Mutex<Vec<Value>>>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<hyper::body::Incoming>| {
                            let seen = seen.clone();
                            async move {
                                let body = req.into_body().collect().await.map(|c| c.to_bytes());
                                if let Some(v) = body
                                    .ok()
                                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                                {
                                    seen.lock().expect("seen lock").push(v);
                                }
                                let (tx, rx) = tokio::sync::mpsc::channel::<
                                    Result<Frame<Bytes>, std::io::Error>,
                                >(8);
                                tokio::spawn(async move {
                                    for f in [
                                        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"claude\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n".to_vec(),
                                        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n".to_vec(),
                                        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec(),
                                    ] {
                                        if tx.send(Ok(Frame::data(Bytes::from(f)))).await.is_err() {
                                            return;
                                        }
                                        tokio::time::sleep(Duration::from_millis(2)).await;
                                    }
                                });
                                Ok::<_, Infallible>(
                                    Response::builder()
                                        .status(200)
                                        .header("content-type", "text/event-stream")
                                        .body(StreamBody::new(
                                            tokio_stream::wrappers::ReceiverStream::new(rx),
                                        ))
                                        .unwrap(),
                                )
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    (addr, task)
}

/// Names of the tools the proxy put into the request it forwarded upstream.
fn injected_tool_names(request: &Value) -> Vec<String> {
    request
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(Value::as_str)
                        .or_else(|| t.get("function")?.get("name")?.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn forward_and_capture(path: &str, body: Value) -> Vec<String> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (addr, _upstream) = recording_upstream(seen.clone()).await;
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().to_path_buf();
    let proxy = start_proxy_with(&format!("http://{addr}"), move |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        c.ctx_offload = true;
        c.ctx_store_dir = Some(store_dir);
    })
    .await;

    let _ = reqwest::Client::new()
        .post(format!("{}{path}", proxy.url()))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(serde_json::to_vec(&body).unwrap())
        .send()
        .await
        .expect("proxy responds")
        .bytes()
        .await;
    proxy.shutdown().await;

    let captured = seen.lock().expect("seen lock").clone();
    captured.iter().flat_map(injected_tool_names).collect()
}

/// The Anthropic path resolves `headroom_retrieve` on both arms, so it may
/// advertise it. This is the positive half of the invariant — without it the
/// test below could be satisfied by injecting nothing anywhere.
#[tokio::test]
async fn anthropic_streaming_may_advertise_the_retrieve_tool() {
    let names = forward_and_capture(
        "/v1/messages",
        json!({
            "model": "claude-3-haiku-20240307",
            "stream": true,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        }),
    )
    .await;

    assert!(
        names.iter().any(|n| n == "headroom_retrieve"),
        "the Anthropic stream path resolves this tool, so it should offer it; got {names:?}"
    );
}

/// OpenAI-shaped clients are offered none of the proxy's own tools, streaming
/// or not.
///
/// The injection block in `proxy.rs` runs for the Anthropic endpoint only, so
/// its OpenAI arm never fires — those clients were never handed an
/// unanswerable call. This test pins that, so anyone who lifts the
/// Anthropic-only restriction has to face the resolver question rather than
/// discover it in production. If you make it fail by injecting on an OpenAI
/// path, write the resolver for that path's *streaming* arm first.
#[tokio::test]
async fn openai_shaped_clients_are_offered_no_proxy_tools() {
    for stream in [true, false] {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let names = forward_and_capture(
                path,
                json!({
                    "model": "gpt-4o",
                    "stream": stream,
                    "messages": [{"role": "user", "content": "hi"}],
                    "tools": [{
                        "type": "function",
                        "function": {"name": "Read", "parameters": {"type": "object"}},
                    }],
                }),
            )
            .await;

            for owned in PROXY_OWNED_TOOLS {
                assert!(
                    !names.iter().any(|n| n == owned),
                    "{path} (stream={stream}) was offered {owned}, which nothing \
                     on that path resolves. Either write the resolver or do not \
                     inject. Got {names:?}"
                );
            }
        }
    }
}

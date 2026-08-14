//! A `headroom_retrieve` call on a streamed turn is answered by the proxy.
//!
//! The proxy injects that tool into intercepted requests, so it owns the job
//! of running it. Until the stream rewriter landed it only did so on buffered
//! responses; every interactive client streams, so the call reached a client
//! that had never heard of the tool and the turn died with `No such tool
//! available: headroom_retrieve`.
//!
//! The upstream here answers in two shapes, which is what the real one does:
//! the first request has `stream: true` and gets SSE ending in a
//! `headroom_retrieve` tool_use; the continuation has `stream: false` (the
//! rewriter forces it) and gets plain JSON carrying the answer.

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
use serde_json::json;
use tempfile::TempDir;

/// The hash the fake model asks for. Stored in the CCR store by the test.
const HASH: &str = "abcdef1234567890abcdef12";
const ORIGINAL: &str = "the original uncompressed tool result";

/// Upstream that streams a retrieval request, then answers the continuation.
/// `rounds` counts continuation requests so the test can prove one happened.
async fn ccr_upstream(rounds: Arc<AtomicUsize>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let rounds = rounds.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<hyper::body::Incoming>| {
                            let rounds = rounds.clone();
                            async move {
                                let body = req.into_body().collect().await.map(|c| c.to_bytes());
                                let parsed: serde_json::Value = body
                                    .ok()
                                    .and_then(|b| serde_json::from_slice(&b).ok())
                                    .unwrap_or(serde_json::Value::Null);
                                let streaming = parsed
                                    .get("stream")
                                    .and_then(serde_json::Value::as_bool)
                                    .unwrap_or(false);

                                if !streaming {
                                    // The continuation. Answer with the text the
                                    // model produced after seeing the retrieval.
                                    rounds.fetch_add(1, Ordering::SeqCst);
                                    let payload = json!({
                                        "id": "msg_2",
                                        "type": "message",
                                        "role": "assistant",
                                        "model": "claude",
                                        "content": [
                                            {"type": "text", "text": "ANSWER_AFTER_RETRIEVAL"}
                                        ],
                                        "stop_reason": "end_turn",
                                        "usage": {"input_tokens": 900, "output_tokens": 12},
                                    });
                                    return Ok::<_, Infallible>(
                                        Response::builder()
                                            .status(200)
                                            .header("content-type", "application/json")
                                            .body(StreamBody::new(
                                                tokio_stream::wrappers::ReceiverStream::new({
                                                    let (tx, rx) = tokio::sync::mpsc::channel::<
                                                        Result<Frame<Bytes>, std::io::Error>,
                                                    >(2);
                                                    let bytes =
                                                        serde_json::to_vec(&payload).unwrap();
                                                    tokio::spawn(async move {
                                                        let _ = tx
                                                            .send(Ok(Frame::data(Bytes::from(
                                                                bytes,
                                                            ))))
                                                            .await;
                                                    });
                                                    rx
                                                }),
                                            ))
                                            .unwrap(),
                                    );
                                }

                                // Round one: some text, then the retrieval call.
                                let (tx, rx) = tokio::sync::mpsc::channel::<
                                    Result<Frame<Bytes>, std::io::Error>,
                                >(16);
                                tokio::spawn(async move {
                                    let frames: Vec<Vec<u8>> = vec![
                                        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude\",\"usage\":{\"input_tokens\":500,\"output_tokens\":0}}}\n\n".to_vec(),
                                        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_vec(),
                                        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"VISIBLE_PREFIX\"}}\n\n".to_vec(),
                                        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_vec(),
                                        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"headroom_retrieve\",\"input\":{}}}\n\n".to_vec(),
                                        format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"hash\\\":\\\"{HASH}\\\"}}\"}}}}\n\n").into_bytes(),
                                        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n".to_vec(),
                                        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n".to_vec(),
                                        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec(),
                                    ];
                                    for f in frames {
                                        if tx.send(Ok(Frame::data(Bytes::from(f)))).await.is_err() {
                                            return;
                                        }
                                        tokio::time::sleep(Duration::from_millis(5)).await;
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

/// Send one streamed turn through the proxy and return the raw SSE the client
/// received.
async fn client_stream(dir: &TempDir, rounds: Arc<AtomicUsize>) -> String {
    let (addr, _upstream) = ccr_upstream(rounds).await;
    let store_dir = dir.path().to_path_buf();
    let proxy = start_proxy_with_state(
        &format!("http://{addr}"),
        move |c| {
            // `compression` is the interception master switch; without it the
            // proxy is a byte pipe and never looks at the stream.
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
            c.ctx_offload = true;
            c.ctx_store_dir = Some(store_dir);
            c.ccr_handle_responses = true;
        },
        |s| {
            // Seed the store with the content the model is about to ask for.
            s.ctx_offload
                .as_ref()
                .expect("ctx_offload runtime")
                .store
                .ccr()
                .put(HASH, ORIGINAL);
            s
        },
    )
    .await;

    let body = json!({
        "model": "claude-3-haiku-20240307",
        "stream": true,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "what did that say"}]
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(serde_json::to_vec(&body).unwrap())
        .send()
        .await
        .expect("proxy responds");
    assert_eq!(resp.status(), 200);
    let text = String::from_utf8_lossy(&resp.bytes().await.expect("stream body")).to_string();
    proxy.shutdown().await;
    text
}

#[tokio::test]
async fn retrieval_is_resolved_without_reaching_the_client() {
    let dir = TempDir::new().unwrap();
    let rounds = Arc::new(AtomicUsize::new(0));
    let sse = client_stream(&dir, rounds.clone()).await;

    assert!(
        !sse.contains("headroom_retrieve"),
        "the client must never be handed a tool it cannot run:\n{sse}"
    );
    assert_eq!(
        rounds.load(Ordering::SeqCst),
        1,
        "the proxy should have run exactly one continuation round"
    );
    assert!(
        sse.contains("ANSWER_AFTER_RETRIEVAL"),
        "the continuation's content must reach the client:\n{sse}"
    );
    assert!(
        sse.contains("VISIBLE_PREFIX"),
        "text streamed before the retrieval must survive:\n{sse}"
    );
}

#[tokio::test]
async fn the_spliced_turn_is_one_well_formed_message() {
    let dir = TempDir::new().unwrap();
    let sse = client_stream(&dir, Arc::new(AtomicUsize::new(0))).await;

    // One message envelope: the continuation must not open a second one.
    assert_eq!(
        sse.matches("event: message_start").count(),
        1,
        "client must see exactly one message_start:\n{sse}"
    );
    assert_eq!(
        sse.matches("event: message_stop").count(),
        1,
        "client must see exactly one message_stop:\n{sse}"
    );
    // The suppressed block must not leave a hole in the numbering: the
    // continuation's text takes index 1, right after the prefix at index 0.
    assert!(
        sse.contains(r#""index":1"#),
        "continuation block must be numbered 1:\n{sse}"
    );
    assert!(
        !sse.contains(r#""index":2"#),
        "numbering must close the gap left by the suppressed block:\n{sse}"
    );
    // The turn ends on the continuation's reason, not the retrieval's.
    assert!(
        sse.contains("end_turn"),
        "final stop_reason must come from the continuation:\n{sse}"
    );
    assert!(
        !sse.contains("tool_use"),
        "the retrieval's stop_reason must not leak:\n{sse}"
    );
}

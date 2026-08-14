//! A `headroom_retrieve` call on a *routed* model is answered by the proxy.
//!
//! `handlers::local_model` injects the tool and hands compression a CCR store,
//! so it emits retrieval markers too — and until now resolved neither. The
//! call travelled to a client that has no such tool, exactly as it did on the
//! Claude path. Claude Code streams, so the streaming arm is the one that
//! matters; the buffered arm is covered too because it injects the same tool.
//!
//! The fake upstream speaks OpenAI chat-completions: SSE with a tool_call for
//! the first request, plain JSON for the continuation.

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

const HASH: &str = "abcdef1234567890abcdef12";
const ORIGINAL: &str = "the original uncompressed tool result";
const ROUTED_MODEL: &str = "routed-test-model";

async fn openai_upstream(rounds: Arc<AtomicUsize>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
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
                                // A continuation is identified by the tool
                                // result the proxy appended, never by `stream`
                                // — the client's own request may be buffered,
                                // and keying on `stream` would answer it as if
                                // a retrieval had already happened.
                                let is_continuation = parsed
                                    .get("messages")
                                    .and_then(serde_json::Value::as_array)
                                    .map(|msgs| {
                                        msgs.iter().any(|m| {
                                            m.get("role").and_then(serde_json::Value::as_str)
                                                == Some("tool")
                                        })
                                    })
                                    .unwrap_or(false);

                                let (tx, rx) = tokio::sync::mpsc::channel::<
                                    Result<Frame<Bytes>, std::io::Error>,
                                >(16);

                                if is_continuation {
                                    rounds.fetch_add(1, Ordering::SeqCst);
                                    let payload = json!({
                                        "id": "c2",
                                        "object": "chat.completion",
                                        "choices": [{
                                            "index": 0,
                                            "message": {
                                                "role": "assistant",
                                                "content": "ANSWER_AFTER_RETRIEVAL",
                                            },
                                            "finish_reason": "stop",
                                        }],
                                        "usage": {"prompt_tokens": 900, "completion_tokens": 12},
                                    });
                                    let bytes = serde_json::to_vec(&payload).unwrap();
                                    tokio::spawn(async move {
                                        let _ = tx.send(Ok(Frame::data(Bytes::from(bytes)))).await;
                                    });
                                    return Ok::<_, Infallible>(
                                        Response::builder()
                                            .status(200)
                                            .header("content-type", "application/json")
                                            .body(StreamBody::new(
                                                tokio_stream::wrappers::ReceiverStream::new(rx),
                                            ))
                                            .unwrap(),
                                    );
                                }

                                if !streaming {
                                    // Buffered first turn: same retrieval, in
                                    // the non-streaming chat-completions shape.
                                    let payload = json!({
                                        "id": "c1",
                                        "object": "chat.completion",
                                        "choices": [{
                                            "index": 0,
                                            "message": {
                                                "role": "assistant",
                                                "content": "VISIBLE_PREFIX",
                                                "tool_calls": [{
                                                    "id": "call_1",
                                                    "type": "function",
                                                    "function": {
                                                        "name": "headroom_retrieve",
                                                        "arguments":
                                                            format!("{{\"hash\":\"{HASH}\"}}"),
                                                    },
                                                }],
                                            },
                                            "finish_reason": "tool_calls",
                                        }],
                                        "usage": {"prompt_tokens": 500, "completion_tokens": 9},
                                    });
                                    let bytes = serde_json::to_vec(&payload).unwrap();
                                    tokio::spawn(async move {
                                        let _ = tx.send(Ok(Frame::data(Bytes::from(bytes)))).await;
                                    });
                                    return Ok::<_, Infallible>(
                                        Response::builder()
                                            .status(200)
                                            .header("content-type", "application/json")
                                            .body(StreamBody::new(
                                                tokio_stream::wrappers::ReceiverStream::new(rx),
                                            ))
                                            .unwrap(),
                                    );
                                }

                                tokio::spawn(async move {
                                    let args = format!("{{\\\"hash\\\":\\\"{HASH}\\\"}}");
                                    let frames: Vec<String> = vec![
                                        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"VISIBLE_PREFIX\"},\"finish_reason\":null}]}\n\n".to_string(),
                                        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"headroom_retrieve\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n".to_string(),
                                        format!("data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{args}\"}}}}]}},\"finish_reason\":null}}]}}\n\n"),
                                        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":500,\"completion_tokens\":9}}\n\n".to_string(),
                                        "data: [DONE]\n\n".to_string(),
                                    ];
                                    for f in frames {
                                        if tx
                                            .send(Ok(Frame::data(Bytes::from(f.into_bytes()))))
                                            .await
                                            .is_err()
                                        {
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

/// Drive one routed turn and return what the client received.
async fn routed_turn(dir: &TempDir, rounds: Arc<AtomicUsize>, stream: bool) -> String {
    let (addr, _upstream) = openai_upstream(rounds).await;
    let store_dir = dir.path().to_path_buf();
    let upstream_url = format!("http://{addr}");
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        move |c| {
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
            c.ctx_offload = true;
            c.ctx_store_dir = Some(store_dir);
            c.ccr_handle_responses = true;
            c.local_model = Some(ROUTED_MODEL.to_string());
            c.local_upstream = Some(upstream_url.parse().expect("upstream url"));
        },
        |s| {
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
        "model": ROUTED_MODEL,
        "stream": stream,
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
    let text = String::from_utf8_lossy(&resp.bytes().await.expect("body")).to_string();
    proxy.shutdown().await;
    text
}

#[tokio::test]
async fn routed_streaming_turn_resolves_retrieval() {
    let dir = TempDir::new().unwrap();
    let rounds = Arc::new(AtomicUsize::new(0));
    let out = routed_turn(&dir, rounds.clone(), true).await;

    assert!(
        !out.contains("headroom_retrieve"),
        "a routed model's retrieval must not reach the client either:\n{out}"
    );
    assert_eq!(
        rounds.load(Ordering::SeqCst),
        1,
        "the proxy should have run one continuation against the routed upstream"
    );
    assert!(
        out.contains("ANSWER_AFTER_RETRIEVAL"),
        "the continuation's answer must reach the client:\n{out}"
    );
    assert!(
        out.contains("VISIBLE_PREFIX"),
        "text streamed before the retrieval must survive:\n{out}"
    );
    assert_eq!(
        out.matches("event: message_stop").count(),
        1,
        "the spliced turn must end exactly once:\n{out}"
    );
}

#[tokio::test]
async fn routed_buffered_turn_resolves_retrieval() {
    let dir = TempDir::new().unwrap();
    let rounds = Arc::new(AtomicUsize::new(0));
    let out = routed_turn(&dir, rounds.clone(), false).await;

    assert!(
        !out.contains("headroom_retrieve"),
        "the buffered arm injects the same tool and must resolve it too:\n{out}"
    );
    assert_eq!(rounds.load(Ordering::SeqCst), 1);
    assert!(
        out.contains("ANSWER_AFTER_RETRIEVAL"),
        "the continuation's answer must reach the client:\n{out}"
    );
}

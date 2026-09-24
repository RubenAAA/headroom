//! A `headroom_retrieve` call on a streamed turn is answered by the proxy.
//!
//! The proxy injects that tool into intercepted requests, so it owns the job
//! of running it. Until the stream rewriter landed it only did so on buffered
//! responses; every interactive client streams, so the call reached a client
//! that had never heard of the tool and the turn died with `No such tool
//! available: headroom_retrieve`.
//!
//! The upstream here answers both rounds in SSE, which is what the real one
//! does: the first request gets a stream ending in a `headroom_retrieve`
//! tool_use, and the continuation — which carries the assistant turn and the
//! tool result, and so has more than one message — gets a stream carrying the
//! answer. Continuations stream so that a slow round cannot be mistaken for a
//! stalled one; the proxy folds that second stream back into a turn.

use super::common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
                                // Both rounds stream, so the message count is
                                // what tells them apart: the client sends one,
                                // the continuation appends the assistant turn
                                // and the tool result.
                                let is_continuation = parsed
                                    .get("messages")
                                    .and_then(serde_json::Value::as_array)
                                    .is_some_and(|m| m.len() > 1);
                                // A de-streamed continuation would hold its
                                // headers until generation finished, which is
                                // what the 30s headers bound used to kill. Only
                                // count a round that asked to stream, so the
                                // round assertion below fails if that regresses.
                                let streamed = parsed
                                    .get("stream")
                                    .and_then(serde_json::Value::as_bool)
                                    .unwrap_or(false);

                                if is_continuation && streamed {
                                    // The continuation. Answer with the text the
                                    // model produced after seeing the retrieval.
                                    rounds.fetch_add(1, Ordering::SeqCst);
                                    let payload = concat!(
                                        "event: message_start\n",
                                        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"model\":\"claude\",\"usage\":{\"input_tokens\":900,\"output_tokens\":0}}}\n\n",
                                        "event: content_block_start\n",
                                        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                                        "event: content_block_delta\n",
                                        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ANSWER_AFTER_RETRIEVAL\"}}\n\n",
                                        "event: content_block_stop\n",
                                        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                                        "event: message_delta\n",
                                        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":12}}\n\n",
                                        "event: message_stop\n",
                                        "data: {\"type\":\"message_stop\"}\n\n",
                                    );
                                    return Ok::<_, Infallible>(
                                        Response::builder()
                                            .status(200)
                                            .header("content-type", "text/event-stream")
                                            .body(StreamBody::new(
                                                tokio_stream::wrappers::ReceiverStream::new({
                                                    let (tx, rx) = tokio::sync::mpsc::channel::<
                                                        Result<Frame<Bytes>, std::io::Error>,
                                                    >(2);
                                                    let bytes = payload.as_bytes().to_vec();
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
                                        // Yield so the receiver interleaves
                                        // like a live stream, without
                                        // wall-clock pacing: assertions only
                                        // cover end states.
                                        tokio::task::yield_now().await;
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

/// Same flow, but the CCR store never gets the block — only another project's
/// content index does.
///
/// This is the production failure: `ccr.db` keeps a block for an idle week and
/// then evicts it, while the content index keeps it forever under whichever
/// project was current when it was offloaded. Joining every indexed
/// `content_hash` against the live CCR rows on a real store found no block
/// indexed inside the TTL window missing from `ccr.db`, and 12,807 of the older
/// ones missing. Every miss was an expiry, and the cold copy was still there.
async fn client_stream_cold_tier_only(dir: &TempDir, rounds: Arc<AtomicUsize>) -> String {
    let (addr, _upstream) = ccr_upstream(rounds).await;
    let store_dir = dir.path().to_path_buf();
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
            // Deliberately no `ccr().put(...)`: the hot tier has expired this
            // block. Index it under a project the request will not resolve to,
            // so answering it has to cross projects.
            s.ctx_offload
                .as_ref()
                .expect("ctx_offload runtime")
                .store
                .stores()
                .content("/home/dev/somewhere-else")
                .expect("content store opens")
                .index_content(
                    "the tool call that produced it",
                    ORIGINAL,
                    &headroom_core::ctx::IndexOpts {
                        content_hash: Some(HASH.to_string()),
                        plain_text_lines: Some(50),
                        ..Default::default()
                    },
                )
                .expect("index write");
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
async fn a_block_expired_from_the_ccr_store_is_recovered_from_the_content_index() {
    let dir = TempDir::new().unwrap();
    let rounds = Arc::new(AtomicUsize::new(0));
    let before = headroom_proxy::observability::ccr_retrieval::cross_project_hits_get();
    let sse = client_stream_cold_tier_only(&dir, rounds.clone()).await;

    assert_eq!(
        rounds.load(Ordering::SeqCst),
        1,
        "the retrieval should have been answered by a continuation round, not \
         abandoned:\n{sse}"
    );
    assert!(
        sse.contains("ANSWER_AFTER_RETRIEVAL"),
        "the model should have been given the recovered content:\n{sse}"
    );
    assert!(
        !sse.contains("headroom_retrieve"),
        "the client must never be handed a tool it cannot run:\n{sse}"
    );
    assert_eq!(
        headroom_proxy::observability::ccr_retrieval::cross_project_hits_get(),
        before + 1,
        "the recovery should have been counted in \
         proxy_ccr_cross_project_hits_total"
    );
}

/// Upstream whose retrieval call can never resolve: unknown hash, no cold
/// tier. The proxy must downgrade the turn and say so, with the marker the
/// Stop hook matches to continue the turn.
async fn ccr_upstream_unresolvable(
    rounds: Arc<AtomicUsize>,
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
            let rounds = rounds.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<hyper::body::Incoming>| {
                            let rounds = rounds.clone();
                            async move {
                                let _ = rounds.fetch_add(1, Ordering::SeqCst);
                                let (_parts, body) = req.into_parts();
                                let _ = body.collect().await;
                                // Round one: the retrieval call. Every later
                                // round is a continuation the mock cannot
                                // answer, so it repeats a bare tool_use
                                // stop: no content, nothing the client can run.
                                let frames: Vec<Vec<u8>> = vec![
                                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n".to_vec(),
                                    b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"headroom_retrieve\",\"input\":{}}}\n\n".to_vec(),
                                    b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"hash\\\":\\\"aaaaaaaaaaaaaaaaaaaaaaaa\\\"}\"}}\n\n".to_vec(),
                                    b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_vec(),
                                    b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n".to_vec(),
                                    b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec(),
                                ];
                                let (tx, rx) = tokio::sync::mpsc::channel::<
                                    Result<Frame<Bytes>, std::io::Error>,
                                >(16);
                                tokio::spawn(async move {
                                    for f in frames {
                                        if tx.send(Ok(Frame::data(Bytes::from(f)))).await.is_err() {
                                            return;
                                        }
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

#[tokio::test]
async fn a_failed_retrieval_is_answered_in_place_without_the_hook_marker() {
    let dir = TempDir::new().unwrap();
    let (addr, _upstream) = ccr_upstream_unresolvable(Arc::new(AtomicUsize::new(0))).await;
    let store_dir = dir.path().to_path_buf();
    let proxy = start_proxy_with_state(
        &format!("http://{addr}"),
        move |c| {
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
            c.ctx_offload = true;
            c.ctx_store_dir = Some(store_dir);
            c.ccr_handle_responses = true;
        },
        |s| s,
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
    let sse = String::from_utf8_lossy(&resp.bytes().await.expect("stream body")).to_string();
    proxy.shutdown().await;
    // A failed retrieval is answered in place (splice_ccr_results_as_text),
    // not dropped: the error text reaches the client, so there is no empty
    // turn and no hook marker. The marker fires only when the splice drops a
    // call the client expected and downgrades the turn (unit-tested in
    // empty_turn_text_tests::empty_turns_carry_the_hook_marker).
    assert!(
        sse.contains("CCR content not found"),
        "the miss must be answered in place, not dropped:\n{sse}"
    );
    assert!(
        !sse.contains("[headroom: a proxy tool call was dropped"),
        "an answered-in-place miss must not carry the drop marker:\n{sse}"
    );
    // The miss note names `headroom_retrieve` in prose (the query-keywords
    // recovery path), so match the JSON tool shape, not the bare name: the
    // client must never be handed a `tool_use` block for a tool it cannot
    // run, but prose may mention it.
    assert!(
        !sse.contains("\"headroom_retrieve\""),
        "the client must never be handed a tool it cannot run:\n{sse}"
    );
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

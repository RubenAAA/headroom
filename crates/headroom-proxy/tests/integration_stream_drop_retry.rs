//! A stream that dies before the client is committed is re-issued.
//!
//! The status-code and in-band-error retries both stand on the same ground:
//! nothing has been forwarded, so a second attempt cannot duplicate output. A
//! body that dies mid-stream stood outside that — the retry loop had long
//! since returned, and the error went straight to the client as a dead turn.
//!
//! The proxy now holds the opening bytes back, which keeps the response
//! uncommitted for `retry_stream_hold_bytes`. These tests pin all three edges:
//! a drop inside the hold is retried and the dead attempt's bytes never
//! surface, a drop after the hold is closed off as a truncated but well-formed
//! message by `sse::stream_finisher`, and a response smaller than the hold
//! still arrives in full.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with;
use http_body_util::StreamBody;
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use serde_json::json;

const GOOD_TEXT: &str = "REAL_CONTENT";
/// Text of the attempt that dies. Must never reach the client on a retry.
const GHOST_TEXT: &str = "GHOST_CONTENT";

fn preamble() -> Vec<Vec<u8>> {
    vec![
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"claude\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n".to_vec(),
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_vec(),
    ]
}

fn delta(text: &str) -> Vec<u8> {
    format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n").into_bytes()
}

fn tail() -> Vec<Vec<u8>> {
    vec![
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_vec(),
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n".to_vec(),
        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec(),
    ]
}

/// Upstream that answers the first `fail_times` requests with a 200 that opens
/// normally, emits one delta, then kills the body part-way. Later requests are
/// answered in full.
async fn dropping_upstream(
    fail_times: usize,
    attempts: Arc<AtomicUsize>,
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
            let attempts = attempts.clone();
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |_req: Request<hyper::body::Incoming>| {
                            let attempts = attempts.clone();
                            async move {
                                let n = attempts.fetch_add(1, Ordering::SeqCst);
                                let (tx, rx) = tokio::sync::mpsc::channel::<
                                    Result<Frame<Bytes>, std::io::Error>,
                                >(8);
                                tokio::spawn(async move {
                                    let mut frames = preamble();
                                    if n < fail_times {
                                        frames.push(delta(GHOST_TEXT));
                                    } else {
                                        frames.push(delta(GOOD_TEXT));
                                        frames.extend(tail());
                                    }
                                    for f in frames {
                                        if tx.send(Ok(Frame::data(Bytes::from(f)))).await.is_err() {
                                            return;
                                        }
                                        tokio::time::sleep(Duration::from_millis(2)).await;
                                    }
                                    if n < fail_times {
                                        // Abort the body. Hyper cuts the chunked
                                        // encoding short and the client's
                                        // decoder reports a transport error.
                                        let _ = tx.send(Err(std::io::Error::other("boom"))).await;
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

/// Run one streamed turn. Returns whatever body bytes arrived, and whether the
/// client's own read finished cleanly.
async fn turn_against(
    fail_times: usize,
    attempts: Arc<AtomicUsize>,
    hold_bytes: usize,
) -> (String, bool) {
    let (addr, _upstream) = dropping_upstream(fail_times, attempts).await;
    let proxy = start_proxy_with(&format!("http://{addr}"), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        c.retry_enabled = true;
        c.retry_max_attempts = 3;
        c.retry_overload_max_attempts = 6;
        c.retry_base_delay_ms = 1;
        c.retry_max_delay_ms = 5;
        c.retry_stream_hold_bytes = hold_bytes;
    })
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(
            serde_json::to_vec(&json!({
                "model": "claude-3-haiku-20240307",
                "stream": true,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        )
        .send()
        .await
        .expect("proxy responds");
    assert_eq!(resp.status(), 200);
    let (body, clean) = match resp.bytes().await {
        Ok(b) => (String::from_utf8_lossy(&b).to_string(), true),
        Err(_) => (String::new(), false),
    };
    proxy.shutdown().await;
    (body, clean)
}

#[tokio::test]
async fn a_drop_inside_the_hold_is_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    // The dead attempt is a few hundred bytes, well inside the hold.
    let (body, clean) = turn_against(1, attempts.clone(), 4096).await;
    assert!(clean, "client read should finish cleanly after the retry");
    assert!(
        body.contains(GOOD_TEXT),
        "the retry's content should reach the client: {body}"
    );
    assert!(
        !body.contains(GHOST_TEXT),
        "the dead attempt's bytes must never surface: {body}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "one drop should cost exactly one extra upstream request"
    );
}

#[tokio::test]
async fn a_drop_past_the_hold_is_closed_off_cleanly() {
    let attempts = Arc::new(AtomicUsize::new(0));
    // A hold of one byte commits on the first chunk, so re-issuing the request
    // is off the table. `stream_finisher` takes it from there: the turn ends as
    // a well-formed message rather than a reset socket.
    let (body, clean) = turn_against(1, attempts.clone(), 1).await;
    assert!(
        clean,
        "a committed drop should still end the body cleanly: {body}"
    );
    assert!(
        body.contains("message_stop"),
        "the turn should be closed off: {body}"
    );
    assert!(
        body.contains("[truncated"),
        "a cut-off reply must say so: {body}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "a committed response must not be re-sent"
    );
}

#[tokio::test]
async fn a_response_shorter_than_the_hold_still_arrives() {
    let attempts = Arc::new(AtomicUsize::new(0));
    // Nothing fails here, and the whole body is smaller than the hold, so it
    // only reaches the client if the flush on clean end works.
    let (body, clean) = turn_against(0, attempts.clone(), 1024 * 1024).await;
    assert!(clean, "client read should finish cleanly");
    assert!(
        body.contains(GOOD_TEXT) && body.contains("message_stop"),
        "a held-then-flushed body should be complete: {body}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_zero_hold_disables_the_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let (body, clean) = turn_against(1, attempts.clone(), 0).await;
    // The hold is what buys a second attempt, so turning it off means the drop
    // is never retried. Closing the turn off is a separate guarantee and does
    // not depend on the hold.
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(clean, "the turn should still be closed off: {body}");
    assert!(body.contains("message_stop"), "{body}");
}

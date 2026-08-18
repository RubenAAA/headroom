//! Errors Anthropic reports inside a 200 stream are retried.
//!
//! When the client asks for a stream, Anthropic answers rate limits and
//! overload with HTTP 200 and an SSE body whose first event is
//! `{"type":"error","error":{"type":"overloaded_error"}}`. Both retry loops
//! branched on `r.status()` alone, so this looked like success: the client got
//! a turn that never started, and the retry budget went unspent.
//!
//! Retrying is only sound while nothing has been forwarded. These tests pin
//! both halves of that: a *leading* error is retried, and a stream that has
//! already produced content is left alone even if an error follows.

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

/// Upstream that answers the first `fail_times` requests with a 200 whose SSE
/// body opens with `error`, then answers normally.
async fn flaky_upstream(
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
                                    let frames: Vec<Vec<u8>> = if n < fail_times {
                                        vec![b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n".to_vec()]
                                    } else {
                                        vec![
                                            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"claude\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n".to_vec(),
                                            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_vec(),
                                            format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{GOOD_TEXT}\"}}}}\n\n").into_bytes(),
                                            b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_vec(),
                                            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n".to_vec(),
                                            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec(),
                                        ]
                                    };
                                    for f in frames {
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

async fn turn_against(fail_times: usize, attempts: Arc<AtomicUsize>) -> String {
    turn_against_budget(fail_times, attempts, 6).await
}

async fn turn_against_budget(
    fail_times: usize,
    attempts: Arc<AtomicUsize>,
    overload_budget: u32,
) -> String {
    let (addr, _upstream) = flaky_upstream(fail_times, attempts).await;
    let proxy = start_proxy_with(&format!("http://{addr}"), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        c.retry_enabled = true;
        c.retry_max_attempts = 3;
        c.retry_overload_max_attempts = overload_budget;
        c.retry_base_delay_ms = 1;
        c.retry_max_delay_ms = 5;
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
    let body = String::from_utf8_lossy(&resp.bytes().await.expect("body")).to_string();
    proxy.shutdown().await;
    body
}

#[tokio::test]
async fn a_leading_in_band_error_is_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let body = turn_against(1, attempts.clone()).await;

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "the first attempt opened with an error event and should have been re-sent"
    );
    assert!(
        body.contains(GOOD_TEXT),
        "the client should receive the successful retry, not the error:\n{body}"
    );
    assert!(
        !body.contains("overloaded_error"),
        "the error the proxy retried past must not also reach the client:\n{body}"
    );
}

/// The retry budget is finite, and a client waiting forever is worse than a
/// client told the truth. Once attempts run out the error goes through.
#[tokio::test]
async fn a_persistent_in_band_error_is_delivered_after_the_budget() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let body = turn_against(99, attempts.clone()).await;

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        6,
        "should stop at retry_overload_max_attempts rather than loop"
    );
    assert!(
        body.contains("overloaded_error"),
        "after the budget the upstream's own error is what the client gets:\n{body}"
    );
}

/// An overload inside a 200 body outlasts a transport blip: measured over 77
/// turns the proxy gave up on, the bursts ran 27 to 245 seconds, so three
/// attempts and three seconds of waiting cleared 21% of them. This budget is
/// its own knob for that reason, and it must not be capped by the
/// transport-level one.
#[tokio::test]
async fn the_overload_budget_outlives_the_transport_budget() {
    let attempts = Arc::new(AtomicUsize::new(0));
    // Five leading errors: past `retry_max_attempts` of 3, inside the
    // overload budget of 6.
    let body = turn_against(5, attempts.clone()).await;

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        6,
        "the sixth attempt is the one that succeeds; three would have given up"
    );
    assert!(
        body.contains(GOOD_TEXT),
        "the client should receive the successful retry:\n{body}"
    );
}

/// The knob is the knob. Lowering it below the transport budget must not
/// shorten the loop past what the transport branch still needs.
#[tokio::test]
async fn a_smaller_overload_budget_never_undercuts_the_transport_one() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let body = turn_against_budget(99, attempts.clone(), 1).await;

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "the floor is retry_max_attempts, not the smaller overload budget"
    );
    assert!(body.contains("overloaded_error"));
}

/// Nothing was peeked away: a clean stream arrives byte-complete, including
/// the `message_start` the peek had to read to decide.
#[tokio::test]
async fn a_clean_stream_keeps_its_first_event() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let body = turn_against(0, attempts.clone()).await;

    assert_eq!(attempts.load(Ordering::SeqCst), 1, "no retry was needed");
    assert!(
        body.contains("event: message_start"),
        "the peeked first event must be put back in front of the stream:\n{body}"
    );
    assert!(body.contains(GOOD_TEXT));
    assert!(body.contains("event: message_stop"));
}

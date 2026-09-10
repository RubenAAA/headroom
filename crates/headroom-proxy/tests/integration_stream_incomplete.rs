//! A streamed turn is only booked once `message_stop` arrives.
//!
//! Anthropic reports the turn's final `output_tokens` on the `message_delta`
//! that precedes `message_stop`. A stream cut short — client disconnect, a
//! mid-stream error event, an upstream that just stops — carries whatever
//! partial count had arrived. The proxy used to build its `RequestOutcome`
//! from that partial state anyway, so a truncated turn landed in the savings
//! and cost books looking like a small one.
//!
//! These tests assert the two halves of the contract against one proxy's own
//! savings tracker (pointed at a temp file, so nothing here depends on the
//! shared global Prometheus registry or on the developer's real savings file):
//! a truncated stream books nothing, an identical complete stream books one
//! request.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with_state;
use http_body_util::StreamBody;
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::json;

/// An Anthropic SSE upstream that always sends `message_start` and
/// `message_delta` (carrying the final usage) and sends `message_stop` only
/// when `complete` is set. Everything else about the two streams is identical,
/// which is what makes the pair a controlled comparison.
async fn anthropic_upstream(complete: bool) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    anthropic_upstream_with_status(complete, 200).await
}

/// As [`anthropic_upstream`], but answering with `status`. A 5xx answers a
/// single `error` event (the shape an exhausted upstream returns) instead of
/// the usage-carrying frames — the parser lands `Errored`, never
/// `MessageStop`.
async fn anthropic_upstream_with_status(
    complete: bool,
    status: u16,
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
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |_req: Request<hyper::body::Incoming>| async move {
                            let (tx, rx) = tokio::sync::mpsc::channel::<
                                Result<Frame<Bytes>, std::io::Error>,
                            >(8);
                            tokio::spawn(async move {
                                let mut frames: Vec<Vec<u8>> = if status >= 500 {
                                    vec![
                                        b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n".to_vec(),
                                    ]
                                } else {
                                    vec![
                                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_trunc\",\"model\":\"claude\",\"usage\":{\"input_tokens\":300,\"output_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n".to_vec(),
                                    b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":300,\"output_tokens\":64,\"cache_read_input_tokens\":0}}\n\n".to_vec(),
                                ]
                                };
                                if complete && status < 500 {
                                    frames.push(
                                        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                                            .to_vec(),
                                    );
                                }
                                for f in frames {
                                    if tx.send(Ok(Frame::data(Bytes::from(f)))).await.is_err() {
                                        return;
                                    }
                                    tokio::time::sleep(Duration::from_millis(10)).await;
                                }
                                // Dropping `tx` closes the body. For the
                                // truncated case that is exactly the shape of a
                                // stream that died before its terminal event.
                            });
                            let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header("content-type", "text/event-stream")
                                    .body(StreamBody::new(stream))
                                    .unwrap(),
                            )
                        }),
                    )
                    .await;
            });
        }
    });
    (addr, task)
}

/// Drive one streamed turn through a proxy whose savings tracker writes to
/// `savings_path`, and return how many requests that tracker booked.
async fn booked_requests(complete: bool, savings_path: std::path::PathBuf) -> i64 {
    let (addr, _upstream) = anthropic_upstream(complete).await;
    let tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
        Some(savings_path),
        false,
    ));
    let probe = tracker.clone();
    let proxy = start_proxy_with_state(
        &format!("http://{addr}"),
        |c| {
            // `compression` is the interception master switch, and interception
            // is what tees the response into the SSE state machine that builds
            // the outcome. `for_test` leaves it off, so without this the proxy
            // is a byte pipe and there is no outcome to gate. The mode stays
            // `Off` so the request body itself is untouched.
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        },
        move |mut s| {
            s.savings_tracker = tracker;
            s
        },
    )
    .await;

    let body = json!({
        "model": "claude-3-haiku-20240307",
        "stream": true,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}]
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
    // Drain so the state-machine task runs to the end of the channel.
    let _ = resp.bytes().await;
    // Let the spawned state-machine task finish its end-of-stream work.
    tokio::time::sleep(Duration::from_millis(120)).await;

    let booked = probe.snapshot()["lifetime"]["requests"]
        .as_i64()
        .expect("lifetime.requests is a number");
    proxy.shutdown().await;
    booked
}

#[tokio::test]
async fn truncated_stream_is_not_booked() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        booked_requests(false, dir.path().join("savings.json")).await,
        0,
        "a stream that ended without message_stop carries partial usage; \
         booking it reports a turn that cost less than it did"
    );
}

#[tokio::test]
async fn complete_stream_is_booked() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        booked_requests(true, dir.path().join("savings.json")).await,
        1,
        "the same stream with its terminal event must still be booked — \
         the gate must not swallow healthy turns"
    );
}

/// Upstream 4949cd55: an errored stream on a 5xx HTTP status is failed work,
/// not a success and not an unbooked turn. Before the port the 529 arrived
/// as `text/event-stream`, skipped the failure branch, and booked a success
/// at stream close (inflating the save-rate); the `Errored` parser state
/// alone only reached the unbooked counter.
#[tokio::test]
async fn errored_stream_on_5xx_is_booked_failed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let savings_path = dir.path().join("savings.json");
    let (addr, _upstream) = anthropic_upstream_with_status(false, 529).await;
    let tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
        Some(savings_path),
        false,
    ));
    let probe = tracker.clone();
    let proxy = start_proxy_with_state(
        &format!("http://{addr}"),
        |c| {
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
            c.retry_enabled = true;
            c.retry_max_attempts = 3;
            c.retry_base_delay_ms = 1;
            c.retry_max_delay_ms = 10;
        },
        move |mut s| {
            s.savings_tracker = tracker;
            s
        },
    )
    .await;

    let body = json!({
        "model": "claude-3-haiku-20240307",
        "stream": true,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(serde_json::to_vec(&body).unwrap())
        .send()
        .await
        .expect("proxy responds");
    assert_eq!(resp.status(), 529);
    let _ = resp.bytes().await;
    tokio::time::sleep(Duration::from_millis(120)).await;

    let snap = probe.snapshot();
    assert_eq!(
        snap["lifetime"]["requests"].as_i64().unwrap_or(-1),
        0,
        "an exhausted 529 must not feed the success stats"
    );
    assert_eq!(
        snap["failed_work"]["requests"].as_i64().unwrap_or(-1),
        1,
        "an exhausted 529 must land in failed work"
    );
    assert_eq!(
        snap["failed_work"]["by_status"]["529"]
            .as_i64()
            .unwrap_or(-1),
        1,
        "failed work is attributed by status"
    );
    proxy.shutdown().await;
}

//! PR-A8 / P5-59: oversized request bodies surface as 413 Payload Too
//! Large, not 400 Bad Request. The pre-A8 path classified buffer
//! overflow as `InvalidHeader` (400), which broke clients with a
//! retry-on-413 backoff (they retried instead of giving up).
//!
//! Two paths:
//!   1. `Content-Length` header present and oversized: 413 returned
//!      immediately, body never consumed (cheap rejection).
//!   2. `Content-Length` missing (chunked): buffer-then-fail; still
//!      surfaces 413 once the cap is hit.
//!
//! Both trip `--max-body-bytes`. A body over `--compression-max-body-bytes`
//! but within the hard limit is forwarded unchanged instead.

use super::common;

use common::start_proxy_with;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn body_size_overflow_returns_413_not_400() {
    // Set the buffer cap small so we trip it without uploading
    // megabytes. The test exercises the chunked path (no
    // Content-Length on the wire — reqwest sends it but the proxy
    // still buffers).
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.max_body_bytes = 1024; // 1 KB cap
        c.compression_max_body_bytes = 1024;
    })
    .await;

    // Build a JSON body that's 4 KB — well over the 1 KB cap.
    let big_text = "A".repeat(4096);
    let body = format!(
        r#"{{"model":"claude-3-5-sonnet","messages":[{{"role":"user","content":"{}"}}]}}"#,
        big_text
    );
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    // 413 Payload Too Large — NOT 400 Bad Request.
    assert_eq!(
        resp.status().as_u16(),
        413,
        "expected 413 Payload Too Large; got {} (was 400 pre-A8)",
        resp.status()
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn body_size_overflow_with_content_length_header_returns_413_without_consuming() {
    let upstream = MockServer::start().await;
    // Mount a handler that records whether the upstream got hit; we
    // want to confirm the proxy short-circuited and never forwarded.
    let upstream_hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let upstream_hit_clone = upstream_hit.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_req: &wiremock::Request| {
            upstream_hit_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
        })
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.max_body_bytes = 1024;
        c.compression_max_body_bytes = 1024;
    })
    .await;

    // Use reqwest's default which sets Content-Length on a fixed body.
    let big_text = "B".repeat(8192);
    let body = format!(
        r#"{{"model":"claude-3-5-sonnet","messages":[{{"role":"user","content":"{}"}}]}}"#,
        big_text
    );
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("content-length", body.len().to_string())
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 413);
    // The pre-check rejected before forwarding — upstream never saw
    // the request.
    assert!(
        !upstream_hit.load(std::sync::atomic::Ordering::SeqCst),
        "Content-Length pre-check should reject before forwarding to upstream"
    );
    proxy.shutdown().await;
}

/// `--compression-max-body-bytes` promises "forwarded unchanged" for a
/// larger body, but the buffer read answered 413 until upstream 50bf775e8
/// was ported. Within `--max-body-bytes` the body must reach upstream
/// byte-for-byte, untouched by any transform.
#[tokio::test]
async fn body_over_compression_cap_but_under_hard_limit_is_forwarded_unchanged() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_max_body_bytes = 1024;
    })
    .await;

    // Spacing and key order a transform would normalise, so any rewrite shows.
    let big_text = "C".repeat(8192);
    let body = format!(
        r#"{{ "model": "claude-3-5-sonnet",  "max_tokens": 16, "messages": [ {{"role":"user","content":"{big_text}"}} ] }}"#
    );
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("content-length", body.len().to_string())
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body, body.as_bytes());
    proxy.shutdown().await;
}

/// A chunked body has no `Content-Length` to judge by. Past
/// `--compression-max-body-bytes` but within `--max-body-bytes` it must
/// still reach upstream unchanged; it used to meet the compression buffer
/// limit and get 413.
#[tokio::test]
async fn chunked_body_over_compression_cap_but_under_hard_limit_is_forwarded_unchanged() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.max_body_bytes = 64 * 1024;
        c.compression_max_body_bytes = 1024;
    })
    .await;

    let big_text = "D".repeat(8192);
    let body = format!(
        r#"{{ "model": "claude-3-5-sonnet",  "max_tokens": 16, "messages": [ {{"role":"user","content":"{big_text}"}} ] }}"#
    );
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(chunked(&body))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].body, body.as_bytes());
    proxy.shutdown().await;
}

/// The same chunked body past `--max-body-bytes` is still refused, and
/// never reaches upstream.
#[tokio::test]
async fn chunked_body_over_hard_limit_returns_413_when_limits_differ() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.max_body_bytes = 4096;
        c.compression_max_body_bytes = 1024;
    })
    .await;

    let big_text = "E".repeat(8192);
    let body = format!(
        r#"{{"model":"claude-3-5-sonnet","messages":[{{"role":"user","content":"{big_text}"}}]}}"#
    );
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(chunked(&body))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 413);
    assert!(upstream.received_requests().await.unwrap().is_empty());
    proxy.shutdown().await;
}

/// A body reqwest sends with `Transfer-Encoding: chunked` and no
/// `Content-Length`, in 1 KiB pieces.
fn chunked(body: &str) -> reqwest::Body {
    let pieces: Vec<Result<bytes::Bytes, std::io::Error>> = body
        .as_bytes()
        .chunks(1024)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();
    reqwest::Body::wrap_stream(futures::stream::iter(pieces))
}

/// A body past axum's 2 MiB default but under `--max-body-bytes` must reach
/// upstream. Only the Bedrock sub-router carried a `DefaultBodyLimit`, so
/// `/v1/messages` kept the framework default and answered 413 "Failed to
/// buffer the request body: length limit exceeded" — which Claude Code
/// reports to the user as "Request too large (max 32MB). Accumulated images
/// and attachments...", naming a limit and a cause that have nothing to do
/// with it. Seen on 2026-09-14 in six sessions, none holding an image.
#[tokio::test]
async fn body_over_axum_default_but_under_cap_is_forwarded() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.max_body_bytes = 32 * 1024 * 1024;
        c.compression_max_body_bytes = 32 * 1024 * 1024;
    })
    .await;

    // 3 MiB of text: over axum's 2 MiB default, well under the cap.
    let big_text = "A".repeat(3 * 1024 * 1024);
    let body = format!(
        r#"{{"model":"claude-3-5-sonnet","messages":[{{"role":"user","content":"{big_text}"}}],"max_tokens":16}}"#
    );
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "3 MiB under the cap must be forwarded, not rejected by the 2 MiB default"
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    proxy.shutdown().await;
}

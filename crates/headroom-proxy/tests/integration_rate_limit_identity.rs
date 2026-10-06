//! Rate-limit buckets are keyed on the peer unless the caller authenticated.
//!
//! Port of the upstream Python fix `6fb7cadf` (#3860, VAPT 01-F3): each
//! provider handler used to key its buckets on a client-supplied credential
//! header, so rotating the header bought a fresh bucket. The admission gate
//! now keys unauthenticated callers on the peer alone and authenticated
//! (or direct-loopback) callers on peer plus credential.
//!
//! The harness always connects from loopback, so these tests simulate a
//! remote peer the way a gateway deployment sees one: the proxy trusts
//! `X-Forwarded-For` from a configured gateway CIDR, and a relayed request
//! is never trusted without the proxy token.

mod common;

use common::start_proxy_with_state;
use headroom_proxy::forwarded_headers::load_trusted_gateway_cidrs;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROXY_TOKEN: &str = "test-proxy-token";
const REMOTE_PEER: &str = "203.0.113.9";
const OTHER_PEER: &str = "198.51.100.7";

fn messages_body() -> serde_json::Value {
    json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hello"}]
    })
}

fn upstream_reply() -> serde_json::Value {
    json!({
        "id": "msg_rl",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "hello back"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3}
    })
}

fn limited_proxy(
    upstream: &str,
    token: Option<&str>,
) -> impl std::future::Future<Output = common::ProxyHandle> {
    let t = token.map(str::to_string);
    start_proxy_with_state(
        upstream,
        move |c| {
            c.rate_limit_enabled = true;
            c.rate_limit_rpm = 1;
            c.proxy_token = t.clone();
        },
        |mut s| {
            s.trusted_gateway_cidrs = load_trusted_gateway_cidrs("127.0.0.0/8").expect("test CIDR");
            s
        },
    )
}

async fn post_messages(
    proxy_base: &str,
    api_key: &str,
    peer: &str,
    token: Option<&str>,
) -> reqwest::StatusCode {
    let mut req = reqwest::Client::new()
        .post(format!("{proxy_base}/v1/messages"))
        .header("x-api-key", api_key)
        .header("x-forwarded-for", peer)
        .header("anthropic-version", "2023-06-01")
        .json(&messages_body());
    if let Some(t) = token {
        req = req.header("x-headroom-proxy-token", t);
    }
    req.send().await.unwrap().status()
}

/// Rotating the credential from one relayed peer does not escape the bucket:
/// both requests land in `peer:<addr>`, so with rpm=1 the second is refused.
#[tokio::test]
async fn rotating_credential_from_one_peer_does_not_escape_bucket() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_reply()))
        .expect(1)
        .mount(&upstream)
        .await;

    let proxy = limited_proxy(&upstream.uri(), None).await;
    assert_eq!(
        post_messages(&proxy.url(), "key-one", REMOTE_PEER, None).await,
        200
    );
    assert_eq!(
        post_messages(&proxy.url(), "key-two", REMOTE_PEER, None).await,
        429
    );
    proxy.shutdown().await;
}

/// Authenticated callers keep per-credential buckets: a fresh credential
/// from the same relayed peer passes, and the same credential from two
/// different peers never shares a bucket.
#[tokio::test]
async fn authenticated_callers_keep_per_credential_buckets() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_reply()))
        .expect(2)
        .mount(&upstream)
        .await;

    let proxy = limited_proxy(&upstream.uri(), Some(PROXY_TOKEN)).await;
    assert_eq!(
        post_messages(&proxy.url(), "key-one", REMOTE_PEER, Some(PROXY_TOKEN)).await,
        200
    );
    // Same peer, rotated credential: a different bucket, still allowed.
    assert_eq!(
        post_messages(&proxy.url(), "key-two", REMOTE_PEER, Some(PROXY_TOKEN)).await,
        200
    );
    proxy.shutdown().await;
}

/// Same credential from two relayed peers: with the token each peer owns
/// its bucket, so neither request is refused.
#[tokio::test]
async fn same_credential_from_two_peers_does_not_share_bucket() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_reply()))
        .expect(2)
        .mount(&upstream)
        .await;

    let proxy = limited_proxy(&upstream.uri(), Some(PROXY_TOKEN)).await;
    assert_eq!(
        post_messages(&proxy.url(), "key-one", REMOTE_PEER, Some(PROXY_TOKEN)).await,
        200
    );
    assert_eq!(
        post_messages(&proxy.url(), "key-one", OTHER_PEER, Some(PROXY_TOKEN)).await,
        200
    );
    proxy.shutdown().await;
}

/// A repeated credential from one authenticated peer still shares its
/// bucket: per-credential does not mean unlimited.
#[tokio::test]
async fn repeated_credential_from_one_peer_shares_its_bucket() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_reply()))
        .expect(1)
        .mount(&upstream)
        .await;

    let proxy = limited_proxy(&upstream.uri(), Some(PROXY_TOKEN)).await;
    assert_eq!(
        post_messages(&proxy.url(), "key-one", REMOTE_PEER, Some(PROXY_TOKEN)).await,
        200
    );
    assert_eq!(
        post_messages(&proxy.url(), "key-one", REMOTE_PEER, Some(PROXY_TOKEN)).await,
        429
    );
    proxy.shutdown().await;
}

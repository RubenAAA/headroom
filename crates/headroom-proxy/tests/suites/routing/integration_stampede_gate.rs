//! Same-head stampede gate on the live request path.
//!
//! Two requests with one cacheable head sent in the same instant: the second
//! must reach the upstream only after the first's response has begun, and
//! two requests with different heads must not wait on each other.

use super::common;

use common::start_proxy_with;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const UPSTREAM_DELAY: Duration = Duration::from_millis(120);

/// Records when each request arrived at the upstream and answers after
/// `UPSTREAM_DELAY`, standing in for a provider's time to first byte.
async fn mount_slow_upstream(upstream: &MockServer) -> Arc<Mutex<Vec<(Instant, String)>>> {
    let arrivals: Arc<Mutex<Vec<(Instant, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = arrivals.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).expect("json body");
            let system = body["system"][0]["text"].as_str().unwrap_or("").to_string();
            sink.lock().unwrap().push((Instant::now(), system));
            ResponseTemplate::new(200)
                .set_delay(UPSTREAM_DELAY)
                .set_body_json(json!({
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-3-5-sonnet-20241022",
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 10, "output_tokens": 1}
                }))
        })
        .mount(upstream)
        .await;
    arrivals
}

fn payload(system: &str, user: &str) -> Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 32,
        "system": [
            {"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}
        ],
        "messages": [{"role": "user", "content": user}]
    })
}

async fn post(proxy_url: &str, body: Value) {
    let resp = common::shared_client()
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-ant-api03-stampede")
        .json(&body)
        .send()
        .await
        .expect("proxy responds");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn follower_waits_for_the_leader_first_byte() {
    let upstream = MockServer::start().await;
    let arrivals = mount_slow_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.cache_stampede_gate = true;
        c.cache_stampede_wait_cap = Duration::from_secs(5);
    })
    .await;

    let url = proxy.url();
    let a = tokio::spawn({
        let url = url.clone();
        async move { post(&url, payload("shared head", "first")).await }
    });
    // Prove the leader is actually in flight instead of assuming 30ms
    // sufficed: the follower must overlap it at the gate.
    assert!(
        common::wait_until(Duration::from_secs(2), Duration::from_millis(5), || {
            !arrivals.lock().unwrap().is_empty()
        })
        .await,
        "leader request never reached the upstream"
    );
    let b = tokio::spawn({
        let url = url.clone();
        async move { post(&url, payload("shared head", "second")).await }
    });
    a.await.unwrap();
    b.await.unwrap();

    let seen = arrivals.lock().unwrap();
    assert_eq!(seen.len(), 2, "both requests reached the upstream");
    let gap = seen[1].0.duration_since(seen[0].0);
    assert!(
        gap >= UPSTREAM_DELAY - Duration::from_millis(50),
        "follower arrived {gap:?} after the leader; expected about {UPSTREAM_DELAY:?}"
    );
}

#[tokio::test]
async fn different_heads_do_not_wait() {
    let upstream = MockServer::start().await;
    let arrivals = mount_slow_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.cache_stampede_gate = true;
        c.cache_stampede_wait_cap = Duration::from_secs(5);
    })
    .await;

    let url = proxy.url();
    let a = tokio::spawn({
        let url = url.clone();
        async move { post(&url, payload("head one", "first")).await }
    });
    // Prove the first request is actually in flight instead of assuming
    // 30ms sufficed.
    assert!(
        common::wait_until(Duration::from_secs(2), Duration::from_millis(5), || {
            !arrivals.lock().unwrap().is_empty()
        })
        .await,
        "first request never reached the upstream"
    );
    let b = tokio::spawn({
        let url = url.clone();
        async move { post(&url, payload("head two", "second")).await }
    });
    a.await.unwrap();
    b.await.unwrap();

    let seen = arrivals.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let gap = seen[1].0.duration_since(seen[0].0);
    assert!(
        gap < UPSTREAM_DELAY,
        "unrelated heads should not queue; gap was {gap:?}"
    );
}

#[tokio::test]
async fn warm_head_does_not_wait_on_the_next_turn() {
    let upstream = MockServer::start().await;
    let arrivals = mount_slow_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.cache_stampede_gate = true;
        c.cache_stampede_wait_cap = Duration::from_secs(5);
    })
    .await;

    let url = proxy.url();
    post(&url, payload("shared head", "turn one")).await;
    let a = tokio::spawn({
        let url = url.clone();
        async move { post(&url, payload("shared head", "turn two")).await }
    });
    // Prove turn two is actually in flight instead of assuming 30ms
    // sufficed: the sibling must overlap it at the gate.
    assert!(
        common::wait_until(Duration::from_secs(2), Duration::from_millis(5), || {
            !arrivals.lock().unwrap().is_empty()
        })
        .await,
        "turn two never reached the upstream"
    );
    let b = tokio::spawn({
        let url = url.clone();
        async move { post(&url, payload("shared head", "sibling")).await }
    });
    a.await.unwrap();
    b.await.unwrap();

    let seen = arrivals.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let gap = seen[2].0.duration_since(seen[1].0);
    assert!(
        gap < UPSTREAM_DELAY,
        "a warm head must not park its followers; gap was {gap:?}"
    );
}

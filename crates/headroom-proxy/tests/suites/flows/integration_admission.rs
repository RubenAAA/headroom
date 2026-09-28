//! The admission gate: request rate, token rate and spend budget are refused
//! with 429 on every generation route before anything reaches the upstream,
//! and limits that are not configured refuse nothing. See `src/admission.rs`.

use super::common;

use common::{start_proxy_with, start_proxy_with_state};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn ok_upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;
    upstream
}

async fn post(url: &str, key: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(url)
        .header("x-api-key", key)
        .json(body)
        .send()
        .await
        .unwrap()
}

fn messages_body(text: &str) -> Value {
    json!({"model": "claude-sonnet-4", "max_tokens": 8,
           "messages": [{"role": "user", "content": text}]})
}

/// `/v1/messages` has no handler of its own (it falls through to the
/// catch-all), which is the route the old per-handler check never covered.
#[tokio::test]
async fn request_rate_limits_messages_per_credential() {
    let upstream = ok_upstream().await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.rate_limit_enabled = true;
        c.rate_limit_rpm = 2;
    })
    .await;
    let url = format!("{}/v1/messages", proxy.url());

    for _ in 0..2 {
        assert_eq!(
            post(&url, "key-a", &messages_body("hi")).await.status(),
            200
        );
    }
    let refused = post(&url, "key-a", &messages_body("hi")).await;
    assert_eq!(refused.status(), 429);
    let retry_after: u64 = refused.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=31).contains(&retry_after), "retry-after {retry_after}");
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");

    // Another credential has its own bucket.
    assert_eq!(
        post(&url, "key-b", &messages_body("hi")).await.status(),
        200
    );
    // count_tokens is not a generation and is never charged.
    let count = post(
        &format!("{}/v1/messages/count_tokens", proxy.url()),
        "key-a",
        &messages_body("hi"),
    )
    .await;
    assert_ne!(count.status(), 429);

    let generations = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/messages")
        .count();
    assert_eq!(
        generations, 3,
        "the refused request must not reach upstream"
    );
    proxy.shutdown().await;
}

/// A request larger than the whole token bucket is admitted once, not
/// refused forever, and the debt it leaves refuses the next one.
#[tokio::test]
async fn token_rate_admits_an_oversized_request_then_makes_the_next_wait() {
    let upstream = ok_upstream().await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.rate_limit_enabled = true;
        c.rate_limit_tpm = 100;
    })
    .await;
    let url = format!("{}/v1/chat/completions", proxy.url());
    let big = json!({"model": "gpt-4o",
                     "messages": [{"role": "user", "content": "word ".repeat(2000)}]});

    assert_eq!(post(&url, "k", &big).await.status(), 200);
    let refused = post(&url, "k", &json!({"model": "gpt-4o", "messages": []})).await;
    assert_eq!(refused.status(), 429);
    let body: Value = refused.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("Token rate limited"), "{message}");
    // Upstream sees the admitted body intact, re-assembled after counting.
    let seen = upstream.received_requests().await.unwrap();
    assert_eq!(seen.len(), 1);
    let forwarded: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(
        forwarded["messages"][0]["content"],
        big["messages"][0]["content"]
    );
    proxy.shutdown().await;
}

/// Once the period's spend passes `--budget-limit-usd`, every generation
/// route refuses — not only Anthropic's, as upstream had it before
/// `f734c573`.
#[tokio::test]
async fn spent_budget_refuses_every_generation_route() {
    let upstream = ok_upstream().await;
    let proxy = start_proxy_with_state(
        &upstream.uri(),
        |c| c.budget_limit_usd = Some(0.01),
        |state| {
            state.cost_tracker.record_tokens(
                "claude-sonnet-4",
                &headroom_core::cost_tracker::TokenRecord {
                    tokens_sent: 1_000_000,
                    uncached_tokens: 1_000_000,
                    ..Default::default()
                },
            );
            state
        },
    )
    .await;

    for route in [
        "/v1/messages",
        "/anthropic/v1/messages",
        "/v1/chat/completions",
        "/v1/responses",
        "/v1beta/models/gemini-2.5-pro:generateContent",
    ] {
        let r = post(
            &format!("{}{route}", proxy.url()),
            "k",
            &messages_body("hi"),
        )
        .await;
        assert_eq!(r.status(), 429, "{route}");
        let body: Value = r.json().await.unwrap();
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("Budget exceeded"), "{route}: {message}");
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    proxy.shutdown().await;
}

/// No limit configured: nothing is refused, however fast the requests come.
#[tokio::test]
async fn unconfigured_limits_refuse_nothing() {
    let upstream = ok_upstream().await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.rate_limit_enabled = true;
        c.rate_limit_rpm = 0;
        c.rate_limit_tpm = 0;
    })
    .await;
    let url = format!("{}/v1/messages", proxy.url());
    for _ in 0..100 {
        assert_eq!(post(&url, "k", &messages_body("hi")).await.status(), 200);
    }
    proxy.shutdown().await;
}

/// The ledger tells the proxy's own 429 from the provider's (upstream
/// `5ff4ea1e`): one is fixed by raising the cap, the other by backing off.
#[tokio::test]
async fn ledger_splits_rate_limits_by_who_refused() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_json(
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}),
        ))
        .mount(&upstream)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let tracker = std::sync::Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
        Some(dir.path().join("savings.json")),
        false,
    ));
    let probe = tracker.clone();
    let proxy = start_proxy_with_state(
        &upstream.uri(),
        |c| {
            // Interception builds the outcome that books the upstream 429.
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
            c.rate_limit_enabled = true;
            c.rate_limit_rpm = 1;
            c.retry_enabled = false;
        },
        move |mut s| {
            s.savings_tracker = tracker;
            s
        },
    )
    .await;
    let url = format!("{}/v1/messages", proxy.url());

    // The first request reaches the provider, which refuses it; the second
    // is over our own rate and never leaves.
    for _ in 0..2 {
        assert_eq!(post(&url, "k", &messages_body("hi")).await.status(), 429);
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);

    let snapshot = probe.metrics_snapshot(&json!({}));
    let requests = &snapshot["requests"];
    assert_eq!(requests["rate_limited"], 2, "{snapshot}");
    assert_eq!(
        requests["rate_limited_by_source"],
        json!({"headroom": 1, "upstream": 1})
    );
    assert_eq!(
        requests["rate_limited_by_provider"],
        json!({"anthropic": 2})
    );
    proxy.shutdown().await;
}

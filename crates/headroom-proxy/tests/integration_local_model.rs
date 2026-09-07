//! Integration tests for local model routing.
//!
//! When `local_model` is not configured, `/v1/messages` passes through
//! to the upstream transparently. When configured, matching requests
//! are translated to OpenAI format and forwarded to the local upstream.

mod common;

use common::{start_proxy_with, start_proxy_with_state};
use headroom_proxy::config::ModelRoute;
use headroom_proxy::model_router::ModelRouterConfig;
use serde_json::json;
use std::sync::Arc;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// When local_model is not configured, /v1/messages passes through
/// to the upstream transparently (no format translation).
#[tokio::test]
async fn passthrough_when_local_model_disabled() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let proxy = start_proxy_with(mock.uri().as_str(), |_| {}).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-key")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "hello");

    proxy.shutdown().await;
}

/// When local_model IS configured but the model doesn't match,
/// the request falls through to the default upstream transparently.
#[tokio::test]
async fn non_matching_model_falls_through() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "from upstream"}],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let proxy = start_proxy_with(mock.uri().as_str(), |cfg| {
        cfg.local_model = Some("qwen36-uncensored".to_string());
        cfg.local_upstream = Some("http://127.0.0.1:19999".parse().unwrap());
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "from upstream");

    proxy.shutdown().await;
}

async fn codex_responses_sse_upstream() -> MockServer {
    let mock = MockServer::start().await;

    let response_body = [
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_test\",\"model\":\"gpt-5.5\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"hello from responses\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
    ]
    .join("");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("cache-control", "no-cache")
                .set_body_string(response_body),
        )
        .mount(&mock)
        .await;

    mock
}

/// Gateway discovery should expose only exact-match routed models with
/// discoverable IDs, so Claude Code can populate its model picker.
#[tokio::test]
async fn gateway_model_discovery_lists_discoverable_routes() {
    let mock = MockServer::start().await;

    let proxy = start_proxy_with(mock.uri().as_str(), |cfg| {
        cfg.local_model = Some("claude-local".to_string());
        cfg.model_routes = vec![
            ModelRoute {
                model_prefix: "claude-codex-5.5".to_string(),
                prefix_match: false,
                upstream: Some(Url::parse("https://api.openai.com/v1").unwrap()),
                translate: true,
                cursor_agent: None,
                target_model: Some("gpt-5.5".to_string()),
                auth_env: None,
            },
            ModelRoute {
                model_prefix: "codex-*".to_string(),
                prefix_match: true,
                upstream: Some(Url::parse("https://api.openai.com/v1").unwrap()),
                translate: true,
                cursor_agent: None,
                target_model: Some("gpt-5.5".to_string()),
                auth_env: None,
            },
            ModelRoute {
                model_prefix: "anthropic-routed".to_string(),
                prefix_match: false,
                upstream: None,
                translate: false,
                cursor_agent: Some("cursor-grok-4.6-high".to_string()),
                target_model: None,
                auth_env: None,
            },
        ];
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/v1/models", proxy.url()))
        .header("x-api-key", "test-key")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let ids: Vec<String> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect();

    assert!(ids.contains(&"claude-local".to_string()));
    assert!(ids.contains(&"claude-codex-5.5".to_string()));
    assert!(ids.contains(&"anthropic-routed".to_string()));
    assert!(!ids.contains(&"codex-*".to_string()));

    proxy.shutdown().await;
}

/// Codex-targeted translate routes should route to OpenAI Responses
/// instead of Chat Completions, so the upstream path matches the
/// Codex/OpenAI surface.
#[tokio::test]
async fn codex_translate_route_uses_responses_endpoint() {
    let mock = codex_responses_sse_upstream().await;
    let upstream_url = Url::parse(&mock.uri()).unwrap();

    let proxy = start_proxy_with(&mock.uri(), |cfg| {
        cfg.model_routes = vec![ModelRoute {
            model_prefix: "claude-codex-5.5".to_string(),
            prefix_match: false,
            upstream: Some(upstream_url.clone()),
            translate: true,

            cursor_agent: None,
            target_model: Some("gpt-5.5".to_string()),
            auth_env: None,
        }];
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let body: String = resp.text().await.unwrap();
    assert!(body.contains("message_start"));
    assert!(body.contains("hello from responses"));

    let received_requests = mock.received_requests().await.unwrap();
    let upstream_body = received_requests
        .last()
        .expect("upstream request")
        .body
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&upstream_body).unwrap();
    assert_eq!(parsed["model"], "gpt-5.5");
    assert_eq!(parsed["store"], false);
    assert_eq!(parsed["stream"], true);
    assert!(parsed.get("max_output_tokens").is_none());
    assert!(parsed.get("max_tokens").is_none());
    assert!(parsed.get("temperature").is_none());
    assert!(parsed.get("messages").is_none());
    assert_eq!(parsed["input"][0]["role"], "user");

    proxy.shutdown().await;
}

/// Non-stream Codex requests still need to validate as normal Anthropic
/// JSON, even though the upstream transport is streamed Responses SSE.
#[tokio::test]
async fn codex_translate_route_buffers_non_stream_responses() {
    let mock = codex_responses_sse_upstream().await;
    let upstream_url = Url::parse(&mock.uri()).unwrap();

    let proxy = start_proxy_with(&mock.uri(), |cfg| {
        cfg.model_routes = vec![ModelRoute {
            model_prefix: "claude-codex-5.5".to_string(),
            prefix_match: false,
            upstream: Some(upstream_url.clone()),
            translate: true,

            cursor_agent: None,
            target_model: Some("gpt-5.5".to_string()),
            auth_env: None,
        }];
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "stream": false,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["usage"]["input_tokens"], 5);
    assert_eq!(body["usage"]["output_tokens"], 2);
    assert_eq!(body["content"][0]["text"], "hello from responses");

    proxy.shutdown().await;
}

/// A Retry-After longer than the proxy's in-request wait cap must not be
/// shortened into an early retry. Return the original response so the client
/// sees both the 429 and the server's requested delay.
#[tokio::test]
async fn over_cap_retry_after_returns_immediately_and_preserves_header() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "31")
                .set_body_json(json!({"error": {"message": "rate limited"}})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let upstream_url = Url::parse(&mock.uri()).unwrap();
    let savings_dir = tempfile::tempdir().unwrap();
    let savings_path = savings_dir.path().join("savings.json");
    let proxy = start_proxy_with_state(
        &mock.uri(),
        |cfg| {
            cfg.compression = true;
            cfg.compression_mode = headroom_proxy::config::CompressionMode::Off;
            cfg.retry_enabled = true;
            cfg.retry_max_attempts = 3;
            cfg.retry_max_delay_ms = 30_000;
            cfg.model_routes = vec![ModelRoute {
                model_prefix: "claude-codex-5.5".to_string(),
                prefix_match: false,
                upstream: Some(upstream_url.clone()),
                translate: true,
                cursor_agent: None,
                target_model: Some("gpt-5.5".to_string()),
                auth_env: None,
            }];
        },
        move |mut state| {
            state.savings_tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
                Some(savings_path),
                false,
            ));
            state
        },
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "31");
    assert!(resp.text().await.unwrap().contains("rate limited"));
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);

    proxy.shutdown().await;
}

#[tokio::test]
async fn direct_anthropic_over_cap_retry_after_is_not_retried_early() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "31")
                .set_body_json(json!({"error": {"message": "slow down"}})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let savings_dir = tempfile::tempdir().unwrap();
    let savings_path = savings_dir.path().join("savings.json");
    let proxy = start_proxy_with_state(
        &mock.uri(),
        |cfg| {
            cfg.retry_enabled = true;
            cfg.retry_max_attempts = 3;
            cfg.retry_max_delay_ms = 30_000;
        },
        move |mut state| {
            state.savings_tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
                Some(savings_path),
                false,
            ));
            state
        },
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "31");
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);

    proxy.shutdown().await;
}

#[tokio::test]
async fn exhausted_5xx_retries_land_only_in_failed_work() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(529).set_body_json(json!({"error": {"message": "overloaded"}})),
        )
        .expect(3)
        .mount(&mock)
        .await;

    let savings_dir = tempfile::tempdir().unwrap();
    let savings_path = savings_dir.path().join("savings.json");
    let proxy = start_proxy_with_state(
        &mock.uri(),
        |cfg| {
            cfg.compression = true;
            cfg.compression_mode = headroom_proxy::config::CompressionMode::Off;
            cfg.retry_enabled = true;
            cfg.retry_max_attempts = 3;
            cfg.retry_base_delay_ms = 1;
            cfg.retry_max_delay_ms = 10;
        },
        move |mut state| {
            state.savings_tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
                Some(savings_path),
                false,
            ));
            state
        },
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 529);

    let stats: serde_json::Value = client
        .get(format!("{}/stats", proxy.url()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let success = &stats["persistent_savings"]["lifetime"];
    let failed = &stats["persistent_savings"]["failed_work"];
    assert_eq!(success["requests"], 0);
    assert_eq!(success["tokens_saved"], 0);
    assert_eq!(failed["requests"], 1);
    assert_eq!(failed["upstream_attempts"], 3);
    assert!(failed["forwarded_tokens"].as_i64().unwrap() > 0);
    assert_eq!(
        failed["forwarded_tokens_at_risk"].as_i64().unwrap(),
        failed["forwarded_tokens"].as_i64().unwrap() * 3
    );
    assert_eq!(failed["provider_usage_observed_requests"], 0);
    assert_eq!(failed["by_status"]["529"], 1);

    proxy.shutdown().await;
}

// ---------------------------------------------------------------------------
// Cost-aware routing into the route table: a rule rewriting a request's model
// to a route alias must be served from that route's upstream.
// ---------------------------------------------------------------------------

/// Proxy with a Zen-shaped Responses mock as the route upstream and a
/// cost-aware rule sending small tool-less turns at the route's alias. The
/// default upstream answering means the rule did not fire.
fn spark_router_config(zen: &MockServer) -> (String, ModelRoute, ModelRouterConfig) {
    let route = ModelRoute {
        model_prefix: "claude-muse-spark-1.3".to_string(),
        prefix_match: false,
        upstream: Some(Url::parse(&zen.uri()).unwrap()),
        translate: true,
        cursor_agent: None,
        target_model: Some("muse-spark-1.3".to_string()),
        auth_env: Some("none".to_string()),
    };
    let router = ModelRouterConfig::from_env(
        Some("1"),
        Some(
            r#"[{"name": "small-no-tools->spark", "require_no_tools": true,
                  "max_input_tokens": 4000, "to_model": "claude-muse-spark-1.3"}]"#,
        ),
    );
    (zen.uri(), route, router)
}

/// A small tool-less turn on a Claude model is rewritten to the spark alias
/// and served from the route's upstream — never touching the default one.
#[tokio::test]
async fn cost_aware_rule_routes_a_small_tool_less_turn() {
    let default = MockServer::start().await;
    let default_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = default_hits.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_: &wiremock::Request| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_default", "type": "message", "role": "assistant",
                "content": [{"type": "text", "text": "from default"}],
                "model": "claude-opus-5", "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }))
        })
        .mount(&default)
        .await;

    let zen = codex_responses_sse_upstream().await;
    let (_uri, route, router) = spark_router_config(&zen);
    let proxy = start_proxy_with(&default.uri(), |cfg| {
        cfg.model_routes = vec![route];
        cfg.model_router = router;
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-opus-5",
            "max_tokens": 100,
            "stream": false,
            "messages": [{"role": "user", "content": "What is 2+2?"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "hello from responses");
    assert_eq!(
        default_hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a routed turn must not touch the default upstream"
    );

    let received = zen.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(upstream_body["model"], "muse-spark-1.3");

    proxy.shutdown().await;
}

/// A turn declaring tools does not match the rule and keeps the direct path.
#[tokio::test]
async fn cost_aware_rule_skips_a_tool_using_turn() {
    let default = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_default", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "from default"}],
            "model": "claude-opus-5", "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
        .mount(&default)
        .await;

    let zen = codex_responses_sse_upstream().await;
    let (_uri, route, router) = spark_router_config(&zen);
    let proxy = start_proxy_with(&default.uri(), |cfg| {
        cfg.model_routes = vec![route];
        cfg.model_router = router;
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-opus-5",
            "max_tokens": 100,
            "stream": false,
            "messages": [{"role": "user", "content": "Read the file."}],
            "tools": [{"name": "Read", "description": "read",
                       "input_schema": {"type": "object"}}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "from default");
    assert!(
        zen.received_requests().await.unwrap().is_empty(),
        "a tool-using turn must not reach the routed upstream"
    );

    proxy.shutdown().await;
}

/// Claude Code appends `?beta=true` to ordinary `/v1/messages` turns, and a
/// streamed routed turn has to survive it whole.
///
/// Both halves of this matter. The router once sat behind an
/// `if uri.query().is_none()` guard, which read as a narrow exclusion and
/// was in fact a kill switch: every real turn carries the query, so nothing
/// was ever routed, while the tests above kept passing because they omit it.
/// The stream then has to arrive complete — a truncated one is what the
/// guard was added to chase, so asserting `message_stop` here is what tells
/// the two failures apart next time.
#[tokio::test]
async fn a_beta_query_does_not_disable_cost_aware_routing() {
    let default = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_default", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "from default"}],
            "model": "claude-opus-5", "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
        .mount(&default)
        .await;

    let zen = codex_responses_sse_upstream().await;
    let (_uri, route, router) = spark_router_config(&zen);
    let proxy = start_proxy_with(&default.uri(), |cfg| {
        cfg.model_routes = vec![route];
        cfg.model_router = router;
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages?beta=true", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .header("accept", "text/event-stream")
        .json(&json!({
            "model": "claude-opus-5",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "What is 2+2?"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let sse = resp.text().await.unwrap();
    assert!(
        sse.contains("event: message_start"),
        "routed stream never opened: {sse}"
    );
    assert!(
        sse.contains("hello from responses"),
        "routed stream carried no text: {sse}"
    );
    assert!(
        sse.contains("event: message_stop"),
        "routed stream ended before message_stop: {sse}"
    );

    let received = zen.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "the query string kept the turn off the routed upstream"
    );
    let upstream_body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(upstream_body["model"], "muse-spark-1.3");
    assert!(
        received[0].url.query().is_none(),
        "the routed upstream URL must not carry the client's query"
    );

    proxy.shutdown().await;
}

// ---------------------------------------------------------------------------
// Fallback and cooldown: a routed upstream that will not serve the turn must
// not cost the client the turn.
// ---------------------------------------------------------------------------

/// A Zen-shaped upstream that refuses every request with a 503.
async fn failing_zen_upstream() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .mount(&mock)
        .await;
    mock
}

/// A default upstream answering `/v1/messages` with one Anthropic turn.
async fn anthropic_default_upstream(text: &'static str) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_default", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "model": "claude-opus-5", "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
        .mount(&mock)
        .await;
    mock
}

/// Keep the retry budget's backoff out of the test's wall time. The count
/// stays at the default 3 so the assertion below is about the real budget.
fn fast_retries(cfg: &mut headroom_proxy::Config) {
    cfg.retry_base_delay_ms = 1;
    cfg.retry_max_delay_ms = 5;
    // Buffered rather than relayed blind, which is what puts a served turn in
    // front of the outcome funnel the accounting assertions read. `Off` keeps
    // the bytes untouched, so this changes what is measured, not what is sent.
    cfg.compression = true;
    cfg.compression_mode = headroom_proxy::config::CompressionMode::Off;
    // The response cache would serve the second turn of a test from the first
    // turn's answer, which would make "the router did not fire" pass without
    // the router having been consulted at all.
    cfg.cache_enabled = false;
}

fn tool_less_turn(prompt: &str) -> serde_json::Value {
    json!({
        "model": "claude-opus-5",
        "max_tokens": 100,
        "stream": false,
        "messages": [{"role": "user", "content": prompt}]
    })
}

/// The whole point of the fallback: the router's choice failing is headroom's
/// problem, not the client's. The client asked for `claude-opus-5` and gets a
/// complete answer from it, and the failed attempt's bytes never appear.
#[tokio::test]
async fn a_failing_routed_upstream_falls_back_to_the_clients_model() {
    let default = anthropic_default_upstream("from default").await;
    let zen = failing_zen_upstream().await;
    let (_uri, route, router) = spark_router_config(&zen);

    let mut cooldowns: Option<headroom_proxy::model_router::ModelCooldowns> = None;
    let proxy = start_proxy_with_state(
        &default.uri(),
        |cfg| {
            fast_retries(cfg);
            cfg.model_routes = vec![route];
            cfg.model_router = router;
        },
        |state| {
            cooldowns = Some(state.model_route_cooldowns.clone());
            state
        },
    )
    .await;
    let cooldowns = cooldowns.expect("state captured");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&tool_less_turn("What is 2+2?"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "the client must not see the 503");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "from default");
    assert_eq!(
        body["type"], "message",
        "the reply must be Anthropic-shaped"
    );

    assert_eq!(
        zen.received_requests().await.unwrap().len(),
        3,
        "the routed upstream should have had the full retry budget first"
    );
    assert!(
        cooldowns.remaining("claude-muse-spark-1.3").is_some(),
        "the failed target must be parked in cooldown"
    );

    // The next tool-less turn is not offered to the same target again.
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&tool_less_turn("And what is 3+3?"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "from default");
    assert_eq!(
        zen.received_requests().await.unwrap().len(),
        3,
        "a turn inside the cooldown window must not reach the routed upstream"
    );

    // Accounting: each turn is booked once, against the model that answered.
    // Booking the alias would price Zen tokens off a row nobody was served
    // from, and would credit the reroute's savings to a reroute that did not
    // happen.
    let stats: serde_json::Value = client
        .get(format!("{}/stats", proxy.url()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let models: Vec<&str> = stats["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["model"].as_str().unwrap())
        .collect();
    assert_eq!(models, vec!["claude-opus-5", "claude-opus-5"], "{stats:#}");
    assert_eq!(stats["total_logged"], 2, "the failed attempt must not book");

    proxy.shutdown().await;
}

/// The cooldown is a pause, not a kill switch: once the window passes, the
/// target is tried again without a restart.
#[tokio::test]
async fn the_routed_target_is_tried_again_after_the_cooldown_window() {
    let default = anthropic_default_upstream("from default").await;
    let zen = failing_zen_upstream().await;
    let (_uri, route, mut router) = spark_router_config(&zen);
    router.cooldown = Some(std::time::Duration::from_millis(250));

    let proxy = start_proxy_with(&default.uri(), |cfg| {
        fast_retries(cfg);
        cfg.model_routes = vec![route];
        cfg.model_router = router;
    })
    .await;

    let client = reqwest::Client::new();
    let send = |prompt: &'static str| {
        let client = client.clone();
        let url = format!("{}/v1/messages", proxy.url());
        async move {
            client
                .post(url)
                .header("content-type", "application/json")
                .header("x-api-key", "test-key")
                .header("anthropic-version", "2023-06-01")
                .json(&tool_less_turn(prompt))
                .send()
                .await
                .unwrap()
        }
    };

    assert_eq!(send("first").await.status(), 200);
    let after_first = zen.received_requests().await.unwrap().len();
    assert_eq!(after_first, 3);

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    assert_eq!(send("second").await.status(), 200);
    assert_eq!(
        zen.received_requests().await.unwrap().len(),
        6,
        "once the window expires the router must offer the target again"
    );

    proxy.shutdown().await;
}

/// A client that names the routed alias itself gets that route's answer,
/// error included. Silently serving a different model would be the surprise.
#[tokio::test]
async fn an_explicit_pick_of_the_routed_model_still_returns_the_error() {
    let default = anthropic_default_upstream("from default").await;
    let zen = failing_zen_upstream().await;
    let (_uri, route, router) = spark_router_config(&zen);

    let mut cooldowns: Option<headroom_proxy::model_router::ModelCooldowns> = None;
    let proxy = start_proxy_with_state(
        &default.uri(),
        |cfg| {
            fast_retries(cfg);
            cfg.model_routes = vec![route];
            cfg.model_router = router;
        },
        |state| {
            cooldowns = Some(state.model_route_cooldowns.clone());
            state
        },
    )
    .await;
    let cooldowns = cooldowns.expect("state captured");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-muse-spark-1.3",
            "max_tokens": 100,
            "stream": false,
            "messages": [{"role": "user", "content": "What is 2+2?"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 503, "an explicit pick keeps its own error");
    assert!(
        default.received_requests().await.unwrap().is_empty(),
        "the client's own choice must not be second-guessed"
    );
    assert!(
        cooldowns.remaining("claude-muse-spark-1.3").is_none(),
        "no fallback happened, so nothing goes into cooldown"
    );

    proxy.shutdown().await;
}

/// A streamed turn has to fall back whole: the client asked for SSE and must
/// get one complete Anthropic stream, with nothing of the failed attempt in
/// front of it.
#[tokio::test]
async fn a_streamed_turn_falls_back_to_a_complete_anthropic_stream() {
    let default = MockServer::start().await;
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"from default\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&default)
        .await;

    let zen = failing_zen_upstream().await;
    let (_uri, route, router) = spark_router_config(&zen);
    let proxy = start_proxy_with(&default.uri(), |cfg| {
        fast_retries(cfg);
        cfg.model_routes = vec![route];
        cfg.model_router = router;
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages?beta=true", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .header("accept", "text/event-stream")
        .json(&json!({
            "model": "claude-opus-5",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role": "user", "content": "What is 2+2?"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.starts_with("event: message_start"),
        "the stream must open cleanly, with no trace of the failed attempt: {body}"
    );
    assert!(body.contains("from default"), "{body}");
    assert!(
        body.contains("event: message_stop"),
        "the fallback stream must end whole: {body}"
    );
    assert!(
        !body.contains("upstream unavailable"),
        "the failed attempt's body must never reach the client: {body}"
    );

    proxy.shutdown().await;
}

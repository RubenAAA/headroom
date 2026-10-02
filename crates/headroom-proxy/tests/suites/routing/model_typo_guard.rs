//! A mistyped routed alias fails with the list of names that exist, and
//! Claude Code's `/model` check reaches the model it names.
//!
//! Both failed together on 2026-10-02: `/model calude-muse-spark-1.3` passed
//! because the tool-less check was routed to Spark, and the next real turn
//! went to Anthropic, which answered 404.

use super::common;

use common::start_proxy_with;
use headroom_proxy::config::ProviderRoute;
use headroom_proxy::model_router::ModelRouterConfig;
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Never set in the test environment, so the key route reads as missing.
const UNSET_KEY: &str = "HEADROOM_TEST_TYPO_GUARD_UNSET_KEY";

async fn upstream(text: &'static str) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_x", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "model": "m", "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&mock)
        .await;
    mock
}

fn routes(zen: &MockServer) -> Vec<ProviderRoute> {
    let zen = Url::parse(&zen.uri()).unwrap();
    vec![
        ProviderRoute {
            model_prefix: "claude-muse-spark-1.3".to_string(),
            prefix_match: false,
            upstream: Some(zen.clone()),
            translate: true,
            cursor_agent: None,
            target_model: Some("muse-spark-1.3".to_string()),
            auth_env: Some("none".to_string()),
        },
        ProviderRoute {
            model_prefix: "claude-union-alpha".to_string(),
            prefix_match: false,
            upstream: Some(zen),
            translate: false,
            cursor_agent: None,
            target_model: Some("union-alpha".to_string()),
            auth_env: Some(UNSET_KEY.to_string()),
        },
    ]
}

fn spark_rule() -> ModelRouterConfig {
    ModelRouterConfig::from_env(
        Some("1"),
        Some(
            r#"[{"name": "no-tools->spark", "require_no_tools": true,
                 "to_model": "claude-muse-spark-1.3"}]"#,
        ),
    )
}

async fn post(url: &str, body: Value) -> (u16, Value) {
    let resp = common::shared_client()
        .post(format!("{url}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The session's exact typo: answered locally with the alias meant and every
/// routed model's access, and nothing goes upstream.
#[tokio::test]
async fn a_mistyped_alias_names_the_one_meant() {
    let default = upstream("from anthropic").await;
    let zen = upstream("from zen").await;
    let zen_routes = routes(&zen);
    let proxy = start_proxy_with(&default.uri(), |cfg| {
        cfg.model_routes = zen_routes;
        cfg.model_router = spark_rule();
    })
    .await;

    let (status, body) = post(
        &proxy.url(),
        json!({
            "model": "calude-muse-spark-1.3",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "continue"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        }),
    )
    .await;

    assert_eq!(status, 400);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Did you mean 'claude-muse-spark-1.3'?"),
        "{message}"
    );
    assert!(
        message.contains("claude-muse-spark-1.3  free, no login"),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "claude-union-alpha     API key in {UNSET_KEY}: not connected"
        )),
        "{message}"
    );
    assert!(default.received_requests().await.unwrap().is_empty());
    assert!(zen.received_requests().await.unwrap().is_empty());

    proxy.shutdown().await;
}

/// Claude Code's `/model` check (one output token, no tools) is not routed,
/// so the model it names answers, and a name nobody serves fails there.
#[tokio::test]
async fn the_model_check_reaches_the_named_model() {
    let default = upstream("from anthropic").await;
    let zen = upstream("from zen").await;
    let zen_routes = routes(&zen);
    let proxy = start_proxy_with(&default.uri(), |cfg| {
        cfg.model_routes = zen_routes;
        cfg.model_router = spark_rule();
    })
    .await;

    let (status, _) = post(
        &proxy.url(),
        json!({
            "model": "claude-opsu-9",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Hi"}]}]
        }),
    )
    .await;

    assert_eq!(status, 200);
    let sent = default.received_requests().await.unwrap();
    assert_eq!(sent.len(), 1, "the check must reach the default upstream");
    let sent: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(sent["model"], "claude-opsu-9");
    assert!(zen.received_requests().await.unwrap().is_empty());

    proxy.shutdown().await;
}

/// The `/models` command's source: each routed model with what it needs and
/// whether it is connected.
#[tokio::test]
async fn models_status_reports_access_and_connection() {
    let default = upstream("from anthropic").await;
    let zen = upstream("from zen").await;
    let zen_routes = routes(&zen);
    let proxy = start_proxy_with(&default.uri(), |cfg| {
        cfg.model_routes = zen_routes;
    })
    .await;

    let body: Value = common::shared_client()
        .get(format!("{}/models-status", proxy.url()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        body["models"],
        json!([
            {"model": "claude-muse-spark-1.3", "upstream_model": "muse-spark-1.3",
             "needs": "free, no login", "free": true, "connected": true, "fix": null},
            {"model": "claude-union-alpha", "upstream_model": "union-alpha",
             "needs": format!("API key in {UNSET_KEY}"), "free": false, "connected": false,
             "fix": format!("export {UNSET_KEY} and restart-headroom.sh")},
        ])
    );

    proxy.shutdown().await;
}

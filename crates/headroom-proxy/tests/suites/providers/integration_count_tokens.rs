//! POST /v1/messages/count_tokens: routed aliases are answered locally,
//! real Anthropic models forward byte-identical (exact upstream count).

use super::common;

use common::start_proxy_with;
use headroom_proxy::config::ProviderRoute;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn spark_route(upstream: &str) -> ProviderRoute {
    ProviderRoute {
        model_prefix: "claude-muse-spark-1.3".into(),
        prefix_match: false,
        upstream: Some(upstream.parse().unwrap()),
        translate: true,
        cursor_agent: None,
        target_model: Some("muse-spark-1.3-contributor-free".into()),
        auth_env: Some("none".into()),
        chat: false,
    }
}

fn count_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "system": "You are helpful.",
        "messages": [
            {"role": "user", "content": "hello world, this is a test message"},
        ],
    })
}

fn cursor_route() -> ProviderRoute {
    ProviderRoute {
        model_prefix: "claude-grok-4.6".into(),
        prefix_match: false,
        upstream: None,
        translate: false,
        cursor_agent: Some("grok-4.6".into()),
        target_model: None,
        auth_env: None,
        chat: false,
    }
}

#[tokio::test]
async fn count_tokens_routed_alias_answered_locally() {
    let upstream = MockServer::start().await;
    let proxy = start_proxy_with(&upstream.uri(), |cfg| {
        cfg.model_routes.push(spark_route(&upstream.uri()));
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", proxy.url()))
        .json(&count_body("claude-muse-spark-1.3"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let n = body["input_tokens"].as_u64().expect("input_tokens number");
    assert!(n > 0, "a real body counts for something: {body}");

    // Answered locally: the upstream never saw it (today it would 404 there).
    assert!(
        upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "routed count must not reach upstream"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn count_tokens_cursor_alias_answered_locally() {
    // Cursor routes run a subprocess: no HTTP upstream could count them, so
    // a forward would 404 on the default upstream. Must answer locally.
    let upstream = MockServer::start().await;
    let proxy = start_proxy_with(&upstream.uri(), |cfg| {
        cfg.model_routes.push(cursor_route());
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", proxy.url()))
        .json(&count_body("claude-grok-4.6"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let n = body["input_tokens"].as_u64().expect("input_tokens number");
    assert!(n > 0, "a real body counts for something: {body}");

    assert!(
        upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "cursor count must not reach upstream"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn count_tokens_anthropic_target_alias_answered_locally() {
    let upstream = MockServer::start().await;
    let proxy = start_proxy_with(&upstream.uri(), |cfg| {
        cfg.model_routes.push(ProviderRoute {
            model_prefix: "claude-union-alpha".into(),
            prefix_match: false,
            upstream: Some(upstream.uri().parse().unwrap()),
            translate: false,
            cursor_agent: None,
            target_model: Some("union-alpha".into()),
            auth_env: Some("none".into()),
            chat: false,
        });
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", proxy.url()))
        .json(&count_body("claude-union-alpha"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let n = body["input_tokens"].as_u64().expect("input_tokens number");
    assert!(n > 0, "a real body counts for something: {body}");

    assert!(
        upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "anthropic-target count must not reach upstream"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn count_tokens_unrouted_model_forwards_byte_identical() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "input_tokens": 123,
        })))
        .mount(&upstream)
        .await;
    let proxy = start_proxy_with(&upstream.uri(), |cfg| {
        cfg.model_routes.push(spark_route(&upstream.uri()));
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", proxy.url()))
        .json(&count_body("claude-sonnet-5"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["input_tokens"], 123,
        "unrouted models keep the exact upstream count: {body}"
    );
    proxy.shutdown().await;
}

/// A 4-page PDF costs ~1,500 tokens a page where the block reaches an
/// Anthropic-protocol upstream, but translated and cursor routes send a
/// one-line placeholder instead, so pricing its pages there would compact
/// the client early.
#[tokio::test]
async fn count_tokens_prices_pdf_pages_only_where_the_pdf_is_sent() {
    let upstream = MockServer::start().await;
    let proxy = start_proxy_with(&upstream.uri(), |cfg| {
        cfg.model_routes.push(spark_route(&upstream.uri()));
        cfg.model_routes.push(cursor_route());
        cfg.model_routes.push(ProviderRoute {
            model_prefix: "claude-union-alpha".into(),
            prefix_match: false,
            upstream: Some(upstream.uri().parse().unwrap()),
            translate: false,
            cursor_agent: None,
            target_model: Some("union-alpha".into()),
            auth_env: Some("none".into()),
            chat: false,
        });
    })
    .await;
    let pdf: &[u8] = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n\
        2 0 obj << /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R] /Count 4 >> endobj\n%%EOF\n";
    let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pdf);

    let client = reqwest::Client::new();
    let count = |body: serde_json::Value| {
        let client = client.clone();
        let url = format!("{}/v1/messages/count_tokens", proxy.url());
        async move {
            let resp = client.post(url).json(&body).send().await.unwrap();
            assert_eq!(resp.status(), 200);
            let body: serde_json::Value = resp.json().await.unwrap();
            body["input_tokens"].as_u64().expect("input_tokens number")
        }
    };
    for (model, sent) in [
        ("claude-muse-spark-1.3", false),
        ("claude-grok-4.6", false),
        ("claude-union-alpha", true),
    ] {
        let base = count(count_body(model)).await;
        let mut with_pdf = count_body(model);
        with_pdf["messages"].as_array_mut().unwrap().push(
            serde_json::json!({"role": "user", "content": [{"type": "document",
                "source": {"type": "base64", "media_type": "application/pdf", "data": data}}]}),
        );
        let added = count(with_pdf).await - base;
        if sent {
            assert_eq!(added, 4 * 1500, "{model} receives the PDF");
        } else {
            assert!(
                added > 0 && added < 20,
                "{model} sends a placeholder: {added}"
            );
        }
    }
    proxy.shutdown().await;
}

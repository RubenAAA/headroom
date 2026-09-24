//! Health endpoints: own /healthz always 200; /healthz/upstream reflects upstream.

mod common;

use common::start_proxy;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn healthz_ok_when_upstream_down() {
    let proxy = start_proxy("http://127.0.0.1:1").await; // unroutable port
    let resp = reqwest::get(format!("{}/healthz", proxy.url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    proxy.shutdown().await;
}

#[tokio::test]
async fn healthz_upstream_503_when_upstream_down() {
    let proxy = start_proxy("http://127.0.0.1:1").await;
    let resp = reqwest::get(format!("{}/healthz/upstream", proxy.url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    proxy.shutdown().await;
}

#[tokio::test]
async fn healthz_upstream_200_when_upstream_healthy() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&upstream)
        .await;
    let proxy = start_proxy(&upstream.uri()).await;
    let resp = reqwest::get(format!("{}/healthz/upstream", proxy.url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    proxy.shutdown().await;
}

#[tokio::test]
async fn debug_zen_egresses_lists_only_opaque_ids_for_rotation_watcher() {
    let proxy = common::start_proxy_with("http://127.0.0.1:1", |config| {
        config.zen_http_proxy_pool = vec![
            "socks5h://secret-user:secret-pass@127.0.0.1:1081".to_string(),
            "socks5h://other-user:other-pass@127.0.0.1:1082".to_string(),
        ];
    })
    .await;

    let response = reqwest::get(format!("{}/debug/zen-egresses", proxy.url()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    let inventory: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(inventory["pool_enabled"], true);
    let egresses = inventory["egresses"].as_array().unwrap();
    assert_eq!(egresses.len(), 2);
    assert_eq!(egresses[0]["slot"], 0);
    assert_eq!(egresses[1]["slot"], 1);
    for egress in egresses {
        assert!(egress["egress_id"].as_str().unwrap().starts_with("proxy-"));
    }
    assert!(!body.contains("secret-user"));
    assert!(!body.contains("secret-pass"));
    assert!(!body.contains("other-user"));
    assert!(!body.contains("other-pass"));

    proxy.shutdown().await;
}

#[tokio::test]
async fn debug_zen_egresses_is_empty_without_pool() {
    let proxy = start_proxy("http://127.0.0.1:1").await;
    let inventory: serde_json::Value = reqwest::get(format!("{}/debug/zen-egresses", proxy.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(inventory["pool_enabled"], false);
    assert_eq!(inventory["egresses"], serde_json::json!([]));
    proxy.shutdown().await;
}

#[tokio::test]
async fn debug_zen_egress_maintenance_gates_and_restores_only_the_selected_egress() {
    let proxy = common::start_proxy_with("http://127.0.0.1:1", |config| {
        config.zen_http_proxy_pool = vec![
            "socks5h://127.0.0.1:21801".to_string(),
            "socks5h://127.0.0.1:21802".to_string(),
        ];
    })
    .await;
    let inventory_url = format!("{}/debug/zen-egresses", proxy.url());
    let inventory: serde_json::Value = reqwest::get(&inventory_url)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let egresses = inventory["egresses"].as_array().unwrap();
    let first_id = egresses[0]["egress_id"].as_str().unwrap();
    let second_id = egresses[1]["egress_id"].as_str().unwrap();

    let maintenance_url = format!("{inventory_url}/{first_id}/maintenance");
    let response = reqwest::Client::new()
        .post(&maintenance_url)
        .json(&serde_json::json!({"rotating": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response: serde_json::Value = reqwest::get(&inventory_url)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["egresses"][0]["rotating"], true);
    assert_eq!(response["egresses"][1]["rotating"], false);

    let response = reqwest::Client::new()
        .post(&maintenance_url)
        .json(&serde_json::json!({"rotating": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response: serde_json::Value = reqwest::get(&inventory_url)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["egresses"][0]["rotating"], false);

    let unknown = reqwest::Client::new()
        .post(format!("{inventory_url}/proxy-ffffffffffff/maintenance"))
        .json(&serde_json::json!({"rotating": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404, "unknown egress IDs must fail closed");
    assert_ne!(first_id, second_id);

    proxy.shutdown().await;
}

/// A plain HTTP forward proxy standing in for one Zen egress. Every request
/// gets a two-part chat-completions stream; a request whose body mentions
/// `HOLD` stops after the first part until `release` is notified.
async fn fake_zen_egress(release: std::sync::Arc<tokio::sync::Notify>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let release = release.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                let body_start = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0, "client closed before sending a request");
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                while request.len() < body_start + length {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0, "client closed mid-body");
                    request.extend_from_slice(&buf[..n]);
                }
                let hold = String::from_utf8_lossy(&request[body_start..]).contains("HOLD");
                let first = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\n";
                let rest = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1,\"total_tokens\":6}}\n\ndata: [DONE]\n\n";
                let chunk = |data: &str| format!("{:x}\r\n{data}\r\n", data.len());
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(chunk(first).as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
                if hold {
                    release.notified().await;
                }
                let _ = socket.write_all(chunk(rest).as_bytes()).await;
                let _ = socket.write_all(b"0\r\n\r\n").await;
                let _ = socket.shutdown().await;
            });
        }
    });
    url
}

async fn egress_in_flight(proxy_url: &str) -> serde_json::Map<String, serde_json::Value> {
    let inflight: serde_json::Value = reqwest::get(format!("{proxy_url}/debug/inflight"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        inflight["in_flight"].is_u64(),
        "old fields stay: {inflight}"
    );
    assert!(inflight["zen_held"].is_u64(), "old fields stay: {inflight}");
    inflight["egress_in_flight"].as_object().unwrap().clone()
}

/// Poll until `egress_id` shows `want` requests in flight.
async fn wait_for_egress_count(proxy_url: &str, egress_id: &str, want: u64) {
    for _ in 0..200 {
        if egress_in_flight(proxy_url).await[egress_id] == want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!(
        "{egress_id} never reached {want} in flight: {:?}",
        egress_in_flight(proxy_url).await
    );
}

#[tokio::test]
async fn debug_inflight_counts_zen_requests_per_egress_until_the_stream_ends() {
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let pool = vec![
        fake_zen_egress(release.clone()).await,
        fake_zen_egress(release.clone()).await,
    ];
    let proxy = common::start_proxy_with("http://127.0.0.1:1", |config| {
        config.zen_http_proxy_pool = pool;
        config.model_routes = vec![headroom_proxy::config::ProviderRoute {
            model_prefix: "claude-muse-spark-1.3".to_string(),
            prefix_match: false,
            upstream: Some(url::Url::parse("http://opencode.ai/zen/v1").unwrap()),
            translate: true,
            cursor_agent: None,
            target_model: Some("muse-spark-1.3".to_string()),
            auth_env: Some("none".to_string()),
        }];
    })
    .await;
    let url = proxy.url();
    let counts = egress_in_flight(&url).await;
    assert_eq!(counts.len(), 2, "one entry per pool egress: {counts:?}");
    assert!(counts.values().all(|n| n == 0), "idle pool: {counts:?}");

    let turn = |system: &str, text: &str| {
        common::shared_client()
            .post(format!("{url}/v1/messages"))
            .header("content-type", "application/json")
            .header("x-api-key", "test-key")
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "claude-muse-spark-1.3",
                "max_tokens": 100,
                "stream": true,
                "system": system,
                "messages": [{"role": "user", "content": text}]
            }))
            .send()
    };

    // A stream that stalls mid-body on whichever egress its lane lands on.
    let held = tokio::spawn(turn("agent one", "HOLD please"));
    let mut busy = None;
    for _ in 0..200 {
        let counts = egress_in_flight(&url).await;
        busy = counts
            .iter()
            .find(|(_, n)| **n == 1)
            .map(|(id, _)| id.clone());
        if busy.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let busy = busy.expect("the stalled stream shows up on one egress");
    let counts = egress_in_flight(&url).await;
    let idle = counts.keys().find(|id| **id != busy).unwrap().clone();
    assert_eq!(
        counts[&idle], 0,
        "only the stream's egress counts it: {counts:?}"
    );

    // The handler has answered and the body is still streaming: the count
    // must follow the body, not the handler.
    let held = tokio::time::timeout(std::time::Duration::from_secs(10), held)
        .await
        .expect("proxy answers headers while the upstream body stalls")
        .unwrap()
        .unwrap();
    assert_eq!(held.status(), 200);
    assert_eq!(egress_in_flight(&url).await[&busy], 1);

    // Rotating the busy egress turns its lane away; the other egress serves.
    let maintenance = common::shared_client()
        .post(format!("{url}/debug/zen-egresses/{busy}/maintenance"))
        .json(&serde_json::json!({"rotating": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(maintenance.status(), 200);
    let rejected = turn("agent one", "HOLD please").await.unwrap();
    assert_eq!(rejected.status(), 503, "same lane, rotating egress");
    let served = turn("agent two", "no stall").await.unwrap();
    assert_eq!(served.status(), 200, "other lane, other egress");
    served.text().await.unwrap();
    wait_for_egress_count(&url, &idle, 0).await;
    assert_eq!(egress_in_flight(&url).await[&busy], 1);

    release.notify_one();
    held.text().await.unwrap();
    wait_for_egress_count(&url, &busy, 0).await;

    proxy.shutdown().await;
}

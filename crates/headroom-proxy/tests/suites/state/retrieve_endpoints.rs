//! CCR retrieve HTTP endpoints: POST /v1/retrieve + POST /v1/retrieve/tool_call.
//!
//! Port of the upstream retrieve-endpoint coverage (`proxy/server.py`
//! `ccr_retrieve` / `ccr_handle_tool_call`, incl. the #3955 same-origin
//! check on `tool_call`): hit, miss-with-TTL detail, expired-with-age
//! detail, Anthropic + OpenAI tool_call envelopes, rejection of
//! non-CCR calls, and loopback + same-origin gating.

use super::common;

use common::start_proxy_with_state;
use tempfile::TempDir;

const HASH: &str = "abcdef1234567890abcdef12";
const PAYLOAD: &str = r#"{"rows":[{"id":1,"name":"ada"}]}"#;
const UNKNOWN_HASH: &str = "0123456789abcdef01234567";

fn retrieve_config(store_dir: std::path::PathBuf) -> impl FnOnce(&mut headroom_proxy::Config) {
    move |config: &mut headroom_proxy::Config| {
        config.ctx_offload = true;
        config.ctx_capture = true;
        config.ctx_store_dir = Some(store_dir);
        config.ctx_offload_min_bytes = 100;
        config.ccr_inject_marker = true;
    }
}

fn seed(
    hash: &str,
    payload: &str,
) -> impl FnOnce(headroom_proxy::AppState) -> headroom_proxy::AppState {
    let (hash, payload) = (hash.to_string(), payload.to_string());
    move |s: headroom_proxy::AppState| {
        let stored = s
            .ctx_offload
            .as_ref()
            .expect("ctx_offload runtime")
            .store
            .ccr()
            .put(&hash, &payload);
        assert!(stored, "seed payload into CCR store");
        s
    }
}

/// Age a seeded row past its idle window by rewriting its timestamps
/// directly in `<store_dir>/ccr.db` — the integration equivalent of the
/// raw-timestamp unit tests in the sqlite backend.
fn age_row_past_ttl(store_dir: &std::path::Path, hash: &str, age_seconds: i64) {
    let db = store_dir.join("ccr.db");
    let conn = rusqlite::Connection::open(db).expect("ccr.db opens");
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs() as i64
        - age_seconds;
    let updated = conn
        .execute(
            "UPDATE ccr_entries SET created_at = ?1, last_accessed = ?1 WHERE hash = ?2",
            rusqlite::params![old, hash],
        )
        .expect("age update runs");
    assert_eq!(updated, 1, "seeded row exists to age");
}

#[tokio::test]
async fn retrieve_hit_returns_original_content() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve", proxy.url()))
        .json(&serde_json::json!({ "hash": HASH }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "retrieved");
    assert_eq!(body["hash"], HASH);
    assert_eq!(body["original_content"], PAYLOAD);
    proxy.shutdown().await;
}

#[tokio::test]
async fn retrieve_requires_a_hash() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve", proxy.url()))
        .json(&serde_json::json!({ "hash": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    proxy.shutdown().await;
}

#[tokio::test]
async fn retrieve_missing_names_the_store_ttl() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve", proxy.url()))
        .json(&serde_json::json!({ "hash": UNKNOWN_HASH }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("Entry not found (CCR TTL: ") && detail.ends_with(" seconds)"),
        "missing detail names the TTL, got: {detail}"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn retrieve_expired_names_ttl_and_age() {
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().to_path_buf();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(store_dir.clone()),
        seed(HASH, PAYLOAD),
    )
    .await;
    // Test-config TTL is 7 days idle; age well past it (but nowhere near
    // the 8x max-lifetime ceiling — idle expiry alone must trip).
    age_row_past_ttl(&store_dir, HASH, 700_000);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve", proxy.url()))
        .json(&serde_json::json!({ "hash": HASH }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("Entry expired (CCR TTL: ") && detail.contains("; age: "),
        "expired detail names TTL and age, got: {detail}"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn tool_call_anthropic_returns_tool_result_envelope() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve/tool_call", proxy.url()))
        .json(&serde_json::json!({
            "provider": "anthropic",
            "tool_call": {
                "id": "toolu_1",
                "name": "headroom_retrieve",
                "input": { "hash": HASH },
            },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["tool_result"]["type"], "tool_result");
    assert_eq!(body["tool_result"]["tool_use_id"], "toolu_1");
    assert_eq!(body["data"]["hash"], HASH);
    assert_eq!(body["data"]["original_content"], PAYLOAD);
    proxy.shutdown().await;
}

#[tokio::test]
async fn tool_call_openai_returns_tool_role_envelope() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve/tool_call", proxy.url()))
        .json(&serde_json::json!({
            "provider": "openai",
            "tool_call": {
                "id": "call_1",
                "function": {
                    "name": "headroom_retrieve",
                    "arguments": format!("{{\"hash\": \"{HASH}\"}}"),
                },
            },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["tool_result"]["role"], "tool");
    assert_eq!(body["tool_result"]["tool_call_id"], "call_1");
    let content: serde_json::Value =
        serde_json::from_str(body["tool_result"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["original_content"], PAYLOAD);
    proxy.shutdown().await;
}

#[tokio::test]
async fn tool_call_rejects_non_ccr_calls() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve/tool_call", proxy.url()))
        .json(&serde_json::json!({
            "provider": "anthropic",
            "tool_call": {
                "id": "toolu_9",
                "name": "some_other_tool",
                "input": { "hash": HASH },
            },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    proxy.shutdown().await;
}

#[tokio::test]
async fn tool_call_miss_is_unsuccessful_envelope_with_ttl_detail() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve/tool_call", proxy.url()))
        .json(&serde_json::json!({
            "provider": "anthropic",
            "tool_call": {
                "id": "toolu_2",
                "name": "headroom_retrieve",
                "input": { "hash": UNKNOWN_HASH },
            },
        }))
        .send()
        .await
        .unwrap();
    // Upstream answers a miss inside the envelope, so the agent still gets
    // a tool_result to splice.
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["success"], false);
    assert_eq!(body["tool_result"]["tool_use_id"], "toolu_2");
    assert_eq!(body["data"]["status"], "missing");
    assert!(
        body["data"]["error"]
            .as_str()
            .unwrap()
            .starts_with("Entry not found (CCR TTL: "),
        "miss detail names the TTL, got: {}",
        body["data"]["error"]
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn cross_origin_browser_request_is_forbidden_on_both_routes() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let client = reqwest::Client::new();
    for (path, payload) in [
        ("/v1/retrieve", serde_json::json!({ "hash": HASH })),
        (
            "/v1/retrieve/tool_call",
            serde_json::json!({
                "provider": "anthropic",
                "tool_call": {
                    "id": "toolu_1",
                    "name": "headroom_retrieve",
                    "input": { "hash": HASH },
                },
            }),
        ),
    ] {
        let resp = client
            .post(format!("{}{path}", proxy.url()))
            .header("Origin", "https://evil.example")
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "cross-origin must 403 on {path}");
    }
    proxy.shutdown().await;
}

#[tokio::test]
async fn same_origin_browser_request_passes_the_gate() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with_state(
        "http://127.0.0.1:1",
        retrieve_config(dir.path().to_path_buf()),
        seed(HASH, PAYLOAD),
    )
    .await;
    let origin = proxy.url();
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/retrieve", proxy.url()))
        .header("Origin", origin)
        .json(&serde_json::json!({ "hash": HASH }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    proxy.shutdown().await;
}

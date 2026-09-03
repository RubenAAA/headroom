//! Claude Code's spinner-text sidecar, end to end through a real proxy.
//!
//! Three things have to hold, and only the third is obvious:
//!
//! 1. The sidecar leaves the proxy shrunk — a few tail messages, no tools, the
//!    sidecar model, 64 output tokens.
//! 2. It leaves no per-conversation state behind. This is the expensive half of
//!    the bug: the sidecar used to file its own forwarded prefix in the replay
//!    store, so the next real turn no longer matched anything and was logged as
//!    an unexplained re-cache. The test drives turn 1, a sidecar, then turn 2,
//!    and demands that turn 2 still replays turn 1's *compressed* bytes.
//! 3. A request without the block is forwarded exactly as before.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::start_proxy_with;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A clean Anthropic SSE session. `message_stop` matters: the replay store only
/// commits a turn that completed.
fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_sidecar\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":2048,\"cache_read_input_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

async fn mount_capture(upstream: &MockServer) -> Arc<Mutex<Vec<Vec<u8>>>> {
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock().unwrap().push(req.body.clone());
            ResponseTemplate::new(200).set_body_raw(sse_body(), "text/event-stream")
        })
        .mount(upstream)
        .await;
    captured
}

const DESCRIBE: &str = "Describe your most recent action in 3-5 words using present tense \
                        (-ing). Name the file or function, not the branch. Do not use tools.";

fn tools() -> Value {
    json!([
        {"name": "Read", "description": "read a file", "input_schema": {"type": "object"}},
        {"name": "Bash", "description": "run a command", "input_schema": {"type": "object"}}
    ])
}

/// Twelve messages in Claude Code's usual shape: alternating turns, the
/// assistant calling a tool and the user answering it, plus one `thinking`
/// block carrying a signature that the sidecar model could not verify.
fn twelve_messages() -> Vec<Value> {
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "text", "text": "start the work"}
    ]})];
    for i in 0..5 {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "considering", "signature": format!("sig-{i}")},
            {"type": "tool_use", "id": format!("toolu_{i}"), "name": "Read", "input": {"path": "a.rs"}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{i}"), "content": "file body"}
        ]}));
    }
    messages.push(json!({"role": "user", "content": [
        {"type": "text", "text": "carry on"}
    ]}));
    messages
}

fn sidecar_body() -> Value {
    let mut messages = twelve_messages();
    // The block rides on the last user message, exactly as the client sends it.
    messages
        .last_mut()
        .unwrap()
        .get_mut("content")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "text", "text": DESCRIBE}));
    json!({
        "model": "claude-opus-5",
        "max_tokens": 64000,
        "stream": true,
        "thinking": {"type": "adaptive"},
        "context_management": {"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]},
        "system": [{"type": "text", "text": "a very long project preamble"}],
        "tools": tools(),
        "messages": messages,
    })
}

async fn post(client: &reqwest::Client, proxy_url: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-ant-sidecar-test")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert_eq!(resp.status(), 200);
    // Drain the body so any spawned completion hook runs before we look.
    let _ = resp.bytes().await.expect("response body");
}

#[tokio::test]
async fn a_sidecar_is_forwarded_shrunk() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.prefix_replay = true;
    })
    .await;
    let client = reqwest::Client::new();

    post(&client, &proxy.url(), &sidecar_body()).await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "exactly one upstream call");
    let fwd: Value = serde_json::from_slice(&bodies[0]).expect("upstream body is JSON");

    let messages = fwd["messages"].as_array().expect("messages array");
    assert!(
        messages.len() <= 4,
        "sidecar forwarded {} messages, want at most 4",
        messages.len()
    );
    assert_eq!(messages[0]["role"], "user", "tail must open on a user turn");
    assert_eq!(fwd["model"], "claude-haiku-4-5-20251001");
    assert_eq!(fwd["max_tokens"], 64);
    assert!(fwd["system"].is_string(), "system collapses to one line");
    assert!(fwd.get("tools").is_none(), "tools are dropped");
    assert!(fwd.get("thinking").is_none(), "thinking is dropped");
    assert!(
        fwd.get("context_management").is_none(),
        "context_management is dropped"
    );
    assert_eq!(fwd["stream"], true, "streaming is the client's call");

    // Signed thinking blocks cannot travel to another model.
    let serialised = serde_json::to_string(&fwd).unwrap();
    assert!(
        !serialised.contains("\"thinking\""),
        "no thinking block survives the rewrite"
    );
    assert!(
        !serialised.contains("cache_control"),
        "a one-shot request has nothing to cache"
    );

    assert!(
        headroom_proxy::observability::sidecar::detected_get("describe_action") >= 1,
        "the sidecar counter was not incremented"
    );

    proxy.shutdown().await;
}

/// The same 12 messages without the block must reach the upstream whole.
#[tokio::test]
async fn a_normal_request_is_untouched() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.prefix_replay = true;
    })
    .await;
    let client = reqwest::Client::new();

    let mut body = sidecar_body();
    // Drop only the appended block; everything else stays identical.
    let last = body["messages"].as_array_mut().unwrap().last_mut().unwrap();
    last["content"].as_array_mut().unwrap().pop();
    post(&client, &proxy.url(), &body).await;

    let bodies = captured.lock().unwrap().clone();
    let fwd: Value = serde_json::from_slice(&bodies[0]).expect("upstream body is JSON");
    assert_eq!(fwd["messages"].as_array().unwrap().len(), 12);
    assert_eq!(fwd["model"], "claude-opus-5");
    assert_eq!(fwd["max_tokens"], 64000);
    assert_eq!(fwd["tools"].as_array().unwrap().len(), 2);
    assert!(fwd["system"].is_array(), "the real system prompt is kept");

    proxy.shutdown().await;
}

// ---------------------------------------------------------------------------
// The state test: a sidecar between two real turns must be invisible.
// ---------------------------------------------------------------------------

/// 200 homogeneous dicts in a `tool_result` — the fixture the live-zone
/// dispatcher is guaranteed to compress, borrowed from
/// `integration_prefix_replay`.
fn big_tool_result_message() -> Value {
    let rows: Vec<Value> = (0..200)
        .map(|i| json!({"id": i, "status": "ok", "value": format!("repeat-pattern-{}", i % 3)}))
        .collect();
    json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": "toolu_sidecar_test",
            "content": serde_json::to_string(&rows).unwrap(),
        }],
    })
}

fn turn1_body(big: &Value) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": "you are a helpful assistant",
        "messages": [big],
    })
}

fn turn2_body(big: &Value) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": "you are a helpful assistant",
        "messages": [
            big,
            {"role": "assistant", "content": "done."},
            {"role": "user", "content": "next step please"},
        ],
    })
}

/// A sidecar built on turn 1's history, the way the client actually sends it.
fn sidecar_between(big: &Value) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64000,
        "system": "you are a helpful assistant",
        "tools": tools(),
        "messages": [
            big,
            {"role": "assistant", "content": "done."},
            {"role": "user", "content": [{"type": "text", "text": DESCRIBE}]},
        ],
    })
}

fn without_cache_control(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "cache_control")
                .map(|(key, value)| (key.clone(), without_cache_control(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(without_cache_control).collect()),
        _ => value.clone(),
    }
}

fn messages_of(body: &[u8]) -> Vec<Value> {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    v["messages"].as_array().expect("messages array").clone()
}

fn tool_result_content(msg: &Value) -> String {
    msg["content"][0]["content"]
        .as_str()
        .expect("tool_result content is a string")
        .to_string()
}

#[tokio::test]
async fn a_sidecar_between_turns_leaves_the_replay_prefix_alone() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
    })
    .await;
    let client = reqwest::Client::new();
    let big = big_tool_result_message();
    let original_payload = tool_result_content(&big);

    // Turn 1: the big tool_result is the live zone, so it is compressed and the
    // compressed bytes are what the provider caches.
    post(&client, &proxy.url(), &turn1_body(&big)).await;
    let fwd1 = messages_of(&captured.lock().unwrap()[0]);
    assert_ne!(
        tool_result_content(&fwd1[0]),
        original_payload,
        "precondition: turn 1 must actually compress the tool_result"
    );

    // The sidecar lands between the two real turns.
    post(&client, &proxy.url(), &sidecar_between(&big)).await;
    let sidecar_fwd: Value =
        serde_json::from_slice(captured.lock().unwrap().last().unwrap()).unwrap();
    assert_eq!(
        sidecar_fwd["model"], "claude-haiku-4-5-20251001",
        "precondition: the middle request was recognised as a sidecar"
    );

    // Turn 2: append-only growth. The big message has left the live zone, so
    // the dispatcher re-emits its original bytes and the replay overlay has to
    // put turn 1's compressed bytes back. If the sidecar had overwritten the
    // stored prefix, there would be nothing to put back and the original bytes
    // would go out — the re-cache this change exists to stop.
    let turn2 = turn2_body(&big);
    let mut replayed = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        post(&client, &proxy.url(), &turn2).await;
        let got = messages_of(captured.lock().unwrap().last().unwrap());
        if without_cache_control(&got[0]) == without_cache_control(&fwd1[0]) {
            replayed = true;
            break;
        }
    }
    assert!(
        replayed,
        "turn 2 did not replay turn 1's compressed prefix — the sidecar \
         overwrote per-conversation state"
    );

    proxy.shutdown().await;
}

// ---------------------------------------------------------------------------
// The other entry path.
// ---------------------------------------------------------------------------

/// With model routes configured, `/v1/messages` is served by
/// `handlers::local_model::handle_messages` rather than falling through to
/// `forward_http`, so the detector has to sit in both. The route here matches
/// the sidecar's own model and points somewhere else entirely: if the detector
/// did not run first, the request would leave for the route's upstream whole.
#[tokio::test]
async fn the_routed_entry_path_also_shrinks_the_sidecar() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    // Never answers; reaching it is the failure.
    let route_upstream = MockServer::start().await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.prefix_replay = true;
        c.model_routes = vec![headroom_proxy::config::ModelRoute {
            model_prefix: "claude-opus-5".to_string(),
            prefix_match: false,
            upstream: Some(route_upstream.uri().parse().unwrap()),
            translate: true,
            cursor_agent: None,
            target_model: Some("gpt-5.5".to_string()),
            auth_env: None,
        }];
    })
    .await;
    let client = reqwest::Client::new();

    post(&client, &proxy.url(), &sidecar_body()).await;

    assert!(
        route_upstream.received_requests().await.unwrap().is_empty(),
        "the sidecar was routed by model instead of being answered as a sidecar"
    );
    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    let fwd: Value = serde_json::from_slice(&bodies[0]).expect("upstream body is JSON");
    assert_eq!(fwd["model"], "claude-haiku-4-5-20251001");
    assert!(fwd["messages"].as_array().unwrap().len() <= 4);
    assert!(fwd.get("tools").is_none());

    proxy.shutdown().await;
}

/// `--sidecar-model` picks the model the shrunk request is sent to.
#[tokio::test]
async fn the_sidecar_model_is_configurable() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.sidecar_model = Some("claude-3-5-haiku-latest".to_string());
    })
    .await;
    let client = reqwest::Client::new();

    post(&client, &proxy.url(), &sidecar_body()).await;

    let fwd: Value = serde_json::from_slice(&captured.lock().unwrap()[0]).unwrap();
    assert_eq!(fwd["model"], "claude-3-5-haiku-latest");

    proxy.shutdown().await;
}

// ---------------------------------------------------------------------------
// Concurrency and response shape.
// ---------------------------------------------------------------------------

/// Claude Code fires the sidecar alongside the turn it is describing, so the
/// sidecar must not queue behind it. The mock holds the main turn open for well
/// over a second; the sidecar has to come back while that is still in flight.
///
/// This is also the observable form of "takes no per-conversation lock". The
/// replay store's mutex is private, so a test cannot assert the lock was never
/// acquired; what it can assert is the consequence — a sidecar issued against
/// the very session a slow turn is holding does not wait for it.
#[tokio::test]
async fn a_sidecar_does_not_queue_behind_the_turn_it_describes() {
    let upstream = MockServer::start().await;
    // The slow main turn: same path, distinguished by its model.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains(
            "claude-sonnet-4-6",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body(), "text/event-stream")
                .set_delay(Duration::from_millis(1500)),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains("claude-haiku"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body(), "text/event-stream"))
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.prefix_replay = true;
    })
    .await;
    let client = reqwest::Client::new();
    let big = big_tool_result_message();

    let url = proxy.url();
    let slow = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        let body = turn2_body(&big);
        async move { post(&client, &url, &body).await }
    });

    // Give the slow turn a head start so it is provably in flight.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    post(&client, &url, &sidecar_between(&big)).await;
    let sidecar_took = started.elapsed();

    assert!(
        !slow.is_finished(),
        "the slow turn finished first; the timing below proves nothing"
    );
    assert!(
        sidecar_took < Duration::from_millis(1000),
        "sidecar took {sidecar_took:?}, so it serialised behind the main turn"
    );
    slow.await.expect("slow turn finished");

    proxy.shutdown().await;
}

/// The client parses the sidecar reply with the same code it uses for a turn,
/// so every event has to arrive in the shape it expects. The proxy hands the
/// upstream stream straight back, which this pins byte for byte.
#[tokio::test]
async fn the_sidecar_reply_keeps_the_event_shape_the_client_expects() {
    let upstream = MockServer::start().await;
    let full_sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"model\":\"claude-haiku-4-5-20251001\",\"usage\":{\"input_tokens\":42,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Reading sidecar.rs\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(full_sse, "text/event-stream"))
        .mount(&upstream)
        .await;
    let proxy = start_proxy_with(&upstream.uri(), |c| c.compression = true).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-ant-sidecar-test")
        .body(serde_json::to_vec(&sidecar_body()).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let got = resp.text().await.expect("response body");

    assert_eq!(
        got, full_sse,
        "the client must see the upstream stream verbatim"
    );
    for event in [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
    ] {
        assert!(
            got.contains(&format!("event: {event}\n")),
            "missing {event}"
        );
    }
    assert!(
        got.contains("\"usage\":{\"input_tokens\":42"),
        "usage must survive"
    );

    proxy.shutdown().await;
}

/// The cap has to hold end to end, not just in the unit test.
#[tokio::test]
async fn a_huge_tool_result_is_capped_before_it_reaches_the_upstream() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| c.compression = true).await;

    let mut body = sidecar_body();
    let huge = "x".repeat(300_000);
    // Replace the last answered tool_result with something enormous.
    body["messages"][10]["content"][0]["content"] = json!(huge);

    post(&reqwest::Client::new(), &proxy.url(), &body).await;

    let fwd = captured.lock().unwrap()[0].clone();
    assert!(
        fwd.len() < 20_000,
        "forwarded {} bytes; the 300 KB tool_result was not capped",
        fwd.len()
    );
    assert!(
        String::from_utf8_lossy(&fwd).contains("[truncated]"),
        "the cap must mark what it cut"
    );

    proxy.shutdown().await;
}

/// The sidecar rides the same retry knobs as a normal turn, so a transient 529
/// is retried rather than surfaced as a dead spinner.
#[tokio::test]
async fn a_transient_upstream_status_is_retried() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let upstream = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_: &wiremock::Request| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(529).set_body_string("overloaded")
            } else {
                ResponseTemplate::new(200).set_body_raw(sse_body(), "text/event-stream")
            }
        })
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.retry_enabled = true;
        c.retry_max_attempts = 3;
        c.retry_base_delay_ms = 1;
        c.retry_max_delay_ms = 5;
    })
    .await;

    post(&reqwest::Client::new(), &proxy.url(), &sidecar_body()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the 529 should have been retried exactly once"
    );

    proxy.shutdown().await;
}

// ---------------------------------------------------------------------------
// Fallback: a failed sidecar must never leave the client worse off.
// ---------------------------------------------------------------------------

/// `proxy_sidecar_total` is process-global, so the two tests that assert on the
/// fallback count take turns.
static FALLBACK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Calls the mock served on the sidecar model, across both tests below. Reset by
/// each test while it holds `FALLBACK_LOCK`.
static SIDECAR_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Answer the sidecar model with `sidecar_status` and everything else with a
/// normal SSE reply. Returns the bodies of the non-sidecar calls.
async fn mount_split_upstream(
    upstream: &MockServer,
    sidecar_status: u16,
) -> Arc<Mutex<Vec<Vec<u8>>>> {
    SIDECAR_CALLS.store(0, Ordering::SeqCst);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains("claude-haiku"))
        .respond_with(move |_: &wiremock::Request| {
            SIDECAR_CALLS.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(sidecar_status).set_body_string("refused")
        })
        .mount(upstream)
        .await;

    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock().unwrap().push(req.body.clone());
            ResponseTemplate::new(200).set_body_raw(sse_body(), "text/event-stream")
        })
        .mount(upstream)
        .await;
    captured
}

/// If the shrunk request fails for any reason, the client must end up exactly
/// where it would have been without this feature. Here the sidecar model
/// answers 400 and the original body succeeds.
#[tokio::test]
async fn a_rejected_sidecar_falls_back_to_the_normal_path() {
    let _serial = FALLBACK_LOCK.lock().await;
    let upstream = MockServer::start().await;
    let captured = mount_split_upstream(&upstream, 400).await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.retry_enabled = false;
    })
    .await;

    let before = headroom_proxy::observability::sidecar::detected_get("fallback");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-ant-sidecar-test")
        .body(serde_json::to_vec(&sidecar_body()).unwrap())
        .send()
        .await
        .expect("proxy reachable");

    assert_eq!(
        resp.status(),
        200,
        "the client must not see the sidecar's 400"
    );
    let body = resp.text().await.expect("response body");
    assert!(
        body.contains("message_stop"),
        "the client should have received the normal-path reply"
    );
    assert_eq!(
        headroom_proxy::observability::sidecar::detected_get("fallback") - before,
        1,
        "exactly one fallback should have been counted"
    );

    // The original body went upstream untouched: every message and both tools.
    let normal = captured.lock().unwrap().clone();
    assert_eq!(normal.len(), 1, "exactly one normal-path call followed");
    let fwd: Value = serde_json::from_slice(&normal[0]).expect("upstream body is JSON");
    assert_eq!(fwd["messages"].as_array().unwrap().len(), 12);
    assert_eq!(fwd["model"], "claude-opus-5");
    assert_eq!(fwd["max_tokens"], 64000);
    assert_eq!(fwd["tools"].as_array().unwrap().len(), 2);

    proxy.shutdown().await;
}

/// With retries off the sidecar makes exactly one attempt before falling back,
/// rather than spending a budget the operator switched off.
#[tokio::test]
async fn retries_disabled_means_a_single_sidecar_attempt() {
    let _serial = FALLBACK_LOCK.lock().await;
    let upstream = MockServer::start().await;
    mount_split_upstream(&upstream, 529).await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.retry_enabled = false;
    })
    .await;

    post(&reqwest::Client::new(), &proxy.url(), &sidecar_body()).await;

    assert_eq!(
        SIDECAR_CALLS.load(Ordering::SeqCst),
        1,
        "the sidecar model should have been called exactly once"
    );

    proxy.shutdown().await;
}

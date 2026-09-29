//! JSON embedded in a text tool result: crushed once, then frozen.
//!
//! A `gh api` dump or MCP result reads as `PlainText` to the detector, so the
//! live-zone dispatcher crushes the JSON spans inside it (`embedded_json`).
//! That only pays if the provider prefix stays stable: turn 1 forwards the
//! crushed bytes, and turns 2 and 3 must forward those same bytes. Otherwise
//! the rewrite saves tokens once and then busts the cache on every later turn.
//!
//! The negative tests pin the false-positive side: text that merely contains
//! brackets, scalars or a small object goes out byte-identical.

use super::common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::start_proxy_with;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LEAD: usize = 2;
const HEAD: &str = "Successfully retrieved query executions\n";
const TAIL: &str = "\n-- 200 rows, exit 0 --\n";

fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_embedded\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":2048,\"cache_read_input_tokens\":0}}}\n\n",
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

fn rows_json() -> String {
    let rows: Vec<Value> = (0..200)
        .map(|i| {
            json!({
                "id": i,
                "status": "SUCCEEDED",
                "database": "analytics",
                "note": format!("repeat-pattern-{}", i % 3),
            })
        })
        .collect();
    serde_json::to_string(&rows).unwrap()
}

fn result_message(text: &str) -> Value {
    json!({
        "role": "user",
        "content": [{"type": "tool_result", "tool_use_id": "toolu_embedded", "content": text}],
    })
}

fn lead() -> [Value; LEAD] {
    [
        json!({"role": "user", "content": "run the report"}),
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_embedded", "name": "mcp__athena__query", "input": {}}
        ]}),
    ]
}

/// `turn` extra exchanges after the tool result; 0 is the first turn.
fn body(result: &Value, turn: usize) -> Value {
    let [ask, call] = lead();
    let mut messages = vec![ask, call, result.clone()];
    for i in 0..turn {
        messages.push(json!({"role": "assistant", "content": format!("step {i} done.")}));
        messages.push(json!({"role": "user", "content": format!("continue {i}")}));
    }
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": "you are a helpful assistant",
        "messages": messages,
    })
}

fn forwarded_result(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    v["messages"][LEAD]["content"][0]["content"]
        .as_str()
        .expect("tool_result content is a string")
        .to_string()
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-ant-embedded-json-test")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert_eq!(resp.status(), 200);
    // Drain the SSE body so the replay store commits the turn.
    let _ = resp.bytes().await.expect("response body");
}

async fn proxy_and_capture(
    upstream: &MockServer,
) -> (common::ProxyHandle, Arc<Mutex<Vec<Vec<u8>>>>) {
    let captured = mount_capture(upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
    })
    .await;
    (proxy, captured)
}

/// Send `turn` until the proxy replays what `first` forwarded, then return the
/// forwarded tool result. The store commits from a spawned task, so poll.
async fn replayed_result(
    client: &reqwest::Client,
    proxy_url: &str,
    captured: &Arc<Mutex<Vec<Vec<u8>>>>,
    turn: &Value,
    first: &str,
) -> String {
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        post_turn(client, proxy_url, turn).await;
        let got = forwarded_result(captured.lock().unwrap().last().unwrap());
        if got == first {
            return got;
        }
    }
    forwarded_result(captured.lock().unwrap().last().unwrap())
}

#[tokio::test]
async fn embedded_json_is_crushed_once_and_replayed_byte_identical() {
    let upstream = MockServer::start().await;
    let (proxy, captured) = proxy_and_capture(&upstream).await;
    let client = reqwest::Client::new();
    let original = format!("{HEAD}{}{TAIL}", rows_json());
    let result = result_message(&original);

    post_turn(&client, &proxy.url(), &body(&result, 0)).await;
    let first = forwarded_result(&captured.lock().unwrap()[0]);
    assert!(
        first.len() < original.len(),
        "precondition: embedded JSON must be crushed ({} -> {})",
        original.len(),
        first.len()
    );
    assert!(
        first.starts_with(HEAD) && first.contains(TAIL),
        "the text around the span must survive byte-exact"
    );

    for turn in [1, 2] {
        let got = replayed_result(
            &client,
            &proxy.url(),
            &captured,
            &body(&result, turn),
            &first,
        )
        .await;
        assert_eq!(
            got,
            first,
            "turn {} forwarded different bytes for the frozen tool result: the crush \
             would save tokens once and bust the provider cache from here on",
            turn + 1
        );
    }

    proxy.shutdown().await;
}

/// Same crush from a cold proxy: what turn 1 forwards is a function of the
/// input alone, so a restart does not move the bytes either.
#[tokio::test]
async fn a_second_proxy_forwards_the_same_crushed_bytes() {
    let original = format!("{HEAD}{}{TAIL}", rows_json());
    let result = result_message(&original);
    let mut forwarded = Vec::new();
    for _ in 0..2 {
        let upstream = MockServer::start().await;
        let (proxy, captured) = proxy_and_capture(&upstream).await;
        post_turn(&reqwest::Client::new(), &proxy.url(), &body(&result, 0)).await;
        forwarded.push(forwarded_result(&captured.lock().unwrap()[0]));
        proxy.shutdown().await;
    }
    assert_eq!(forwarded[0], forwarded[1]);
}

/// Text that has brackets or JSON in it but nothing the crusher acts on goes
/// out untouched.
#[tokio::test]
async fn text_without_a_routable_json_array_is_forwarded_untouched() {
    let scalars = (0..900)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let cases = [
        (
            "array of scalars",
            format!("ids: [{scalars}]\n{}", "x".repeat(300)),
        ),
        (
            "small object",
            format!(
                "config: {{\"a\":1,\"b\":[1,2,3]}}\n{}",
                "prose. ".repeat(200)
            ),
        ),
        (
            "unbalanced brackets",
            format!("{}{}", "[{ ".repeat(400), "tail text ".repeat(50)),
        ),
    ];
    for (name, original) in cases {
        let upstream = MockServer::start().await;
        let (proxy, captured) = proxy_and_capture(&upstream).await;
        let result = result_message(&original);
        post_turn(&reqwest::Client::new(), &proxy.url(), &body(&result, 0)).await;
        let got = forwarded_result(&captured.lock().unwrap()[0]);
        assert_eq!(got, original, "{name}: forwarded bytes changed");
        proxy.shutdown().await;
    }
}

/// Every marker that reaches the wire is logged as `ccr_marker_offered`, and
/// its hash is the one in the forwarded `<<ccr:HASH>>`. That join is what
/// `scripts/ccr-marker-rate.py` reads to price a compressor's retrievals.
#[tokio::test]
async fn an_offered_marker_is_logged_with_the_hash_on_the_wire() {
    let _capture = common::tracing_capture::serial().await;
    let buf = common::tracing_capture::buffer();
    buf.lock().unwrap().clear();

    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let store_dir = store.path().to_path_buf();
    let proxy = start_proxy_with(&upstream.uri(), move |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.ctx_offload = true;
        c.ctx_store_dir = Some(store_dir);
    })
    .await;

    let original = format!("{HEAD}{}{TAIL}", rows_json());
    post_turn(
        &reqwest::Client::new(),
        &proxy.url(),
        &body(&result_message(&original), 0),
    )
    .await;
    let forwarded = forwarded_result(&captured.lock().unwrap()[0]);
    let hash = forwarded
        .split("<<ccr:")
        .last()
        .and_then(|rest| rest.split(">>").next())
        .expect("forwarded block carries a CCR marker")
        .to_string();

    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    let line = logs
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| v["fields"].clone())
        .find(|f| f["event"] == "ccr_marker_offered")
        .expect("no ccr_marker_offered event was logged");
    assert_eq!(line["hash"], hash.as_str());
    assert_eq!(line["strategy"], "embedded_json");
    let (before, after) = (
        line["original_tokens"].as_u64().unwrap(),
        line["compressed_tokens"].as_u64().unwrap(),
    );
    assert!(
        after < before,
        "marker offered for a block that did not shrink"
    );

    proxy.shutdown().await;
}

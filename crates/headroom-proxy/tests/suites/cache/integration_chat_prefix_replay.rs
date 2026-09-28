//! Prefix replay on `/v1/chat/completions` (upstream `deca575c`).
//!
//! Turn 1's newest message is a large JSON tool result, which the chat live
//! zone compresses; the provider caches the compressed bytes. Turn 2 answers
//! a second tool call, so the big result is no longer the latest tool
//! message, has left the live zone, and the client resends it raw. With
//! `prefix_replay` on, the proxy must forward turn 1's compressed bytes
//! again, whether the turn was buffered or streamed; with it off, the raw
//! bytes go out (the bust), which keeps the replay assertion load-bearing.

use super::common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::start_proxy_with;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn json_reply() -> Value {
    json!({
        "id": "chatcmpl-replay",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "done."}}],
        "usage": {"prompt_tokens": 900, "completion_tokens": 2, "total_tokens": 902,
                  "prompt_tokens_details": {"cached_tokens": 0}},
    })
}

fn sse_reply() -> String {
    concat!(
        "data: {\"id\":\"chatcmpl-replay\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-replay\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl-replay\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":900,\"completion_tokens\":2,\"total_tokens\":902,\"prompt_tokens_details\":{\"cached_tokens\":0}}}\n\n",
        "data: [DONE]\n\n",
    )
    .to_string()
}

async fn mount_capture(upstream: &MockServer, stream: bool) -> Arc<Mutex<Vec<Vec<u8>>>> {
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock().unwrap().push(req.body.clone());
            if stream {
                ResponseTemplate::new(200).set_body_raw(sse_reply(), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(json_reply())
            }
        })
        .mount(upstream)
        .await;
    captured
}

/// Index of the tool message in every turn.
const TOOL: usize = 2;

/// 1500 low-uniqueness dicts, the fixture the chat compression suite uses:
/// SmartCrusher compresses it in the live zone.
fn big_tool_output() -> String {
    let rows: Vec<Value> = (0..1500)
        .map(|i| json!({"id": i, "kind": "row", "value": format!("repeat-{}", i % 5), "status": "ok"}))
        .collect();
    serde_json::to_string(&rows).unwrap()
}

fn turn(tool_output: &str, extra: &[Value], stream: bool) -> Value {
    let mut messages = vec![
        json!({"role": "user", "content": "read the report"}),
        json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_1", "type": "function",
             "function": {"name": "list_rows", "arguments": "{}"}}
        ]}),
        json!({"role": "tool", "tool_call_id": "call_1", "content": tool_output}),
    ];
    messages.extend(extra.iter().cloned());
    json!({"model": "gpt-4o", "stream": stream, "messages": messages})
}

fn forwarded_tool_content(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    v["messages"][TOOL]["content"]
        .as_str()
        .expect("tool content is a string")
        .to_string()
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-chat-replay-test")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert_eq!(resp.status(), 200);
    // Drain so the stream's close hook commits the turn.
    let _ = resp.bytes().await.expect("response body");
}

/// Runs two turns and returns (turn 1 forwarded, turn 2 forwarded, raw) tool
/// content. Turn 2 is re-sent until it replays, because a streamed turn
/// commits from a spawned task after the body drains; a turn that never
/// replays is returned as last seen.
async fn two_turns(stream: bool, replay: bool) -> (String, String, String) {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream, stream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = replay;
    })
    .await;
    let client = reqwest::Client::new();
    let raw = big_tool_output();

    post_turn(&client, &proxy.url(), &turn(&raw, &[], stream)).await;
    let fwd1 = forwarded_tool_content(&captured.lock().unwrap()[0]);

    let turn2 = turn(
        &raw,
        &[
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_2", "type": "function",
                 "function": {"name": "list_rows", "arguments": "{\"page\":2}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "call_2", "content": "no more rows"}),
        ],
        stream,
    );
    let mut fwd2 = String::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        post_turn(&client, &proxy.url(), &turn2).await;
        fwd2 = forwarded_tool_content(captured.lock().unwrap().last().unwrap());
        if fwd2 == fwd1 || !replay {
            break;
        }
    }
    proxy.shutdown().await;
    (fwd1, fwd2, raw)
}

#[tokio::test]
async fn buffered_chat_turn_replays_the_compressed_tool_result() {
    let (fwd1, fwd2, raw) = two_turns(false, true).await;
    assert_ne!(
        fwd1, raw,
        "precondition: turn 1 must compress the tool result"
    );
    assert_eq!(fwd2, fwd1, "turn 2 must resend turn 1's forwarded bytes");
}

#[tokio::test]
async fn streamed_chat_turn_replays_the_compressed_tool_result() {
    let (fwd1, fwd2, raw) = two_turns(true, true).await;
    assert_ne!(
        fwd1, raw,
        "precondition: turn 1 must compress the tool result"
    );
    assert_eq!(fwd2, fwd1, "turn 2 must resend turn 1's forwarded bytes");
}

#[tokio::test]
async fn without_replay_turn_two_resends_the_raw_tool_result() {
    let (fwd1, fwd2, raw) = two_turns(false, false).await;
    assert_ne!(
        fwd1, raw,
        "precondition: turn 1 must compress the tool result"
    );
    assert_eq!(fwd2, raw, "the bust this feature fixes");
}


use super::*;
use crate::observability::proxy_counters;
use headroom_core::request_outcome::RequestOutcome;

fn turn_with(assistant: serde_json::Value, user: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"messages": [
        {"role": "user", "content": [{"type": "text", "text": "go"}]},
        {"role": "assistant", "content": assistant},
        {"role": "user", "content": user},
    ]})
}

#[test]
fn a_paired_turn_reports_nothing_unanswered() {
    let body = turn_with(
        serde_json::json!([{"type": "tool_use", "id": "tu_1", "name": "Bash"}]),
        serde_json::json!([{"type": "tool_result", "tool_use_id": "tu_1"}]),
    );
    assert!(unanswered_tool_uses(&body).is_empty());
}

#[test]
fn an_unanswered_call_is_reported_with_its_message_index() {
    let body = turn_with(
        serde_json::json!([
            {"type": "tool_use", "id": "tu_1", "name": "Bash"},
            {"type": "tool_use", "id": "tu_2", "name": "Skill"},
        ]),
        serde_json::json!([{"type": "tool_result", "tool_use_id": "tu_1"}]),
    );
    assert_eq!(unanswered_tool_uses(&body), vec![(1, "tu_2".to_string())]);
}

/// A result in a later message does not count: Anthropic wants it in the
/// message immediately after the call.
#[test]
fn a_result_two_messages_later_does_not_answer_the_call() {
    let mut body = turn_with(
        serde_json::json!([{"type": "tool_use", "id": "tu_1", "name": "Bash"}]),
        serde_json::json!([{"type": "text", "text": "nothing here"}]),
    );
    body["messages"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "tu_1"}]
        }));
    assert_eq!(unanswered_tool_uses(&body), vec![(1, "tu_1".to_string())]);
}

/// The last assistant message has no next message at all. Upstream still
/// refuses it, so it is still worth naming.
#[test]
fn a_trailing_call_with_no_next_message_is_unanswered() {
    let body = serde_json::json!({"messages": [
        {"role": "user", "content": [{"type": "text", "text": "go"}]},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "tu_1", "name": "Bash"}]},
    ]});
    assert_eq!(unanswered_tool_uses(&body), vec![(1, "tu_1".to_string())]);
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Overhead and TTFB only update their bounds when positive. A zero means
/// "not measured" — treating it as a sample would pin the minimum at 0
/// forever and report a floor that never happened.
#[test]
fn a_zero_timing_never_lowers_the_minimum() {
    let _g = lock();
    proxy_counters::record_request("t-probe", "m", 1, 1, 0, 40.0, false, 25.0, 60.0);
    let after_real = (
        proxy_counters::overhead_min_for_test(),
        proxy_counters::ttfb_min_for_test(),
    );
    assert!(
        after_real.0 > 0.0,
        "a real overhead sample must set the min"
    );
    assert!(after_real.1 > 0.0, "a real ttfb sample must set the min");

    // A follow-up request that measured neither must not drag them to 0.
    proxy_counters::record_request("t-probe", "m", 1, 1, 0, 40.0, false, 0.0, 0.0);
    assert_eq!(proxy_counters::overhead_min_for_test(), after_real.0);
    assert_eq!(proxy_counters::ttfb_min_for_test(), after_real.1);
}

/// The sink is the single funnel every handler's outcome passes through, so
/// the timing fields have to survive the trip into it.
#[test]
fn the_outcome_carries_the_timing_fields() {
    let outcome = RequestOutcome {
        provider: "anthropic".to_string(),
        model: "m".to_string(),
        overhead_ms: 12.5,
        ttfb_ms: 340.0,
        total_latency_ms: 900.0,
        ..Default::default()
    };
    assert_eq!(outcome.overhead_ms, 12.5);
    assert_eq!(outcome.ttfb_ms, 340.0);
    // Overhead is headroom's own cost and must not exceed the wall clock.
    assert!(outcome.overhead_ms <= outcome.total_latency_ms);
}

// ── signed reasoning blocks ──────────────────────────────────

fn client_body_with_thinking() -> serde_json::Value {
    serde_json::json!({
        "model": "claude-sonnet-4-5[1m]",
        "tools": [{"name": "a"}, {"name": "b"}],
        "messages": [
            {"role": "user", "content": "solve this"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "private reasoning", "signature": "sig123"},
                {"type": "text", "text": "42"}
            ]},
            {"role": "user", "content": "continue"}
        ]
    })
}

fn as_bytes(v: &serde_json::Value) -> bytes::Bytes {
    bytes::Bytes::from(serde_json::to_vec(v).unwrap())
}

/// The common case: the pipeline changed something outside the message
/// array, so the signed blocks still match and the body goes as built.
#[test]
fn a_body_whose_reasoning_blocks_survive_is_forwarded_as_built() {
    let original = as_bytes(&client_body_with_thinking());
    let mut sent = client_body_with_thinking();
    sent["model"] = serde_json::json!("claude-sonnet-4-5");
    sent["tools"] = serde_json::json!([{"name": "a"}]);
    let sent = as_bytes(&sent);

    let out = restore_client_reasoning_blocks(sent.clone(), &original, "r1");
    assert_eq!(out, sent);
}

/// A body with no signed block never takes the restore path, however much
/// the pipeline rewrote it.
#[test]
fn a_body_without_reasoning_blocks_is_forwarded_as_built() {
    let original = as_bytes(&serde_json::json!({
        "messages": [{"role": "user", "content": "hello"}]
    }));
    let sent = as_bytes(&serde_json::json!({
        "messages": [{"role": "user", "content": "compressed"}]
    }));

    let out = restore_client_reasoning_blocks(sent.clone(), &original, "r1");
    assert_eq!(out, sent);
}

/// Editing a signed block is what Anthropic refuses. The client's message
/// array goes back; the model rewrite outside it stays.
#[test]
fn an_edited_reasoning_block_restores_the_client_messages() {
    let original = as_bytes(&client_body_with_thinking());
    let mut sent = client_body_with_thinking();
    sent["model"] = serde_json::json!("claude-sonnet-4-5");
    sent["messages"][1]["content"][0]["thinking"] = serde_json::json!("edited");
    let sent = as_bytes(&sent);

    let out = restore_client_reasoning_blocks(sent, &original, "r1");
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        parsed["messages"][1]["content"][0]["thinking"],
        "private reasoning"
    );
    assert_eq!(parsed["model"], "claude-sonnet-4-5");
}

/// Dropping the message that held the block counts as altering it: the
/// signed blocks on the wire no longer match what the client sent.
#[test]
fn a_prior_turn_reasoning_block_dropped_whole_is_forwarded_as_built() {
    let mut body = client_body_with_thinking();
    body["messages"].as_array_mut().unwrap().extend([
        serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "later reasoning", "signature": "sig456"},
            {"type": "text", "text": "43"}
        ]}),
        serde_json::json!({"role": "user", "content": "and then"}),
    ]);
    let original = as_bytes(&body);
    // The first assistant turn loses its block; the last keeps its own.
    body["messages"][1]["content"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let sent = as_bytes(&body);
    let out = restore_client_reasoning_blocks(sent.clone(), &original, "r1");
    assert_eq!(out, sent);

    // Dropping the LAST assistant turn's block still restores — but only
    // that message. The earlier turn keeps the strip it was given, which
    // is `prior_thinking` doing its job and no business of this guard.
    body["messages"][3]["content"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let out = restore_client_reasoning_blocks(as_bytes(&body), &original, "r1");
    let restored: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        restored["messages"][3],
        client_body_with_thinking_extended()["messages"][3]
    );
    assert_eq!(restored["messages"][1]["content"][0]["type"], "text");
}

/// The point of repairing narrowly: a message the guard has no quarrel
/// with keeps whatever the pipeline did to it. Reverting those too is
/// what killed the cached prefix — the opening messages are in it.
#[test]
fn an_untouched_message_keeps_its_rewrite_when_another_is_restored() {
    let original = as_bytes(&client_body_with_thinking_extended());
    let mut body = client_body_with_thinking_extended();
    // Stand-in for a ctx-offload placeholder in the cached prefix.
    body["messages"][0]["content"] = serde_json::json!("[offloaded #abc123]");
    // And the breakage the guard exists for, in the last assistant turn.
    body["messages"][3]["content"][0]["thinking"] = serde_json::json!("edited");

    let out = restore_client_reasoning_blocks(as_bytes(&body), &original, "r1");
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(parsed["messages"][0]["content"], "[offloaded #abc123]");
    assert_eq!(
        parsed["messages"][3]["content"][0]["thinking"],
        "later reasoning"
    );
}

/// Arrays that cannot be lined up index for index fall back to the whole
/// client array, which is the only repair that is certainly correct.
#[test]
fn a_changed_message_count_falls_back_to_the_whole_array() {
    let original = as_bytes(&client_body_with_thinking_extended());
    let mut body = client_body_with_thinking_extended();
    body["messages"][0]["content"] = serde_json::json!("[offloaded #abc123]");
    body["messages"][3]["content"][0]["thinking"] = serde_json::json!("edited");
    body["messages"].as_array_mut().unwrap().pop();

    let out = restore_client_reasoning_blocks(as_bytes(&body), &original, "r1");
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        parsed["messages"],
        client_body_with_thinking_extended()["messages"]
    );
}

fn client_body_with_thinking_extended() -> serde_json::Value {
    let mut body = client_body_with_thinking();
    body["messages"].as_array_mut().unwrap().extend([
        serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "later reasoning", "signature": "sig456"},
            {"type": "text", "text": "43"}
        ]}),
        serde_json::json!({"role": "user", "content": "and then"}),
    ]);
    body
}

#[test]
fn a_dropped_reasoning_block_restores_the_client_messages() {
    let original = as_bytes(&client_body_with_thinking());
    let mut sent = client_body_with_thinking();
    sent["messages"].as_array_mut().unwrap().remove(1);
    let sent = as_bytes(&sent);

    let out = restore_client_reasoning_blocks(sent, &original, "r1");
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(parsed["messages"].as_array().unwrap().len(), 3);
    assert_eq!(parsed["messages"][1]["content"][0]["signature"], "sig123");
}

// ── cache_control TTL ordering ───────────────────────────────

/// A body with no 1h marker cannot break the rule, and pays no parse.
#[test]
fn a_body_with_no_1h_marker_skips_the_ttl_repair() {
    let body = as_bytes(&serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}
        ]}]
    }));
    let out = enforce_cache_control_ttl_order(body.clone(), &body, false, "r1");
    assert_eq!(out, body);
}

/// The `/btw` case end to end: the client's turn is in the 5m lane and a
/// replayed 1h marker sits behind its breakpoints.
#[test]
fn a_replayed_1h_marker_is_contained_before_forwarding() {
    let client = as_bytes(&serde_json::json!({
        "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "u"}]}]
    }));
    let sent = as_bytes(&serde_json::json!({
        "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "u",
             "cache_control": {"type": "ephemeral", "ttl": "1h"}}
        ]}]
    }));

    let out = enforce_cache_control_ttl_order(sent, &client, false, "r1");
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(parsed["messages"][0]["content"][0]["cache_control"]
        .get("ttl")
        .is_none());
}

/// B1 authors those 1h markers on purpose, so they are not a leak and the
/// pin must survive the guard.
#[test]
fn the_forced_1h_pin_survives_the_ttl_repair() {
    let client = as_bytes(&serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}
        ]}]
    }));
    let sent = as_bytes(&serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "u",
             "cache_control": {"type": "ephemeral", "ttl": "1h"}}
        ]}]
    }));

    let out = enforce_cache_control_ttl_order(sent.clone(), &client, true, "r1");
    assert_eq!(out, sent);
}

// ── turn-hook re-drive accounting ──────────────────────────────────

/// Each surface spells the same quantities differently, and reading a
/// response by the wrong name silently bills it as zero.
#[test]
fn usage_is_read_by_the_provider_shape() {
    let anthropic = serde_json::json!({"usage": {
        "input_tokens": 10, "output_tokens": 2,
        "cache_read_input_tokens": 3, "cache_creation_input_tokens": 4
    }});
    assert_eq!(response_usage(&anthropic, "anthropic"), (10, 2, 3, 4));

    let responses = serde_json::json!({"usage": {
        "input_tokens": 10, "output_tokens": 2,
        "input_tokens_details": {"cached_tokens": 3}
    }});
    assert_eq!(
        response_usage(&responses, "openai_responses"),
        (10, 2, 3, 0)
    );

    let chat = serde_json::json!({"usage": {
        "prompt_tokens": 10, "completion_tokens": 2,
        "prompt_tokens_details": {"cached_tokens": 3}
    }});
    assert_eq!(response_usage(&chat, "openai_chat"), (10, 2, 3, 0));

    // A response with no usage block reads as zeros rather than failing.
    assert_eq!(
        response_usage(&serde_json::json!({}), "anthropic"),
        (0, 0, 0, 0)
    );
}

/// No re-drive: the one response recorded is the one the outcome block
/// reads, so the accounting has to come out untouched.
#[test]
fn a_hook_that_never_calls_the_model_reports_nothing() {
    let response = serde_json::json!({"usage": {"input_tokens": 100, "output_tokens": 10}});
    let mut usage = TurnHookUsage::default();
    usage.record(&response, "anthropic");
    usage.settle(&response, "anthropic");
    assert!(usage.is_empty());
}

/// Whichever response the hook hands back, what is left is the spend the
/// outcome block would otherwise miss.
#[test]
fn settling_leaves_only_the_calls_the_outcome_block_misses() {
    let original = serde_json::json!({"usage": {"input_tokens": 100, "output_tokens": 10}});
    let redrive = serde_json::json!({"usage": {"input_tokens": 300, "output_tokens": 20}});

    // The hook looked and left the response alone: its own call is the delta.
    let mut kept = TurnHookUsage::default();
    kept.record(&original, "anthropic");
    kept.record(&redrive, "anthropic");
    kept.settle(&original, "anthropic");
    assert_eq!(kept.calls, 1);
    assert_eq!(kept.input_tokens, 300);
    assert_eq!(kept.output_tokens, 20);

    // The hook returned the re-drive: now the original is the unread one.
    let mut replaced = TurnHookUsage::default();
    replaced.record(&original, "anthropic");
    replaced.record(&redrive, "anthropic");
    replaced.settle(&redrive, "anthropic");
    assert_eq!(replaced.calls, 1);
    assert_eq!(replaced.input_tokens, 100);
    assert_eq!(replaced.output_tokens, 10);
}

/// A hook may hand back a response it built itself, matching no upstream
/// call. The delta plus what the outcome block reads still has to come to
/// what was really billed.
#[test]
fn a_synthesised_response_still_totals_the_real_spend() {
    let original = serde_json::json!({"usage": {"input_tokens": 100, "output_tokens": 10}});
    let redrive = serde_json::json!({"usage": {"input_tokens": 300, "output_tokens": 20}});

    let mut usage = TurnHookUsage::default();
    usage.record(&original, "anthropic");
    usage.record(&redrive, "anthropic");
    usage.settle(&serde_json::json!({"id": "made-up"}), "anthropic");
    // The outcome block reads nothing off the synthetic body, so the delta
    // carries both real calls.
    assert_eq!(usage.input_tokens, 400);
    assert_eq!(usage.output_tokens, 30);
}

/// Inflated figures in a synthesised response must not drive the delta
/// negative and bill less than the turn cost.
#[test]
fn settling_never_goes_negative() {
    let original = serde_json::json!({"usage": {"input_tokens": 100, "output_tokens": 10}});
    let inflated = serde_json::json!({"usage": {"input_tokens": 9000, "output_tokens": 900}});
    let mut usage = TurnHookUsage::default();
    usage.record(&original, "anthropic");
    usage.settle(&inflated, "anthropic");
    assert!(usage.is_empty());
}

/// A hook that calls the model twice made two billed requests, and the
/// outcome block reads neither.
#[tokio::test]
async fn call_model_records_every_redrive() {
    use crate::turn_hooks::CallModel;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "redrive",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 7,
                "cache_read_input_tokens": 40,
                "cache_creation_input_tokens": 5
            }
        })))
        .mount(&server)
        .await;

    let usage = Arc::new(std::sync::Mutex::new(TurnHookUsage::default()));
    let call_model = ProxyCallModel {
        template: serde_json::json!({"model": "claude-x", "messages": []}),
        upstream_url: format!("{}/v1/messages", server.uri()).parse().unwrap(),
        client: reqwest::Client::new(),
        headers: http::HeaderMap::new(),
        request_id: "req-hook".to_string(),
        usage: Arc::clone(&usage),
        usage_provider: "anthropic".to_string(),
    };

    assert_eq!(call_model.call(vec![]).await["id"], "redrive");
    call_model.call(vec![]).await;

    let recorded = *usage.lock().unwrap();
    assert_eq!(recorded.calls, 2);
    assert_eq!(recorded.input_tokens, 200);
    assert_eq!(recorded.output_tokens, 14);
    assert_eq!(recorded.cache_read_tokens, 80);
    assert_eq!(recorded.cache_write_tokens, 10);
}

/// A call that never reached the upstream was not billed, so it must not
/// show up as spend.
#[tokio::test]
async fn a_failed_redrive_records_nothing() {
    use crate::turn_hooks::CallModel;

    let usage = Arc::new(std::sync::Mutex::new(TurnHookUsage::default()));
    let call_model = ProxyCallModel {
        template: serde_json::json!({"model": "claude-x", "messages": []}),
        // Reserved as invalid by RFC 6890; nothing is listening.
        upstream_url: "http://192.0.2.1:1/v1/messages".parse().unwrap(),
        client: crate::ssl_context::client_builder()
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .unwrap(),
        headers: http::HeaderMap::new(),
        request_id: "req-hook".to_string(),
        usage: Arc::clone(&usage),
        usage_provider: "anthropic".to_string(),
    };

    assert!(call_model.call(vec![]).await.is_null());
    assert!(usage.lock().unwrap().is_empty());
}

use super::*;
use crate::openai::request::anthropic_to_openai_responses_request;
use crate::test_support::EventCapture;
use serde_json::json;
fn translator_with_outcome(
    model: &str,
    tokens_saved: i64,
) -> (
    StreamTranslator,
    std::sync::Arc<crate::request_logger::RequestLogger>,
    std::sync::Arc<headroom_core::cost_tracker::CostTracker>,
) {
    redirect_savings_ledger();
    redirect_savings_tracker();
    let cost_tracker = std::sync::Arc::new(headroom_core::cost_tracker::CostTracker::new(
        None, "monthly",
    ));
    let request_logger = std::sync::Arc::new(crate::request_logger::RequestLogger::new(None));
    let ctx = RoutedOutcomeContext {
        sink: std::sync::Arc::new(crate::proxy::ProxyOutcomeSink {
            cost_tracker: cost_tracker.clone(),
            savings_tracker: std::sync::Arc::new(
                headroom_core::savings_tracker::SavingsTracker::new(None, false),
            ),
            request_logger: request_logger.clone(),
        }),
        request_id: "req-test".to_string(),
        replay_store: None,
        usage_observer: None,
        reroute: None,
        session_key: "sess-test".to_string(),
        model: model.to_string(),
        provider: "openai_responses".to_string(),
        client: None,
        project: None,
        session_id: None,
        agent_id: None,
        tokens_saved,
        transforms_applied: vec!["ctx_offload".to_string()],
        num_messages: 3,
        started_at: std::time::Instant::now(),
        overhead_ms: 1.5,
        forwarded_tokens_estimate: 777,
        outbound_bytes: 0,
        upstream_attempts: 1,
        egress_slot: None,
        redact_store: None,
        conversation_key: None,
    };
    let t = StreamTranslator::new(model.to_string()).with_outcome(Some(ctx));
    (t, request_logger, cost_tracker)
}

#[test]
fn translator_without_outcome_context_books_nothing() {
    let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
    t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 1}}}).to_string(),
    );
}

fn drive(t: &mut StreamTranslator, frames: &[(&str, &str)]) -> String {
    let mut all = String::new();
    for (event, data) in frames {
        for e in t.process_frame(Some(event), data) {
            all.push_str(&e);
        }
    }
    all
}

/// The whole point of the envelope: a reasoning item leaves in the thinking
/// block's signature and comes back from the client's own history, with no
/// proxy-side state in between.
#[test]
fn truncated_completion_reports_max_tokens() {
    for (frames, expected) in [
        (
            vec![(
                "response.completed",
                r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"output_tokens":9}}}"#,
            )],
            "max_tokens",
        ),
        (
            vec![
                (
                    "response.output_item.added",
                    r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
                ),
                (
                    "response.completed",
                    r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"output_tokens":9}}}"#,
                ),
            ],
            "max_tokens",
        ),
        (
            vec![
                (
                    "response.output_item.added",
                    r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
                ),
                (
                    "response.completed",
                    r#"{"response":{"usage":{"output_tokens":9}}}"#,
                ),
            ],
            "tool_use",
        ),
        (
            vec![(
                "response.completed",
                r#"{"response":{"usage":{"output_tokens":9}}}"#,
            )],
            "end_turn",
        ),
    ] {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(&mut t, &frames);
        assert!(
            sse.contains(&format!(r#""stop_reason":"{expected}""#)),
            "expected {expected}, got: {sse}"
        );
    }
}

/// `[DONE]` with an announced-but-never-closed tool call is a truncated
/// call, closed like a dropped connection: the translator leaves the tool
/// block open and the turn unstopped, and `finish_on_drop` withholds the
/// partial block and marks the turn. Closing it here instead released the
/// half-streamed call to the client as if complete.
#[tokio::test]
async fn done_with_open_tool_block_withholds_the_partial_call() {
    use futures_util::StreamExt as _;
    let mut t = StreamTranslator::new("claude-muse-spark-1.3".to_string());
    let sse = drive(
        &mut t,
        &[
            (
                "response.output_text.delta",
                r#"{"delta":"I'll run the tests now"}"#,
            ),
            (
                "response.output_item.added",
                r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
            ),
            ("", "[DONE]"),
        ],
    );
    assert!(
        !sse.contains("event: message_stop"),
        "the finisher owns the stop: {sse}"
    );
    assert!(
        !sse.contains(r#""stop_reason":"tool_use""#),
        "half-streamed call must not be offered, got: {sse}"
    );
    let closed: Vec<String> = crate::sse::stream_finisher::finish_on_drop(
        futures_util::stream::iter(std::iter::once(Ok::<_, std::io::Error>(
            bytes::Bytes::from(sse),
        ))),
        "test".to_string(),
    )
    .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
    .collect()
    .await;
    let client = closed.join("");
    assert!(
        !client.contains(r#""type":"tool_use""#),
        "the partial call must not reach the client: {client}"
    );
    assert!(
        client.contains("I'll run the tests now"),
        "the text still does: {client}"
    );
    assert_eq!(
        client.matches("event: message_stop").count(),
        1,
        "closed exactly once: {client}"
    );
}

/// A function_call whose `output_item.added` never arrived must still
/// go out: the `.done` item carries the whole call, so the translator
/// rebuilds the tool block from it instead of silently dropping the
/// call and handing the client an announcement with `end_turn`.
#[test]
fn function_call_without_added_is_recovered_from_done() {
    let mut t = StreamTranslator::new("claude-muse-spark-1.3".to_string());
    let sse = drive(
        &mut t,
        &[
            (
                "response.output_text.delta",
                r#"{"delta":"I'll run the tests now"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"function_call","call_id":"c9","name":"Bash","arguments":"{\"command\":\"x\"}"}}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ),
        ],
    );
    assert!(
        sse.contains(r#""type":"tool_use""#),
        "tool block must be recovered, got: {sse}"
    );
    assert!(
        sse.contains(r#""name":"Bash""#),
        "recovered block keeps the name, got: {sse}"
    );
    assert!(
        sse.contains(r#""stop_reason":"tool_use""#),
        "stop must offer the call, got: {sse}"
    );
}

/// An upstream reusing one `call_id` across calls in a turn would give
/// the client same-id `tool_use` blocks whose results the upstream then
/// rejects; the translator renames the repeats so every block carries a
/// distinct id.
#[test]
fn duplicate_call_ids_are_renamed() {
    let mut t = StreamTranslator::new("claude-muse-spark-1.3".to_string());
    let sse = drive(
        &mut t,
        &[
            (
                "response.output_item.added",
                r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash","arguments":"{}"}}"#,
            ),
            (
                "response.output_item.added",
                r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash","arguments":"{}"}}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ),
        ],
    );
    assert!(
        sse.contains(r#""id":"c1""#),
        "first claim keeps the id, got: {sse}"
    );
    assert!(
        sse.contains(r#""id":"c1_2""#),
        "repeat is renamed, got: {sse}"
    );
    assert!(
        sse.contains(r#""stop_reason":"tool_use""#),
        "stop must offer the calls, got: {sse}"
    );
}

/// Zen's real order for a multi-call turn (raw capture, 2026-10-06):
/// each call's `.added` and arguments stream in place, interleaved with
/// commentary messages, and every call's `.done` arrives only after the
/// last item. Each call must reach the client exactly once with its own
/// arguments; re-emitting the late `.done`s made the client run every
/// call after the first twice.
#[test]
fn late_function_call_done_is_not_reemitted() {
    let mut t = StreamTranslator::new("claude-muse-spark-1.3".to_string());
    let mut frames: Vec<(&str, String)> = Vec::new();
    let calls = [(2, "c_a", "/a"), (4, "c_b", "/b"), (6, "c_c", "/c")];
    for (i, id, path) in calls {
        frames.push((
            "response.output_item.added",
            format!(
                r#"{{"output_index":{},"item":{{"type":"message","phase":"commentary"}}}}"#,
                i - 1
            ),
        ));
        frames.push((
            "response.output_text.delta",
            format!(r#"{{"output_index":{},"delta":"Reading {path}."}}"#, i - 1),
        ));
        frames.push((
                "response.output_item.added",
                format!(r#"{{"output_index":{i},"item":{{"type":"function_call","call_id":"{id}","name":"Read"}}}}"#),
            ));
        frames.push((
            "response.function_call_arguments.delta",
            format!(r#"{{"output_index":{i},"delta":"{{\"path\":\"{path}\"}}"}}"#),
        ));
    }
    for (i, id, path) in calls {
        frames.push((
            "response.function_call_arguments.done",
            format!(r#"{{"output_index":{i},"arguments":"{{\"path\":\"{path}\"}}"}}"#),
        ));
        frames.push((
                "response.output_item.done",
                format!(r#"{{"output_index":{i},"item":{{"type":"function_call","call_id":"{id}","name":"Read","arguments":"{{\"path\":\"{path}\"}}"}}}}"#),
            ));
    }
    frames.push((
        "response.completed",
        r#"{"response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#.to_string(),
    ));
    let frames: Vec<(&str, &str)> = frames.iter().map(|(e, d)| (*e, d.as_str())).collect();
    let sse = drive(&mut t, &frames);
    assert_eq!(
        sse.matches(r#""type":"tool_use""#).count(),
        3,
        "one block per call, got: {sse}"
    );
    for (_, id, _) in calls {
        assert!(
            sse.contains(&format!(r#""id":"{id}""#)) && !sse.contains(&format!("{id}_2")),
            "{id} must go out under its own id, never renamed, got: {sse}"
        );
    }
    for (_, _, path) in calls {
        assert_eq!(
            sse.matches(&format!(r#"{{\"path\":\"{path}\"}}"#)).count(),
            1,
            "{path} arguments go out once, got: {sse}"
        );
    }
    assert!(
        sse.contains(r#""stop_reason":"tool_use""#),
        "stop must offer the calls, got: {sse}"
    );
    assert_eq!(t.last_message_phase.as_deref(), Some("commentary"));
}

/// A call that only `response.completed` lists is delivered once and
/// turns the stop into `tool_use`; one already streamed is not repeated.
#[test]
fn call_listed_only_in_completed_is_delivered() {
    let mut t = StreamTranslator::new("claude-muse-spark-1.3".to_string());
    let sse = drive(
        &mut t,
        &[
            (
                "response.output_item.added",
                r#"{"output_index":0,"item":{"type":"function_call","call_id":"c1","name":"Read"}}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"output_index":0,"delta":"{\"path\":\"/a\"}"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"output_index":0,"item":{"type":"function_call","call_id":"c1","name":"Read","arguments":"{\"path\":\"/a\"}"}}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"delta":"Let me pull it."}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"output":[{"type":"function_call","call_id":"c1","name":"Read","arguments":"{\"path\":\"/a\"}"},{"type":"message"},{"type":"function_call","call_id":"c2","name":"Read","arguments":"{\"path\":\"/b\"}"}],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ),
        ],
    );
    assert_eq!(sse.matches(r#""type":"tool_use""#).count(), 2, "got: {sse}");
    assert!(
        sse.contains(r#""id":"c2""#),
        "lost call delivered, got: {sse}"
    );
    assert_eq!(sse.matches(r#"{\"path\":\"/b\"}"#).count(), 1, "got: {sse}");
    assert!(sse.contains(r#""stop_reason":"tool_use""#), "got: {sse}");
}

/// A text-only turn whose last message is `commentary` raises the flag
/// the orphan continuation reads; a `final_answer` turn does not.
#[test]
fn text_only_commentary_stop_is_flagged() {
    for (phase, flagged) in [("commentary", true), ("final_answer", false)] {
        let deferred = DeferredCcrBooking::new();
        let mut t = StreamTranslator::new("claude-muse-spark-1.3".to_string())
            .with_deferred_ccr(deferred.clone());
        let added =
            format!(r#"{{"output_index":0,"item":{{"type":"message","phase":"{phase}"}}}}"#);
        drive(
            &mut t,
            &[
                ("response.output_item.added", added.as_str()),
                (
                    "response.output_text.delta",
                    r#"{"output_index":0,"delta":"Committing in 13 chunks."}"#,
                ),
                (
                    "response.completed",
                    r#"{"response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
                ),
            ],
        );
        assert_eq!(
            deferred
                .ended_on_commentary
                .load(std::sync::atomic::Ordering::Acquire),
            flagged,
            "{phase}"
        );
    }
}

/// A signature-only block must not collide with a block already open, or
/// two content blocks share an index and the client sees a torn stream.
#[test]
fn signature_only_block_closes_open_text_first() {
    let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
    let sse = drive(
        &mut t,
        &[
            ("response.output_text.delta", r#"{"delta":"partial"}"#),
            (
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_4","summary":[],"encrypted_content":"ENC_4"}}"#,
            ),
        ],
    );
    assert!(signature_from_stream(&sse).is_some());
    let indices: Vec<i64> = sse
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|p| serde_json::from_str::<Value>(p).ok())
        .filter(|e| e["type"] == "content_block_start")
        .filter_map(|e| e["index"].as_i64())
        .collect();
    let mut unique = indices.clone();
    unique.dedup();
    assert_eq!(indices, unique, "two content blocks opened on one index");
}

/// An id with no blob (or the reverse) is not replayable, so no signature
/// is minted and the summary stays a plain thinking block.
#[test]
fn incomplete_reasoning_item_emits_no_signature() {
    let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
    let sse = drive(
        &mut t,
        &[(
            "response.output_item.done",
            r#"{"item":{"type":"reasoning","id":"rs_3","summary":[]}}"#,
        )],
    );
    assert!(signature_from_stream(&sse).is_none());
}

/// Thinking blocks we did not mint must never become reasoning items: a
/// real Anthropic signature, or none at all, is dropped on the way out.
#[test]
fn stream_translator_translates_reasoning_summary_to_thinking() {
    let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
    let mut all = String::new();
    for (event, data) in [
        (
            "response.reasoning_summary_text.delta",
            r#"{"delta":"Consider the"}"#,
        ),
        (
            "response.reasoning_summary_text.delta",
            r#"{"delta":" edge cases"}"#,
        ),
        ("response.reasoning_summary_part.added", r#"{}"#),
        ("response.output_text.delta", r#"{"delta":"Answer"}"#),
        (
            "response.completed",
            r#"{"response":{"usage":{"output_tokens":5}}}"#,
        ),
    ] {
        for e in t.process_frame(Some(event), data) {
            all.push_str(&e);
        }
    }
    assert!(all.contains(r#""type":"thinking","thinking":"""#));
    assert!(all.contains(r#""thinking":"Consider the""#));
    assert!(all.contains(r#""text":"Answer""#));
    assert!(all.contains(r#""stop_reason":"end_turn""#));
}

#[test]
fn stream_translator_translates_responses_function_call_frames() {
    let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
    let mut all = String::new();
    for (event, data) in [
        (
            "response.created",
            r#"{"response":{"model":"gpt-5.6-terra"}}"#,
        ),
        (
            "response.output_item.added",
            r#"{"item":{"type":"function_call","call_id":"call_1","name":"Bash","arguments":""}}"#,
        ),
        (
            "response.function_call_arguments.delta",
            r#"{"delta":"{\"command\":"}"#,
        ),
        (
            "response.function_call_arguments.delta",
            r#"{"delta":"\"ls\"}"}"#,
        ),
        (
            "response.output_item.done",
            r#"{"item":{"type":"function_call","call_id":"call_1","name":"Bash"}}"#,
        ),
        (
            "response.completed",
            r#"{"response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ),
    ] {
        for e in t.process_frame(Some(event), data) {
            all.push_str(&e);
        }
    }
    assert!(all.contains(r#""type":"tool_use","id":"call_1","name":"Bash""#));
    assert!(all.contains(r#""type":"input_json_delta""#));
    assert!(all.contains(r#"\"command\":"#));
    assert!(all.contains(r#""stop_reason":"tool_use""#));
    assert!(all.contains("message_stop"));
}

#[test]
fn stream_translator_restores_renamed_tool_names() {
    use crate::routed::tool_alias::ToolAlias;
    let alias = ToolAlias::derive(Some(&vec![
        json!({"name": "Bash"}),
        json!({"name": "Read"}),
    ]));
    let mut t =
        StreamTranslator::new("claude-muse-spark-1.3".to_string()).with_tool_aliases(Some(alias));
    let mut all = String::new();
    for (event, data) in [
        (
            "response.created",
            r#"{"response":{"model":"muse-spark-1.3-contributor-free"}}"#,
        ),
        (
            "response.output_item.added",
            r#"{"item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":""}}"#,
        ),
        (
            "response.function_call_arguments.delta",
            r#"{"delta":"{\"command\":"}"#,
        ),
        (
            "response.output_item.done",
            r#"{"item":{"type":"function_call","call_id":"call_1","name":"bash"}}"#,
        ),
        (
            "response.completed",
            r#"{"response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ),
    ] {
        for e in t.process_frame(Some(event), data) {
            all.push_str(&e);
        }
    }
    // The client must see its own `Bash`, never the upstream `bash`.
    assert!(all.contains(r#""type":"tool_use","id":"call_1","name":"Bash""#));
    assert!(!all.contains(r#""name":"bash""#));
    assert!(all.contains(r#""stop_reason":"tool_use""#));
}

#[test]
fn stream_translator_text_only() {
    let mut translator = StreamTranslator::new("test-model".to_string());

    let chunk1 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
    let chunk2 = r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
    let chunk3 = r#"{"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#;
    let chunk4 = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;

    let mut all_events = Vec::new();
    all_events.extend(translator.process_line(chunk1));
    all_events.extend(translator.process_line(chunk2));
    all_events.extend(translator.process_line(chunk3));
    all_events.extend(translator.process_line(chunk4));

    let output = all_events.join("");
    assert!(output.contains("event: message_start"));
    assert!(output.contains("event: content_block_start"));
    assert!(output.contains("\"text_delta\""));
    assert!(output.contains("Hello"));
    assert!(output.contains(" world"));
    assert!(output.contains("event: content_block_stop"));
    assert!(output.contains("event: message_delta"));
    assert!(output.contains("event: message_stop"));
}

#[test]
fn stream_translator_tool_calls() {
    let mut translator = StreamTranslator::new("test-model".to_string());

    let chunk1 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
    let chunk2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}"#;
    let chunk3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"co"}}]},"finish_reason":null}]}"#;
    let chunk4 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"mmand\"}"}}]},"finish_reason":null}]}"#;
    let chunk5 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

    let mut all_events = Vec::new();
    all_events.extend(translator.process_line(chunk1));
    all_events.extend(translator.process_line(chunk2));
    all_events.extend(translator.process_line(chunk3));
    all_events.extend(translator.process_line(chunk4));
    all_events.extend(translator.process_line(chunk5));

    let output = all_events.join("");
    assert!(output.contains("event: message_start"));
    assert!(output.contains("\"tool_use\""));
    assert!(output.contains("bash"));
    assert!(output.contains("\"input_json_delta\""));
    assert!(output.contains("\"stop_reason\":\"tool_use\""));
    assert!(output.contains("event: message_stop"));
}
#[test]
fn reasoning_envelope_round_trips_through_the_client() {
    let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
    let sse = drive(
        &mut t,
        &[
            (
                "response.reasoning_summary_text.delta",
                r#"{"delta":"weighing it"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC_BLOB"}}"#,
            ),
        ],
    );
    assert!(sse.contains(r#""thinking":"weighing it""#));
    let signature = signature_from_stream(&sse).expect("signature delta emitted");

    // Next turn: the client echoes that thinking block back verbatim.
    let request = json!({
        "model": "claude-codex-5.6",
        "max_tokens": 100,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "weighing it", "signature": signature},
                {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "ok"}
            ]}
        ]
    });
    let out = anthropic_to_openai_responses_request(&request, true).unwrap();
    let input = out["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "reasoning");
    assert_eq!(input[0]["id"], "rs_1");
    assert_eq!(input[0]["encrypted_content"], "ENC_BLOB");
    assert_eq!(
        input[0]["summary"],
        json!([{ "type": "summary_text", "text": "weighing it" }])
    );
    // Reasoning has to stay ahead of the call it preceded.
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_1");
}

/// With reasoning summaries disabled no thinking block is ever opened by a
/// summary delta, so the item's only carrier is a signature-only block.
#[test]
fn reasoning_envelope_survives_when_summaries_are_disabled() {
    let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
    let sse = drive(
        &mut t,
        &[(
            "response.output_item.done",
            r#"{"item":{"type":"reasoning","id":"rs_2","summary":[],"encrypted_content":"ENC_2"}}"#,
        )],
    );
    assert!(sse.contains(r#""type":"thinking","thinking":"""#));
    let signature = signature_from_stream(&sse).expect("signature emitted without summary");

    let request = json!({
        "model": "claude-codex-5.6",
        "max_tokens": 100,
        "messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": signature}
        ]}]
    });
    let out = anthropic_to_openai_responses_request(&request, true).unwrap();
    assert_eq!(out["input"][0]["type"], "reasoning");
    assert_eq!(out["input"][0]["encrypted_content"], "ENC_2");
}

/// Zen discards reasoning server-side and the proxy strips it client-side
/// next turn, so on spark a reasoning item with no visible summary text
/// must not emit a thinking block holding nothing but the opaque envelope:
/// it renders as an empty thinking box and churns the prefix the provider
/// never sees. The turn still terminates cleanly.
#[test]
fn spark_silent_reasoning_item_emits_no_thinking_block() {
    let mut t = StreamTranslator::new("muse-spark-1.3-contributor-free".to_string());
    let sse = drive(
        &mut t,
        &[
            (
                "response.output_item.added",
                r#"{"item":{"type":"reasoning","id":"rs_s","summary":[]}}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_s","summary":[],"encrypted_content":"ENC_S"}}"#,
            ),
            ("response.output_text.delta", r#"{"delta":"the answer"}"#),
            (
                "response.completed",
                r#"{"response":{"usage":{"input_tokens":5,"output_tokens":9}}}"#,
            ),
        ],
    );
    assert!(
        !sse.contains(r#""type":"thinking""#),
        "silent spark reasoning leaked a thinking block: {sse}"
    );
    assert!(
        signature_from_stream(&sse).is_none(),
        "silent spark reasoning minted an unreplayable signature"
    );
    assert!(sse.contains(r#""text":"the answer""#));
    assert!(sse.contains(r#""stop_reason":"end_turn""#));
    assert!(sse.contains("message_stop"));
}

/// The skip is per reasoning item, not per turn: an item whose summaries
/// did stream keeps its signature block even on spark, while a later
/// silent item is still dropped.
#[test]
fn spark_reasoning_with_visible_summaries_keeps_its_block() {
    let mut t = StreamTranslator::new("muse-spark-1.3-contributor-free".to_string());
    let sse = drive(
        &mut t,
        &[
            (
                "response.reasoning_summary_text.delta",
                r#"{"delta":"weighing it"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_v","summary":[],"encrypted_content":"ENC_V"}}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_q","summary":[],"encrypted_content":"ENC_Q"}}"#,
            ),
        ],
    );
    assert!(sse.contains(r#""thinking":"weighing it""#));
    assert!(
        signature_from_stream(&sse).is_some(),
        "visible thinking lost its signature block"
    );
    assert_eq!(
        sse.matches(r#""type":"thinking","thinking":"""#).count(),
        1,
        "the silent second item must not open another thinking block: {sse}"
    );
}

/// A turn cut off at the token ceiling must say so, even when it completed
/// and even when it had started a tool call.
#[test]
fn reasoning_does_not_leak_across_models() {
    let mut first = StreamTranslator::new("model-a".to_string());
    let sse = drive(
        &mut first,
        &[(
            "response.output_item.done",
            r#"{"item":{"type":"reasoning","id":"rs_a","summary":[],"encrypted_content":"ENC_A"}}"#,
        )],
    );
    let signature = signature_from_stream(&sse).unwrap();

    // A turn on another model that does not echo the block gets nothing.
    let clean = json!({
        "model": "model-b",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "next"}]
    });
    let out = anthropic_to_openai_responses_request(&clean, true).unwrap();
    assert!(
        !out["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "reasoning")
    );

    // And what is echoed is carried by the request itself, not a cache.
    let echoed = json!({
        "model": "model-a",
        "max_tokens": 100,
        "messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": signature}
        ]}]
    });
    let out = anthropic_to_openai_responses_request(&echoed, true).unwrap();
    assert_eq!(out["input"][0]["encrypted_content"], "ENC_A");
}

#[test]
fn reasoning_item_id_and_blob_may_arrive_on_separate_events() {
    // `added` announces the id, `done` carries the blob; neither event is
    // complete on its own but the pair is.
    let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
    let sse = drive(
        &mut t,
        &[
            (
                "response.output_item.added",
                r#"{"item":{"type":"reasoning","id":"rs_split","summary":[]}}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","summary":[],"encrypted_content":"ENC_SPLIT"}}"#,
            ),
        ],
    );
    let signature = signature_from_stream(&sse).expect("signature from the merged pair");
    let request = json!({
        "model": "claude-codex-5.6",
        "max_tokens": 100,
        "messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": signature}
        ]}]
    });
    let out = anthropic_to_openai_responses_request(&request, true).unwrap();
    assert_eq!(out["input"][0]["id"], "rs_split");
    assert_eq!(out["input"][0]["encrypted_content"], "ENC_SPLIT");
}
#[test]
fn stream_end_emits_one_joinable_missing_quota_event() {
    use tracing_subscriber::layer::SubscriberExt;

    // Non-Zen model: the warn still fires (joinable single event).
    let (translator, _logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
    let capture = EventCapture::default();
    let lines = capture.0.clone();
    let subscriber = tracing_subscriber::registry().with(capture);
    tracing::subscriber::with_default(subscriber, || {
        let mut translator =
            translator.with_codex_limits(crate::codex_rate_limits::CodexRateLimitStore::new());
        translator.finish_rate_limit_observation();
        translator.finish_rate_limit_observation();
    });

    let joined = lines.lock().unwrap().join("\n");
    let missing: Vec<_> = joined
        .lines()
        .filter(|line| line.contains("codex_rate_limits_missing"))
        .collect();
    assert_eq!(missing.len(), 1, "{joined}");
    assert!(missing[0].contains("request_id=req-test"), "{joined}");
}

#[test]
fn zen_model_missing_quota_stays_debug_not_warn() {
    use tracing_subscriber::layer::SubscriberExt;

    // Zen never sends quota: the event fires at debug so the log
    // stays clean on every routed turn.
    let (translator, _logger, _cost) = translator_with_outcome("claude-muse-spark-1.3", 0);
    let capture = EventCapture::default();
    let lines = capture.0.clone();
    let subscriber = tracing_subscriber::registry().with(capture);
    tracing::subscriber::with_default(subscriber, || {
        let mut translator =
            translator.with_codex_limits(crate::codex_rate_limits::CodexRateLimitStore::new());
        translator.finish_rate_limit_observation();
    });

    let joined = lines.lock().unwrap().join("\n");
    let missing: Vec<_> = joined
        .lines()
        .filter(|line| line.contains("codex_rate_limits_missing"))
        .collect();
    assert_eq!(missing.len(), 1, "{joined}");
    assert!(
        missing[0].contains("expected on Zen"),
        "zen missing-quota must be debug: {joined}"
    );
}

#[test]
fn observed_stream_quota_suppresses_the_missing_event() {
    use tracing_subscriber::layer::SubscriberExt;

    let (translator, _logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
    let capture = EventCapture::default();
    let lines = capture.0.clone();
    let subscriber = tracing_subscriber::registry().with(capture);
    tracing::subscriber::with_default(subscriber, || {
        let mut translator =
            translator.with_codex_limits(crate::codex_rate_limits::CodexRateLimitStore::new());
        translator.process_frame(
            Some("response.created"),
            &json!({"rate_limits": {"primary": {"used_percent": 4}}}).to_string(),
        );
        translator.finish_rate_limit_observation();
    });

    let joined = lines.lock().unwrap().join("\n");
    assert!(!joined.contains("codex_rate_limits_missing"), "{joined}");
}

/// The gap this closes: routed traffic used to reach no tracker at all, so
/// codex spend was invisible in /stats and the dashboard.
// Async because a saving > 0 reaches `record_savings_ledger`, which pushes
// the flocked disk append onto a blocking thread.
#[tokio::test]
async fn completed_responses_stream_books_a_request_outcome() {
    let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 400);
    t.process_frame(
        Some("response.completed"),
        &json!({
            "response": {
                "usage": {
                    "input_tokens": 10_000,
                    "output_tokens": 250,
                    "input_tokens_details": {"cached_tokens": 9_000}
                }
            }
        })
        .to_string(),
    );

    let entries = logger.get_recent(10);
    assert_eq!(entries.len(), 1, "the turn should be booked exactly once");
    let e = &entries[0];
    assert_eq!(e.model, "gpt-5.6-luna");
    assert_eq!(e.provider, "openai_responses");
    assert_eq!(e.output_tokens, 250);
    // Forwarded size is what upstream counted; the original is that plus
    // what the transforms removed.
    assert_eq!(e.input_tokens_optimized, 10_000);
    assert_eq!(e.input_tokens_original, 10_400);
    assert_eq!(e.tokens_saved, 400);
    assert!(e.cache_hit, "9k of 10k input tokens were served from cache");
    assert_eq!(e.transforms_applied, vec!["ctx_offload".to_string()]);
}

/// A stream carrying both a terminal event and a trailing `[DONE]`, plus
/// the drop at the end, must still book exactly one turn.
#[test]
fn a_turn_is_booked_only_once() {
    let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
    t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 1}}}).to_string(),
    );
    t.process_frame(None, "[DONE]");
    drop(t);
    assert_eq!(logger.get_recent(10).len(), 1);
}

/// A deferred turn records first-round usage for the completion guard but
/// never books: not at the terminal event, not at drop. The guard owns
/// the single booking, with continuation rounds folded in.
#[test]
fn deferred_ccr_turn_records_without_booking() {
    let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
    let deferred = DeferredCcrBooking::new();
    t = t.with_deferred_ccr(deferred.clone());
    t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 100, "output_tokens": 20}}}).to_string(),
    );
    assert_eq!(
        logger.get_recent(10).len(),
        0,
        "booking waits for the completion guard"
    );
    let first = deferred.first.lock().unwrap().clone();
    assert_eq!(
        first
            .usage
            .as_ref()
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_i64()),
        Some(100),
        "first-round usage must survive for the guard: {first:?}"
    );
    assert_eq!(first.status_code, 200);
    drop(t);
    assert_eq!(
        logger.get_recent(10).len(),
        0,
        "drop must not book a deferred turn either"
    );
}

/// A turn cut off before any terminal event still spent tokens, and the
/// Claude path books those too.
#[test]
fn dropped_stream_still_books_the_turn() {
    let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
    t.process_frame(
        Some("response.output_text.delta"),
        &json!({"delta": "partial"}).to_string(),
    );
    assert_eq!(logger.get_recent(10).len(), 0, "not booked mid-stream");
    drop(t);
    assert_eq!(
        logger.get_recent(10).len(),
        1,
        "dropping the translator books the interrupted turn"
    );
}

/// A translated stream dropped before any usage arrived releases its parked
/// entry without the first-turn marker: two invented zeros would mark the
/// conversation seen, and its real first turn would then go uncounted.
#[test]
fn dropped_translated_stream_without_usage_leaves_the_first_turn_open() {
    use crate::cache_stabilization::usage_observer::UsageObserver;
    let obs = std::sync::Arc::new(UsageObserver::new());
    obs.begin_request("req-test", "conv-spark".into(), None, None, None);
    let (mut t, _logger, _cost) = translator_with_outcome("muse-spark-1.3", 0);
    t.outcome.as_mut().unwrap().usage_observer = Some(obs.clone());
    t.process_frame(
        Some("response.output_text.delta"),
        &json!({"delta": "partial"}).to_string(),
    );
    drop(t);

    obs.begin_request("req-2", "conv-spark".into(), None, None, None);
    obs.complete_translated_first_turn("req-2", 0, 50_000);
    assert_eq!(
        obs.snapshot().first_turn_writes_total,
        1,
        "the first turn with usage is still the conversation's first"
    );
}

/// `response.failed` routes to `record_failed`, which deliberately skips
/// the success funnel — a failed turn must not inflate the save rate.
#[test]
fn failed_response_is_not_logged_as_a_served_request() {
    let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 100);
    t.process_frame(
        Some("response.failed"),
        &json!({"response": {"error": {"message": "boom"}}}).to_string(),
    );
    assert_eq!(logger.get_recent(10).len(), 0);
}

/// Chat Completions delivers usage on its own chunk rather than a terminal
/// event, so the numbers have to survive until the stream ends.
#[test]
fn chat_completions_usage_is_booked_at_stream_end() {
    let (mut t, logger, _cost) = translator_with_outcome("qwen-local", 0);
    t.process_frame(
        None,
        &json!({
            "choices": [{"delta": {"content": "hi"}}],
            "usage": {"prompt_tokens": 700, "completion_tokens": 20,
                      "prompt_tokens_details": {"cached_tokens": 500}}
        })
        .to_string(),
    );
    t.process_frame(None, "[DONE]");

    let entries = logger.get_recent(10);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].input_tokens_optimized, 700);
    assert_eq!(entries[0].output_tokens, 20);
}

/// Provider-wins parity with the buffered arms: text and tool deltas
/// before the usage chunk must not inflate the booked output past what
/// the provider reported. The translator's running count survives only
/// for provider-silent turns.
#[test]
fn booked_output_trusts_provider_over_translator_count() {
    let (mut t, logger, _cost) = translator_with_outcome("qwen-local", 0);
    for frame in [
        json!({"choices": [{"delta": {"content": "VISIBLE_PREFIX"}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1"}]}}]}),
        json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 500, "completion_tokens": 9}
        }),
    ] {
        t.process_frame(None, &frame.to_string());
    }
    t.process_frame(None, "[DONE]");

    let entries = logger.get_recent(10);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].input_tokens_optimized, 500);
    assert_eq!(
        entries[0].output_tokens, 9,
        "provider-reported 9, not the translator count: {entries:?}"
    );
}

/// Unit tests elsewhere in this file build translators with no outcome
/// context; that must stay a no-op rather than panicking on drop.
fn redirect_savings_ledger() {
    static LEDGER: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let path = LEDGER.get_or_init(|| {
        let dir = std::mem::ManuallyDrop::new(tempfile::tempdir().expect("tempdir"));
        dir.path().join("savings_events.jsonl")
    });
    unsafe { std::env::set_var("HEADROOM_SAVINGS_EVENTS_PATH", path) };
}

/// Same isolation as [`redirect_savings_ledger`] for the lifetime state
/// file: a `None`-path tracker resolves the developer's live
/// `~/.headroom/proxy_savings.json`, and every booking test would append
/// test turns to real lifetime totals. Must run before any
/// `SavingsTracker::new(None, …)` in this process.
fn redirect_savings_tracker() {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let path = DIR.get_or_init(|| {
        let dir = std::mem::ManuallyDrop::new(tempfile::tempdir().expect("tempdir"));
        dir.path().join("proxy_savings.json")
    });
    unsafe { std::env::set_var("HEADROOM_SAVINGS_PATH", path) };
}

fn signature_from_stream(sse: &str) -> Option<String> {
    for line in sse.lines() {
        // Skip the `event:` and blank lines that frame each SSE record.
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if event["delta"]["type"] == "signature_delta" {
            return event["delta"]["signature"].as_str().map(String::from);
        }
    }
    None
}

/// The bug behind "Muse Spark randomly stops": a reasoning turn arrived as
/// a reasoning item plus a finished message item, with no
/// `output_text.delta` for the message. We translated the reasoning into a
/// thinking block and dropped the message, so the client got a turn made of
/// a thought and nothing else -- which Claude Code renders as a stopped
/// turn and answers with "your previous response had no visible output".
#[test]
fn a_message_item_that_never_streamed_deltas_still_reaches_the_client() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(
        t.process_frame(
            Some("response.output_item.done"),
            &json!({
                "item": {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "the answer is 391"}]
                }
            })
            .to_string(),
        ),
    );
    out.extend(t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 9}}}).to_string(),
    ));

    let joined = out.join("");
    assert!(
        joined.contains("the answer is 391"),
        "the only copy of the answer was dropped: {joined}"
    );
    assert!(
        joined.contains("\"type\":\"text\""),
        "it has to arrive as a text block, not a thought: {joined}"
    );
}

/// The other half: when deltas did arrive, the `done` event is a summary of
/// what the client already has and must not be replayed on top of it.
#[test]
fn a_message_item_that_streamed_is_not_sent_twice() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.output_text.delta"),
        &json!({"delta": "the answer is 391"}).to_string(),
    ));
    out.extend(
        t.process_frame(
            Some("response.output_item.done"),
            &json!({
                "item": {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "the answer is 391"}]
                }
            })
            .to_string(),
        ),
    );

    let joined = out.join("");
    assert_eq!(
        joined.matches("the answer is 391").count(),
        1,
        "the streamed text came through twice: {joined}"
    );
}

/// Two messages in one response are judged separately: the second having
/// no deltas must not be silenced by the first having had some.
#[test]
fn each_message_item_is_judged_on_its_own_deltas() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.output_text.delta"),
        &json!({"delta": "first"}).to_string(),
    ));
    let done = json!({
        "item": {"type": "message", "content": [{"type": "output_text", "text": "first"}]}
    })
    .to_string();
    out.extend(t.process_frame(Some("response.output_item.done"), &done));
    out.extend(
        t.process_frame(
            Some("response.output_item.done"),
            &json!({
                "item": {"type": "message", "content": [{"type": "output_text", "text": "second"}]}
            })
            .to_string(),
        ),
    );

    let joined = out.join("");
    assert_eq!(joined.matches("first").count(), 1, "{joined}");
    assert!(joined.contains("second"), "{joined}");
}

/// A failed turn still has to end on the wire. Before, `response.failed`
/// booked the outcome but emitted no terminal events, and the `[DONE]`
/// fallback could not rescue it (the block was already closed, which is
/// the fallback's trigger) — so the client hung.
///
/// The first fix emitted `end_turn` + `message_stop`, which traded the
/// hang for a lie: a provider failure arrived indistinguishable from a
/// voluntary stop, and the client accepted it instead of retrying. Now a
/// failure ends loud, as an in-band `api_error` with no `message_stop` —
/// the finisher treats the error as the end of the message, and the
/// client surfaces it and retries the turn.
#[test]
fn a_failed_turn_still_terminates() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.output_text.delta"),
        &json!({"delta": "partial"}).to_string(),
    ));
    out.extend(t.process_frame(
        Some("response.failed"),
        &json!({"response": {"error": {"message": "boom"}}}).to_string(),
    ));
    out.extend(t.process_frame(None, "[DONE]"));

    let joined = out.join("");
    assert!(
        joined.contains("\"type\":\"api_error\""),
        "failed turn must end loud so the client retries: {joined}"
    );
    assert!(
        joined.contains("boom"),
        "the provider's error message must reach the client: {joined}"
    );
    assert!(
        !joined.contains("message_stop"),
        "an errored turn must not also close clean: {joined}"
    );
    assert!(
        !joined.contains("end_turn"),
        "a failed turn must not masquerade as a voluntary stop: {joined}"
    );
    assert_eq!(
        joined.matches("event: error").count(),
        1,
        "the [DONE] fallback must not double-terminate: {joined}"
    );
}

/// A second terminal for one turn is provider noise, not a second turn:
/// `failed` after `completed` must not error a turn that already closed
/// clean, and `completed` after `failed` must not re-close an errored one.
#[test]
fn a_second_terminal_event_is_swallowed() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 9}}}).to_string(),
    ));
    out.extend(t.process_frame(
        Some("response.failed"),
        &json!({"response": {}}).to_string(),
    ));

    let joined = out.join("");
    assert!(
        joined.contains("message_stop"),
        "the clean close must stand: {joined}"
    );
    assert!(
        !joined.contains("api_error"),
        "a straggler failed must not error a completed turn: {joined}"
    );

    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.failed"),
        &json!({"response": {}}).to_string(),
    ));
    out.extend(t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 9}}}).to_string(),
    ));

    let joined = out.join("");
    assert_eq!(
        joined.matches("event: error").count(),
        1,
        "the error must stand alone: {joined}"
    );
    assert!(
        !joined.contains("message_stop"),
        "a straggler completed must not re-close an errored turn: {joined}"
    );
}

/// `response.incomplete` without a reason still ended the turn upstream;
/// the client must see terminal events, not silence plus an outcome.
#[test]
fn an_incomplete_turn_without_a_reason_still_terminates() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.incomplete"),
        &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 9}}}).to_string(),
    ));

    let joined = out.join("");
    assert!(
        joined.contains("message_stop"),
        "reason-less incomplete left the client hanging: {joined}"
    );
}

/// A stream ending mid-frame — no trailing blank line, no trailing
/// newline — must still deliver its last event. Before, the adapter only
/// dispatched on blank lines, so a cut-off `response.completed` took the
/// usage, the stop reason, and the client's `message_stop` with it.
#[tokio::test]
async fn trailing_frame_without_terminator_is_flushed() {
    use futures_util::{StreamExt, stream};
    redirect_savings_ledger();
    let sse = concat!(
        "event: response.created\n",
        "data: {}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"hi\"}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}",
    );
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![Ok(bytes::Bytes::from(sse))];
    let translated = translate_openai_stream_to_anthropic(
        stream::iter(chunks),
        "muse-spark-1.3".to_string(),
        crate::codex_rate_limits::CodexRateLimitStore::new(),
        false,
        None,
        None,
        None,
        Some("test".to_string()),
        false,
    );
    let out: Vec<String> = translated
        .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
        .collect()
        .await;
    let joined = out.join("");
    assert!(joined.contains("hi"), "delta lost: {joined}");
    assert!(
        joined.contains("event: message_stop"),
        "unterminated completed dropped the turn end: {joined}"
    );
}

/// A reqwest stream that has yielded an error yields that same error on
/// every later poll. The fold used to close the turn and keep the stream
/// alive, so it re-closed and re-logged as fast as the executor allowed:
/// on 2026-09-10 that wrote a 59 GB log at 55 MB/s and grew the proxy by
/// 3.2 GB a minute until the machine's OOM killer took it. The upstream
/// must not be polled again once it has failed.
#[tokio::test]
async fn a_failed_upstream_is_never_polled_again() {
    use futures_util::StreamExt;
    redirect_savings_ledger();
    // A real `reqwest::Error`, made without touching the network.
    let err = reqwest::Client::new()
        .get("http://")
        .send()
        .await
        .expect_err("no host in the url");
    let started = concat!(
        "event: response.created\n",
        "data: {}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"hi\"}\n",
        "\n",
    );
    let mut step = 0;
    let mut err = Some(err);
    let upstream = futures_util::stream::poll_fn(move |_| {
        step += 1;
        std::task::Poll::Ready(match step {
            1 => Some(Ok(bytes::Bytes::from(started))),
            2 => Some(Err(err.take().expect("one error"))),
            _ => panic!("upstream polled after it failed"),
        })
    });
    let translated = translate_openai_stream_to_anthropic(
        Box::pin(upstream),
        "muse-spark-1.3".to_string(),
        crate::codex_rate_limits::CodexRateLimitStore::new(),
        false,
        None,
        None,
        None,
        Some("test".to_string()),
        false,
    );
    let items: Vec<Result<bytes::Bytes, std::io::Error>> = translated.collect().await;
    // The upstream error follows the abort events, so the finisher can
    // log why the turn died instead of "body ended early".
    let (last, events) = items.split_last().expect("stream yielded nothing");
    let err = last
        .as_ref()
        .expect_err("the upstream error must follow the abort");
    assert!(
        format!("{err:?}").contains("reqwest"),
        "source chain lost: {err:?}"
    );
    let out: Vec<String> = events
        .iter()
        .map(|r| String::from_utf8(r.as_ref().unwrap().to_vec()).unwrap())
        .collect();
    let joined = out.join("");
    assert!(joined.contains("hi"), "delta lost: {joined}");
    // The translator leaves the turn unstopped on purpose: the stop
    // (and the truncation marker) is `finish_on_drop`'s job downstream.
    // A `message_stop` here would read as a clean finish with no marker.
    assert!(
        !joined.contains("event: message_stop"),
        "abort stopped the turn; the finisher would read it as clean: {joined}"
    );
    assert!(
        joined.contains("end_turn"),
        "abort must downgrade to end_turn: {joined}"
    );
    // And the finisher does close it, exactly once, marked truncated.
    let closed: Vec<String> = crate::sse::stream_finisher::finish_on_drop(
        futures_util::stream::iter(
            out.into_iter()
                .map(|s| Ok::<_, std::io::Error>(bytes::Bytes::from(s)))
                .chain(std::iter::once(Err(std::io::Error::other("reset")))),
        ),
        "test".to_string(),
    )
    .map(|r| {
        String::from_utf8(
            r.expect("finisher must not error on an aborted turn")
                .to_vec(),
        )
        .unwrap()
    })
    .collect()
    .await;
    let shut = closed.join("");
    assert!(
        shut.contains("dropped mid-response"),
        "no truncation marker; the turn reads as finished: {shut}"
    );
    assert_eq!(
        shut.matches("event: message_stop").count(),
        1,
        "the turn must be closed exactly once: {shut}"
    );
}

/// A refusal is the turn's only text and must reach the client as text,
/// not vanish into an empty `end_turn`.
#[test]
fn a_refused_turn_reaches_the_client_as_text() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.refusal.delta"),
        &json!({"delta": "I cannot"}).to_string(),
    ));
    out.extend(t.process_frame(
        Some("response.refusal.done"),
        &json!({"refusal": "I cannot do that"}).to_string(),
    ));
    out.extend(t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 9}}}).to_string(),
    ));

    let joined = out.join("");
    assert!(
        joined.contains("I cannot do that") || joined.contains("I cannot"),
        "refusal never reached the client: {joined}"
    );
    assert_eq!(
        joined.matches("I cannot").count(),
        1,
        "refusal streamed twice: {joined}"
    );
}

/// `arguments.done` carries the whole arguments string: when no argument
/// delta arrived for the call, it is the only copy.
#[test]
fn a_tool_call_without_argument_deltas_uses_the_done_fallback() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.output_item.added"),
        &json!({"item": {"type": "function_call", "call_id": "c1", "name": "bash"}}).to_string(),
    ));
    out.extend(t.process_frame(
        Some("response.function_call_arguments.done"),
        &json!({"arguments": "{\"command\":\"ls\"}"}).to_string(),
    ));
    out.extend(t.process_frame(
        Some("response.output_item.done"),
        &json!({"item": {"type": "function_call", "call_id": "c1", "name": "bash"}}).to_string(),
    ));

    let joined = out.join("");
    assert!(
        joined.contains("ls"),
        "arguments lost when deltas never arrived: {joined}"
    );
}

/// A transport error after the turn started must leave the close to
/// `finish_on_drop` downstream: no `message_stop` here (a stopped turn
/// reads as cleanly finished, with no marker), and straggler frames
/// after the abort must not reopen it. Before anything started there is
/// no turn to close, so the abort is empty and the error propagates.
#[test]
fn an_aborted_turn_stays_open_for_the_finisher_and_stays_closed() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    assert!(t.abort_terminal().is_empty());
    t.process_frame(Some("response.created"), &json!({}).to_string());
    t.process_frame(
        Some("response.output_text.delta"),
        &json!({"delta": "partial"}).to_string(),
    );
    let term = t.abort_terminal().join("");
    assert!(
        term.contains("end_turn"),
        "half-streamed call must not run: {term}"
    );
    assert!(
        !term.contains("event: message_stop"),
        "abort stopped the turn; the finisher would read it as clean: {term}"
    );
    let after = t
        .process_frame(
            Some("response.output_text.delta"),
            &json!({"delta": "late"}).to_string(),
        )
        .join("");
    assert!(
        !after.contains("late"),
        "straggler reopened the turn: {after}"
    );
}

/// A half-streamed tool call must reach the finisher unclosed: a
/// `content_block_stop` here would look complete downstream and the call
/// could run on truncated input. The finisher withholds the partial
/// block and names it in the marker instead.
#[test]
fn an_aborted_tool_call_stays_open_for_the_finisher() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    t.process_frame(Some("response.created"), &json!({}).to_string());
    t.process_frame(
        Some("response.output_item.added"),
        &json!({"item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}})
            .to_string(),
    );
    t.process_frame(
        Some("response.function_call_arguments.delta"),
        &json!({"delta": "{\"comma"}).to_string(),
    );
    let term = t.abort_terminal().join("");
    assert!(
        term.contains("end_turn"),
        "half-streamed call must not run: {term}"
    );
    assert!(
        !term.contains("content_block_stop"),
        "abort closed the partial tool call; it would run truncated: {term}"
    );
    assert!(
        !term.contains("event: message_stop"),
        "abort stopped the turn; the finisher would read it as clean: {term}"
    );
}

/// End to end across the seam: translator abort frames through
/// `finish_on_drop` must come out marked truncated, ended `end_turn`,
/// stopped exactly once — with the partial tool input withheld, never
/// run. This is the contract the abort shape above exists for; it fails
/// if either side drifts (translator stopping the turn, finisher
/// releasing the partial call).
#[test]
fn aborted_routed_turn_closes_marked_through_the_finisher() {
    use futures_util::StreamExt;
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut frames: Vec<String> = Vec::new();
    frames.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    frames.extend(t.process_frame(
        Some("response.output_text.delta"),
        &json!({"delta": "working on it"}).to_string(),
    ));
    frames.extend(
        t.process_frame(
            Some("response.output_item.added"),
            &json!({"item": {"type": "function_call", "call_id": "call_9", "name": "Read"}})
                .to_string(),
        ),
    );
    frames.extend(t.process_frame(
        Some("response.function_call_arguments.delta"),
        &json!({"delta": "{\"path\""}).to_string(),
    ));
    frames.extend(t.abort_terminal());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = frames
        .into_iter()
        .map(|s| Ok(bytes::Bytes::from(s)))
        .collect();
    let out = rt.block_on(async {
        let mut acc = String::new();
        let mut s = Box::pin(crate::sse::stream_finisher::finish_on_drop(
            futures_util::stream::iter(chunks),
            "test".to_string(),
        ));
        while let Some(item) = s.next().await {
            let b = item.expect("finisher must not error on an aborted turn");
            acc.push_str(&String::from_utf8_lossy(&b));
        }
        acc
    });
    assert!(
        out.contains("did NOT run"),
        "no truncation marker; the turn reads as finished: {out}"
    );
    assert!(
        out.contains("`Read`"),
        "marker must name the discarded call so the model can re-issue it: {out}"
    );
    assert!(
        !out.contains("{\"path\""),
        "partial tool input reached the client and could run truncated: {out}"
    );
    assert!(
        out.contains("\"stop_reason\":\"end_turn\""),
        "turn must end end_turn, never tool_use: {out}"
    );
    assert_eq!(
        out.matches("event: message_stop").count(),
        1,
        "turn must stop exactly once: {out}"
    );
}

/// `output_text.done` carries the whole text: when no delta arrived for
/// the item, it is the only copy.
#[test]
fn a_text_part_without_deltas_uses_the_done_fallback() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.output_text.done"),
        &json!({"text": "the answer is 391"}).to_string(),
    ));
    out.extend(
        t.process_frame(
            Some("response.output_item.done"),
            &json!({
                "item": {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "the answer is 391"}]
                }
            })
            .to_string(),
        ),
    );

    let joined = out.join("");
    assert_eq!(
        joined.matches("the answer is 391").count(),
        1,
        "text lost or doubled without deltas: {joined}"
    );
}

/// An untranslated event is counted per name, so the log line can say how
/// often it happened, and the table stops growing at its cap.
#[test]
fn unhandled_events_are_counted_per_name_and_the_table_is_bounded() {
    assert_eq!(count_unhandled_event("response.test_a.first"), 1);
    assert_eq!(count_unhandled_event("response.test_a.first"), 2);
    assert_eq!(count_unhandled_event("response.test_a.second"), 1);
    for i in 0..UNHANDLED_NAMES {
        count_unhandled_event(&format!("response.test_flood.{i}"));
    }
    assert_eq!(count_unhandled_event("response.test_never_seen"), u64::MAX);
    assert_eq!(count_unhandled_event("response.test_a.first"), 3);
}
/// A logged shape keeps every field name but not the text behind it.
#[test]
fn an_event_shape_keeps_fields_and_drops_text() {
    let long = "x".repeat(500);
    let chunk = json!({
        "item": {"type": "message", "content": [{"text": long}, {"text": "second"}]},
        "usage": {"input_tokens": 7, "details": {"cached": 3}},
    });
    assert_eq!(
        shape_of(&chunk),
        json!({
            "item": {"type": "message", "content": [{"text": format!("{}...", "x".repeat(SHAPE_STRING))}]},
            "usage": {"input_tokens": 7, "details": {"cached": 3}},
        })
    );
    assert!(first_sighting("response.test_shape/once"));
    assert!(!first_sighting("response.test_shape/once"));
}

fn terminal_delta_usage(output: &str) -> serde_json::Value {
    let (_, data) = output
        .split("event: message_delta")
        .nth(1)
        .and_then(|rest| rest.split_once("data: "))
        .expect("no message_delta in output");
    let payload: serde_json::Value =
        serde_json::from_str(data.lines().next().unwrap_or("")).expect("delta data parses");
    payload.get("usage").cloned().expect("delta has usage")
}

/// Without the flag a completed turn reports only what the translator
/// counted itself: output tokens, no input.
#[test]
fn terminal_delta_keeps_legacy_shape_by_default() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 500, "output_tokens": 42, "input_tokens_details": {"cached_tokens": 300}}}})
            .to_string(),
    ));

    assert_eq!(
        terminal_delta_usage(&out.join("")),
        json!({"output_tokens": 42})
    );
}

/// With the flag the provider's own counts go out in Anthropic's key
/// vocabulary, including cached tokens. OpenAI's input count includes the
/// cached ones; Anthropic's excludes them, so 500 with 300 cached is 200.
#[test]
fn terminal_delta_reports_provider_truth_when_flagged() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string()).with_report_true_usage(true);
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"input_tokens": 500, "output_tokens": 42, "input_tokens_details": {"cached_tokens": 300}}}})
            .to_string(),
    ));

    assert_eq!(
        terminal_delta_usage(&out.join("")),
        json!({"input_tokens": 200, "output_tokens": 42, "cache_read_input_tokens": 300})
    );
}

/// An incomplete turn reports truth too: usage was already extracted
/// before the delta went out.
#[test]
fn incomplete_delta_reports_provider_truth_when_flagged() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string()).with_report_true_usage(true);
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.incomplete"),
        &json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 500, "output_tokens": 42}}})
            .to_string(),
    ));

    assert_eq!(
        terminal_delta_usage(&out.join("")),
        json!({"input_tokens": 500, "output_tokens": 42, "cache_read_input_tokens": 0})
    );
}

/// Chat Completions truth arrives under its own key names and is stored
/// on `last_usage`; the `finish_reason` delta maps them to Anthropic's.
#[test]
fn chat_finish_reason_delta_maps_truth_when_flagged() {
    let mut t = StreamTranslator::new("codex-model".to_string()).with_report_true_usage(true);
    let mut out = Vec::new();
    out.extend(t.process_frame(
        None,
        &json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": null}]}).to_string(),
    ));
    out.extend(t.process_frame(
        None,
        &json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 700, "completion_tokens": 12, "prompt_tokens_details": {"cached_tokens": 650}}})
            .to_string(),
    ));

    assert_eq!(
        terminal_delta_usage(&out.join("")),
        json!({"input_tokens": 50, "output_tokens": 12, "cache_read_input_tokens": 650})
    );
}

/// A Chat turn whose usage arrives only with `[DONE]` keeps the legacy
/// shape: `finish_reason` already closed the turn without truth.
#[test]
fn chat_done_after_finish_reason_keeps_legacy_shape() {
    let mut t = StreamTranslator::new("codex-model".to_string()).with_report_true_usage(true);
    let mut out = Vec::new();
    out.extend(t.process_frame(
        None,
        &json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}).to_string(),
    ));
    // Usage arrives with no turn left to carry it: the `[DONE]` is a
    // no-op and `last_usage` stays empty.
    out.extend(t.process_frame(
        None,
        &json!({"usage": {"prompt_tokens": 700, "completion_tokens": 12}}).to_string(),
    ));
    out.extend(t.process_frame(None, "[DONE]"));

    assert_eq!(
        terminal_delta_usage(&out.join("")),
        json!({"output_tokens": 0})
    );
}

/// A Chat turn that ends at `[DONE]` with an open block reports the
/// stored truth, since no `finish_reason` closed it first.
#[test]
fn chat_done_with_open_block_reports_truth_when_flagged() {
    let mut t = StreamTranslator::new("codex-model".to_string()).with_report_true_usage(true);
    let mut out = Vec::new();
    out.extend(t.process_frame(
        None,
        &json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": null}], "usage": {"prompt_tokens": 700, "completion_tokens": 12}})
            .to_string(),
    ));
    out.extend(t.process_frame(None, "[DONE]"));

    assert_eq!(
        terminal_delta_usage(&out.join("")),
        json!({"input_tokens": 700, "output_tokens": 12, "cache_read_input_tokens": 0})
    );
}

/// A truth block missing a count falls back to the legacy shape rather
/// than telling the client a zero it would trust.
#[test]
fn partial_truth_falls_back_to_legacy_shape() {
    let mut t = StreamTranslator::new("muse-spark-1.3".to_string()).with_report_true_usage(true);
    let mut out = Vec::new();
    out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
    out.extend(t.process_frame(
        Some("response.completed"),
        &json!({"response": {"usage": {"output_tokens": 42}}}).to_string(),
    ));

    assert_eq!(
        terminal_delta_usage(&out.join("")),
        json!({"output_tokens": 42})
    );
}

//! Buffered (non-streaming) OpenAI → Anthropic response translation.
//!
//! Both endpoints land here: `responses_stream_to_turn` folds a Responses API
//! SSE transcript back into a single turn, and `openai_to_anthropic_response`
//! reshapes a Chat Completions body into an Anthropic message.

use serde_json::{json, Value};

/// Fold a Responses SSE stream into the buffered `output[]` turn the rest of
/// the CCR machinery speaks.
///
/// Tool calls ride this stream as `output[]` items and never as text deltas, so
/// a reader that accumulates only `output_text` sees none of them — which is
/// how every call on this path, `headroom_retrieve` included, used to vanish.
///
/// The fold runs through the shared [`crate::sse::openai_responses::ResponseState`]
/// machine, so it understands both the `response.`-prefixed event names
/// (`response.output_item.done`, …) and the bare form (`output_item.done`, …)
/// that Codex and compatible gateways emit, plus the incremental
/// `function_call_arguments.delta/done` frames a call can stream as instead of
/// arriving whole in `output_item.done`. Returns the turn and the output-token
/// count the outcome is booked with.
pub(crate) fn responses_stream_to_turn(responses_text: &str) -> (Value, u64) {
    use crate::sse::{openai_responses::ResponseState, SseFramer};

    let mut framer = SseFramer::new();
    framer.push(responses_text.as_bytes());
    let mut state = ResponseState::new();
    // Text that arrived without an `item_id` (older fixtures and some
    // gateways send bare `{"delta": …}` frames). The state machine needs
    // `item_id` to attribute text, so these are carried globally exactly as
    // the previous fold did.
    let mut global_text = String::new();

    while let Some(ev) = framer.next_event() {
        let Ok(ev) = ev else {
            continue;
        };
        if ev.is_done_sentinel() {
            continue;
        }
        // Item_id-less text frames predate per-item attribution (bare
        // `{"delta": …}` fixtures); the state machine needs `item_id`, so
        // these are carried globally exactly as the previous fold did.
        // Peeked before apply because `output_text.done` without an item id
        // returns Ok from the machine (it ignores done payloads) yet must
        // still land in the global text.
        if matches!(
            ev.event_name.as_deref(),
            Some("response.output_text.delta") | Some("output_text.delta")
        ) {
            if let Ok(chunk) = serde_json::from_slice::<Value>(&ev.data) {
                if chunk.get("item_id").and_then(|v| v.as_str()).is_none() {
                    if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str()) {
                        global_text.push_str(delta);
                    }
                }
            }
        } else if matches!(
            ev.event_name.as_deref(),
            Some("response.output_text.done") | Some("output_text.done")
        ) {
            if let Ok(chunk) = serde_json::from_slice::<Value>(&ev.data) {
                if chunk.get("item_id").and_then(|v| v.as_str()).is_none() {
                    if let Some(text) = chunk
                        .get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| chunk.get("delta").and_then(|v| v.as_str()))
                    {
                        if global_text.is_empty() {
                            global_text.push_str(text);
                        }
                    }
                }
            }
        }
        let _ = state.apply(ev);
    }

    let (mut turn, output_tokens) = state.to_responses_turn();

    // Deltas and items are two views of one turn. A stream that sent its text
    // only as item_id-less deltas still needs it carried; one that already
    // sent a `message` item must not have it carried twice. Text leads the
    // calls it introduces, so it goes first.
    if !global_text.is_empty() {
        let has_message = turn
            .get("output")
            .and_then(|o| o.as_array())
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("message"))
            });
        if !has_message {
            if let Some(items) = turn.get_mut("output").and_then(|o| o.as_array_mut()) {
                items.insert(
                    0,
                    json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": global_text}],
                    }),
                );
            }
        }
    }

    (turn, output_tokens)
}

pub(crate) fn openai_to_anthropic_response(openai: &Value, original: &Value) -> Value {
    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let choice = openai
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first());

    let message = choice.and_then(|c| c.get("message"));

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|r| r.as_str())
        .unwrap_or("stop");

    let mut stop_reason = match finish_reason {
        "stop" => "end_turn",
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        _ => "end_turn",
    };

    let mut content: Vec<Value> = Vec::new();

    if let Some(msg) = message {
        // Handle reasoning_content (thinking tokens from models like Qwen).
        // Deliberately UNSIGNED: chat reasoning is bare text with no
        // replayable identity, and the sealed envelope needs a backend-issued
        // id + encrypted_content pair (see reasoning_signature). Sealing a
        // fabricated pair would replay an item the upstream never produced.
        // The client sees the reasoning once; next turn the request
        // translator drops it as foreign, so it never reaches an upstream.
        if let Some(reasoning) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
            if !reasoning.is_empty() {
                content.push(json!({"type": "thinking", "thinking": reasoning}));
            }
        }

        match msg.get("content") {
            Some(Value::String(text)) if !text.is_empty() => {
                content.push(json!({"type": "text", "text": text}));
            }
            // Some models return content as an array of parts rather than a
            // string; the string-only read above dropped those turns whole.
            Some(Value::Array(parts)) => {
                let text: String = parts
                    .iter()
                    .filter_map(|p| {
                        p.get("text")
                            .and_then(|t| t.as_str())
                            .or_else(|| p.get("refusal").and_then(|r| r.as_str()))
                    })
                    .collect();
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
            }
            _ => {}
        }

        // A chat-level refusal is the turn's only text; without this the
        // client receives an empty `end_turn` and cannot tell refusal apart
        // from silence.
        if let Some(refusal) = msg.get("refusal").and_then(|v| v.as_str()) {
            if !refusal.is_empty() {
                content.push(json!({"type": "text", "text": refusal}));
            }
        }

        if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                // A call without identity cannot round-trip: the client
                // discards the whole turn on it, so the broken call goes out
                // alone rather than taking any text with it.
                if id.is_empty() || name.is_empty() {
                    tracing::debug!(
                        event = "openai_tool_call_without_identity",
                        "dropped a tool call with no id or name instead of emitting an unplayable tool_use block"
                    );
                    continue;
                }
                let arguments = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let input: Value = serde_json::from_str(arguments).unwrap_or(json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input
                }));
            }
        }
    }

    // A promised tool call with no surviving block is what the client reports
    // as "the model's tool call could not be parsed", killing the turn. The
    // streamed path derives the stop reason from surviving content; do the
    // same here.
    if stop_reason == "tool_use"
        && !content
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
    {
        stop_reason = "end_turn";
    }

    let default_usage = json!({});
    let usage = openai.get("usage").unwrap_or(&default_usage);
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let raw_id = uuid::Uuid::new_v4().to_string().replace('-', "");
    let msg_id = format!("msg_{}", &raw_id[..raw_id.len().min(24)]);

    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": original_model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn responses_stream_keeps_tool_calls() {
        let stream = concat!(
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",",
            "\"name\":\"headroom_retrieve\",\"arguments\":\"{}\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_1\",\"usage\":",
            "{\"input_tokens\":10,\"output_tokens\":5}}}\n",
            "\n",
        );

        let (turn, output_tokens) = responses_stream_to_turn(stream);

        assert_eq!(output_tokens, 5);
        assert_eq!(turn["id"], "resp_1");
        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 1, "tool call was dropped: {turn}");
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["name"], "headroom_retrieve");
    }

    #[test]
    fn responses_stream_keeps_text_alongside_a_tool_call() {
        let stream = concat!(
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"looking that up\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",",
            "\"name\":\"headroom_retrieve\",\"arguments\":\"{}\"}}\n",
            "\n",
        );

        let (turn, _) = responses_stream_to_turn(stream);

        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 2, "text was dropped: {turn}");
        // Text leads the call it introduces.
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "looking that up");
        assert_eq!(items[1]["type"], "function_call");
    }

    #[test]
    fn responses_stream_does_not_duplicate_text_already_sent_as_an_item() {
        let stream = concat!(
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hello\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"message\",\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}}\n",
            "\n",
        );

        let (turn, _) = responses_stream_to_turn(stream);

        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 1, "text carried twice: {turn}");
    }

    /// Codex and compatible gateways emit the bare form (`output_item.done`)
    /// where OpenAI documents the `response.`-prefixed one. The old fold
    /// matched only the prefixed form, so a Codex continuation folded to an
    /// empty turn, the CCR loop fell back to splicing `<retrieved_context>`
    /// as final text, and the subagent stopped instead of continuing.
    #[test]
    fn responses_stream_keeps_bare_output_item_done() {
        let stream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_codex\"}}\n",
            "\n",
            "event: output_item.done\n",
            "data: {\"type\":\"output_item.done\",\"item\":{\"id\":\"fc_1\",",
            "\"type\":\"function_call\",\"call_id\":\"call_1\",",
            "\"name\":\"headroom_retrieve\",",
            "\"arguments\":\"{\\\"hash\\\":\\\"ea06bec713db19c6a40258ad\\\"}\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_codex\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
            "\n",
        );

        let (turn, output_tokens) = responses_stream_to_turn(stream);

        assert_eq!(output_tokens, 5);
        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 1, "bare Codex item was dropped: {turn}");
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["name"], "headroom_retrieve");
    }

    /// A call can stream as incremental `function_call_arguments` frames with
    /// no whole arguments in `output_item.done`. The old fold never read
    /// those frames, so the reassembled call lost its hash and CCR could not
    /// match the retrieval.
    #[test]
    fn responses_stream_reassembles_incremental_function_call_arguments() {
        let stream = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,",
            "\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",",
            "\"call_id\":\"call_1\",\"name\":\"headroom_retrieve\"}}\n",
            "\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",",
            "\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":\"{\\\"hash\\\":\"}}\n",
            "\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",",
            "\"item_id\":\"fc_1\",\"output_index\":0,",
            "\"delta\":\"\\\"ea06bec713db19c6a40258ad\\\"}\"}\n",
            "\n",
            "event: response.function_call_arguments.done\n",
            "data: {\"type\":\"response.function_call_arguments.done\",",
            "\"item_id\":\"fc_1\",\"output_index\":0,",
            "\"arguments\":\"{\\\"hash\\\":\\\"ea06bec713db19c6a40258ad\\\"}\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
            "\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",",
            "\"call_id\":\"call_1\",\"name\":\"headroom_retrieve\",",
            "\"arguments\":\"{\\\"hash\\\":\\\"ea06bec713db19c6a40258ad\\\"}\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
            "\n",
        );

        let (turn, _) = responses_stream_to_turn(stream);

        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 1, "incremental call was dropped: {turn}");
        assert_eq!(items[0]["type"], "function_call");
        let args = items[0]
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            args.contains("ea06bec713db19c6a40258ad"),
            "arguments not reassembled: {turn}"
        );
    }

    /// Bare incremental form end to end: undotted added/deltas/done plus a
    /// completed envelope with no `output[]` (Codex omits it when items
    /// already streamed). The accumulated items must survive.
    #[test]
    fn responses_stream_keeps_bare_incremental_items_without_completed_output() {
        let stream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_codex\"}}\n",
            "\n",
            "event: output_item.added\n",
            "data: {\"type\":\"output_item.added\",\"item\":{\"id\":\"fc_1\",",
            "\"type\":\"function_call\",\"call_id\":\"call_1\",",
            "\"name\":\"headroom_retrieve\"}}\n",
            "\n",
            "event: function_call_arguments.delta\n",
            "data: {\"type\":\"function_call_arguments.delta\",",
            "\"item_id\":\"fc_1\",\"delta\":\"{\\\"hash\\\":\"}}\n",
            "\n",
            "event: function_call_arguments.done\n",
            "data: {\"type\":\"function_call_arguments.done\",",
            "\"item_id\":\"fc_1\",",
            "\"arguments\":\"{\\\"hash\\\":\\\"ea06bec713db19c6a40258ad\\\"}\"}\n",
            "\n",
            "event: output_item.done\n",
            "data: {\"type\":\"output_item.done\",\"item\":{\"id\":\"fc_1\",",
            "\"type\":\"function_call\",\"call_id\":\"call_1\",",
            "\"name\":\"headroom_retrieve\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_codex\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
            "\n",
        );

        let (turn, _) = responses_stream_to_turn(stream);

        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 1, "bare incremental call was dropped: {turn}");
        assert_eq!(items[0]["type"], "function_call");
        let args = items[0]
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            args.contains("ea06bec713db19c6a40258ad"),
            "arguments not reassembled from bare deltas: {turn}"
        );
    }

    /// The completed envelope stays authoritative: when it carries
    /// `output[]`, that wins over the incrementally gathered items.
    #[test]
    fn responses_stream_prefers_completed_output() {
        let stream = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",",
            "\"item\":{\"id\":\"fc_stale\",\"type\":\"function_call\",",
            "\"call_id\":\"call_stale\",\"name\":\"other_tool\",",
            "\"arguments\":\"{}\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",",
            "\"output\":[{\"id\":\"fc_final\",\"type\":\"function_call\",",
            "\"call_id\":\"call_final\",\"name\":\"headroom_retrieve\",",
            "\"arguments\":\"{}\"}],",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
            "\n",
        );

        let (turn, _) = responses_stream_to_turn(stream);

        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 1, "completed output did not win: {turn}");
        assert_eq!(items[0]["id"], "fc_final");
    }

    /// Recorded shape of 2026-09-22 request
    /// `6ffbe940-22a6-4637-9c39-068a1f73539f`: a completed 186 KB
    /// GPT-5.6 Responses stream whose `response.completed` envelope
    /// carried `output: []`. The previous fold treated that empty
    /// array as authoritative and discarded the message already
    /// gathered from `output_item.done` / text deltas, so
    /// `continuation_turn_from_body` returned None and CCR spliced
    /// the store-fetched retrieval as the whole turn.
    ///
    /// `continuation_body_is_sse` is ruled out: the body opened
    /// `event: response.created` and Content-Type was empty, which
    /// sniffs as SSE. The two `reasoning_summary_part.{added,done}`
    /// events in the same timestamp were unknown to the fold (the
    /// live translator already recognised them as a no-op boundary).
    #[test]
    fn responses_stream_keeps_items_when_completed_output_is_empty() {
        let stream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":",
            "\"resp_013e0a0c70eb6903016ab24d60f6b887d1ac0f6217c6325439\",",
            "\"object\":\"response\",\"status\":\"in_progress\"}}\n",
            "\n",
            "event: response.reasoning_summary_text.delta\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",",
            "\"item_id\":\"rs_1\",\"delta\":\"thinking about the retrieve\"}\n",
            "\n",
            "event: response.reasoning_summary_part.added\n",
            "data: {\"type\":\"response.reasoning_summary_part.added\",",
            "\"item_id\":\"rs_1\"}\n",
            "\n",
            "event: response.reasoning_summary_part.done\n",
            "data: {\"type\":\"response.reasoning_summary_part.done\",",
            "\"item_id\":\"rs_1\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{",
            "\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"output_text\",\"text\":",
            "\"the retrieved file says X\"}]}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":",
            "\"resp_013e0a0c70eb6903016ab24d60f6b887d1ac0f6217c6325439\",",
            "\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":45}}}\n",
            "\n",
        );

        let (turn, output_tokens) = responses_stream_to_turn(stream);

        assert_eq!(output_tokens, 45);
        let items = turn["output"].as_array().expect("output array");
        assert_eq!(
            items.len(),
            1,
            "empty completed output[] wiped the streamed message: {turn}"
        );
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "the retrieved file says X");
    }

    /// Minimal `AppState` for exercising the request-side stages.
    #[test]
    fn openai_to_anthropic_text_response() {
        let openai = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["type"], "message");
        assert_eq!(output["role"], "assistant");
        assert_eq!(output["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(output["stop_reason"], "end_turn");
        assert_eq!(output["content"][0]["type"], "text");
        assert_eq!(output["content"][0]["text"], "Hello!");
        assert_eq!(output["usage"]["input_tokens"], 10);
        assert_eq!(output["usage"]["output_tokens"], 5);
    }

    #[test]
    fn openai_to_anthropic_tool_calls() {
        let openai = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["stop_reason"], "tool_use");
        assert_eq!(output["content"][0]["type"], "tool_use");
        assert_eq!(output["content"][0]["id"], "call_123");
        assert_eq!(output["content"][0]["name"], "bash");
        assert_eq!(output["content"][0]["input"]["command"], "ls");
    }

    /// A `tool_calls` finish with no usable call used to go out as
    /// `content: []` + `stop_reason: tool_use`, which the client discards
    /// whole ("the model's tool call could not be parsed").
    #[test]
    fn unusable_tool_calls_downgrade_to_end_turn() {
        let openai = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "here is what I found",
                    "tool_calls": [{"id": "", "function": {"name": "", "arguments": "{"}}]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["stop_reason"], "end_turn");
        assert!(
            !output["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_use"),
            "broken call must go out alone, not take the text with it: {output}"
        );
        assert_eq!(output["content"][0]["text"], "here is what I found");
    }

    /// Array-shaped content used to be dropped by the string-only read,
    /// emptying the turn.
    #[test]
    fn array_content_parts_are_kept() {
        let openai = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["content"][0]["text"], "ab");
    }

    /// A refusal is the turn's only text; surface it instead of an empty
    /// `end_turn` the client cannot tell apart from silence.
    #[test]
    fn chat_refusal_reaches_the_client_as_text() {
        let openai = json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "refusal": "I cannot do that"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["content"][0]["type"], "text");
        assert_eq!(output["content"][0]["text"], "I cannot do that");
    }
}

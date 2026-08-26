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
/// Returns the turn and the output-token count the outcome is booked with.
pub(crate) fn responses_stream_to_turn(responses_text: &str) -> (Value, u64) {
    let mut current_event: Option<String> = None;
    let mut current_data: Vec<String> = Vec::new();
    let mut assistant_text = String::new();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    // The whole usage block, kept for the request outcome — the two counters
    // above drop the cache details the funnel wants.
    let mut usage_seen: Option<Value> = None;
    let mut output_items: Vec<Value> = Vec::new();
    let mut response_id: Option<String> = None;

    let mut flush_frame = |event_name: Option<&str>, data: &str| {
        if data.trim().is_empty() {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match event_name {
            Some("response.output_text.delta") | Some("output_text.delta") => {
                if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str()) {
                    assistant_text.push_str(delta);
                }
            }
            Some("response.output_text.done") | Some("output_text.done") => {
                if let Some(text) = chunk
                    .get("text")
                    .and_then(|v| v.as_str())
                    .or_else(|| chunk.get("delta").and_then(|v| v.as_str()))
                {
                    if assistant_text.is_empty() {
                        assistant_text.push_str(text);
                    }
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = chunk.get("item").filter(|v| v.is_object()) {
                    output_items.push(item.clone());
                }
            }
            Some("response.completed") => {
                let response = chunk.get("response");
                // `response.completed` carries the finished `output[]`, so it
                // wins over the items gathered frame by frame: a call whose
                // `output_item.done` never arrived is still in here.
                if let Some(items) = response
                    .and_then(|v| v.get("output"))
                    .and_then(Value::as_array)
                {
                    output_items = items.clone();
                }
                if let Some(id) = response.and_then(|v| v.get("id")).and_then(Value::as_str) {
                    response_id = Some(id.to_string());
                }
                if let Some(usage) = response.and_then(|v| v.get("usage")) {
                    if let Some(tokens) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        input_tokens = tokens;
                    }
                    if let Some(tokens) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        output_tokens = tokens;
                    }
                    usage_seen = Some(usage.clone());
                }
            }
            _ => {}
        }
    };

    for line in responses_text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            let data = current_data.join("\n");
            flush_frame(current_event.as_deref(), &data);
            current_event = None;
            current_data.clear();
            continue;
        }

        if let Some(event) = line.strip_prefix("event:") {
            current_event = Some(event.trim().to_string());
            continue;
        }

        if let Some(data) = line.strip_prefix("data:") {
            current_data.push(data.trim_start().to_string());
            continue;
        }
    }
    let data = current_data.join("\n");
    flush_frame(current_event.as_deref(), &data);

    // Deltas and items are two views of one turn. A stream that sent its text
    // only as deltas still needs it carried; one that already sent a `message`
    // item must not have it carried twice. Keying this off "no items at all"
    // instead would drop the text of any turn that also made a tool call. Text
    // leads the calls it introduces, so it goes first.
    let has_message = output_items
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("message"));
    if !has_message && !assistant_text.is_empty() {
        output_items.insert(
            0,
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": assistant_text}],
            }),
        );
    }
    let mut responses_turn = json!({
        "output": output_items,
        "usage": usage_seen.clone().unwrap_or_else(|| json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        })),
    });
    if let Some(id) = response_id {
        responses_turn["id"] = json!(id);
    }

    (responses_turn, output_tokens)
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

    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        _ => "end_turn",
    };

    let mut content: Vec<Value> = Vec::new();

    if let Some(msg) = message {
        // Handle reasoning_content (thinking tokens from models like Qwen).
        if let Some(reasoning) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
            if !reasoning.is_empty() {
                content.push(json!({"type": "thinking", "thinking": reasoning}));
            }
        }

        if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
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
}

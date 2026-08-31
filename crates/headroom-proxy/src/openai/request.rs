//! Anthropic Messages → OpenAI, on the way out.
//!
//! Split out of `handlers::local_model` on 2026-08-26, where it had nothing to
//! do with local models: this is the wire translation every `translate` route
//! uses, Codex included.
//!
//! Two targets, not one. `anthropic_to_openai_request` builds a Chat
//! Completions body; `anthropic_to_openai_responses_request` builds a Responses
//! body. They are kept apart rather than unified because the two APIs disagree
//! about the things that matter here — how a tool result is shaped, where an
//! image goes, and whether reasoning survives the round trip.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine as _;
use serde_json::{json, Value};

use headroom_core::parser::extract_tool_result_text;

use crate::handlers::reasoning_signature::{decode_reasoning_signature, reasoning_input_item};

pub(crate) fn anthropic_to_openai_request(
    anthropic: &Value,
    include_max_output_tokens: bool,
    include_tool_calls: bool,
) -> Result<Value, String> {
    let model = anthropic
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("missing model field")?;

    let max_tokens = anthropic.get("max_tokens").and_then(|v| v.as_u64());

    let stream = anthropic
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let temperature = anthropic.get("temperature");

    let mut messages: Vec<Value> = Vec::new();

    // System prompt → system message.
    if let Some(system) = anthropic.get("system") {
        match system {
            Value::String(s) => {
                messages.push(json!({"role": "system", "content": s}));
            }
            Value::Array(arr) => {
                let text: String = arr
                    .iter()
                    .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(json!({"role": "system", "content": text}));
            }
            _ => {}
        }
    }

    if let Some(msgs) = anthropic.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            match role {
                "user" => translate_user_message(msg, &mut messages),
                "assistant" => translate_assistant_message(msg, &mut messages, include_tool_calls),
                _ => {}
            }
        }
    }

    let tools = anthropic
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|anthropic_tools| {
            anthropic_tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name")?.as_str()?;
                    let description = tool.get("description").and_then(|d| d.as_str());
                    let input_schema = tool.get("input_schema");
                    let default_schema = json!({});
                    let params = input_schema.unwrap_or(&default_schema);
                    Some(json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description.unwrap_or(""),
                            "parameters": params
                        }
                    }))
                })
                .collect::<Vec<_>>()
        });

    let mut openai = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });

    if include_max_output_tokens {
        if let Some(mt) = max_tokens {
            // Chat Completions uses `max_tokens`; `max_output_tokens` is a
            // Responses-API field and would be silently ignored here.
            openai["max_tokens"] = json!(mt);
        }
    }
    if let Some(t) = temperature {
        openai["temperature"] = t.clone();
    }
    if let Some(tools) = tools {
        if !tools.is_empty() {
            openai["tools"] = json!(tools);
        }
    }

    Ok(openai)
}

pub(crate) fn anthropic_to_openai_responses_request(
    anthropic: &Value,
    include_max_output_tokens: bool,
) -> Result<Value, String> {
    let model = anthropic
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("missing model field")?;

    let max_tokens = anthropic.get("max_tokens").and_then(|v| v.as_u64());
    let stream = anthropic
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut input: Vec<Value> = Vec::new();
    let mut instructions: Vec<String> = Vec::new();

    if let Some(system) = anthropic.get("system") {
        match system {
            Value::String(s) => {
                if !s.is_empty() {
                    instructions.push(s.clone());
                }
            }
            Value::Array(arr) => {
                let text = arr
                    .iter()
                    .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            _ => {}
        }
    }

    if let Some(msgs) = anthropic.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            match role {
                "user" => translate_user_message_to_responses(msg, &mut input),
                "assistant" => translate_assistant_message_to_responses(msg, &mut input),
                "system" | "developer" => {
                    let text = plain_message_text(msg);
                    if !text.is_empty() {
                        instructions.push(text);
                    }
                }
                _ => {}
            }
        }
    }

    // Reasoning items are already back in `input`: they were decoded from the
    // thinking blocks the client echoed, in the position they originally held.

    // Responses API uses a flat tool shape, unlike Chat Completions where
    // the fields are nested under `function`.
    let tools = anthropic
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|anthropic_tools| {
            anthropic_tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name")?.as_str()?;
                    let description = tool.get("description").and_then(|d| d.as_str());
                    let input_schema = tool.get("input_schema");
                    let default_schema = json!({});
                    let mut params = input_schema.unwrap_or(&default_schema).clone();
                    // Non-Claude models tend to fill every optional field.
                    // Strip the Agent tool's `mode` so spawned subagents
                    // inherit the session's permission mode instead of
                    // getting an explicit override.
                    if name == "Agent" {
                        if let Some(props) =
                            params.get_mut("properties").and_then(|p| p.as_object_mut())
                        {
                            props.remove("mode");
                        }
                    }
                    Some(json!({
                        "type": "function",
                        "name": name,
                        "description": description.unwrap_or(""),
                        "parameters": params,
                        // Must be present and false, not merely absent: codex
                        // turns on strict constrained decoding for translated
                        // function tools that omit the field, which forces
                        // every optional parameter into every call. That is
                        // the same "fills every optional field" behaviour the
                        // `Agent`/`mode` strip above works around, so the
                        // strip may be redundant once this is live.
                        // Behaviour confirmed in raine/claude-code-proxy.
                        "strict": false
                    }))
                })
                .collect::<Vec<_>>()
        });

    let mut openai = json!({
        "model": model,
        "input": input,
        "stream": stream,
    });

    let has_tools = tools.as_ref().is_some_and(|t| !t.is_empty());
    if let Some(tools) = tools {
        if !tools.is_empty() {
            openai["tools"] = json!(tools);
        }
    }
    if let Some(tool_choice) = anthropic.get("tool_choice") {
        let translated = match tool_choice.get("type").and_then(|t| t.as_str()) {
            Some("auto") => Some(json!("auto")),
            Some("any") => Some(json!("required")),
            Some("none") => Some(json!("none")),
            Some("tool") => tool_choice
                .get("name")
                .and_then(|n| n.as_str())
                .map(|n| json!({"type": "function", "name": n})),
            _ => None,
        };
        if let Some(tc) = translated {
            openai["tool_choice"] = tc;
        }
    }
    // The Codex client always sends these explicitly rather than relying on
    // server defaults (codex-api/src/common.rs: ResponsesApiRequest).
    if has_tools && openai.get("tool_choice").is_none() {
        openai["tool_choice"] = json!("auto");
    }
    openai["parallel_tool_calls"] = json!(false);
    openai["include"] = json!(["reasoning.encrypted_content"]);
    if !instructions.is_empty() {
        openai["instructions"] = json!(instructions.join("\n\n"));
    }
    if include_max_output_tokens {
        if let Some(mt) = max_tokens {
            openai["max_output_tokens"] = json!(mt);
        }
    }
    // Note: `temperature` is deliberately NOT forwarded — the Codex
    // ResponsesApiRequest has no such field and the real CLI never sends it.
    // Map Anthropic's thinking budget onto Responses reasoning effort so the
    // client's thinking setting isn't silently upgraded to the backend default.
    // `output_config.effort` is what `/effort` and `--effort` set, and it is
    // sent whatever `thinking` says. Claude Code now sends
    // `thinking: {"type": "adaptive"}` with no budget, so reading only the
    // budget below meant the client's effort never reached the backend at all.
    let selector_effort = crate::output_shaper::requested_effort(anthropic).map(|e| match e {
        // The Responses API takes minimal/low/medium/high. Anything above
        // `high` in Claude Code's vocabulary lands on `high` rather than being
        // dropped for being unrecognised.
        "xhigh" | "max" => "high",
        other => other,
    });
    if let Some(effort) = selector_effort {
        openai["reasoning"] = json!({"effort": effort, "summary": "auto"});
        openai["stream_options"] = json!({"reasoning_summary_delivery": "sequential_cutoff"});
    } else if let Some(thinking) = anthropic.get("thinking") {
        if thinking.get("type").and_then(|t| t.as_str()) == Some("enabled") {
            let effort = match thinking.get("budget_tokens").and_then(|v| v.as_u64()) {
                Some(b) if b <= 4096 => "low",
                Some(b) if b <= 16384 => "medium",
                Some(_) => "high",
                None => "medium",
            };
            // `summary: auto` + sequential delivery makes the backend stream
            // reasoning summaries, which we translate into thinking blocks.
            openai["reasoning"] = json!({"effort": effort, "summary": "auto"});
            openai["stream_options"] = json!({"reasoning_summary_delivery": "sequential_cutoff"});
        }
    }
    // Stable per-session cache key enables upstream prompt caching; Claude
    // Code's metadata.user_id includes the session id.
    if let Some(user_id) = anthropic
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
    {
        openai["prompt_cache_key"] = json!(user_id);
    }

    Ok(openai)
}

fn plain_message_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

fn translate_user_message_to_responses(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"type": "message", "role": "user", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        Some(Value::Null) | None => {
            out.push(json!({"type": "message", "role": "user", "content": ""}));
            return;
        }
        Some(other) => {
            out.push(json!({"type": "message", "role": "user", "content": other.to_string()}));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut emitted_message = false;
    let mut emitted_tool_result = false;

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_result") => {
                if !text_parts.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": text_parts.join("\n")
                    }));
                    text_parts.clear();
                    emitted_message = true;
                }
                emitted_tool_result = true;
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // `content` is a plain string OR a list of blocks. Images that
                // the backend accepts survive as `input_image`; everything
                // else collapses to text, with a placeholder where a block
                // could not be carried.
                let is_error = block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": tool_result_output_value(block.get("content"), is_error)
                }));
            }
            _ => {}
        }
    }

    if !text_parts.is_empty() || (!emitted_message && !emitted_tool_result) {
        out.push(json!({
            "type": "message",
            "role": "user",
            "content": text_parts.join("\n")
        }));
    }
}

/// One piece of a rendered tool result: either text, or an image the Responses
/// API can actually accept.
enum ToolResultPart {
    Text(String),
    Image(String),
}

/// Media types the Responses API accepts as `input_image`.
const RESPONSES_IMAGE_MEDIA_TYPES: [&str; 4] =
    ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Turn an Anthropic `image` block into a data URL, or `None` if the Responses
/// API could not accept it.
///
/// The payload is decoded and re-encoded rather than passed through: Anthropic
/// tolerates whitespace and missing padding in base64 where the data-URL form
/// does not, and shipping a blob the backend rejects loses the whole turn.
fn image_block_to_data_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    if source.get("type").and_then(|v| v.as_str()) != Some("base64") {
        return None;
    }
    let media_type = source.get("media_type").and_then(|v| v.as_str())?;
    if !RESPONSES_IMAGE_MEDIA_TYPES.contains(&media_type) {
        return None;
    }
    let data = source.get("data").and_then(|v| v.as_str())?;
    let compact: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if compact.is_empty() {
        return None;
    }
    let decoded = STANDARD
        .decode(&compact)
        .or_else(|_| STANDARD_NO_PAD.decode(&compact))
        .ok()?;
    Some(format!(
        "data:{media_type};base64,{}",
        STANDARD.encode(decoded)
    ))
}

/// Render `tool_result.content` into ordered parts.
///
/// Anything that cannot be carried leaves a placeholder in the position it
/// occupied, so the model is told something was there. A silent drop reads to
/// the model as if the tool returned less than it did.
fn render_tool_result_parts(content: Option<&Value>) -> Vec<ToolResultPart> {
    match content {
        Some(Value::String(s)) => vec![ToolResultPart::Text(s.clone())],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => ToolResultPart::Text(
                    block
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                ),
                Some("image") => match image_block_to_data_url(block) {
                    Some(url) => ToolResultPart::Image(url),
                    None => ToolResultPart::Text(
                        "[unsupported content block omitted: image]".to_string(),
                    ),
                },
                Some(other) => {
                    ToolResultPart::Text(format!("[unsupported content block omitted: {other}]"))
                }
                None => {
                    ToolResultPart::Text("[unsupported content block omitted: unknown]".to_string())
                }
            })
            .collect(),
        Some(other) => vec![ToolResultPart::Text(other.to_string())],
        None => Vec::new(),
    }
}

/// Build the `function_call_output.output` value.
///
/// Stays a plain string unless an image survived — the string form is what the
/// backend sees for the overwhelming majority of turns, and switching shape
/// only when necessary keeps those bytes identical to before.
fn tool_result_output_value(content: Option<&Value>, is_error: bool) -> Value {
    let parts = render_tool_result_parts(content);
    let has_image = parts.iter().any(|p| matches!(p, ToolResultPart::Image(_)));

    if !has_image {
        let joined = parts
            .iter()
            .filter_map(|p| match p {
                ToolResultPart::Text(t) => Some(t.as_str()),
                ToolResultPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Value::String(if is_error {
            format!("Error: {joined}")
        } else {
            joined
        });
    }

    let mut items: Vec<Value> = parts
        .into_iter()
        .map(|p| match p {
            ToolResultPart::Text(text) => json!({"type": "input_text", "text": text}),
            ToolResultPart::Image(image_url) => {
                json!({"type": "input_image", "image_url": image_url})
            }
        })
        .collect();
    if is_error {
        items.insert(0, json!({"type": "input_text", "text": "Error:"}));
    }
    Value::Array(items)
}

fn translate_assistant_message_to_responses(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"type": "message", "role": "assistant", "content": s}));
            return;
        }
        Some(Value::Null) => {
            out.push(json!({"type": "message", "role": "assistant", "content": ""}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            out.push(json!({"type": "message", "role": "assistant", "content": ""}));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                if !text_parts.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": text_parts.join("\n")
                    }));
                    text_parts.clear();
                }
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let default_input = json!({});
                let input = block.get("input").unwrap_or(&default_input);
                let arguments = serde_json::to_string(input).unwrap_or_default();
                out.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments
                }));
            }
            // A thinking block carrying our envelope is a reasoning item on the
            // way home. Anything else in that signature slot — a genuine
            // Anthropic signature, another proxy's envelope — decodes to None
            // and the block is dropped, which is what the client would expect
            // of history the backend never produced.
            Some("thinking") => {
                let Some(replay) = block
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .and_then(decode_reasoning_signature)
                else {
                    continue;
                };
                // Reasoning must sit ahead of the text it preceded, so flush
                // first rather than letting the message swallow the ordering.
                if !text_parts.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": text_parts.join("\n")
                    }));
                    text_parts.clear();
                }
                out.push(reasoning_input_item(replay));
            }
            _ => {}
        }
    }

    if !text_parts.is_empty() {
        out.push(json!({
            "type": "message",
            "role": "assistant",
            "content": text_parts.join("\n")
        }));
    }
}

fn translate_user_message(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"role": "user", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            out.push(json!({"role": "user", "content": ""}));
            return;
        }
    };

    // Single tool_result block → tool message.
    if content.len() == 1 {
        if let Some(block) = content.first() {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let result_content = extract_tool_result_text(block);
                let is_error = block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": if is_error { format!("Error: {result_content}") } else { result_content.to_string() }
                }));
                return;
            }
        }
    }

    // Mixed content: text blocks + tool_result blocks.
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_results: Vec<&Value> = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_result") => {
                tool_results.push(block);
            }
            _ => {}
        }
    }

    if !text_parts.is_empty() {
        out.push(json!({
            "role": "user",
            "content": text_parts.join("\n")
        }));
    }

    for tr in tool_results {
        let tool_use_id = tr.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
        let result_content = extract_tool_result_text(tr);
        let is_error = tr
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        out.push(json!({
            "role": "tool",
            "tool_call_id": tool_use_id,
            "content": if is_error { format!("Error: {result_content}") } else { result_content.to_string() }
        }));
    }
}

fn translate_assistant_message(msg: &Value, out: &mut Vec<Value>, include_tool_calls: bool) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"role": "assistant", "content": s}));
            return;
        }
        Some(Value::Null) => {
            out.push(json!({"role": "assistant", "content": ""}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            out.push(json!({"role": "assistant", "content": ""}));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let default_input = json!({});
                let input = block.get("input").unwrap_or(&default_input);
                let arguments = serde_json::to_string(input).unwrap_or_default();
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }));
            }
            _ => {}
        }
    }

    let mut assistant_msg = json!({"role": "assistant"});
    if text_parts.is_empty() {
        assistant_msg["content"] = json!("");
    } else {
        assistant_msg["content"] = json!(text_parts.join("\n"));
    }
    if include_tool_calls && !tool_calls.is_empty() {
        assistant_msg["tool_calls"] = json!(tool_calls);
    }

    out.push(assistant_msg);
}

// ---------------------------------------------------------------------------
// Response translation: OpenAI → Anthropic (non-streaming)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_to_openai_responses_request_serializes_tool_turns() {
        let input = json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "stream": false,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "file1\nfile2"}
                ]}
            ]
        });
        let output = anthropic_to_openai_responses_request(&input, true).unwrap();
        assert_eq!(output["instructions"], "You are helpful.");
        assert_eq!(output["input"][0]["type"], "message");
        assert_eq!(output["input"][0]["role"], "user");
        assert_eq!(output["input"][1]["type"], "function_call");
        assert_eq!(output["input"][1]["call_id"], "call_1");
        assert_eq!(output["input"][1]["name"], "bash");
        assert_eq!(output["input"][2]["type"], "function_call_output");
        assert_eq!(output["input"][2]["call_id"], "call_1");
        assert_eq!(output["input"][2]["output"], "file1\nfile2");
        assert_eq!(output["max_output_tokens"], 100);
        assert!(!output["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["role"] == "tool"));
        assert!(!output["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["role"] == "system"));
    }

    /// Anthropic sends `tool_result.content` as a plain string *or* a list of
    /// blocks. Reading it with `as_str()` blanked every array-shaped result,
    /// so the model saw each tool call answered by an empty string.
    #[test]
    fn anthropic_to_openai_responses_request_preserves_block_shaped_tool_result() {
        let input = json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": [
                        {"type": "text", "text": "file1"},
                        {"type": "image", "source": {"type": "base64", "data": "aGk="}},
                        {"type": "text", "text": "file2"}
                    ]}
                ]}
            ]
        });
        let output = anthropic_to_openai_responses_request(&input, true).unwrap();
        assert_eq!(output["input"][1]["type"], "function_call_output");
        assert_eq!(output["input"][1]["call_id"], "call_1");
        // Text survives and keeps its order. This image has no media_type so
        // it cannot be carried, and leaves a marker where it stood.
        assert_eq!(
            output["input"][1]["output"],
            "file1\n[unsupported content block omitted: image]\nfile2"
        );
    }

    #[test]
    fn anthropic_to_openai_responses_request_marks_block_shaped_tool_error() {
        let input = json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "is_error": true, "content": [
                        {"type": "text", "text": "boom"}
                    ]}
                ]}
            ]
        });
        let output = anthropic_to_openai_responses_request(&input, true).unwrap();
        assert_eq!(output["input"][1]["output"], "Error: boom");
    }

    /// A base64 image the backend accepts becomes a real `input_image`, and the
    /// surrounding text keeps its position around it.
    #[test]
    fn tool_result_images_become_input_image_in_place() {
        let out = tool_result_output_value(
            Some(&json!([
                {"type": "text", "text": "before"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="
                }},
                {"type": "text", "text": "after"}
            ])),
            false,
        );
        assert_eq!(
            out,
            json!([
                {"type": "input_text", "text": "before"},
                {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="},
                {"type": "input_text", "text": "after"}
            ])
        );
    }

    /// Whitespace and missing padding are legal on the way in but not in a data
    /// URL, so the payload is normalized rather than forwarded as-is.
    #[test]
    fn tool_result_image_payload_is_normalized() {
        let out = tool_result_output_value(
            Some(&json!([{"type": "image", "source": {
                "type": "base64", "media_type": "image/png", "data": "iVBOR w0KGgo"
            }}])),
            false,
        );
        assert_eq!(out[0]["image_url"], "data:image/png;base64,iVBORw0KGgo=");
    }

    /// What cannot be carried leaves a marker where it stood, so the model is
    /// not told the tool returned less than it did.
    #[test]
    fn uncarriable_tool_result_blocks_leave_placeholders() {
        let out = tool_result_output_value(
            Some(&json!([
                {"type": "text", "text": "before"},
                {"type": "image", "source": {"type": "url", "url": "https://example.invalid/a.png"}},
                {"type": "image", "source": {"type": "base64", "media_type": "text/plain", "data": "aGk="}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "not base64!"}},
                {"type": "tool_reference", "tool_name": "TaskCreate"},
                {"type": "text", "text": "after"}
            ])),
            false,
        );
        // No image survived, so the output stays a plain string.
        assert_eq!(
            out,
            json!(
                "before\n[unsupported content block omitted: image]\n[unsupported content block omitted: image]\n[unsupported content block omitted: image]\n[unsupported content block omitted: tool_reference]\nafter"
            )
        );
    }

    #[test]
    fn tool_result_error_marker_survives_both_shapes() {
        assert_eq!(
            tool_result_output_value(Some(&json!([{"type": "text", "text": "boom"}])), true),
            json!("Error: boom")
        );
        let with_image = tool_result_output_value(
            Some(&json!([
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="
                }}
            ])),
            true,
        );
        assert_eq!(
            with_image[0],
            json!({"type": "input_text", "text": "Error:"})
        );
        assert_eq!(with_image[1]["type"], "input_image");
    }

    /// The string shape must be byte-identical to before for the common case,
    /// or every cached prefix moves.
    #[test]
    fn text_only_tool_results_keep_the_plain_string_shape() {
        assert_eq!(
            tool_result_output_value(Some(&json!("plain")), false),
            json!("plain")
        );
        assert_eq!(
            tool_result_output_value(
                Some(&json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])),
                false
            ),
            json!("a\nb")
        );
        assert_eq!(tool_result_output_value(None, false), json!(""));
    }

    /// Same bug, Chat Completions path — both the single-block fast path and
    /// the mixed text+tool_result path read `content` with `as_str()`.
    #[test]
    fn anthropic_to_openai_request_preserves_block_shaped_tool_result() {
        let sole = json!({
            "model": "local",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": [
                        {"type": "text", "text": "only"}
                    ]}
                ]}
            ]
        });
        let output = anthropic_to_openai_request(&sole, true, true).unwrap();
        let tool_msg = output["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool message");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
        assert_eq!(tool_msg["content"], "only");

        let mixed = json!({
            "model": "local",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "context"},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": [
                        {"type": "text", "text": "mixed"}
                    ]}
                ]}
            ]
        });
        let output = anthropic_to_openai_request(&mixed, true, true).unwrap();
        let tool_msg = output["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool message");
        assert_eq!(tool_msg["tool_call_id"], "call_2");
        assert_eq!(tool_msg["content"], "mixed");
    }

    /// Pull the signature out of whatever SSE the translator emitted.
    #[test]
    fn foreign_thinking_blocks_are_dropped_not_replayed() {
        for block in [
            json!({"type": "thinking", "thinking": "hm", "signature": "ErUBCkYIBRgCKkDzS1nT"}),
            json!({"type": "thinking", "thinking": "hm"}),
            json!({"type": "redacted_thinking", "data": "opaque"}),
        ] {
            let request = json!({
                "model": "claude-codex-5.6",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    block, json!({"type": "text", "text": "answer"})
                ]}]
            });
            let out = anthropic_to_openai_responses_request(&request, true).unwrap();
            let input = out["input"].as_array().unwrap();
            assert!(
                !input.iter().any(|i| i["type"] == "reasoning"),
                "foreign thinking block became a reasoning item"
            );
            assert_eq!(input[0]["content"], "answer");
        }
    }

    /// Reasoning now travels with the conversation, so a `/model` switch cannot
    /// leak one model's items into another's request — there is no shared store
    /// to leak through. Only what the client echoes is replayed.
    #[test]
    fn anthropic_to_openai_responses_request_forwards_tools() {
        let input = json!({
            "model": "claude-codex-5.6",
            "max_tokens": 100,
            "stream": false,
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [
                {"name": "Bash", "description": "Run a command", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}},
                {"name": "Read", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {"type": "auto"}
        });
        let output = anthropic_to_openai_responses_request(&input, true).unwrap();
        let tools = output["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "Bash");
        assert_eq!(tools[0]["description"], "Run a command");
        assert_eq!(
            tools[0]["parameters"]["properties"]["command"]["type"],
            "string"
        );
        // Flat Responses-API shape, not nested under `function`.
        assert!(tools[0].get("function").is_none());
        assert_eq!(tools[1]["name"], "Read");
        assert_eq!(output["tool_choice"], "auto");
        // Present and false on every tool. Omitting it lets codex turn on
        // strict constrained decoding, which fills every optional parameter.
        for tool in tools {
            assert_eq!(
                tool["strict"],
                json!(false),
                "tool {} must pin strict=false",
                tool["name"]
            );
        }
    }

    #[test]
    fn anthropic_to_openai_responses_request_sends_codex_defaults() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert_eq!(output["tool_choice"], "auto");
        assert_eq!(output["parallel_tool_calls"], false);
        assert_eq!(output["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn injected_recall_block_keeps_front_position_through_translation() {
        // Mimics what the inject engine produces: a recall text block
        // prepended to the first user message. It must stay at the very
        // front of the translated Responses input so the codex prompt-cache
        // prefix is byte-stable across turns.
        let parsed = json!({
            "model": "claude-codex-5.6",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "<<<RECALL>>> prior context digest"},
                    {"type": "text", "text": "build a parser"}
                ]
            }]
        });
        let out = anthropic_to_openai_responses_request(&parsed, false).unwrap();
        let first = &out["input"][0];
        assert_eq!(first["type"], "message");
        assert_eq!(first["role"], "user");
        let text = serde_json::to_string(&first["content"]).unwrap();
        let recall_pos = text.find("<<<RECALL>>>").expect("recall block present");
        let query_pos = text.find("build a parser").expect("query present");
        assert!(recall_pos < query_pos, "recall must precede the user query");
    }

    #[test]
    fn offload_transform_shrinks_large_tool_result_before_translation() {
        // A large tool_result should be offloaded to a digest, and the
        // digest must survive translation into the Responses input.
        let big = "X".repeat(80_000);
        let mut parsed = json!({
            "model": "claude-codex-5.6",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_big", "name": "Bash", "input": {"command": "cat huge.log"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_big", "content": big.clone()}
                ]},
                {"role": "user", "content": "done?"}
            ]
        });
        let cfg = crate::compression::ctx_offload::CtxOffloadConfig {
            min_bytes: 50_000,
            stale_margin: 0,
            stale_window: 0,
        };
        let out =
            crate::compression::ctx_offload::offload_anthropic_request(&mut parsed, &cfg, None);
        assert!(out.changed(), "expected a large tool_result to offload");
        let translated = anthropic_to_openai_responses_request(&parsed, false).unwrap();
        let serialized = serde_json::to_string(&translated).unwrap();
        assert!(!serialized.contains(&big), "raw payload must not survive");
        assert!(
            serialized.contains("headroom_retrieve"),
            "digest pointer expected"
        );
    }

    #[test]
    fn anthropic_to_openai_responses_request_drops_temperature() {
        let input = json!({
            "model": "claude-codex-5.6",
            "temperature": 1.0,
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert!(output.get("temperature").is_none());
    }

    #[test]
    fn anthropic_to_openai_responses_request_maps_thinking_and_cache_key() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}],
            "thinking": {"type": "enabled", "budget_tokens": 10000},
            "metadata": {"user_id": "user_abc_session_123"}
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert_eq!(output["reasoning"]["effort"], "medium");
        assert_eq!(output["prompt_cache_key"], "user_abc_session_123");

        let low = json!({
            "model": "m", "messages": [],
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        });
        let output = anthropic_to_openai_responses_request(&low, false).unwrap();
        assert_eq!(output["reasoning"]["effort"], "low");

        let high = json!({
            "model": "m", "messages": [],
            "thinking": {"type": "enabled", "budget_tokens": 32000}
        });
        let output = anthropic_to_openai_responses_request(&high, false).unwrap();
        assert_eq!(output["reasoning"]["effort"], "high");

        let disabled = json!({
            "model": "m", "messages": [],
            "thinking": {"type": "disabled"}
        });
        let output = anthropic_to_openai_responses_request(&disabled, false).unwrap();
        assert!(output.get("reasoning").is_none());
        assert!(output.get("prompt_cache_key").is_none());
    }

    #[test]
    fn anthropic_to_openai_responses_request_strips_agent_mode_param() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "name": "Agent",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string"},
                        "mode": {"type": "string", "enum": ["default", "plan"]}
                    },
                    "required": ["prompt"]
                }
            }]
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        let props = &output["tools"][0]["parameters"]["properties"];
        assert!(props.get("mode").is_none());
        assert!(props.get("prompt").is_some());
    }

    #[test]
    fn anthropic_to_openai_responses_request_translates_forced_tool_choice() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "Bash"}
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert_eq!(output["tool_choice"]["type"], "function");
        assert_eq!(output["tool_choice"]["name"], "Bash");
    }

    #[test]
    fn anthropic_to_openai_responses_request_omits_tools_when_absent() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert!(output.get("tools").is_none());
        assert!(output.get("tool_choice").is_none());
    }

    #[test]
    fn anthropic_to_openai_simple_text() {
        let input = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "stream": false,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let output = anthropic_to_openai_request(&input, true, true).unwrap();
        assert_eq!(output["messages"][0]["role"], "system");
        assert_eq!(output["messages"][0]["content"], "You are helpful.");
        assert_eq!(output["messages"][1]["role"], "user");
        assert_eq!(output["messages"][1]["content"], "Hello");
        assert_eq!(output["max_tokens"], 1024);
        assert!(output.get("max_output_tokens").is_none());
        assert_eq!(output["stream"], false);
    }

    #[test]
    fn anthropic_to_openai_codex_route_omits_max_output_tokens() {
        let input = json!({
            "model": "claude-codex-5.5",
            "max_tokens": 1024,
            "stream": false,
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let output = anthropic_to_openai_request(&input, false, false).unwrap();
        assert!(output.get("max_output_tokens").is_none());
    }

    #[test]
    fn anthropic_to_openai_codex_route_strips_tool_calls() {
        let input = json!({
            "model": "claude-codex-5.5",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "file1\nfile2"}
                ]}
            ]
        });
        let output = anthropic_to_openai_request(&input, false, false).unwrap();
        assert_eq!(output["messages"][1]["role"], "assistant");
        assert_eq!(output["messages"][1]["content"], "");
        assert!(output["messages"][1].get("tool_calls").is_none());
        assert_eq!(output["messages"][2]["role"], "tool");
        assert_eq!(output["messages"][2]["tool_call_id"], "call_1");
    }

    #[test]
    fn anthropic_to_openai_tool_use() {
        let input = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Run ls"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "file1\nfile2"}
                ]}
            ],
            "tools": [
                {"name": "bash", "description": "Run bash", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}}
            ]
        });
        let output = anthropic_to_openai_request(&input, true, true).unwrap();
        let assistant = &output["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["tool_calls"].is_array());
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "bash");
        let tool_msg = &output["messages"][2];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
        assert_eq!(tool_msg["content"], "file1\nfile2");
    }

    #[test]
    fn anthropic_to_openai_assistant_empty_content_uses_empty_string() {
        let input = json!({
            "model": "claude-codex-5.5",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": null}
            ]
        });
        let output = anthropic_to_openai_request(&input, true, true).unwrap();
        assert_eq!(output["messages"][1]["role"], "assistant");
        assert_eq!(output["messages"][1]["content"], "");
        assert!(output["messages"][1].get("tool_calls").is_none());
    }

    #[test]
    fn anthropic_to_openai_assistant_tool_calls_with_null_content_uses_empty_string() {
        let input = json!({
            "model": "claude-codex-5.5",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]}
            ]
        });
        let output = anthropic_to_openai_request(&input, true, true).unwrap();
        assert_eq!(output["messages"][1]["role"], "assistant");
        assert_eq!(output["messages"][1]["content"], "");
        assert!(output["messages"][1]["tool_calls"].is_array());
        assert_eq!(
            output["messages"][1]["tool_calls"][0]["function"]["name"],
            "bash"
        );
    }
}

#[cfg(test)]
mod selector_effort_tests {
    use super::*;

    fn body(extra: Value) -> Value {
        let mut b = json!({
            "model": "claude-codex-5.6-sol",
            "max_tokens": 32000,
            "messages": [{"role": "user", "content": "hi"}],
        });
        for (k, v) in extra.as_object().expect("object") {
            b[k.as_str()] = v.clone();
        }
        b
    }

    fn out(extra: Value) -> Value {
        anthropic_to_openai_responses_request(&body(extra), false).expect("translates")
    }

    /// The regression this fixes: Claude Code sends `thinking.type = adaptive`
    /// with no budget, so the budget branch never fired and `/effort` reached
    /// the backend as nothing at all.
    #[test]
    fn an_adaptive_thinking_request_still_carries_its_effort() {
        let o = out(json!({
            "thinking": {"type": "adaptive", "display": "omitted"},
            "output_config": {"effort": "low"},
        }));
        assert_eq!(o["reasoning"]["effort"], "low");
    }

    /// `xhigh` is a Claude Code level; the Responses API stops at `high`.
    #[test]
    fn xhigh_is_clamped_rather_than_dropped() {
        let o = out(json!({
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "xhigh"},
        }));
        assert_eq!(o["reasoning"]["effort"], "high");
    }

    /// What the client asked for beats what its budget implies.
    #[test]
    fn the_selector_wins_over_the_thinking_budget() {
        let o = out(json!({
            "thinking": {"type": "enabled", "budget_tokens": 30000},
            "output_config": {"effort": "low"},
        }));
        assert_eq!(o["reasoning"]["effort"], "low");
    }

    /// A client that names no effort keeps the old budget-derived behaviour.
    #[test]
    fn without_a_selector_the_budget_still_decides() {
        let o = out(json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}));
        assert_eq!(o["reasoning"]["effort"], "low");
    }

    /// No effort and no thinking means no `reasoning` field, as before.
    #[test]
    fn a_plain_request_sends_no_reasoning_field() {
        assert!(out(json!({})).get("reasoning").is_none());
    }
}

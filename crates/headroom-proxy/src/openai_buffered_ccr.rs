//! Buffered CCR for native OpenAI `/v1/responses` streams.
//!
//! Port of `upstream-python/headroom/proxy/handlers/openai.py`
//! (`_should_buffer_openai_responses_stream_ccr`,
//! `_has_headroom_retrieve_tool_responses`, `_openai_responses_to_sse`,
//! `_openai_responses_from_sse`) and the buffered-stream branch that calls
//! them.
//!
//! # The gap this closes
//!
//! A `stream: true` `/v1/responses` request whose tool list carries
//! `headroom_retrieve` cannot be answered mid-SSE-stream: there is no
//! OpenAI-native stream rewriter (only `sse::ccr_stream`, which speaks the
//! Anthropic event vocabulary), so the retrieve call used to travel straight
//! to a client that has never heard of it. Python's answer, ported here, is
//! to force a buffered `stream: false` upstream call so retrieval resolves
//! server-side through the existing buffered arm, then resynthesize a minimal
//! SSE stream for the client.
//!
//! # What is deliberately NOT ported
//!
//! * Chat-completions streaming keeps its current behaviour (no injection,
//!   no buffering). Python's `_should_inject_openai_chat_ccr_tool` is
//!   `inject && !stream`, which is exactly what the proxy already does, so
//!   there is nothing to port there.
//! * The ASGI grace-window/heartbeat wrapper (`buffered_ccr_response.py`)
//!   exists to hold a request open while a *streaming* generation runs. Here
//!   the upstream call is already non-streaming, so the full body (and its
//!   status) is in hand before anything is said to the client: status
//!   fidelity is free and there is no idle window to heartbeat over.
//! * Session-sticky chat injection (`apply_session_sticky_ccr_tool`) is a
//!   separate feature, not part of §8.
//!
//! # Fail-closed guards (mirrors Python)
//!
//! * Residual `headroom_retrieve` after handling (max rounds, mixed with a
//!   client tool call) → 502 with a typed SSE error, never an unanswerable
//!   call handed to the client.
//! * A 200 that is not parseable JSON → 502 JSON error, never a fabricated
//!   turn.

use bytes::Bytes;
use http::HeaderMap;
use serde_json::{Value, json};

/// The tool the proxy must answer itself. Keep in step with
/// `headroom_core::ccr::tool_injection::CCR_TOOL_NAME`.
const CCR_TOOL_NAME: &str = "headroom_retrieve";

/// Generic proxy-failure message. Mirrors `_GENERIC_FAILURE_MESSAGE`.
pub(crate) const GENERIC_FAILURE_MESSAGE: &str =
    "An error occurred while processing your request. Please try again.";

/// Port of `_has_headroom_retrieve_tool_responses`: the Responses tool list
/// is flat (`{"type": "function", "name": ...}`), not nested under a
/// `function` key like chat-completions — but accept both shapes, as Python
/// does.
pub(crate) fn has_headroom_retrieve_tool_responses(tools: &Value) -> bool {
    let Some(list) = tools.as_array() else {
        return false;
    };
    list.iter().any(|tool| {
        tool.get("name").and_then(Value::as_str) == Some(CCR_TOOL_NAME)
            || tool
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some(CCR_TOOL_NAME)
    })
}

/// Port of `_should_buffer_openai_responses_stream_ccr`: a streaming
/// Responses turn that offers the retrieve tool must go buffered so the
/// buffered arm can resolve it. ChatGPT-OAuth (Codex subscription) sessions
/// stay streaming — their server-side session owns the transcript and a
/// forced non-streaming call breaks it. OpenCode Zen stays streaming too:
/// Zen validates OpenCode-client attribution on the wire and rejects
/// requests reshaped to `stream: false` with `403 FreeTierError` (#3656).
pub(crate) fn should_buffer_openai_responses_stream_ccr(
    stream: bool,
    ccr_response_handler_enabled: bool,
    tools: Option<&Value>,
    is_chatgpt_auth: bool,
    is_opencode_zen_upstream: bool,
) -> bool {
    stream
        && ccr_response_handler_enabled
        && !is_chatgpt_auth
        && !is_opencode_zen_upstream
        && tools.is_some_and(has_headroom_retrieve_tool_responses)
}

/// Port of `is_opencode_zen_base` (`passthrough.py`): the upstream base
/// targets the OpenCode Zen gateway, whose wire-attribution check rejects
/// reshaped requests even though the caller *is* the OpenCode client.
pub(crate) fn is_opencode_zen_base(base: &url::Url) -> bool {
    matches!(base.host_str(), Some("opencode.ai" | "www.opencode.ai"))
}

/// Read-only port of the `resolve_codex_routing` ChatGPT sniff
/// (`websocket_codex.rs`, itself a port of `openai.py`): an explicit
/// `chatgpt-account-id` header, or the `chatgpt_account_id` claim inside the
/// (unverified) Bearer JWT payload. Unlike the WebSocket version this does
/// not mutate the headers — the native path forwards them untouched.
pub(crate) fn caller_is_chatgpt_auth(headers: &HeaderMap) -> bool {
    if headers.contains_key("chatgpt-account-id") {
        return true;
    }
    let Some(auth) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some((scheme, token)) = auth.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") || token.matches('.').count() < 2 {
        return false;
    }
    let payload = match token.split('.').nth(1) {
        Some(p) => p,
        None => return false,
    };
    use base64::Engine as _;
    let decoded = match base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
    {
        Ok(d) => d,
        Err(_) => return false,
    };
    let Ok(value): Result<Value, _> = serde_json::from_slice(&decoded) else {
        return false;
    };
    value
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn emit(events: &mut Vec<Bytes>, seq: &mut u64, event_type: &str, payload: Value) {
    let mut obj = serde_json::Map::with_capacity(payload.as_object().map_or(1, |m| m.len() + 2));
    obj.insert("type".to_string(), Value::String(event_type.to_string()));
    obj.insert("sequence_number".to_string(), Value::Number((*seq).into()));
    if let Some(extra) = payload.as_object() {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    *seq += 1;
    let line = format!("event: {event_type}\ndata: {}\n\n", Value::Object(obj));
    events.push(Bytes::from(line));
}

/// Port of `_openai_responses_to_sse`: replay a complete Responses JSON body
/// as the incremental event sequence AI-SDK/OpenCode clients render from
/// (`response.created` → `response.in_progress` → per-item added/text
/// deltas/done → `response.output_item.done` → `response.completed` →
/// `[DONE]`). Used only on the buffered-CCR path, where the client asked for
/// `stream: true` but upstream was called with `stream: false`.
pub(crate) fn responses_json_to_sse(response: &Value) -> Vec<Bytes> {
    let mut events = Vec::new();
    let mut seq = 0u64;

    let created_response = {
        let mut m = serde_json::Map::new();
        if let Some(obj) = response.as_object() {
            for (k, v) in obj {
                m.insert(k.clone(), v.clone());
            }
        }
        m.insert(
            "status".to_string(),
            Value::String("in_progress".to_string()),
        );
        m.insert("output".to_string(), Value::Array(Vec::new()));
        Value::Object(m)
    };
    emit(
        &mut events,
        &mut seq,
        "response.created",
        json!({"response": created_response}),
    );
    emit(
        &mut events,
        &mut seq,
        "response.in_progress",
        json!({"response": created_response}),
    );

    let empty = Vec::new();
    let output_items = response
        .get("output")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for (out_idx, item) in output_items.iter().enumerate() {
        let Some(item_obj) = item.as_object() else {
            continue;
        };
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("item_{out_idx}"));

        // `output_item.added` carries the item shell; message content streams
        // via the content-part events below, so start it empty there.
        let added_item = if item_obj.get("type").and_then(Value::as_str) == Some("message") {
            let mut shell = item_obj.clone();
            shell.insert("content".to_string(), Value::Array(Vec::new()));
            Value::Object(shell)
        } else {
            item.clone()
        };
        emit(
            &mut events,
            &mut seq,
            "response.output_item.added",
            json!({"output_index": out_idx, "item": added_item}),
        );

        if item_obj.get("type").and_then(Value::as_str) == Some("message")
            && let Some(content) = item_obj.get("content").and_then(Value::as_array)
        {
            for (c_idx, part) in content.iter().enumerate() {
                let Some(part_obj) = part.as_object() else {
                    continue;
                };
                let loc = json!({
                    "item_id": item_id,
                    "output_index": out_idx,
                    "content_index": c_idx,
                });
                let part_type = part_obj.get("type").and_then(Value::as_str);
                if part_type == Some("output_text") || part_type == Some("text") {
                    let text = part_obj
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let mut added = loc.clone();
                    added["part"] = {
                        let mut p = part_obj.clone();
                        p.insert("text".to_string(), Value::String(String::new()));
                        Value::Object(p)
                    };
                    let mut m = added.as_object().cloned().unwrap_or_default();
                    m.insert(
                        "type".to_string(),
                        Value::String("response.content_part.added".to_string()),
                    );
                    emit(
                        &mut events,
                        &mut seq,
                        "response.content_part.added",
                        Value::Object(m),
                    );
                    if !text.is_empty() {
                        let mut d = loc.clone();
                        d["delta"] = Value::String(text.to_string());
                        let m = d.as_object().cloned().unwrap_or_default();
                        emit(
                            &mut events,
                            &mut seq,
                            "response.output_text.delta",
                            Value::Object(m),
                        );
                    }
                    let mut done = loc.clone();
                    done["text"] = Value::String(text.to_string());
                    let m = done.as_object().cloned().unwrap_or_default();
                    emit(
                        &mut events,
                        &mut seq,
                        "response.output_text.done",
                        Value::Object(m),
                    );
                    let mut pdone = loc.clone();
                    pdone["part"] = part.clone();
                    let m = pdone.as_object().cloned().unwrap_or_default();
                    emit(
                        &mut events,
                        &mut seq,
                        "response.content_part.done",
                        Value::Object(m),
                    );
                } else {
                    // Non-text part (e.g. refusal): add + done with the
                    // full part.
                    let mut added = loc.clone();
                    added["part"] = part.clone();
                    let m = added.as_object().cloned().unwrap_or_default();
                    emit(
                        &mut events,
                        &mut seq,
                        "response.content_part.added",
                        Value::Object(m),
                    );
                    let mut done = loc.clone();
                    done["part"] = part.clone();
                    let m = done.as_object().cloned().unwrap_or_default();
                    emit(
                        &mut events,
                        &mut seq,
                        "response.content_part.done",
                        Value::Object(m),
                    );
                }
            }
        }

        emit(
            &mut events,
            &mut seq,
            "response.output_item.done",
            json!({"output_index": out_idx, "item": item}),
        );
    }

    emit(
        &mut events,
        &mut seq,
        "response.completed",
        json!({"response": response}),
    );
    events.push(Bytes::from_static(b"data: [DONE]\n\n"));
    events
}

/// Port of `_openai_responses_from_sse`: reassemble the terminal Responses
/// JSON body from an SSE stream. Some OpenAI-compatible upstreams answer a
/// `stream: false` request with a valid 200 SSE body; the terminal
/// `response.completed` event carries the complete object, so no delta
/// accumulation is needed. `None` when no terminal event is present — the
/// caller forwards the raw body unchanged in that case.
pub(crate) fn responses_completed_from_sse(sse_text: &str) -> Option<Value> {
    fn consume(lines: &[&str], completed: &mut Option<Value>) {
        if lines.is_empty() {
            return;
        }
        let data_str = lines.join("\n");
        if data_str == "[DONE]" {
            return;
        }
        let Ok(data): Result<Value, _> = serde_json::from_str(&data_str) else {
            return;
        };
        if data.get("type").and_then(Value::as_str) == Some("response.completed")
            && let Some(response) = data.get("response").filter(|v| v.is_object())
        {
            *completed = Some(response.clone());
        }
    }

    let mut completed = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for raw_line in sse_text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            consume(&data_lines, &mut completed);
            data_lines.clear();
        } else if let Some(data) = line.strip_prefix("data:") {
            // Per the SSE spec, strip at most one leading space.
            data_lines.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }
    consume(&data_lines, &mut completed);
    completed
}

/// The OpenAI wire shape for a post-commit stream error. Port of
/// `OPENAI_ERROR_FORMAT.sse_event`: `data: {"error": ...}`, no `event:` line.
pub(crate) fn openai_sse_error_event(error_type: &str, message: &str) -> Bytes {
    Bytes::from(format!(
        "data: {}\n\n",
        json!({"error": {"message": message, "type": error_type}})
    ))
}

/// The OpenAI wire shape for a pre-commit JSON error. Port of
/// `OPENAI_ERROR_FORMAT.json_body`.
pub(crate) fn openai_json_error_body(message: &str) -> Bytes {
    Bytes::from(
        json!({"error": {"message": message, "type": "server_error", "code": "proxy_error"}})
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ccr_tool() -> Value {
        json!({"type": "function", "name": CCR_TOOL_NAME})
    }

    fn unrelated_tool() -> Value {
        json!({"type": "function", "name": "unrelated_tool"})
    }

    // Mirrors `upstream-python/tests/test_proxy_openai_responses_stream_ccr.py`.
    #[test]
    fn buffers_streaming_openai_requests_with_retrieve_tool() {
        assert!(should_buffer_openai_responses_stream_ccr(
            true,
            true,
            Some(&json!([ccr_tool()])),
            false,
            false,
        ));
    }

    #[test]
    fn keeps_chatgpt_oauth_requests_streaming() {
        assert!(!should_buffer_openai_responses_stream_ccr(
            true,
            true,
            Some(&json!([ccr_tool()])),
            true,
            false,
        ));
    }

    #[test]
    fn ignores_requests_without_retrieve_tool() {
        assert!(!should_buffer_openai_responses_stream_ccr(
            true,
            true,
            Some(&json!([unrelated_tool()])),
            false,
            false,
        ));
    }

    #[test]
    fn ignores_non_streaming_and_disabled_handler() {
        assert!(!should_buffer_openai_responses_stream_ccr(
            false,
            true,
            Some(&json!([ccr_tool()])),
            false,
            false,
        ));
        assert!(!should_buffer_openai_responses_stream_ccr(
            true,
            false,
            Some(&json!([ccr_tool()])),
            false,
            false,
        ));
        assert!(!should_buffer_openai_responses_stream_ccr(
            true, true, None, false, false,
        ));
    }

    #[test]
    fn keeps_opencode_zen_requests_streaming() {
        assert!(!should_buffer_openai_responses_stream_ccr(
            true,
            true,
            Some(&json!([ccr_tool()])),
            false,
            true,
        ));
    }

    #[test]
    fn opencode_zen_base_matches_gateway_hosts() {
        for host in ["opencode.ai", "www.opencode.ai"] {
            let base: url::Url = format!("https://{host}/zen/v1").parse().unwrap();
            assert!(is_opencode_zen_base(&base), "{host}");
        }
        let other: url::Url = "https://api.openai.com/v1".parse().unwrap();
        assert!(!is_opencode_zen_base(&other));
    }

    #[test]
    fn detects_nested_function_name_shape() {
        let tools = json!([{"type": "function", "function": {"name": CCR_TOOL_NAME}}]);
        assert!(has_headroom_retrieve_tool_responses(&tools));
        assert!(!has_headroom_retrieve_tool_responses(&json!([
            unrelated_tool()
        ])));
        assert!(!has_headroom_retrieve_tool_responses(&json!(
            "not-an-array"
        )));
    }

    #[test]
    fn chatgpt_sniff_reads_header_and_jwt() {
        use base64::Engine as _;
        let mut headers = HeaderMap::new();
        assert!(!caller_is_chatgpt_auth(&headers));
        headers.insert("authorization", "Bearer sk-plain-key".parse().unwrap());
        assert!(!caller_is_chatgpt_auth(&headers));

        let payload = json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}});
        let token = format!(
            "header.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
        );
        let mut jwt_headers = HeaderMap::new();
        jwt_headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        assert!(caller_is_chatgpt_auth(&jwt_headers));

        let mut explicit = HeaderMap::new();
        explicit.insert("chatgpt-account-id", "acct-1".parse().unwrap());
        assert!(caller_is_chatgpt_auth(&explicit));
    }

    fn parse_events(events: &[Bytes]) -> Vec<Value> {
        events
            .iter()
            .map(|e| {
                let s = std::str::from_utf8(e).unwrap();
                if s.starts_with("data: [DONE]") {
                    return json!({"type": "[DONE]"});
                }
                let data = s.split("data: ").nth(1).unwrap();
                serde_json::from_str(data).unwrap()
            })
            .collect()
    }

    // Mirrors `upstream-python/tests/test_openai_responses_buffered_sse.py`.
    #[test]
    fn sse_replays_incremental_output_text() {
        let resp = json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "model": "gpt-5.3-codex",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Hello world", "annotations": []}],
                },
            ],
            "usage": {"input_tokens": 10, "output_tokens": 3},
        });
        let parsed = parse_events(&responses_json_to_sse(&resp));
        let types: Vec<&str> = parsed
            .iter()
            .map(|p| p.get("type").and_then(Value::as_str).unwrap())
            .collect();

        assert_eq!(types[0], "response.created");
        assert_eq!(types[1], "response.in_progress");
        assert_eq!(types[types.len() - 2], "response.completed");
        assert_eq!(types[types.len() - 1], "[DONE]");

        let deltas: Vec<&Value> = parsed
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("response.output_text.delta"))
            .collect();
        assert_eq!(deltas.len(), 1);
        assert_eq!(
            deltas[0].get("delta").and_then(Value::as_str),
            Some("Hello world")
        );
        assert_eq!(deltas[0].get("output_index"), Some(&json!(1)));
        assert_eq!(deltas[0].get("content_index"), Some(&json!(0)));

        assert_eq!(
            types
                .iter()
                .filter(|t| **t == "response.output_item.added")
                .count(),
            2
        );
        assert_eq!(
            types
                .iter()
                .filter(|t| **t == "response.output_item.done")
                .count(),
            2
        );
        assert!(types.contains(&"response.content_part.added"));
        assert!(types.contains(&"response.output_text.done"));
        assert!(types.contains(&"response.content_part.done"));

        let created = parsed
            .iter()
            .find(|p| p.get("type").and_then(Value::as_str) == Some("response.created"))
            .unwrap();
        assert_eq!(
            created.get("response").and_then(|r| r.get("output")),
            Some(&json!([]))
        );
        let completed = parsed
            .iter()
            .find(|p| p.get("type").and_then(Value::as_str) == Some("response.completed"))
            .unwrap();
        assert_eq!(
            completed.get("response").and_then(|r| r.get("output")),
            resp.get("output")
        );

        let seqs: Vec<u64> = parsed
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) != Some("[DONE]"))
            .map(|p| p.get("sequence_number").and_then(Value::as_u64).unwrap())
            .collect();
        assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    }

    #[test]
    fn sse_empty_output_still_valid() {
        let resp = json!({"id": "resp_2", "status": "completed", "output": [], "usage": {}});
        let parsed = parse_events(&responses_json_to_sse(&resp));
        let types: Vec<&str> = parsed
            .iter()
            .map(|p| p.get("type").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "response.created",
                "response.in_progress",
                "response.completed",
                "[DONE]"
            ]
        );
    }

    #[test]
    fn sse_non_message_item_added_and_done() {
        let resp = json!({
            "id": "resp_3",
            "status": "completed",
            "output": [
                {"type": "function_call", "id": "fc_1", "call_id": "c1", "name": "grep", "arguments": "{}"},
            ],
            "usage": {},
        });
        let parsed = parse_events(&responses_json_to_sse(&resp));
        let types: Vec<&str> = parsed
            .iter()
            .map(|p| p.get("type").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.output_item.done",
                "response.completed",
                "[DONE]"
            ]
        );
        let done = parsed
            .iter()
            .find(|p| p.get("type").and_then(Value::as_str) == Some("response.output_item.done"))
            .unwrap();
        assert_eq!(
            done.get("item")
                .and_then(|i| i.get("name"))
                .and_then(Value::as_str),
            Some("grep")
        );
    }

    #[test]
    fn completed_extraction_round_trips_through_synthesis() {
        let resp = json!({
            "id": "resp_9",
            "status": "completed",
            "output": [{"type": "message", "id": "m", "role": "assistant",
                        "content": [{"type": "output_text", "text": "hi"}]}],
        });
        let sse: String = responses_json_to_sse(&resp)
            .iter()
            .map(|b| std::str::from_utf8(b).unwrap().to_string())
            .collect();
        assert_eq!(responses_completed_from_sse(&sse).as_ref(), Some(&resp));
        let back = responses_completed_from_sse(&sse).unwrap();
        assert_eq!(back.get("id"), resp.get("id"));
        assert_eq!(back.get("output"), resp.get("output"));
    }

    #[test]
    fn completed_extraction_returns_none_without_terminal_event() {
        assert!(
            responses_completed_from_sse("event: response.created\ndata: {\"type\":\"other\"}\n\n")
                .is_none()
        );
        assert!(responses_completed_from_sse("").is_none());
    }
}

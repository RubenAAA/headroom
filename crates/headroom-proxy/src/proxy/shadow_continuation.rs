//! Answer a call to one of the Zen gate's shadow tools, on a turn whose client
//! sent no tools.
//!
//! The gate needs `bash`, `edit`, `glob`, `grep` and `read` in the tool list
//! (`routed::tool_alias::ensure_gate_tools`), and Zen takes only
//! `tool_choice: "auto"`, so a model asked to compute something sometimes
//! calls one. The client has no such tool: it got a `tool_use` and no text. The
//! note added to the developer item cuts that but does not end it (probe of
//! 2026-09-30: a prompt naming a file still ended in a shadow call 4 of 4).
//!
//! This answers the call in the proxy instead. The model gets a tool result
//! saying no tools exist and one more round to write its reply. One round
//! only: a second shadow call in the answer is left standing and the
//! stream rewriter drops it like any unresolved proxy tool.
//!
//! Responses shape only, because that is the only shape the shadows are added
//! to. A turn that also calls a real tool, `headroom_retrieve` or a memory
//! tool is left alone; the other resolvers run first and this sees what they
//! leave.

use std::sync::Arc;

use super::memory_continuation::{
    MemoryRoundRead, append_round_messages, continuation_is_doomed, note_continuation_send,
    read_memory_round_body, send_memory_continuation,
};
use super::{CcrRoundUsage, forward};
use crate::config::Config;

const PROVIDER: &str = "openai_responses";
const ITEMS_FIELD: &str = "input";

/// `call_id`s of the turn's shadow calls, or `None` unless every tool call in
/// the turn is one.
fn shadow_call_ids(response: &serde_json::Value) -> Option<Vec<String>> {
    let output = response.get("output")?.as_array()?;
    let mut ids = Vec::new();
    for item in output {
        if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
            continue;
        }
        let name = item.get("name").and_then(|n| n.as_str())?;
        if !crate::routed::tool_alias::is_gate_shadow_name(name) {
            return None;
        }
        ids.push(item.get("call_id").and_then(|c| c.as_str())?.to_string());
    }
    (!ids.is_empty()).then_some(ids)
}

/// Answer the turn's shadow calls and return the turn the model writes next.
/// Anything that stops the round from running returns the turn unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_shadow_response(
    body_bytes: &bytes::Bytes,
    forwarded_request: &bytes::Bytes,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    config: &Arc<Config>,
    request_id: &str,
    outgoing_headers: &http::HeaderMap,
) -> (bytes::Bytes, CcrRoundUsage) {
    use headroom_core::ccr::response_handler::CCRResponseHandler;

    let mut usage = CcrRoundUsage::default();
    let unchanged = || (body_bytes.clone(), CcrRoundUsage::default());
    let Ok(response) = serde_json::from_slice::<serde_json::Value>(body_bytes) else {
        return unchanged();
    };
    let Some(call_ids) = shadow_call_ids(&response) else {
        return unchanged();
    };
    let Ok(mut request) = serde_json::from_slice::<serde_json::Value>(forwarded_request) else {
        return unchanged();
    };

    let results: Vec<serde_json::Value> = call_ids
        .iter()
        .map(|id| {
            crate::memory::tool_adapter::format_responses_tool_result(
                id,
                crate::routed::tool_alias::NO_CLIENT_TOOLS_NOTE,
            )
        })
        .collect();
    let assistant_msg =
        CCRResponseHandler::new(None).extract_assistant_message(&response, PROVIDER);
    let Some(continuation_body) = append_round_messages(
        &mut request,
        ITEMS_FIELD,
        assistant_msg,
        serde_json::json!({"_openai_responses_tool_results": results}),
        PROVIDER,
        config,
        request_id,
        0,
        upstream_url,
    ) else {
        return unchanged();
    };
    if continuation_is_doomed(&continuation_body, PROVIDER, ITEMS_FIELD, request_id, 0) {
        return unchanged();
    }
    note_continuation_send(&request, None, results.len(), request_id, 0);
    tracing::info!(
        event = "zen_shadow_call_answered",
        request_id = %request_id,
        calls = call_ids.len(),
        "a tool-less turn called a gate shadow tool; answering it and continuing"
    );

    let sent = send_memory_continuation(
        client,
        upstream_url,
        outgoing_headers,
        continuation_body,
        request_id,
        0,
        ITEMS_FIELD,
        &request,
    )
    .await;
    let forward::MemorySendDone::Sent(resp) = sent else {
        return unchanged();
    };
    let mut cut_attempts: u32 = 0;
    match read_memory_round_body(resp, PROVIDER, &mut cut_attempts, request_id).await {
        MemoryRoundRead::Advance(next) => {
            usage.add_response_logged(&response, request_id, "superseded");
            match serde_json::to_vec(&next) {
                Ok(bytes) => (bytes::Bytes::from(bytes), usage),
                Err(_) => unchanged(),
            }
        }
        MemoryRoundRead::Retry | MemoryRoundRead::Done => unchanged(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, id: &str) -> serde_json::Value {
        json!({"type": "function_call", "name": name, "call_id": id, "arguments": "{}"})
    }

    #[test]
    fn only_a_turn_of_nothing_but_shadow_calls_is_answered() {
        let shadows = json!({"output": [
            {"type": "reasoning", "summary": []},
            call("bash", "c1"),
            call("glob", "c2"),
        ]});
        assert_eq!(
            shadow_call_ids(&shadows),
            Some(vec!["c1".into(), "c2".into()])
        );

        let mixed = json!({"output": [call("bash", "c1"), call("headroom_retrieve", "c2")]});
        assert_eq!(
            shadow_call_ids(&mixed),
            None,
            "another tool's call leaves the turn alone"
        );

        let text = json!({"output": [{"type": "message", "content": []}]});
        assert_eq!(shadow_call_ids(&text), None, "no call, nothing to answer");
    }
}

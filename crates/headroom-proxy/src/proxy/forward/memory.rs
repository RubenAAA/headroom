//! Memory and CCR tool plumbing on the forward path: tool injection,
//! memory continuation sends and their classification, and memory context.
//!
//! Moved out of `forward.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Injects memory tool definitions into the request value.
///
/// Creates the tools array on demand; sets changed on injection.
/// Extracted from forward_http without behavior change.
pub(crate) fn inject_memory_tool_definitions(
    value: &mut serde_json::Value,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    changed: &mut bool,
) {
    if let Some(handler) = state.memory_handler.as_ref()
        && handler.is_initialized()
    {
        let provider = match endpoint {
            compression::CompressibleEndpoint::AnthropicMessages => {
                crate::memory::tool_adapter::Provider::Anthropic
            }
            compression::CompressibleEndpoint::OpenAiChatCompletions
            | compression::CompressibleEndpoint::OpenAiResponses => {
                crate::memory::tool_adapter::Provider::Openai
            }
        };
        // Requests without a tools array still get the
        // memory tools - create the array on demand.
        let existing: Vec<serde_json::Value> = value
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let (new_tools, injected) = handler.inject_memory_tools(Some(&existing), provider);
        // Info, not debug: in tool mode this is the only
        // proof the model was ever offered memory. Without
        // it, "the tools never arrived" and "the model
        // chose not to call them" both read as an empty
        // log. Logged on both branches for that reason.
        let added = new_tools.len().saturating_sub(existing.len());
        // Only the definitions we appended, not the whole
        // array: the tools block costs about $32/day in
        // cache reads and the memory tools share of it was
        // a guess. prefix_composition already carries the
        // block total size, so this is the other half of
        // the ratio. Serialising the tail is a handful of
        // small objects; serialising the array would not be.
        let added_bytes: usize = new_tools
            .iter()
            .skip(existing.len())
            .filter_map(|t| serde_json::to_string(t).ok())
            .map(|s| s.len())
            .sum();
        if injected {
            if let Some(obj) = value.as_object_mut() {
                obj.insert("tools".to_string(), serde_json::Value::Array(new_tools));
                *changed = true;
                tracing::info!(
                    request_id = %request_id,
                    event = "memory_tools_injected",
                    tools_added = added,
                    tools_total = existing.len() + added,
                    bytes_added = added_bytes,
                );
            }
        } else {
            tracing::info!(
                request_id = %request_id,
                event = "memory_tools_not_injected",
                tools_present = existing.len(),
            );
        }
    }
}

/// One memory-continuation send attempt, classified into its loop action.
///
/// Extracted from `send_memory_continuation` without behavior change.
pub(crate) enum MemorySendOutcome {
    Done(MemorySendDone),
    Next(u32),
}

/// What a finished send attempt means for the rounds loop: a response to
/// fold, a deterministic rejection (the body will never pass — stop
/// sending it), or a transport failure (also stop; nothing was decided).
pub(crate) enum MemorySendDone {
    Sent(reqwest::Response),
    Rejected,
    Failed,
}

/// Builds and sends one memory-continuation request: fresh Zen request id per
/// send (no-op off zen routes), JSON headers, cloned body, 30s header timeout.
/// Extracted from `send_memory_continuation` without behavior change.
pub(crate) async fn send_memory_continuation_once(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    outgoing_headers: &http::HeaderMap,
    continuation_body: &[u8],
) -> Result<Result<reqwest::Response, reqwest::Error>, tokio::time::error::Elapsed> {
    // Same Zen hygiene as the CCR continuation above: fresh
    // `x-opencode-request` per send, no-op off zen routes.
    let mut continuation_headers = outgoing_headers.clone();
    crate::routed::quirks::refresh_zen_request_id(&mut continuation_headers);
    tokio::time::timeout(
        CCR_CONTINUATION_SEND_TIMEOUT,
        client
            .post(upstream_url.clone())
            .headers(crate::headers::headers_for_json_body(
                &continuation_headers,
                continuation_body,
            ))
            .body(continuation_body.to_vec())
            .send(),
    )
    .await
}

/// Handles the error-status arm of one memory-continuation attempt: logs the
/// rejected item's shape (`input[N]`/`messages[N]`) so the next schema
/// mismatch is diagnosable from the log alone.
/// Extracted from `send_memory_continuation` without behavior change.
pub(crate) async fn log_memory_send_rejection(
    r: reqwest::Response,
    request_id: &str,
    attempt: u32,
    round: usize,
    items_field: &str,
    current_request: &serde_json::Value,
) {
    let status = r.status();
    let detail = r.text().await.unwrap_or_default();
    // A 400 names the item it rejected as `input[N]` (or
    // `messages[N]`); log that item's shape so the next
    // schema mismatch is diagnosable from the log alone.
    let rejected = rejected_item_summary(&detail, current_request, items_field);
    // …but some 400s name no item at all (e.g. "the conversation must end
    // with a user message"), so also snapshot the tail roles: a trailing
    // assistant message is the whole diagnosis for that class, and without
    // this line the next one is as unreadable as the 2026-09-24 Opus
    // prefill rejections were.
    let tail_roles = continuation_tail_summary(current_request, items_field);
    tracing::warn!(
        event = "memory_continuation_rejected",
        request_id = %request_id,
        status = %status,
        attempt,
        round = round + 1,
        detail = %first_bytes(&detail, 600),
        rejected_item = %rejected,
        tail_roles = %tail_roles,
        "memory: upstream returned error during continuation"
    );
}

/// Roles (and, for Responses items, item types) of the last three entries
/// of a continuation array, most recent last. Companions
/// `rejected_item_summary` for rejections that name no item. Shared with
/// the pre-send continuation check in `proxy.rs`, which logs the same
/// shape when it skips a body that would fail the same way.
pub(crate) fn continuation_tail_summary(request: &serde_json::Value, items_field: &str) -> String {
    let items = request
        .get(items_field)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tail: Vec<String> = items
        .iter()
        .rev()
        .take(3)
        .map(|item| {
            if let Some(role) = item.get("role").and_then(|v| v.as_str()) {
                let kinds = item
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b.get("type").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("+")
                    })
                    .unwrap_or_default();
                if kinds.is_empty() {
                    role.to_string()
                } else {
                    format!("{role}[{kinds}]")
                }
            } else {
                item.get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string()
            }
        })
        .collect();
    format!(
        "[{}]",
        tail.iter().rev().cloned().collect::<Vec<_>>().join(",")
    )
}

/// Whether a memory-continuation HTTP status is worth another send.
///
/// 429 and 5xx are the historical retry set on every route. On the Zen
/// route only, 403 joins them. Over 2026-09-22, 4 of 8 memory continuations
/// on muse-spark-1.3-contributor-free returned 403 FreeTierError ("OpenCode's
/// free tier can only be used from within OpenCode") at attempt 0, round 1,
/// while 15 memory continuations on Anthropic-route models and 294 CCR
/// continuations (sonnet/opus; CCR never runs on Zen) all returned 200. The
/// nonce refresh (`refresh_zen_request_id`) was already live and did not
/// stop these 403s.
///
/// Ruled out that window: fallback session id (no zen_session_mint_started
/// events, so a real synced OpenCode session id went out every time) and
/// concurrency (zero other Spark requests in flight at each 403). Body
/// shape is the better-supported hypothesis: all four failures had msgs=2
/// / tok_before about 24.4k, all four passes had msgs=1 / tok_before
/// 12.4-15.8k. This retry is a cheap hedge for the 403s, not a proven
/// fix for that split.
pub(crate) fn memory_status_is_retryable(status: http::StatusCode, zen_route: bool) -> bool {
    status.as_u16() == 429 || status.is_server_error() || (zen_route && status.as_u16() == 403)
}

/// Classifies one `send_memory_continuation` attempt result: break the retry
/// loop with a response, or bump the attempt count and retry after a backoff
/// sleep. Extracted from `send_memory_continuation` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn classify_memory_send(
    sent: Result<Result<reqwest::Response, reqwest::Error>, tokio::time::error::Elapsed>,
    attempt: u32,
    request_id: &str,
    round: usize,
    items_field: &str,
    current_request: &serde_json::Value,
    zen_route: bool,
) -> MemorySendOutcome {
    match sent {
        Err(_) => {
            tracing::warn!(
                event = "memory_continuation_headers_timeout",
                request_id = %request_id,
                attempt,
                round = round + 1,
                timeout_secs = CCR_CONTINUATION_SEND_TIMEOUT.as_secs(),
                "memory: continuation timed out waiting for response headers"
            );
            MemorySendOutcome::Done(MemorySendDone::Failed)
        }
        Ok(Ok(r)) if r.status().is_success() => MemorySendOutcome::Done(MemorySendDone::Sent(r)),
        Ok(Ok(r)) => {
            let status = r.status();
            let retryable = memory_status_is_retryable(status, zen_route);
            if retryable && attempt < MEMORY_CONTINUATION_RETRIES {
                let attempt = attempt + 1;
                tokio::time::sleep(memory_continuation_backoff(attempt)).await;
                return MemorySendOutcome::Next(attempt);
            }
            log_memory_send_rejection(r, request_id, attempt, round, items_field, current_request)
                .await;
            MemorySendOutcome::Done(MemorySendDone::Rejected)
        }
        Ok(Err(e)) => {
            if attempt < MEMORY_CONTINUATION_RETRIES && is_retryable_transport_error(&e) {
                let attempt = attempt + 1;
                tokio::time::sleep(memory_continuation_backoff(attempt)).await;
                return MemorySendOutcome::Next(attempt);
            }
            tracing::warn!(
                event = "memory_continuation_send_failed",
                request_id = %request_id,
                attempt,
                round = round + 1,
                error = %e,
                "memory: upstream request failed during continuation"
            );
            MemorySendOutcome::Done(MemorySendDone::Failed)
        }
    }
}

/// Injects the `headroom_retrieve` tool definition when CCR is on.
///
/// Adds an empty `tools` array first when the body carries none but content
/// was offloaded, so the digest cannot name a tool the model never got. Sets
/// `changed` on injection. Extracted from `forward_http` without behavior
/// change.
pub(crate) fn inject_ccr_retrieve_tool(
    value: &mut serde_json::Value,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    ctx_transform_tokens_saved: i64,
    changed: &mut bool,
) {
    let client_streams = value
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let can_resolve = matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) || !client_streams;
    // A request with no `tools` key still needs the retrieval
    // tool once its content has been offloaded, otherwise the
    // digest points at a tool the model was never given. The
    // memory injector above takes the same line.
    if state.config.ccr_inject_tool
        && can_resolve
        && ctx_transform_tokens_saved > 0
        && value.get("tools").is_none()
    {
        value["tools"] = serde_json::json!([]);
    }
    if state.config.ccr_inject_tool
        && can_resolve
        && let Some(tools) = value.get_mut("tools").and_then(|v| v.as_array_mut())
    {
        let already_has = tools
            .iter()
            .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("headroom_retrieve"));
        if !already_has {
            let ccr_tool = match endpoint {
                compression::CompressibleEndpoint::AnthropicMessages => {
                    serde_json::json!({
                        "name": "headroom_retrieve",
                        "description": "Retrieve original uncompressed content that was compressed to save tokens. Use this when you need more data than what's shown in compressed tool results. Provide `hash` from a compression marker like [N items compressed... hash=abc123], or `query` with keywords to search previously offloaded content. Exactly one of the two.",
                        "input_schema": {
                            "type": "object",
                            "properties": {
                                "hash": {
                                    "type": "string",
                                    "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
                                },
                                "query": {
                                    "type": "string",
                                    "description": "Keyword query to search previously offloaded content (e.g., 'provider squad retry logic'). Use when no marker hash is at hand."
                                }
                            }
                        }
                    })
                }
                _ => {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": "headroom_retrieve",
                            "description": "Retrieve original uncompressed content that was compressed to save tokens. Use this when you need more data than what's shown in compressed tool results. Provide `hash` from a compression marker like [N items compressed... hash=abc123], or `query` with keywords to search previously offloaded content. Exactly one of the two.",
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "hash": {
                                        "type": "string",
                                        "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
                                    },
                                    "query": {
                                        "type": "string",
                                        "description": "Keyword query to search previously offloaded content (e.g., 'provider squad retry logic'). Use when no marker hash is at hand."
                                    }
                                }
                            }
                        }
                    })
                }
            };
            tools.push(ccr_tool);
            *changed = true;
            tracing::debug!(
                request_id = %request_id,
                "ccr: injected headroom_retrieve tool definition"
            );
        }
    }
}

/// Returns a memory answer held back from a turn that also called a client tool.
///
/// Sets `changed` when an answer was put back. Extracted from `forward_http`
/// without behavior change.
pub(crate) fn restore_deferred_memory_answer(
    value: &mut serde_json::Value,
    request_id: &str,
    changed: &mut bool,
) {
    // Memory: put back any answer held from a turn that also
    // called a client tool. This request carries the client's
    // `tool_result`, so the turn can finally be completed —
    // and because it goes out as part of this request, the
    // cache write it causes is the one this turn needed
    // anyway. Prefix replay carries the repaired history
    // forward from here.
    if let Some(messages) = value.get_mut("messages").and_then(|v| v.as_array_mut()) {
        let applied = match crate::memory::deferred::store().lock() {
            Ok(mut held) if !held.is_empty() => held.apply(messages),
            _ => 0,
        };
        if applied > 0 {
            *changed = true;
            tracing::info!(
                request_id = %request_id,
                event = "memory_answer_restored",
                restored = applied,
                "memory: held answer returned to its turn"
            );
        }
    }
}

/// Memory search, injected into the latest user message tail.
///
/// Sets `changed` when the tail grew. Extracted from `forward_http` without
/// behavior change.
pub(crate) async fn inject_memory_context(
    value: &mut serde_json::Value,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    headers_snapshot: &Option<HeaderMap>,
    injection_budget: &crate::injection_budget::InjectionBudget,
    changed: &mut bool,
) {
    if let Some(handler) = state.memory_handler.as_ref()
        && handler.is_initialized()
    {
        let provider = match endpoint {
            compression::CompressibleEndpoint::AnthropicMessages => {
                crate::memory::tool_adapter::Provider::Anthropic
            }
            compression::CompressibleEndpoint::OpenAiChatCompletions
            | compression::CompressibleEndpoint::OpenAiResponses => {
                crate::memory::tool_adapter::Provider::Openai
            }
        };
        if let Some(messages) = value.get("messages").and_then(|v| v.as_array()) {
            let msgs: Vec<serde_json::Value> = messages.clone();
            let base_user_id = headers_snapshot
                .as_ref()
                .and_then(|h| h.get("x-headroom-user-id"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("default");
            // Same partition as the tool path, so switching
            // modes cannot make one project's memories
            // visible to another.
            let user_id = crate::memory::router::scoped_user_id(
                base_user_id,
                &crate::memory::router::RequestContext {
                    headers: header_map_to_lowercase_strings(headers_snapshot.as_ref()),
                    system_prompt: crate::memory::router::extract_system_prompt(value),
                    base_user_id: base_user_id.to_string(),
                    project_root_override: state.config.memory_project_root.clone(),
                },
            );
            // Memory runs last, so it sees whatever the
            // expansion and recall stages left. Clipping
            // here is cache-safe: this appends to the live
            // tail, which is re-sent every turn anyway.
            if let Some(context) = crate::memory::ctx_backend::SEARCH_REQUEST_ID
                .scope(
                    request_id.to_string(),
                    handler.search_and_format_context(
                        &user_id, &msgs, None, // request_context
                        None, // ranker
                        None, // query
                        None, // budget
                    ),
                )
                .await
                .and_then(|context| {
                    injection_budget.take(crate::injection_budget::InjectionStage::Memory, context)
                })
            {
                // `frozen_message_count` indexes into
                // `messages`. This passed the length of the
                // *system* array instead — a count of system
                // blocks standing in for a count of messages.
                // With two system blocks the callee skipped
                // `messages[0..2]`, so a conversation one or
                // two messages long had no eligible tail and
                // got no memory at all.
                //
                // Zero is the honest value here. The real
                // frozen boundary comes from the prefix-replay
                // tracker, which does not run until
                // `apply_prefix_replay` further down. The
                // guard is inert regardless: the callee walks
                // backwards for the last user message, and the
                // turn being sent is by definition not in the
                // cached prefix.
                let (new_msgs, bytes) =
                    crate::memory::handler::MemoryHandler::append_to_latest_user_tail(
                        &msgs, &context, provider, 0,
                    );
                if bytes > 0
                    && let Some(msgs_val) = value.get_mut("messages")
                {
                    *msgs_val = serde_json::Value::Array(new_msgs);
                    *changed = true;
                    tracing::debug!(
                        request_id = %request_id,
                        bytes_appended = bytes,
                        "memory: injected context into user message tail"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod memory_status_is_retryable_tests {
    use super::memory_status_is_retryable;
    use http::StatusCode;

    #[test]
    fn retryable_matrix() {
        let too_many = StatusCode::TOO_MANY_REQUESTS;
        let server = StatusCode::INTERNAL_SERVER_ERROR;
        let forbidden = StatusCode::FORBIDDEN;
        let bad = StatusCode::BAD_REQUEST;
        assert!(memory_status_is_retryable(too_many, false));
        assert!(memory_status_is_retryable(too_many, true));
        assert!(memory_status_is_retryable(server, false));
        assert!(memory_status_is_retryable(server, true));
        assert!(!memory_status_is_retryable(forbidden, false));
        assert!(memory_status_is_retryable(forbidden, true));
        assert!(!memory_status_is_retryable(bad, false));
        assert!(!memory_status_is_retryable(bad, true));
    }
}

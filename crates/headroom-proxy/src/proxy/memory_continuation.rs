//! Memory-tool continuations: run the proxy-owned `memory_*` calls and
//! continue the turn upstream.
//!
//! The proxy injects these tools (see the injection site in `forward_http`),
//! so the proxy has to run them: the client has never heard of
//! `memory_search` and answers a call to it with `No such tool available`.
//!
//! A continuation replays the turn's assistant message plus the freshly-run
//! answers. Any call in that body without a matching answer fails the whole
//! send with a 400 — a client call sharing the turn (its result arrives next
//! request), a history pair the client left dangling, a second memory call
//! that produced no result. Two guards keep such bodies off the wire, in
//! this order:
//!
//! 1. A turn mixing memory calls with client calls on a shape without
//!    deferral is answered in place: the memory answers join the turn as
//!    prose and no continuation is sent, so the client's calls proceed
//!    untouched. Anthropic keeps its deferral (a real `tool_result` beats
//!    prose).
//! 2. A built body that still carries an unanswerable call is never sent:
//!    its memory calls are retired with one hook-matchable notice instead
//!    of re-sending the same bytes once per alternation pass.
//!
//! Moved out of `proxy.rs` without behavior change so the continuation
//! cluster (fetch, send, fold, validate, strand) lives in one place. The
//! entry points stay re-exported from `proxy.rs`, so callers keep their
//! paths.

use std::sync::Arc;

use super::forward;
use super::{
    continuation_api_kind, continuation_cut_retryable, continuation_turn_from_body, extend_or_push,
    header_map_to_lowercase_strings, memory_continuation_backoff, read_continuation_body,
    retail_continuation_breakpoint, AppState, CcrRoundUsage, MEMORY_CONTINUATION_RETRIES,
};
use crate::cache_stabilization::drift_detector::compute_structural_hash;
use crate::config::Config;

/// Build the memory context for a request, or `None` when memory is off or
/// uninitialised. Mirrors the gate on the injection site, so the proxy resolves
/// exactly the turns it injected into.
pub(crate) async fn memory_tool_context(
    state: &AppState,
    headers_snapshot: &Option<http::HeaderMap>,
    provider: Option<&str>,
    request_body: &bytes::Bytes,
) -> Option<MemoryToolContext> {
    let handler = state.memory_handler.as_ref()?;
    if !handler.is_initialized() {
        return None;
    }
    let provider = match provider? {
        "anthropic" => crate::memory::tool_adapter::Provider::Anthropic,
        _ => crate::memory::tool_adapter::Provider::Openai,
    };
    let base_user_id = headers_snapshot
        .as_ref()
        .and_then(|h| h.get("x-headroom-user-id"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("default");
    // The project comes from the system prompt's working directory, since
    // Claude Code sends no project header. It is resolved here rather than read
    // from `project_context`, which is a thread-local and so cannot be trusted
    // across an await on a multi-threaded runtime.
    let parsed: serde_json::Value = serde_json::from_slice(request_body).unwrap_or_default();
    let user_id = crate::memory::router::scoped_user_id(
        base_user_id,
        &crate::memory::router::RequestContext {
            headers: header_map_to_lowercase_strings(headers_snapshot.as_ref()),
            system_prompt: crate::memory::router::extract_system_prompt(&parsed),
            base_user_id: base_user_id.to_string(),
            project_root_override: state.config.memory_project_root.clone(),
        },
    );
    Some(MemoryToolContext {
        handler: handler.clone(),
        provider,
        user_id,
    })
}

/// What a memory continuation needs. Assembled at the seam that has the
/// request in scope, the same way [`crate::routed::ccr::RoutedCcr`]
/// is.
pub(crate) struct MemoryToolContext {
    pub handler: Arc<crate::memory::handler::MemoryHandler>,
    pub provider: crate::memory::tool_adapter::Provider,
    pub user_id: String,
}

/// Fetch one round's memory tool results, or `None` when the turn has no
/// memory calls outstanding (loop ends).
/// Extracted from `handle_memory_response` without behavior change.
async fn fetch_memory_round_calls(
    memory: &MemoryToolContext,
    current_response: &serde_json::Value,
) -> Option<Vec<serde_json::Value>> {
    let handler = memory.handler.as_ref();
    if !handler.has_memory_tool_calls(current_response, memory.provider) {
        return None;
    }
    let results = handler
        .handle_memory_tool_calls(current_response, &memory.user_id, memory.provider, None)
        .await;
    if results.is_empty() {
        return None;
    }
    Some(results)
}

/// Redact memory answers before they join the continuation: they come
/// from the local store, which captured the client's real text.
/// Extracted from `handle_memory_response` without behavior change.
fn redact_memory_results(
    redact: &Option<crate::redact::RedactRef>,
    results: &mut [serde_json::Value],
) {
    if let Some(r) = redact.as_ref() {
        for res in results.iter_mut() {
            crate::redact::redact_value(r, res);
        }
    }
}

/// What one memory round read decided: the next turn JSON, a same-round
/// retry, or the end of the loop.
/// Extracted from `handle_memory_response` without behavior change.
enum MemoryRoundRead {
    Advance(serde_json::Value),
    Retry,
    Done,
}

/// A client tool call sharing the turn makes a continuation impossible: it
/// would send upstream an assistant turn whose client `tool_use` has no
/// `tool_result` — the client has not run it yet — and upstream rejects the
/// whole request, losing the memory answer with it. Answer the memory call
/// now and hold the answer for the next request, which carries that result.
/// Anthropic only: the holding area works in Anthropic block shapes.
/// Returns true when the turn was deferred (caller leaves it alone so the
/// client's own tool call reaches it untouched).
/// Extracted from `handle_memory_response` without behavior change.
async fn defer_mixed_memory_turn(
    response: &serde_json::Value,
    memory: &MemoryToolContext,
    request_id: &str,
    provider: &str,
) -> bool {
    if provider != "anthropic" {
        return false;
    }
    let (ours, client_ids) = crate::memory::deferred::split_tool_calls(response);
    if ours.is_empty() || client_ids.is_empty() {
        return false;
    }
    let results = {
        let handler = memory.handler.as_ref();
        handler
            .handle_memory_tool_calls(response, &memory.user_id, memory.provider, None)
            .await
    };
    let held = pair_results_with_calls(&ours, &results, &client_ids);
    let count = held.len();
    if let Ok(mut store) = crate::memory::deferred::store().lock() {
        for pending in held {
            store.hold(pending);
        }
    }
    tracing::info!(
        request_id = %request_id,
        event = "memory_answer_deferred",
        held = count,
        client_tool_calls = client_ids.len(),
        "memory: turn also calls a client tool; holding the answer for \
         the next request"
    );
    // The calls have run. Leave the turn alone so the client's own tool
    // call reaches it untouched.
    true
}

/// Send one memory continuation with retry: transport blips and 429/5xx
/// get another attempt; anything else is a body we built wrong, and the
/// caller retires its calls (excised with one notice) instead of leaving
/// them standing for the next pass to re-send.
/// Extracted from `handle_memory_response` without behavior change.
///
/// Returns how the send ended: `Sent` carries the response to fold,
/// `Rejected` means upstream refused the body deterministically (a 400
/// names what it disliked — resending the same bytes can only fail the
/// same way), `Failed` covers timeouts and transport errors.
#[allow(clippy::too_many_arguments)]
async fn send_memory_continuation(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    outgoing_headers: &http::HeaderMap,
    continuation_body: Vec<u8>,
    request_id: &str,
    round: usize,
    items_field: &str,
    current_request: &serde_json::Value,
) -> forward::MemorySendDone {
    let mut attempt: u32 = 0;
    // Same presence gate `refresh_zen_request_id` uses: the header is on
    // the map iff this continuation is a Zen-route send.
    let zen_route = outgoing_headers.contains_key("x-opencode-request");
    loop {
        let sent = forward::send_memory_continuation_once(
            client,
            upstream_url,
            outgoing_headers,
            &continuation_body,
        )
        .await;
        match forward::classify_memory_send(
            sent,
            attempt,
            request_id,
            round,
            items_field,
            current_request,
            zen_route,
        )
        .await
        {
            forward::MemorySendOutcome::Done(done) => break done,
            forward::MemorySendOutcome::Next(next) => {
                attempt = next;
                continue;
            }
        }
    }
}

/// Body stall after 200 headers: transport-cut class, same as the CCR
/// path. Bounded; exhaustion ends the loop as before.
/// Extracted from `read_memory_round_body` without behavior change.
async fn note_memory_body_stall(
    error: String,
    mem_cut_attempts: &mut u32,
    request_id: &str,
) -> MemoryRoundRead {
    if *mem_cut_attempts < MEMORY_CONTINUATION_RETRIES {
        *mem_cut_attempts += 1;
        tracing::warn!(
            event = "memory_continuation_body_unreadable",
            request_id = %request_id,
            error = %error,
            cut_attempt = *mem_cut_attempts,
            "memory: failed to read continuation response body; retrying same round"
        );
        tokio::time::sleep(memory_continuation_backoff(*mem_cut_attempts)).await;
        return MemoryRoundRead::Retry;
    }
    tracing::warn!(
        event = "memory_continuation_body_failed",
        request_id = %request_id,
        error = %error,
        "memory: failed to read continuation response body"
    );
    MemoryRoundRead::Done
}

/// Fold a continuation body back into a turn, retrying the same round on
/// a cut-stream fold (terminal-less SSE or truncated JSON). Unlike CCR
/// there is no fallback splice here: exhaustion ends the loop with the
/// calls standing, loudly, as before.
/// Extracted from `read_memory_round_body` without behavior change.
async fn fold_or_retry_round_body(
    bytes: bytes::Bytes,
    content_type: &str,
    provider: &str,
    mem_cut_attempts: &mut u32,
    request_id: &str,
) -> MemoryRoundRead {
    match continuation_turn_from_body(&bytes, Some(content_type), provider) {
        Some(next) => MemoryRoundRead::Advance(next),
        None => {
            // Same cut-stream retry as the CCR path (terminal-less SSE or
            // truncated JSON). Unlike CCR there is no fallback splice here:
            // exhaustion breaks with the calls standing, loudly, as before.
            if continuation_cut_retryable(&bytes, content_type, provider)
                && *mem_cut_attempts < MEMORY_CONTINUATION_RETRIES
            {
                *mem_cut_attempts += 1;
                tracing::warn!(
                    event = "memory_continuation_empty_fold",
                    request_id = %request_id,
                    body_bytes = bytes.len(),
                    cut_attempt = *mem_cut_attempts,
                    "memory: continuation body folded to nothing usable; retrying same round"
                );
                tokio::time::sleep(memory_continuation_backoff(*mem_cut_attempts)).await;
                return MemoryRoundRead::Retry;
            }
            // Giving up here used to be silent, and silence is what made this
            // expensive: the tool ran, its answer was thrown away, and the
            // client was handed a turn that simply stopped. Measured
            // 2026-09-22 on Spark — a continuation came back
            // `response.incomplete` with `incomplete_details.reason:
            // max_output_tokens`, 597 of 600 output tokens spent on reasoning
            // and `output: []`. Nothing in the log said so.
            tracing::warn!(
                request_id = %request_id,
                event = "memory_continuation_folded_empty",
                body_bytes = bytes.len(),
                content_type = content_type,
                terminal = ?responses_terminal_reason(&bytes),
                "memory: continuation folded to no usable turn; the memory answer is lost"
            );
            MemoryRoundRead::Done
        }
    }
}

/// The terminal status of a Responses SSE body, plus the reason when it is
/// `incomplete`, for a log line that says why a fold came back empty.
///
/// Returns `None` for bodies that are not a Responses stream, so the caller's
/// log simply omits it rather than guessing.
fn responses_terminal_reason(bytes: &bytes::Bytes) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut terminal = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let Some(response) = v.get("response") else {
            continue;
        };
        let Some(status) = response.get("status").and_then(|s| s.as_str()) else {
            continue;
        };
        if !matches!(status, "completed" | "incomplete" | "failed") {
            continue;
        }
        terminal = Some(
            match response
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(|r| r.as_str())
            {
                Some(reason) => format!("{status}: {reason}"),
                None => status.to_string(),
            },
        );
    }
    terminal
}

/// Read one memory continuation response: fold SSE back into a turn when a
/// mandating Responses backend streams it, retrying the same round on a
/// transport-cut body or an unusable fold (bounded; exhaustion breaks with
/// the calls standing, loudly, as before).
/// Extracted from `handle_memory_response` without behavior change.
async fn read_memory_round_body(
    resp: reqwest::Response,
    provider: &str,
    mem_cut_attempts: &mut u32,
    request_id: &str,
) -> MemoryRoundRead {
    // As in `handle_ccr_response`: a mandating Responses backend answers
    // a streamed continuation with SSE, which plain JSON parsing cannot
    // read — fold it back into a turn first.
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = match read_continuation_body(resp).await {
        Ok(bytes) => bytes,
        Err(e) => return note_memory_body_stall(e, mem_cut_attempts, request_id).await,
    };
    fold_or_retry_round_body(bytes, &content_type, provider, mem_cut_attempts, request_id).await
}

/// Splice one round's assistant turn + tool results into the continuation
/// request: retail the tail breakpoint, strip unreplayable reasoning, and
/// serialize. Returns `None` when the request has no continuation array or
/// the splice does not serialize (caller ends the loop).
/// Extracted from `handle_memory_response` without behavior change.
#[allow(clippy::too_many_arguments)]
fn append_round_messages(
    current_request: &mut serde_json::Value,
    items_field: &str,
    assistant_msg: serde_json::Value,
    tool_result_msg: serde_json::Value,
    provider: &str,
    config: &Config,
    request_id: &str,
    round: usize,
    upstream_url: &url::Url,
) -> Option<Vec<u8>> {
    let Some(items) = current_request
        .get_mut(items_field)
        .and_then(|v| v.as_array_mut())
    else {
        tracing::warn!(
            event = "memory_no_continuation_array",
            request_id = %request_id,
            field = items_field,
            "memory: no continuation array in request; cannot continue"
        );
        return None;
    };
    extend_or_push(items, assistant_msg, &["_openai_responses_input_items"]);
    extend_or_push(
        items,
        tool_result_msg,
        &["_memory_tool_results", "_openai_responses_tool_results"],
    );

    retail_continuation_breakpoint(current_request, provider, config, request_id, round + 1);

    // The spliced `output[]` carries the model's own reasoning items,
    // `id` and `encrypted_content` included. The translate and sidecar
    // paths strip those for Zen (quirks.rs) because a rotation between
    // rounds invalidates the blob; this path replays the same items and
    // needs the same strip.
    crate::routed::quirks::classify_upstream(upstream_url, false)
        .strip_unreplayable_reasoning(current_request);

    serde_json::to_vec(current_request).ok()
}

/// Note stranded memory calls when the round cap hits with calls still
/// outstanding: the block is suppressed, the budget ran out, and the tool
/// never runs. The log says it to the operator; the trace says it to the
/// model, which otherwise writes its answer as if the lookup had happened.
/// Extracted from `handle_memory_response` without behavior change.
fn note_stranded_memory_calls(
    memory: &MemoryToolContext,
    current_response: &serde_json::Value,
    config: &Config,
    trace: &mut Vec<String>,
    rounds: usize,
    request_id: &str,
) {
    // The cap is the other way a memory call gets stranded: the block is
    // suppressed, the round budget runs out, and the tool never runs. Say so —
    // the alternative is a turn quietly missing work the model asked for.
    if rounds < config.ccr_max_retrieval_rounds {
        return;
    }
    let still_pending = {
        let handler = memory.handler.as_ref();
        handler.has_memory_tool_calls(current_response, memory.provider)
    };
    if !still_pending {
        return;
    }
    tracing::warn!(
        event = "memory_round_cap_reached",
        request_id = %request_id,
        rounds,
        "memory: retrieval round cap reached with calls outstanding; \
         raise HEADROOM_CCR_MAX_RETRIEVAL_ROUNDS"
    );
    // The bracketed marker makes the drop hook-matchable (see
    // retry-dropped-turn.sh): a turn ending on this note with no
    // answer of its own stalls the same way a spliced retrieval
    // does, and the client cannot re-issue a proxy-owned tool.
    let mut stranded = false;
    for name in pending_memory_call_names(current_response, memory.provider) {
        trace.push(format!(
            "{name} → not run: retrieval round cap ({}) reached",
            config.ccr_max_retrieval_rounds
        ));
        stranded = true;
    }
    if stranded {
        trace.push(crate::memory::deferred::DEFERRED_MEMORY_DROPPED_MARKER.to_string());
    }
}

/// Splice the memory trace into the turn head. Anthropic only: this is the
/// shape whose client keeps a transcript and replays it, and the only one
/// whose stream splice passes an added block through (see
/// `crate::sse::ccr_stream::drop_reason`). Leading, because the calls
/// ran before the answer was written.
/// Extracted from `handle_memory_response` without behavior change.
fn splice_memory_trace(provider: &str, trace: &[String], current_response: &mut serde_json::Value) {
    // Anthropic only: this is the shape whose client keeps a transcript and
    // replays it, and the only one whose stream splice passes an added block
    // through (see `sse::ccr_stream::drop_reason`). Leading, because the calls
    // ran before the answer was written.
    if provider == "anthropic" && !trace.is_empty() {
        if let Some(content) = current_response
            .get_mut("content")
            .and_then(|v| v.as_array_mut())
        {
            content.insert(
                0,
                serde_json::json!({
                    "type": "text",
                    "text": format!("[headroom memory]\n{}", trace.join("\n")),
                }),
            );
        }
    }
}

/// Calls in a built continuation body that upstream is certain to refuse.
///
/// A memory continuation replays the turn's assistant message plus the
/// freshly-run answers. Any call in that body without a matching answer —
/// a client call sharing the turn (its result arrives next request), a
/// history pair the client left dangling, a second memory call that
/// produced no result — fails the whole send with a 400, and resending
/// the same bytes fails the same way (measured 2026-09-24: every
/// rejection re-sent up to `MAX_RESOLVER_ALTERNATIONS` times). Returns
/// human-readable descriptions; empty means the body is sendable.
///
/// This is a shape check, not a verdict on the model: history pairs that
/// already served are untouched, only unpaired calls are named.
fn continuation_dangling_calls(body: &serde_json::Value, provider: &str) -> Vec<String> {
    match provider {
        "openai_responses" => responses_dangling_calls(body),
        "anthropic" => anthropic_dangling_calls(body),
        _ => chat_dangling_calls(body),
    }
}

/// Responses: every `function_call` needs a `function_call_output` with its
/// `call_id`.
fn responses_dangling_calls(body: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let mut dangling = Vec::new();
    let empty = Vec::new();
    let items = body
        .get("input")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut calls = Vec::new();
    let mut outputs = std::collections::HashSet::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => match item.get("call_id").and_then(Value::as_str) {
                Some(id) => calls.push(id.to_string()),
                None => dangling.push("function_call without call_id".to_string()),
            },
            Some("function_call_output") => {
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    outputs.insert(id.to_string());
                }
            }
            _ => {}
        }
    }
    for call in calls {
        if !outputs.contains(&call) {
            dangling.push(format!("function_call {call} has no output"));
        }
    }
    dangling
}

/// Anthropic: each assistant `tool_use` needs a `tool_result` in the user
/// message right after it, and the conversation must not end on the
/// assistant.
fn anthropic_dangling_calls(body: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let mut dangling = Vec::new();
    let empty = Vec::new();
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if messages.is_empty() {
        dangling.push("no messages".to_string());
        return dangling;
    }
    for pair in messages.windows(2) {
        anthropic_pair_dangling(&pair[0], &pair[1], &mut dangling);
    }
    if messages
        .last()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
    {
        dangling.push("conversation ends with an assistant message".to_string());
    }
    dangling
}

/// One assistant message and the message after it.
fn anthropic_pair_dangling(
    msg: &serde_json::Value,
    next: &serde_json::Value,
    dangling: &mut Vec<String>,
) {
    use serde_json::Value;
    if msg.get("role").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let blocks = msg
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen_in_turn = std::collections::HashSet::new();
    for b in &blocks {
        let btype = b.get("type").and_then(Value::as_str).unwrap_or("");
        if btype != "tool_use" && btype != "server_tool_use" {
            continue;
        }
        let Some(id) = b.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !seen_in_turn.insert((btype, id)) {
            dangling.push(format!("duplicate {btype} {id} in one assistant message"));
        }
    }
    let next_blocks = next
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let answered: std::collections::HashSet<&str> = next_blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
        .collect();
    // Server-run tools answer themselves; only client-style
    // `tool_use` blocks need a result in the next message.
    // (Anthropic rejects a turn whose tool_use has no
    // `tool_result` immediately after it.)
    if next.get("role").and_then(Value::as_str) != Some("user") {
        for b in &blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_use")
                && b.get("id").and_then(Value::as_str).is_some()
            {
                dangling.push("assistant message not followed by a user message".to_string());
                break;
            }
        }
        return;
    }
    for b in &blocks {
        if b.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        if let Some(id) = b.get("id").and_then(Value::as_str) {
            if !answered.contains(id) {
                dangling.push(format!("tool_use {id} has no tool_result after it"));
            }
        }
    }
}

/// Chat completions: every tool call id must be covered by a later tool
/// message. Order-insensitive: history pairs served long ago stay quiet,
/// only uncovered calls are named.
fn chat_dangling_calls(body: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let mut dangling = Vec::new();
    let empty = Vec::new();
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut calls = Vec::new();
    let mut results = std::collections::HashSet::new();
    for msg in messages {
        match msg.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                if let Some(tcs) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tcs {
                        if let Some(id) = tc.get("id").and_then(Value::as_str) {
                            calls.push(id.to_string());
                        }
                    }
                }
            }
            Some("tool") => {
                if let Some(id) = msg.get("tool_call_id").and_then(Value::as_str) {
                    results.insert(id.to_string());
                }
            }
            _ => {}
        }
    }
    for call in calls {
        if !results.contains(&call) {
            dangling.push(format!("tool_call {call} has no tool message"));
        }
    }
    dangling
}

/// Name of a proxy-owned memory call in any turn shape, for retiring calls
/// whose continuation deterministically failed. Mirrors the name half of
/// `pending_memory_call_names` (flat `name`, or Chat's nested
/// `function.name`); anything else is someone else's call and stays.
fn stranded_memory_call_name(block: &serde_json::Value) -> Option<String> {
    let name = block.get("name").and_then(|v| v.as_str()).or_else(|| {
        block
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
    })?;
    crate::memory::tool_adapter::MEMORY_TOOL_NAMES
        .contains(&name)
        .then(|| name.to_string())
}

/// Remove the turn's proxy-owned memory calls after a deterministic
/// continuation failure, returning the removed names.
///
/// Leaving them standing re-sends the same doomed body on every
/// alternation pass (measured 2026-09-24: four identical 400s per turn)
/// and leaks the call to clients that never declared it on paths without
/// a stream splice. Client and already-answered calls are untouched.
fn excise_failed_memory_calls(response: &mut serde_json::Value, provider: &str) -> Vec<String> {
    use serde_json::Value;
    let mut removed = Vec::new();
    match provider {
        "anthropic" => {
            if let Some(content) = response.get_mut("content").and_then(Value::as_array_mut) {
                let mut kept = Vec::with_capacity(content.len());
                for block in content.drain(..) {
                    let is_memory = block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && stranded_memory_call_name(&block).is_some();
                    if is_memory {
                        if let Some(name) = stranded_memory_call_name(&block) {
                            removed.push(name);
                        }
                    } else {
                        kept.push(block);
                    }
                }
                *content = kept;
            }
        }
        "openai_responses" => {
            if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
                let mut kept = Vec::with_capacity(output.len());
                for item in output.drain(..) {
                    let is_memory = item.get("type").and_then(Value::as_str)
                        == Some("function_call")
                        && stranded_memory_call_name(&item).is_some();
                    if is_memory {
                        if let Some(name) = stranded_memory_call_name(&item) {
                            removed.push(name);
                        }
                    } else {
                        kept.push(item);
                    }
                }
                *output = kept;
            }
        }
        _ => {
            if let Some(calls) = response
                .get_mut("choices")
                .and_then(Value::as_array_mut)
                .and_then(|c| c.first_mut())
                .and_then(|c| c.get_mut("message"))
                .and_then(|m| m.get_mut("tool_calls"))
                .and_then(Value::as_array_mut)
            {
                let mut kept = Vec::with_capacity(calls.len());
                for call in calls.drain(..) {
                    if let Some(name) = stranded_memory_call_name(&call) {
                        removed.push(name);
                    } else {
                        kept.push(call);
                    }
                }
                *calls = kept;
            }
        }
    }
    removed
}

/// Whether the turn carries visible text (thinking does not count): picks
/// the stranded-notice wording. Turn-local only — text already streamed
/// past the resolver is invisible here, so streaming turns that spoke
/// still read as empty. The marker below does the real work either way.
fn turn_has_visible_text(response: &serde_json::Value, provider: &str) -> bool {
    use serde_json::Value;
    fn anthropic_texts(response: &Value) -> Vec<String> {
        response
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn responses_texts(response: &Value) -> Vec<String> {
        response
            .get("output")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|i| i.get("type").and_then(Value::as_str) == Some("message"))
                    .flat_map(|i| {
                        i.get("content")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default()
                    })
                    .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
    let texts: Vec<String> = match provider {
        "anthropic" => anthropic_texts(response),
        "openai_responses" => responses_texts(response),
        _ => response
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
    };
    texts.iter().any(|t| !t.trim().is_empty())
}

/// Append the stranded-call notice in the turn's own shape. Same wording
/// the stream splice uses (including the Stop-hook marker), so whichever
/// path owns the turn the client hears it once. Idempotent: a turn that
/// already carries the marker keeps it exactly once, which is also what
/// stops later alternation passes from stacking notices.
fn append_stranded_notice(response: &mut serde_json::Value, provider: &str, text: &str) {
    use serde_json::{json, Value};
    if serde_json::to_string(response)
        .is_ok_and(|s| s.contains(crate::sse::ccr_stream::RETRIEVAL_DROPPED_MARKER))
    {
        return;
    }

    match provider {
        "anthropic" => {
            let block = json!({"type": "text", "text": text});
            match response.get_mut("content").and_then(Value::as_array_mut) {
                Some(content) => content.push(block),
                None => response["content"] = Value::Array(vec![block]),
            }
        }
        "openai_responses" => {
            let item = json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            });
            match response.get_mut("output").and_then(Value::as_array_mut) {
                Some(output) => output.push(item),
                None => response["output"] = Value::Array(vec![item]),
            }
        }
        _ => {
            if let Some(message) = response
                .get_mut("choices")
                .and_then(Value::as_array_mut)
                .and_then(|c| c.first_mut())
                .and_then(|c| c.get_mut("message"))
            {
                match message.get("content").and_then(Value::as_str) {
                    Some(prev) if !prev.trim().is_empty() => {
                        message["content"] = Value::String(format!("{prev}\n{text}"));
                    }
                    _ => message["content"] = Value::String(text.to_string()),
                }
            }
        }
    }
}

/// Retire memory calls after a deterministic continuation failure: excise
/// the unanswerable calls and leave one hook-matchable notice.
///
/// A rejected (or provably-doomed, see `continuation_dangling_calls`)
/// continuation leaves its calls standing, and every later alternation
/// pass re-sends the same bytes — measured 2026-09-24 as four identical
/// 400s per turn — while buffered paths without a stream splice hand the
/// client a tool it never declared. Excising ends both: later passes find
/// no calls and return the turn unchanged, and the notice (same wording
/// and marker the stream splice uses) tells the client once.
fn strand_failed_memory_calls(
    current_response: &mut serde_json::Value,
    provider: &str,
    request_id: &str,
) {
    let names = excise_failed_memory_calls(current_response, provider);
    if names.is_empty() {
        return;
    }
    let first = names.first().cloned();
    let text = if turn_has_visible_text(current_response, provider) {
        crate::sse::ccr_stream::dropped_call_text(first.as_deref())
    } else {
        crate::sse::ccr_stream::empty_turn_text(first.as_deref())
    };
    append_stranded_notice(current_response, provider, &text);
    tracing::warn!(
        event = "memory_calls_stranded",
        request_id = %request_id,
        tools = ?names,
        "memory: continuation deterministically failed; retired the calls with one notice"
    );
}

/// Execute `memory_*` tool calls the model made, and continue the turn.
///
/// The proxy injects these tools (see the injection site in `forward_http`),
/// so the proxy has to run them: the client has never heard of `memory_search`
/// and answers a call to it with `No such tool available`. `MemoryHandler`
/// could already execute them — until this function existed nothing ever asked
/// it to, on any path, streaming or buffered.
///
/// Deliberately shaped like [`super::handle_ccr_response`], down to the round cap and
/// the mixed-tool rule: a turn that calls a memory tool *and* a client tool is
/// left alone, because we cannot fabricate the client's half.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_memory_response(
    body_bytes: &bytes::Bytes,
    forwarded_request: &bytes::Bytes,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    memory: &MemoryToolContext,
    config: &Config,
    request_id: &str,
    outgoing_headers: &http::HeaderMap,
    provider: &str,
    redact: Option<crate::redact::RedactRef>,
) -> (bytes::Bytes, CcrRoundUsage) {
    use headroom_core::ccr::response_handler::CCRResponseHandler;

    let mut round_usage = CcrRoundUsage::default();
    let items_field = if provider == "openai_responses" {
        "input"
    } else {
        "messages"
    };

    let Some((response, mut current_request)) =
        parse_memory_turn(body_bytes, forwarded_request, memory, request_id)
    else {
        return (body_bytes.clone(), round_usage);
    };

    // As in `handle_ccr_response`: continuations bypass the forwarding path,
    // so the only place their prefix can be checked is here, against the body
    // the first round already had cached.
    let base_kind = continuation_api_kind(provider);
    let base_hash = base_kind.map(|kind| compute_structural_hash(&current_request, kind));

    // A client tool call sharing the turn makes a continuation impossible.
    // Answer the memory call now and hold the answer for the next request.
    if defer_mixed_memory_turn(&response, memory, request_id, provider).await {
        return (body_bytes.clone(), round_usage);
    }

    // Reused purely for its provider-aware message shaping — the CCR handler
    // knows how each provider wants an assistant turn and a tool result
    // expressed, and memory results go back the same way.
    let shaper = CCRResponseHandler::new(None);
    let mut current_response = response;
    let mut rounds = 0;
    // Kept across rounds: the client sees one turn, not one per round.
    let mut trace: Vec<String> = Vec::new();
    // Cut-stream resends, shared by the body-read and fold failure arms
    // below. Per-turn budget like the CCR path; exhaustion breaks with the
    // calls standing (loud) exactly as before.
    let mut mem_cut_attempts: u32 = 0;

    while rounds < config.ccr_max_retrieval_rounds {
        let Some(mut results) = fetch_memory_round_calls(memory, &current_response).await else {
            break;
        };
        // Memory answers come from the local store, which captured the
        // client's real text — redact before they join the continuation.
        redact_memory_results(&redact, &mut results);
        trace.extend(memory_trace_lines(
            &current_response,
            &results,
            memory.provider,
        ));

        // Answer the memory calls in
        // place the way the CCR mixed branch does, and let the client's
        // calls reach the client untouched. Anthropic keeps its deferral
        // (a real tool_result beats prose); every other shape takes the
        // in-place answer. No continuation is sent, so there is nothing
        // to book and the loop ends with the turn resolved. When the
        // splice left a memory call standing (nothing matched it, which
        // cannot happen while ids come from the same turn), fall through
        // to the legacy continuation attempt rather than stranding it.
        if answered_mixed_turn_in_place(
            memory,
            &mut current_response,
            &results,
            provider,
            request_id,
            rounds,
        ) {
            break;
        }

        // `handle_memory_tool_calls` returns provider-shaped tool results
        // already; wrap them the way the continuation array expects.
        let assistant_msg = shaper.extract_assistant_message(&current_response, provider);
        let tool_result_msg = memory_results_message(&results, provider);

        let Some(continuation_body) = append_round_messages(
            &mut current_request,
            items_field,
            assistant_msg,
            tool_result_msg,
            provider,
            config,
            request_id,
            rounds,
            upstream_url,
        ) else {
            break;
        };
        // Never send a continuation we can see will fail: a call in the
        // body without a matching answer fails the whole send with a 400,
        // and every later alternation pass would re-send the same bytes
        // (measured 2026-09-24 as four identical rejections per turn).
        // Skip the send, retire the calls, and say so once.
        if continuation_is_doomed(
            &continuation_body,
            provider,
            items_field,
            request_id,
            rounds,
        ) {
            strand_failed_memory_calls(&mut current_response, provider, request_id);
            break;
        }
        note_continuation_send(
            &current_request,
            base_hash.as_ref().zip(base_kind),
            results.len(),
            request_id,
            rounds,
        );
        // A deterministically-failed continuation retires its memory calls
        // (excised with one notice) instead of leaving them standing for
        // the next pass to re-send; transport blips and 429/5xx keep their
        // retries, and transport failures keep the old leave-standing
        // behavior.
        match send_memory_continuation(
            client,
            upstream_url,
            outgoing_headers,
            continuation_body,
            request_id,
            rounds,
            items_field,
            &current_request,
        )
        .await
        {
            super::forward::MemorySendDone::Sent(resp) => {
                round_usage.add_response(&current_response);
                match read_memory_round_body(resp, provider, &mut mem_cut_attempts, request_id)
                    .await
                {
                    MemoryRoundRead::Advance(next) => {
                        current_response = next;
                        rounds += 1;
                    }
                    MemoryRoundRead::Retry => continue,
                    MemoryRoundRead::Done => break,
                }
            }
            super::forward::MemorySendDone::Rejected => {
                strand_failed_memory_calls(&mut current_response, provider, request_id);
                break;
            }
            super::forward::MemorySendDone::Failed => break,
        }
    }

    note_stranded_memory_calls(
        memory,
        &current_response,
        config,
        &mut trace,
        rounds,
        request_id,
    );

    splice_memory_trace(provider, &trace, &mut current_response);

    match serde_json::to_vec(&current_response) {
        Ok(bytes) => (bytes::Bytes::from(bytes), round_usage),
        Err(_) => (body_bytes.clone(), round_usage),
    }
}

/// Parse the response and the forwarded request, or `None` when memory is
/// not in play for this turn. Extracted from `handle_memory_response`
/// without behavior change.
fn parse_memory_turn(
    body_bytes: &bytes::Bytes,
    forwarded_request: &bytes::Bytes,
    memory: &MemoryToolContext,
    request_id: &str,
) -> Option<(serde_json::Value, serde_json::Value)> {
    let response = serde_json::from_slice::<serde_json::Value>(body_bytes).ok()?;
    let handler = memory.handler.as_ref();
    if !handler.is_initialized() || !handler.has_memory_tool_calls(&response, memory.provider) {
        return None;
    }
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(forwarded_request) else {
        tracing::warn!(
            event = "memory_request_unparseable",
            request_id = %request_id,
            "memory: failed to parse original request; skipping tool handling"
        );
        return None;
    };
    Some((response, request))
}

/// Answer the memory calls in place when the turn also calls client tools on
/// a shape without deferral (every shape but Anthropic). True when that
/// resolved the turn and the round loop should stop. Extracted from
/// `handle_memory_response` without behavior change.
fn answered_mixed_turn_in_place(
    memory: &MemoryToolContext,
    current_response: &mut serde_json::Value,
    results: &[serde_json::Value],
    provider: &str,
    request_id: &str,
    rounds: usize,
) -> bool {
    if provider == "anthropic" || count_non_memory_calls(current_response, memory.provider) == 0 {
        return false;
    }
    let spliced = splice_memory_results_as_text(current_response, results, provider);
    let standing = memory
        .handler
        .has_memory_tool_calls(current_response, memory.provider);
    tracing::info!(
        request_id = %request_id,
        event = "memory_mixed_turn_answered_in_place",
        round = rounds + 1,
        memory_calls = results.len(),
        spliced,
        standing,
        "memory: turn also calls client tools; answering in place instead of continuing"
    );
    !standing
}

/// True when the built continuation carries a call upstream will refuse;
/// logs what dangles. Extracted from `handle_memory_response` without
/// behavior change.
fn continuation_is_doomed(
    continuation_body: &[u8],
    provider: &str,
    items_field: &str,
    request_id: &str,
    rounds: usize,
) -> bool {
    let Ok(built) = serde_json::from_slice::<serde_json::Value>(continuation_body) else {
        return false;
    };
    let dangling = continuation_dangling_calls(&built, provider);
    if dangling.is_empty() {
        return false;
    }
    tracing::warn!(
        event = "memory_continuation_doomed",
        request_id = %request_id,
        round = rounds + 1,
        dangling = ?dangling,
        tail = %super::forward::continuation_tail_summary(&built, items_field),
        "memory: continuation has unanswerable calls; skipping the send"
    );
    true
}

/// Log the send and check the continuation still extends the first round's
/// cached prefix. Extracted from `handle_memory_response` without behavior
/// change.
fn note_continuation_send(
    current_request: &serde_json::Value,
    base: Option<(
        &crate::cache_stabilization::drift_detector::StructuralHash,
        crate::cache_stabilization::drift_detector::ApiKind,
    )>,
    results_count: usize,
    request_id: &str,
    rounds: usize,
) {
    tracing::info!(
        request_id = %request_id,
        round = rounds + 1,
        results_count,
        "memory: sending continuation request"
    );
    if let Some((base, kind)) = base {
        crate::cache_stabilization::drift_detector::check_continuation_prefix(
            base,
            current_request,
            kind,
            request_id,
            rounds + 1,
        );
    }
}

/// Match each memory `tool_use` with the `tool_result` answering it.
///
/// A call whose answer is missing is skipped: restoring it would put an
/// unanswered `tool_use` back into the history, which is the failure this
/// whole path avoids.
fn pair_results_with_calls(
    calls: &[serde_json::Value],
    results: &[serde_json::Value],
    client_ids: &[String],
) -> Vec<crate::memory::deferred::PendingMemoryResult> {
    calls
        .iter()
        .filter_map(|call| {
            let id = call.get("id").and_then(serde_json::Value::as_str)?;
            let answer = results
                .iter()
                .find(|r| r.get("tool_use_id").and_then(serde_json::Value::as_str) == Some(id))?;
            Some(crate::memory::deferred::PendingMemoryResult::new(
                call.clone(),
                answer.clone(),
                client_ids.to_vec(),
            ))
        })
        .collect()
}

/// Names of the memory calls still unanswered in `response`, in call order.
fn pending_memory_call_names(
    response: &serde_json::Value,
    provider: crate::memory::tool_adapter::Provider,
) -> Vec<String> {
    crate::memory::tool_adapter::extract_tool_calls(response, provider)
        .into_iter()
        .filter_map(|call| {
            call.get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| crate::memory::tool_adapter::MEMORY_TOOL_NAMES.contains(name))
                .map(str::to_string)
        })
        .collect()
}

/// One line per memory call the proxy answered, for the client's transcript.
///
/// The proxy runs `memory_*` itself and splices back only the continuation's
/// final answer, so neither the call nor its result ever reaches the client.
/// The client rebuilds the next request from its own transcript, where the
/// model then reads a bare claim with no tool output behind it. A session on
/// 2026-08-31 read that back and concluded it had fabricated four searches it
/// had in fact run, all of which the `memory_tool_call` log recorded. These
/// lines are the receipt: short enough to carry every turn, specific enough to
/// check against that log.
fn memory_trace_lines(
    response: &serde_json::Value,
    results: &[serde_json::Value],
    provider: crate::memory::tool_adapter::Provider,
) -> Vec<String> {
    use serde_json::Value;

    // Char-safe, because a query can end mid-codepoint and this string goes
    // into a response body.
    fn clip(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            return s.replace('\n', " ");
        }
        let head: String = s.chars().take(max).collect();
        format!("{}…", head.replace('\n', " "))
    }

    let mut lines = Vec::new();
    for call in crate::memory::tool_adapter::extract_tool_calls(response, provider) {
        let Some(name) = call.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !crate::memory::tool_adapter::MEMORY_TOOL_NAMES.contains(&name) {
            continue;
        }
        let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
        // The argument worth showing differs per tool, and only the first one
        // present is worth the width.
        let arg = call
            .get("input")
            .and_then(|input| {
                ["query", "content", "new_content", "memory_id"]
                    .iter()
                    .find_map(|key| input.get(*key).and_then(Value::as_str))
            })
            .map(|s| clip(s, 60))
            .unwrap_or_default();

        let outcome = results
            .iter()
            .find(|r| r.get("tool_use_id").and_then(Value::as_str) == Some(id))
            .and_then(|r| r.get("content").and_then(Value::as_str))
            .and_then(|c| serde_json::from_str::<Value>(c).ok())
            .map(|parsed| {
                let status = parsed
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unparsed");
                if status == "error" {
                    let detail = parsed.get("error").and_then(Value::as_str).unwrap_or("");
                    return format!("error: {}", clip(detail, 80));
                }
                if let Some(n) = parsed.get("count").and_then(Value::as_u64) {
                    return format!("{n} result{}", if n == 1 { "" } else { "s" });
                }
                match parsed.get("memory_id").and_then(Value::as_str) {
                    Some(memory_id) => format!("{status} {memory_id}"),
                    None => status.to_string(),
                }
            })
            // A call with no matching result was answered by nothing, which is
            // exactly the case the receipt exists to make visible.
            .unwrap_or_else(|| "no answer".to_string());

        if arg.is_empty() {
            lines.push(format!("{name} → {outcome}"));
        } else {
            lines.push(format!("{name}(\"{arg}\") → {outcome}"));
        }
    }
    lines
}

/// Wrap provider-shaped memory tool results for the continuation array.
///
/// Anthropic wants one user turn holding every `tool_result` block; the OpenAI
/// shapes want one entry per result, so those go behind a sentinel key that
/// [`super::extend_or_push`] expands.
fn memory_results_message(results: &[serde_json::Value], provider: &str) -> serde_json::Value {
    match provider {
        "anthropic" => serde_json::json!({"role": "user", "content": results}),
        "openai_responses" => {
            // The adapter reads Responses `function_call` items but formats
            // every OpenAI result in Chat shape (`role: tool`). A Chat item
            // in a Responses `input` is a 400: Zen answered `input[N] did
            // not match any supported type` 22 times and `Invalid value:
            // 'tool'` twice on 2026-09-14, and the memory round was lost
            // each time. Reshape here, where the wire format is known.
            let items: Vec<serde_json::Value> = results
                .iter()
                .map(|r| {
                    match (
                        r.get("role").and_then(|v| v.as_str()),
                        r.get("tool_call_id"),
                    ) {
                        (Some("tool"), Some(id)) => {
                            crate::memory::tool_adapter::format_responses_tool_result(
                                id.as_str().unwrap_or(""),
                                r.get("content").and_then(|c| c.as_str()).unwrap_or(""),
                            )
                        }
                        _ => r.clone(),
                    }
                })
                .collect();
            serde_json::json!({"_openai_responses_tool_results": items})
        }
        _ => serde_json::json!({"_memory_tool_results": results}),
    }
}

/// Whether this tool-call name belongs to the proxy (a memory tool the proxy
/// injected, so the proxy must answer it) rather than to the client.
fn is_proxy_memory_call(name: &str) -> bool {
    crate::memory::tool_adapter::MEMORY_TOOL_NAMES.contains(&name)
        || name == crate::memory::tool_adapter::NATIVE_MEMORY_TOOL_NAME
}

/// Count the turn's calls that are NOT proxy memory tools, in the shape the
/// provider speaks. A turn mixing memory calls with client calls cannot be
/// continued on shapes without deferral: the client's calls have no results
/// yet, and appending the assistant turn would leave them unanswered
/// upstream (Zen refuses the continuation with 400 "No tool output found
/// for function call"). Anthropic is excluded by the caller — its deferral
/// holds the memory answer for the next request instead.
fn count_non_memory_calls(
    response: &serde_json::Value,
    provider: crate::memory::tool_adapter::Provider,
) -> usize {
    use crate::memory::tool_adapter::{extract_tool_calls, get_tool_name};
    extract_tool_calls(response, provider)
        .iter()
        .filter(|call| !is_proxy_memory_call(&get_tool_name(call, provider)))
        .count()
}

/// Answer memory calls in place as assistant prose, leaving every other call
/// untouched for the client. Mirrors `splice_ccr_results_as_text` for the
/// mixed-turn case no continuation can serve — with a memory wrapper, never
/// `<retrieved_context>`, so the Stop hook's retrieval branch cannot mistake
/// it for a spliced retrieval. Returns how many calls were replaced.
///
/// Only the shapes without deferral need this (the caller gates Anthropic
/// out): `openai_responses` replaces `function_call` items with `message`
/// items, the same item type a text answer arrives as; `openai` removes the
/// calls from `message.tool_calls` and appends the text to
/// `message.content`, which the client already renders.
fn splice_memory_results_as_text(
    response: &mut serde_json::Value,
    results: &[serde_json::Value],
    provider: &str,
) -> usize {
    // `handle_memory_tool_calls` formats every result Chat-shaped
    // (`role: tool` + `tool_call_id`), whatever the turn's own shape.
    fn result_text<'a>(results: &'a [serde_json::Value], id: &str) -> Option<&'a str> {
        results
            .iter()
            .find(|r| r.get("tool_call_id").and_then(|v| v.as_str()) == Some(id))
            .and_then(|r| r.get("content").and_then(|v| v.as_str()))
    }
    fn wrapped(text: &str) -> String {
        format!("<memory_context>\n{text}\n</memory_context>")
    }
    match provider {
        "openai_responses" => {
            let Some(items) = response.get_mut("output").and_then(|v| v.as_array_mut()) else {
                return 0;
            };
            let mut spliced = 0;
            for item in items.iter_mut() {
                if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
                    continue;
                }
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("id").and_then(|v| v.as_str()))
                    .unwrap_or_default();
                let Some(text) = result_text(results, call_id) else {
                    continue;
                };
                *item = serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": wrapped(text),
                    }],
                });
                spliced += 1;
            }
            spliced
        }
        "openai" => {
            let hits: Vec<(String, String)> = response
                .get("choices")
                .and_then(|v| v.as_array())
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("tool_calls"))
                .and_then(|v| v.as_array())
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|call| {
                            let id = call.get("id").and_then(|v| v.as_str())?;
                            let text = result_text(results, id)?;
                            Some((id.to_string(), text.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if hits.is_empty() {
                return 0;
            }
            let Some(message) = response
                .get_mut("choices")
                .and_then(|v| v.as_array_mut())
                .and_then(|c| c.first_mut())
                .and_then(|c| c.get_mut("message"))
            else {
                return 0;
            };
            if let Some(calls) = message.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                calls.retain(|call| {
                    let id = call.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    !hits.iter().any(|(hid, _)| hid == id)
                });
            }
            let joined = hits
                .iter()
                .map(|(_, text)| wrapped(text))
                .collect::<Vec<_>>()
                .join("\n");
            match message.get_mut("content") {
                Some(serde_json::Value::String(prev)) => {
                    if !prev.is_empty() {
                        *prev = format!("{prev}\n{joined}");
                    } else {
                        *prev = joined;
                    }
                }
                Some(serde_json::Value::Array(blocks)) => {
                    blocks.push(serde_json::json!({"type": "text", "text": joined}));
                }
                _ => {
                    message["content"] = serde_json::Value::String(joined);
                }
            }
            hits.len()
        }
        _ => 0,
    }
}

#[cfg(test)]
mod memory_mixed_turn_tests {
    use super::{count_non_memory_calls, splice_memory_results_as_text};
    use crate::memory::tool_adapter::Provider;
    use serde_json::json;

    fn responses_turn() -> serde_json::Value {
        json!({
            "output": [
                {"type": "function_call", "call_id": "call_mem", "name": "memory_search",
                 "arguments": "{\"query\":\"x\"}"},
                {"type": "function_call", "call_id": "call_client", "name": "Read",
                 "arguments": "{\"path\":\"f\"}"},
            ],
        })
    }

    fn chat_result(call_id: &str, content: &str) -> serde_json::Value {
        json!({"role": "tool", "tool_call_id": call_id, "content": content})
    }

    #[test]
    fn counts_only_client_calls() {
        assert_eq!(
            count_non_memory_calls(&responses_turn(), Provider::Openai),
            1
        );
        let pure = json!({
            "output": [
                {"type": "function_call", "call_id": "call_mem", "name": "memory_search",
                 "arguments": "{}"},
            ],
        });
        assert_eq!(count_non_memory_calls(&pure, Provider::Openai), 0);
    }

    #[test]
    fn responses_splice_replaces_only_the_memory_call() {
        let mut turn = responses_turn();
        let results = vec![chat_result("call_mem", "two hits")];
        assert_eq!(
            splice_memory_results_as_text(&mut turn, &results, "openai_responses"),
            1
        );
        let items = turn["output"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        // The memory call is now an answer-shaped message item; the client
        // call is untouched, with its identity intact for the client's run.
        assert_eq!(items[0]["type"], "message");
        assert!(items[0].to_string().contains("two hits"));
        assert!(!items[0].to_string().contains("retrieved_context"));
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call_client");
    }

    #[test]
    fn responses_splice_leaves_unmatched_calls_standing() {
        let mut turn = responses_turn();
        let results = vec![chat_result("call_other", "stray")];
        assert_eq!(
            splice_memory_results_as_text(&mut turn, &results, "openai_responses"),
            0
        );
        assert_eq!(turn["output"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn chat_splice_removes_the_memory_call_and_appends_text() {
        let mut turn = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "looking it up",
                    "tool_calls": [
                        {"id": "call_mem", "type": "function",
                         "function": {"name": "memory_search", "arguments": "{}"}},
                        {"id": "call_client", "type": "function",
                         "function": {"name": "Read", "arguments": "{}"}},
                    ],
                },
                "finish_reason": "tool_calls",
            }],
        });
        let results = vec![chat_result("call_mem", "two hits")];
        assert_eq!(
            splice_memory_results_as_text(&mut turn, &results, "openai"),
            1
        );
        let msg = &turn["choices"][0]["message"];
        let calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_client");
        assert!(msg["content"].as_str().unwrap().contains("two hits"));
    }
}

#[cfg(test)]
mod memory_trace_tests {
    use super::{memory_trace_lines, pending_memory_call_names};
    use crate::memory::tool_adapter::Provider;
    use serde_json::json;

    fn call(id: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    fn result(id: &str, content: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content.to_string(),
        })
    }

    #[test]
    fn search_reports_the_query_and_the_count() {
        let response =
            json!({"content": [call("t1", "memory_search", json!({"query": "raw_payloads"}))]});
        let results = vec![result("t1", json!({"status": "found", "count": 19}))];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_search(\"raw_payloads\") → 19 results"]
        );
    }

    #[test]
    fn save_reports_the_id_so_a_later_turn_can_quote_it() {
        let response = json!({"content": [call("t1", "memory_save", json!({"content": "the proxy runs on 8787"}))]});
        let results = vec![result(
            "t1",
            json!({"status": "saved", "memory_id": "m-42"}),
        )];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_save(\"the proxy runs on 8787\") → saved m-42"]
        );
    }

    #[test]
    fn one_result_is_singular() {
        let response = json!({"content": [call("t1", "memory_search", json!({"query": "q"}))]});
        let results = vec![result("t1", json!({"status": "found", "count": 1}))];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_search(\"q\") → 1 result"]
        );
    }

    #[test]
    fn an_error_says_so_rather_than_reading_as_a_hit() {
        let response = json!({"content": [call("t1", "memory_search", json!({"query": "q"}))]});
        let results = vec![result(
            "t1",
            json!({"status": "error", "error": "backend not initialized"}),
        )];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_search(\"q\") → error: backend not initialized"]
        );
    }

    #[test]
    fn a_call_nothing_answered_is_named_not_hidden() {
        let response = json!({"content": [call("t1", "memory_search", json!({"query": "q"}))]});
        assert_eq!(
            memory_trace_lines(&response, &[], Provider::Anthropic),
            vec!["memory_search(\"q\") → no answer"]
        );
    }

    #[test]
    fn client_tools_sharing_the_turn_are_not_ours_to_report() {
        let response = json!({
            "content": [
                call("t1", "Bash", json!({"command": "ls"})),
                call("t2", "memory_list", json!({})),
            ]
        });
        let results = vec![result("t2", json!({"status": "ok", "count": 44}))];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_list → 44 results"]
        );
    }

    #[test]
    fn results_are_paired_by_id_not_by_position() {
        let response = json!({
            "content": [
                call("t1", "memory_search", json!({"query": "first"})),
                call("t2", "memory_search", json!({"query": "second"})),
            ]
        });
        let results = vec![
            result("t2", json!({"status": "found", "count": 2})),
            result("t1", json!({"status": "found", "count": 7})),
        ];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec![
                "memory_search(\"first\") → 7 results",
                "memory_search(\"second\") → 2 results",
            ]
        );
    }

    #[test]
    fn a_long_query_is_clipped_on_a_char_boundary() {
        let query = "Ünicode ".repeat(20);
        let response = json!({"content": [call("t1", "memory_search", json!({"query": query}))]});
        let results = vec![result("t1", json!({"status": "found", "count": 0}))];
        let lines = memory_trace_lines(&response, &results, Provider::Anthropic);
        assert!(lines[0].starts_with("memory_search(\"Ünicode"), "{lines:?}");
        assert!(lines[0].ends_with("…\") → 0 results"), "{lines:?}");
    }

    #[test]
    fn pending_names_lists_only_unrun_memory_tools() {
        let response = json!({"content": [
            call("t1", "memory_search", json!({"query": "hosts"})),
            call("t2", "Read", json!({"file_path": "/tmp/x"})),
            call("t3", "memory_save", json!({"content": "a fact"})),
        ]});
        assert_eq!(
            pending_memory_call_names(&response, Provider::Anthropic),
            vec!["memory_search".to_string(), "memory_save".to_string()],
        );
    }

    #[test]
    fn pending_names_is_empty_without_memory_calls() {
        let response = json!({"content": [call("t1", "Read", json!({"file_path": "/tmp/x"}))]});
        assert!(pending_memory_call_names(&response, Provider::Anthropic).is_empty());
    }
}

#[cfg(test)]
mod memory_continuation_tests {
    use super::{
        append_stranded_notice, continuation_dangling_calls, excise_failed_memory_calls,
        memory_results_message, turn_has_visible_text,
    };

    #[test]
    fn memory_results_message_reshapes_chat_results_for_responses() {
        let results = vec![
            serde_json::json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"ok\":1}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "call_2", "output": "x"}),
        ];
        let msg = memory_results_message(&results, "openai_responses");
        let items = msg["_openai_responses_tool_results"].as_array().unwrap();
        assert_eq!(
            items[0],
            serde_json::json!({"type": "function_call_output", "call_id": "call_1", "output": "{\"ok\":1}"}),
            "Chat-shaped result becomes a Responses function_call_output"
        );
        assert_eq!(items[1], results[1], "already-Responses items pass through");

        let chat = memory_results_message(&results[..1], "openai");
        assert_eq!(
            chat["_memory_tool_results"][0]["role"], "tool",
            "Chat provider keeps the Chat shape"
        );
    }

    #[test]
    fn continuation_validation_passes_paired_bodies() {
        let responses = serde_json::json!({"input": [
            {"type": "function_call", "call_id": "c1", "name": "memory_search"},
            {"type": "function_call_output", "call_id": "c1", "output": "x"},
        ]});
        assert!(continuation_dangling_calls(&responses, "openai_responses").is_empty());

        let anthropic = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "memory_search"},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "x"},
            ]},
        ]});
        assert!(continuation_dangling_calls(&anthropic, "anthropic").is_empty());

        let chat = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": [{"id": "c1"}]},
            {"role": "tool", "tool_call_id": "c1", "content": "x"},
        ]});
        assert!(continuation_dangling_calls(&chat, "openai").is_empty());
    }

    #[test]
    fn continuation_validation_names_dangling_calls() {
        // The 2026-09-24 Zen shape: a call with no output.
        let responses = serde_json::json!({"input": [
            {"type": "function_call", "call_id": "call_01abad", "name": "memory_search"},
        ]});
        let d = continuation_dangling_calls(&responses, "openai_responses");
        assert_eq!(d, vec!["function_call call_01abad has no output"], "{d:?}");

        // A client call replayed without its result (the toolu_ class).
        let anthropic = serde_json::json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Bash"},
            ]},
            {"role": "user", "content": "oops"},
        ]});
        let d = continuation_dangling_calls(&anthropic, "anthropic");
        assert!(d.iter().any(|s| s.contains("toolu_1")), "{d:?}");

        // Server-run tools answer themselves: no result required.
        let server = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search"},
            ]},
            {"role": "user", "content": "done"},
        ]});
        assert!(continuation_dangling_calls(&server, "anthropic").is_empty());

        // Duplicate server ids in one turn are refused upstream.
        let dup = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search"},
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search"},
            ]},
            {"role": "user", "content": "done"},
        ]});
        let d = continuation_dangling_calls(&dup, "anthropic");
        assert!(d.iter().any(|s| s.contains("duplicate")), "{d:?}");

        // The Opus prefill class: nothing may trail the result.
        let prefill = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "prefill"},
        ]});
        let d = continuation_dangling_calls(&prefill, "anthropic");
        assert!(d.iter().any(|s| s.contains("assistant")), "{d:?}");
    }

    #[test]
    fn stranded_retirement_removes_only_memory_calls_and_notes_once() {
        let mut turn = serde_json::json!({"output": [
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "I'll search."}]},
            {"type": "function_call", "call_id": "c1", "name": "memory_search",
             "arguments": "{}"},
            {"type": "function_call", "call_id": "c2", "name": "Bash",
             "arguments": "{}"},
        ]});
        let removed = excise_failed_memory_calls(&mut turn, "openai_responses");
        assert_eq!(removed, vec!["memory_search"]);
        let items = turn["output"].as_array().unwrap();
        assert_eq!(items.len(), 2, "client call and text survive: {turn}");
        assert!(turn_has_visible_text(&turn, "openai_responses"));

        append_stranded_notice(
            &mut turn,
            "openai_responses",
            &crate::sse::ccr_stream::dropped_call_text(Some("memory_search")),
        );
        append_stranded_notice(
            &mut turn,
            "openai_responses",
            &crate::sse::ccr_stream::dropped_call_text(Some("memory_search")),
        );
        let texts: Vec<&str> = turn["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i.get("content").and_then(|c| c.as_array()))
            .flat_map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            })
            .filter(|t| t.contains("did NOT run"))
            .collect();
        assert_eq!(texts.len(), 1, "notices must not stack: {turn}");
    }
}

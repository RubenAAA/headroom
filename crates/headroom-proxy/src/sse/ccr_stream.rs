//! CCR retrieval on streamed Anthropic turns.
//!
//! # The gap this closes
//!
//! The proxy injects a `headroom_retrieve` tool into every intercepted
//! request (`proxy.rs`, gated on `--ccr-inject-tool`) so the model can ask for
//! content that compression offloaded. Answering that call — look the hash up
//! in the CCR store, hand the original back, let the model carry on — lived
//! only on the buffered branch of `forward_http`, which runs when the response
//! is *not* SSE. Every interactive client streams, so on those turns the proxy
//! advertised a tool and then let the call through to a client that had never
//! heard of it. Claude Code answers that with `No such tool available:
//! headroom_retrieve` and the turn dies.
//!
//! # The approach
//!
//! Sit between the upstream body and the client:
//!
//! - Forward everything the client can use, live and unbuffered.
//! - Swallow the events belonging to a `headroom_retrieve` block. The tool
//!   name arrives on `content_block_start`, before any of that block's bytes
//!   would go out, so nothing has to be retracted.
//! - Hold back `message_delta` and `message_stop`, the only two events that
//!   tell the client the turn is over.
//!
//! At end-of-stream, with no retrieval seen, the held-back events go out
//! verbatim and the client has received the upstream bytes unchanged. With a
//! retrieval seen, the accumulated stream state is rebuilt into the
//! non-streaming response shape and handed to the same
//! `handle_ccr_response` the buffered path uses — store lookup, mixed-tool
//! policy, round cap and usage accounting all come along. The resolved turn is
//! then synthesised back into SSE events numbered after the blocks the client
//! already has.
//!
//! # What the client sees
//!
//! One turn. Text the model wrote before reaching for the tool has already
//! streamed; the continuation's content follows it in the same message. The
//! retrieval round trip is invisible, which is the point — it is the proxy's
//! business, not the client's.
//!
//! # Which upstreams this serves
//!
//! Anything whose stream reaches the client as Anthropic `/v1/messages` SSE.
//! That is the Claude path directly, and the routed-model path once
//! `handlers::local_model` has translated an OpenAI stream into the Anthropic
//! vocabulary. Only the continuation differs between them — see [`CcrShape`].
//!
//! Clients that speak OpenAI natively (`/v1/chat/completions`,
//! `/v1/responses`) read different event vocabularies, which no rewriter
//! covers. Chat streams keep the guard: the injection site in `proxy.rs`
//! skips those tools when such a client asks for a stream, and they stay
//! skipped. Responses streams take the buffered path instead
//! (`openai_buffered_ccr`): upstream is called with `stream: false`, the
//! buffered arm resolves retrieval, and the final JSON is resynthesized as
//! SSE — so a Responses client is never handed an unanswerable tool either,
//! just without a live rewriter.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::sse::anthropic::{AnthropicStreamState, BlockState, StreamStatus};
use crate::sse::{SseEvent, SseFramer};

/// The tool the proxy injects and therefore has to answer itself.
const CCR_TOOL_NAME: &str = "headroom_retrieve";

/// Depth of the channel feeding the client. Matches the telemetry queue: deep
/// enough that a slow client does not stall the parse loop for a chunk or two,
/// shallow enough that backpressure still reaches upstream.
const CLIENT_QUEUE_DEPTH: usize = 64;

/// Which shape the upstream speaks, and therefore what a continuation round
/// has to look like.
///
/// The client is Anthropic-shaped either way — this rewriter only ever runs
/// on a stream the client reads as `/v1/messages` SSE. What differs is the
/// upstream behind it.
pub(crate) enum CcrShape {
    /// Upstream speaks Anthropic messages. The rebuilt turn and the
    /// continuation request go back to it as they are.
    Anthropic,
    /// Upstream is a routed model speaking OpenAI chat-completions, reached
    /// through `handlers::local_model`. The turn is converted into that shape
    /// for the continuation round and converted back for the client.
    RoutedChat {
        /// The client's original Anthropic request, needed to translate the
        /// continuation's OpenAI response back into the Anthropic shape.
        anthropic_request: Value,
    },
    /// Upstream is a routed model speaking the OpenAI Responses API, whose
    /// turn is a flat `output[]` array rather than `choices[].message`.
    RoutedResponses { anthropic_request: Value },
}

/// Everything the rewriter needs to run a continuation round.
pub(crate) struct CcrStreamContext {
    pub client: reqwest::Client,
    pub upstream_url: url::Url,
    pub outgoing_headers: http::HeaderMap,
    /// The request as forwarded upstream, in that upstream's own shape.
    /// `stream` is forced off on the copy used for continuations so those
    /// rounds come back as plain JSON.
    pub forwarded_request: Bytes,
    pub ccr_store: Arc<dyn headroom_core::ccr::CcrStore>,
    /// Per-project content stores, for the cold-tier lookup when `ccr_store`
    /// has expired a block. `None` disables the fallback.
    pub ccr_stores: Option<Arc<crate::ctx::projects::ProjectStores>>,
    pub config: Arc<crate::config::Config>,
    pub request_id: String,
    pub shape: CcrShape,
    /// Present when memory tools were injected into this request. The proxy
    /// runs those too, for the same reason it runs `headroom_retrieve`.
    pub memory: Option<crate::proxy::MemoryToolContext>,
    /// Redaction memory for continuations this turn (see
    /// [`crate::routed::ccr::RoutedCcr::redact`]). `None` on paths
    /// that never redact, where continuations pass through.
    pub redact: Option<crate::redact::RedactRef>,
    /// Booking-owned rounds handle for streamed routed turns: when set, the
    /// rewriter populates this handle (instead of a fresh one) so the
    /// completion guard books first-round usage and rounds together. `None`
    /// everywhere else; the returned handle is then the only copy.
    pub rounds_sink: Option<Arc<Mutex<crate::proxy::CcrRoundUsage>>>,
}

/// Convert a rebuilt Anthropic assistant turn into the OpenAI
/// chat-completions response shape.
///
/// Only what the CCR handler reads: the text and the tool calls. This exists
/// because the routed path's stream arrives already translated to Anthropic
/// events, while its continuation has to go back to an OpenAI upstream.
pub(crate) fn anthropic_turn_as_openai_response(message: &Value) -> Value {
    let empty = Vec::new();
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let text: String = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();

    let tool_calls: Vec<Value> = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|b| {
            json!({
                "id": b.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": b.get("name").cloned().unwrap_or(Value::Null),
                    // OpenAI carries arguments as a JSON *string*, not an
                    // object. Handing it the object makes the hash invisible
                    // to `parse_ccr_tool_calls`.
                    "arguments": serde_json::to_string(
                        &b.get("input").cloned().unwrap_or_else(|| json!({}))
                    ).unwrap_or_else(|_| "{}".into()),
                },
            })
        })
        .collect();

    let mut msg = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        msg["tool_calls"] = json!(tool_calls);
    }
    json!({
        "choices": [{
            "index": 0,
            "message": msg,
            "finish_reason": if tool_calls.is_empty() { "stop" } else { "tool_calls" },
        }],
        "usage": message.get("usage").cloned().unwrap_or_else(|| json!({})),
    })
}

/// Convert a rebuilt Anthropic assistant turn into the OpenAI Responses
/// `output[]` shape — flat `function_call` items, no `choices` wrapper.
pub(crate) fn anthropic_turn_as_responses_output(message: &Value) -> Value {
    let empty = Vec::new();
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let mut output = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => output.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
                }],
            })),
            Some("tool_use") => output.push(json!({
                "type": "function_call",
                "call_id": block.get("id").cloned().unwrap_or(Value::Null),
                "name": block.get("name").cloned().unwrap_or(Value::Null),
                "arguments": serde_json::to_string(
                    &block.get("input").cloned().unwrap_or_else(|| json!({}))
                ).unwrap_or_else(|_| "{}".into()),
            })),
            _ => {}
        }
    }

    json!({
        "output": output,
        "usage": message.get("usage").cloned().unwrap_or_else(|| json!({})),
    })
}

/// Convert a resolved Responses `output[]` turn back into the Anthropic shape
/// the client's stream is synthesised from.
pub(crate) fn responses_output_as_anthropic_turn(resolved: &Value, original: &Value) -> Value {
    let empty = Vec::new();
    let items = resolved
        .get("output")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let mut content = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let text: String = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| {
                                p.get("text").and_then(Value::as_str).or_else(|| {
                                    // A refusal part is the turn's only text;
                                    // without this the client receives an empty
                                    // `end_turn` it cannot tell from silence.
                                    p.get("refusal").and_then(Value::as_str)
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
            }
            Some("function_call") => {
                let id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                // A call without identity cannot round-trip: emitting it as a
                // null-id `tool_use` has the client discard the whole turn.
                // The stop reason below derives from surviving content, so the
                // dropped call downgrades the turn instead of killing it.
                if id.is_empty() || name.is_empty() {
                    continue;
                }
                content.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| json!({})),
                }))
            }
            _ => {}
        }
    }

    let usage = resolved.get("usage").cloned().unwrap_or_else(|| json!({}));
    // A client tool call that survives resolution needs `tool_use` here, or the
    // client reads the turn as finished and never runs it. Only the proxy's own
    // calls get stripped, so whatever is left in `content` belongs to the client.
    let stop_reason = if content
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        "tool_use"
    } else {
        "end_turn"
    };
    json!({
        "id": resolved.get("id").cloned().unwrap_or_else(|| json!("")),
        "type": "message",
        "role": "assistant",
        "model": original.get("model").cloned().unwrap_or_else(|| json!("unknown")),
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.get("input_tokens").cloned().unwrap_or_else(|| json!(0)),
            "output_tokens": usage.get("output_tokens").cloned().unwrap_or_else(|| json!(0)),
        },
    })
}

/// Serialise one SSE event the way Anthropic frames them.
fn event_bytes(name: &str, data: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(name.len() + data.len() + 16);
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\n\n");
    Bytes::from(out)
}

/// Re-frame a parsed event without touching its payload.
fn reframe(ev: &SseEvent) -> Bytes {
    event_bytes(ev.event_name.as_deref().unwrap_or("message"), &ev.data)
}

/// Rebuild one content block from its accumulated deltas.
///
/// Starts from the `content_block` object as it arrived on
/// `content_block_start` so block types this proxy has never heard of survive
/// with their fields intact, and fills in only what the deltas carried.
fn block_to_value(block: &BlockState) -> Value {
    let mut v = block.metadata.clone();
    if !v.is_object() {
        v = json!({ "type": block.block_type });
    }
    let Some(obj) = v.as_object_mut() else {
        return v;
    };
    match block.block_type.as_str() {
        "text" => {
            obj.insert("text".into(), json!(block.text_buffer));
            if !block.citations.is_empty() {
                obj.insert("citations".into(), json!(block.citations));
            }
        }
        "thinking" => {
            obj.insert("thinking".into(), json!(block.text_buffer));
            if let Some(sig) = &block.signature {
                obj.insert("signature".into(), json!(sig));
            }
        }
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            // `input` streams as a JSON fragment; an empty buffer means the
            // model sent no arguments, which is `{}`, not a parse failure.
            let input = if block.partial_json.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&block.partial_json).unwrap_or_else(|_| json!({}))
            };
            obj.insert("input".into(), input);
        }
        _ => {}
    }
    v
}

/// Rebuild the non-streaming response shape from accumulated stream state.
///
/// This is what lets a streamed turn reuse the buffered CCR path: that code
/// reads a `messages` response, and after `message_delta` the stream state
/// holds every field one has.
pub(crate) fn rebuild_message(state: &AnthropicStreamState) -> Value {
    let mut indices: Vec<&usize> = state.blocks.keys().collect();
    indices.sort();
    let content: Vec<Value> = indices
        .iter()
        .filter_map(|i| state.blocks.get(*i))
        .map(block_to_value)
        .collect();

    json!({
        "id": state.message_id.clone().unwrap_or_default(),
        "type": "message",
        "role": "assistant",
        "model": state.model.clone().unwrap_or_default(),
        "content": content,
        "stop_reason": state.stop_reason.clone(),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": state.usage.input_tokens,
            "output_tokens": state.usage.output_tokens,
            "cache_read_input_tokens": state.usage.cache_read_input_tokens,
            "cache_creation_input_tokens": state.usage.cache_creation_input_tokens,
        },
    })
}

/// Fold a finished Anthropic SSE body into the turn JSON the CCR machinery
/// speaks.
///
/// Continuation rounds stream (see [`resolve_retrieval`]), so their response
/// arrives as the same event sequence a client turn does. The parser and the
/// rebuild are the ones round one already runs, fed a complete body instead of
/// a live stream.
///
/// `None` when the body carried no usable turn, which the caller handles as it
/// handles any unparseable continuation. A stream that stopped short counts as
/// that: an `error` event says upstream gave up partway, and a missing
/// `stop_reason` says the body ended before `message_delta` did. Either way the
/// blocks that arrived are not the model's answer, and splicing them would
/// truncate the turn where a buffered round would have failed to parse.
///
/// One thing streaming gives up: an overload that lands after the headers
/// arrives as an `error` event inside a 200, so the round ends here instead of
/// on a 5xx the send loop would have retried. The trade is deliberate — every
/// continuation failure measured so far was a timeout, not an overload.
pub(crate) fn anthropic_stream_to_turn(body: &[u8]) -> Option<Value> {
    let mut framer = SseFramer::new();
    framer.push(body);
    let mut state = AnthropicStreamState::new();
    while let Some(event) = framer.next_event() {
        let Ok(event) = event else { continue };
        let _ = state.apply(event);
    }
    if matches!(state.status, StreamStatus::Errored) {
        return None;
    }
    let turn = rebuild_message(&state);
    let complete = turn
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|c| !c.is_empty())
        && turn.get("stop_reason").is_some_and(Value::is_string);
    complete.then_some(turn)
}

/// Every tool this proxy injects and therefore has to answer itself.
///
/// This is the whole invariant in one list: a name here must have a resolver
/// in [`resolve_proxy_tools`], and a tool injected into a request must appear
/// here. Advertising a tool the client cannot run is the bug both halves
/// exist to prevent.
fn proxy_owned_tool(block: &Value, memory_enabled: bool) -> bool {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return false;
    }
    let Some(name) = block.get("name").and_then(Value::as_str) else {
        return false;
    };
    name == CCR_TOOL_NAME
        || (memory_enabled && crate::memory::tool_adapter::MEMORY_TOOL_NAMES.contains(&name))
}

/// Whether a resolved block is reasoning the continuation call produced.
///
/// Anthropic signs a `thinking` block against the request that produced it and
/// verifies that signature when the block comes back. The continuation is a
/// different request, so its reasoning can never verify inside the
/// conversation the client replays — and the client has already had this
/// turn's own reasoning off the live stream.
fn continuation_thinking(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking") | Some("redacted_thinking")
    )
}

/// Whether the client already received this block on the live stream.
///
/// Matched on `id` where a block has one, so a `tool_use` rebuilt from
/// accumulated deltas still counts as the same block, and on full equality
/// otherwise.
fn already_streamed(block: &Value, live: &[Value]) -> bool {
    let id = block.get("id").and_then(Value::as_str);
    live.iter()
        .any(|seen| match (id, seen.get("id").and_then(Value::as_str)) {
            (Some(a), Some(b)) => a == b,
            _ => seen == block,
        })
}

/// Why a block from the retrieval continuation must not reach the client.
///
/// Recorded per reason rather than as one total, because they do not mean the
/// same thing. `UnresolvedProxyTool` is routine — a retrieval the proxy could
/// not run. The other two are the shapes that made this proxy emit turns the
/// API then refused on the *following* request, at a distance of one turn from
/// their cause; a rise in either is the signal that the splice is putting
/// unusable content back on the wire again.
#[derive(Clone, Copy)]
enum DropReason {
    UnresolvedProxyTool = 0,
    ContinuationThinking = 1,
    AlreadyStreamed = 2,
    DeferredMemoryAnswer = 3,
}

impl DropReason {
    /// Indexed by the discriminant, so `ALL[r as usize] == r`.
    const ALL: [DropReason; 4] = [
        DropReason::UnresolvedProxyTool,
        DropReason::ContinuationThinking,
        DropReason::AlreadyStreamed,
        DropReason::DeferredMemoryAnswer,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::UnresolvedProxyTool => "unresolved_proxy_tool",
            Self::ContinuationThinking => "continuation_thinking",
            Self::AlreadyStreamed => "already_streamed",
            Self::DeferredMemoryAnswer => "deferred_memory_answer",
        }
    }
}

/// What to tell the user when the splice leaves a turn with nothing in it.
///
/// Naming the wrong cause is worse than naming none: someone reading "context
/// retrieval" goes looking at the retrieval machinery, which in most of these
/// turns did nothing wrong. So the message names the tool that was actually
/// dropped, and says nothing about retrieval when no retrieval was dropped.
/// Marker the Stop hook matches to continue a retrieval-ended turn.
/// Shared with `proxy.rs`, which retires deterministically-failed memory
/// calls with the same marker so the hook fires whichever path owns the
/// turn.
///
/// The hook (`retry-dropped-turn.sh`) greps the transcript tail for this
/// literal string, the same way it matches TRUNCATION_MARKER. Plain prose —
/// including the apology above — cannot serve: this session proved a reply
/// quoting the apology re-arms the hook and blocks the next stop. The marker
/// is bracketed and names headroom so ordinary prose never contains it.
pub(crate) const RETRIEVAL_DROPPED_MARKER: &str =
    "[headroom: a proxy tool call was dropped and did NOT run; re-issue it]";

/// Client-visible prose for a turn whose continuation came back carrying
/// nothing at all.
///
/// Distinct from [`empty_turn_text`], which covers a tool call the proxy could
/// not run. Here the call ran and its answer was fetched; the continuation that
/// was supposed to deliver it returned no content, so the answer exists and the
/// turn does not carry it. Saying "ask again" is still the right advice, but
/// the reason differs and the log line differs with it.
fn lost_answer_text(tool: Option<&str>) -> String {
    match tool {
        Some(name) => format!(
            "The proxy ran `{name}` for this turn, but the model's follow-up \
             came back empty, so the answer is missing. Ask again.\n\n{RETRIEVAL_DROPPED_MARKER}"
        ),
        None => format!(
            "The proxy resolved a tool call for this turn, but the model's \
             follow-up came back empty, so the answer is missing. Ask \
             again.\n\n{RETRIEVAL_DROPPED_MARKER}"
        ),
    }
}

/// Client-visible prose for a dropped tool call on a turn that did carry other
/// text. [`empty_turn_text`] says the turn "came back empty", which is false
/// here and contradicts the model's own words sitting right above it.
/// Shared with `proxy.rs`, which retires deterministically-failed memory
/// calls with the same wording (one notice, whichever path owns the turn).
pub(crate) fn dropped_call_text(unresolved_tool: Option<&str>) -> String {
    match unresolved_tool {
        Some(name) => format!(
            "The proxy could not run `{name}` for this turn, so its answer is \
             missing from the reply above. Nothing was lost; ask \
             again.\n\n{RETRIEVAL_DROPPED_MARKER}"
        ),
        None => format!(
            "The proxy could not run a tool call for this turn, so its answer \
             is missing from the reply above. Nothing was lost; ask \
             again.\n\n{RETRIEVAL_DROPPED_MARKER}"
        ),
    }
}

pub(crate) fn empty_turn_text(unresolved_tool: Option<&str>) -> String {
    match unresolved_tool {
        Some(name) => format!(
            "The proxy could not run `{name}` for this turn, so the turn came \
             back empty. Nothing was lost; ask again.\n\n{RETRIEVAL_DROPPED_MARKER}"
        ),
        None => format!(
            "The upstream ended this turn on a tool call that carried no \
             content. Nothing was lost; ask again.\n\n{RETRIEVAL_DROPPED_MARKER}"
        ),
    }
}

/// Client-visible prose. Thinking is not visible: Claude Code renders it as
/// "Thought for Ns" and then an empty turn. Whitespace-only text is the
/// same as none.
fn is_visible_text_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty())
}

/// True when the client would otherwise see no prose for this turn:
/// nothing streamed live except thinking, and nothing left to splice.
fn turn_lacks_visible_text(emit: &[Value], client_saw_visible_text: bool) -> bool {
    !client_saw_visible_text && !emit.iter().any(is_visible_text_block)
}

/// Whether this block's answer is already waiting for a later request.
///
/// A turn that calls a memory tool *and* a client tool cannot be continued, so
/// the proxy runs the memory call and holds the answer for the request that
/// carries the client's `tool_result` (see [`crate::memory::deferred`]).
/// Suppressing the `tool_use` is that design working, not a tool call lost —
/// and logging the two the same way left no way to tell a real stranding from
/// a correct deferral.
fn deferred_answer_held(block: &Value) -> bool {
    let Some(id) = block.get("id").and_then(Value::as_str) else {
        return false;
    };
    crate::memory::deferred::store()
        .lock()
        .map(|store| store.is_held(id))
        .unwrap_or(false)
}

/// The splice's filter. `None` means the block is safe to send on.
fn drop_reason(block: &Value, memory_enabled: bool, live: &[Value]) -> Option<DropReason> {
    if proxy_owned_tool(block, memory_enabled) {
        if deferred_answer_held(block) {
            Some(DropReason::DeferredMemoryAnswer)
        } else {
            Some(DropReason::UnresolvedProxyTool)
        }
    } else if continuation_thinking(block) {
        Some(DropReason::ContinuationThinking)
    } else if already_streamed(block, live) {
        Some(DropReason::AlreadyStreamed)
    } else {
        None
    }
}

/// Turn a resolved message into SSE events, numbered from `start_index`.
///
/// Blocks are opened empty and filled by a delta, which is the shape the wire
/// format specifies and the shape clients are built to parse. Emitting a
/// populated `content_block_start` would be shorter and is not worth the bet.
pub(crate) fn synthesize_blocks(content: &[Value], start_index: usize) -> Vec<Bytes> {
    let mut out = Vec::new();
    for (offset, block) in content.iter().enumerate() {
        let index = start_index + offset;
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("text");
        let (shell, delta) = match block_type {
            "text" => (
                json!({"type": "text", "text": ""}),
                Some(json!({
                    "type": "text_delta",
                    "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
                })),
            ),
            "thinking" => (
                json!({"type": "thinking", "thinking": ""}),
                Some(json!({
                    "type": "thinking_delta",
                    "thinking": block.get("thinking").and_then(Value::as_str).unwrap_or(""),
                })),
            ),
            "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let mut shell = block.clone();
                if let Some(obj) = shell.as_object_mut() {
                    obj.insert("input".into(), json!({}));
                }
                (
                    shell,
                    Some(json!({
                        "type": "input_json_delta",
                        "partial_json": serde_json::to_string(&input)
                            .unwrap_or_else(|_| "{}".into()),
                    })),
                )
            }
            // Anything else (redacted_thinking and whatever ships next) goes
            // out whole on the start event: there is no delta vocabulary for
            // it to be split into.
            _ => (block.clone(), None),
        };

        out.push(event_bytes(
            "content_block_start",
            &serde_json::to_vec(&json!({
                "type": "content_block_start",
                "index": index,
                "content_block": shell,
            }))
            .unwrap_or_default(),
        ));
        if let Some(delta) = delta {
            out.push(event_bytes(
                "content_block_delta",
                &serde_json::to_vec(&json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": delta,
                }))
                .unwrap_or_default(),
            ));
        }
        // A thinking block's signature rides its own delta and must survive
        // byte-equal: Anthropic verifies it on the next call.
        if block_type == "thinking" {
            if let Some(sig) = block.get("signature").and_then(Value::as_str) {
                out.push(event_bytes(
                    "content_block_delta",
                    &serde_json::to_vec(&json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "signature_delta", "signature": sig},
                    }))
                    .unwrap_or_default(),
                ));
            }
        }
        out.push(event_bytes(
            "content_block_stop",
            &serde_json::to_vec(&json!({
                "type": "content_block_stop",
                "index": index,
            }))
            .unwrap_or_default(),
        ));
    }
    out
}

/// Whether the terminal `stop_reason` still claims a tool call the client
/// will never get.
///
/// Upstream sets `tool_use` when the model called a tool, but the splice may
/// have dropped that block — an unresolved proxy tool, most often. The client
/// treats the pair "stop_reason: tool_use, no tool_use block" as a malformed
/// turn and discards the whole thing, so the reason has to follow the content.
pub(crate) fn stop_reason_overclaims_tool_call(
    resolved_stop: Option<&str>,
    client_has_tool_call: bool,
) -> bool {
    resolved_stop == Some("tool_use") && !client_has_tool_call
}

/// The closing `message_delta` + `message_stop` for a synthesised turn.
///
/// Usage comes from the final round, matching what the buffered path returns
/// to the client. The rounds this replaced are accounted separately through
/// [`crate::proxy::CcrRoundUsage`] so nothing is counted twice.
pub(crate) fn synthesize_terminal(message: &Value) -> Vec<Bytes> {
    let usage = message.get("usage").cloned().unwrap_or_else(|| json!({}));
    let stop_reason = message.get("stop_reason").cloned().unwrap_or(Value::Null);
    vec![
        event_bytes(
            "message_delta",
            &serde_json::to_vec(&json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
                "usage": usage,
            }))
            .unwrap_or_default(),
        ),
        event_bytes(
            "message_stop",
            &serde_json::to_vec(&json!({"type": "message_stop"})).unwrap_or_default(),
        ),
    ]
}

/// Per-stream bookkeeping for the rewrite.
struct Rewriter {
    /// Whether memory tools count as proxy-owned on this turn. False when
    /// memory is off, so those names stay the client's business.
    memory_enabled: bool,
    state: AnthropicStreamState,
    /// Upstream block index → the index the client was given. They diverge
    /// once a block has been suppressed.
    index_map: HashMap<usize, usize>,
    suppressed: HashSet<usize>,
    /// Next free client-facing block index.
    next_client_index: usize,
    /// Whether a `tool_use` block has already gone out to the client. The
    /// terminal `stop_reason` has to agree with this or the client rejects
    /// the turn.
    client_saw_tool_use: bool,
    /// Whether a `text` block has already gone out to the client. Thinking
    /// does not count: the client renders it as a timer and then an empty
    /// turn, which is the 2026-09-22 Zen memory-continuation 403 symptom.
    /// Set on any `text` start, not on non-empty content: the start block
    /// is usually empty and the prose arrives in later deltas, which do not
    /// revisit this flag — content-checking here would miss real text.
    client_saw_visible_text: bool,
    /// `message_delta` / `message_stop`, held until we know whether a
    /// continuation has to be spliced in ahead of them.
    withheld: Vec<Bytes>,
    saw_ccr: bool,
}

impl Rewriter {
    fn new(memory_enabled: bool) -> Self {
        Self {
            memory_enabled,
            state: AnthropicStreamState::new(),
            index_map: HashMap::new(),
            suppressed: HashSet::new(),
            next_client_index: 0,
            client_saw_tool_use: false,
            client_saw_visible_text: false,
            withheld: Vec::new(),
            saw_ccr: false,
        }
    }

    /// Decide what the client should receive for one upstream event.
    ///
    /// Telemetry state is fed from the *upstream* event in every case,
    /// including suppressed ones, because the continuation has to be rebuilt
    /// from a complete picture of the turn.
    fn handle(&mut self, ev: SseEvent) -> Vec<Bytes> {
        let parsed: Value = serde_json::from_slice(&ev.data).unwrap_or(Value::Null);
        // Borrow the kind string: the old `.to_string()` allocated once per
        // event (~25ns × ~500 events/stream) for a value only matched on.
        let kind = parsed.get("type").and_then(Value::as_str).unwrap_or("");

        // Feed the state machine first; a parse failure there is not a reason
        // to drop the byte path.
        let _ = self.state.apply(SseEvent {
            event_name: ev.event_name.clone(),
            data: ev.data.clone(),
        });

        match kind {
            "content_block_start" => {
                let index = parsed.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = parsed.get("content_block").cloned().unwrap_or(Value::Null);
                if proxy_owned_tool(&block, self.memory_enabled) {
                    self.suppressed.insert(index);
                    self.saw_ccr = true;
                    return Vec::new();
                }
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    self.client_saw_tool_use = true;
                }
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    self.client_saw_visible_text = true;
                }
                let client_index = self.next_client_index;
                self.next_client_index += 1;
                self.index_map.insert(index, client_index);
                vec![self.forward_with_index(&ev, &parsed, index, client_index)]
            }
            "content_block_delta" | "content_block_stop" => {
                let index = parsed.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if self.suppressed.contains(&index) {
                    return Vec::new();
                }
                let client_index = self.index_map.get(&index).copied().unwrap_or(index);
                vec![self.forward_with_index(&ev, &parsed, index, client_index)]
            }
            "message_delta" | "message_stop" => {
                self.withheld.push(reframe(&ev));
                Vec::new()
            }
            _ => vec![reframe(&ev)],
        }
    }

    /// Forward an indexed event, renumbering only when it actually moved.
    /// Until the first suppression the mapping is the identity and the
    /// original payload goes out untouched.
    fn forward_with_index(
        &self,
        ev: &SseEvent,
        parsed: &Value,
        index: usize,
        client_index: usize,
    ) -> Bytes {
        if index == client_index {
            return reframe(ev);
        }
        let mut rewritten = parsed.clone();
        if let Some(obj) = rewritten.as_object_mut() {
            obj.insert("index".into(), json!(client_index));
        }
        match serde_json::to_vec(&rewritten) {
            Ok(data) => event_bytes(ev.event_name.as_deref().unwrap_or("message"), &data),
            Err(_) => reframe(ev),
        }
    }
}

/// Wrap an Anthropic SSE body so `headroom_retrieve` calls are answered here
/// instead of reaching the client.
///
/// Returns the rewritten stream and a handle to the usage of any continuation
/// rounds, which the caller folds into the request outcome. The handle stays
/// zeroed on every turn that does not retrieve.
/// Generic over the stream's error type: the Anthropic path feeds it
/// `reqwest::Error` straight from the upstream body, the routed path feeds it
/// `std::io::Error` out of its OpenAI→Anthropic translator. Errors are only
/// ever forwarded, never inspected.
pub(crate) fn rewrite_anthropic_stream<S, E>(
    upstream: S,
    ctx: CcrStreamContext,
) -> (
    impl Stream<Item = Result<Bytes, E>>,
    Arc<Mutex<crate::proxy::CcrRoundUsage>>,
)
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    let round_usage = ctx
        .rounds_sink
        .clone()
        .unwrap_or_else(|| Arc::new(Mutex::new(crate::proxy::CcrRoundUsage::default())));
    let usage_handle = round_usage.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, E>>(CLIENT_QUEUE_DEPTH);

    tokio::spawn(async move {
        let mut upstream = Box::pin(upstream);
        let mut framer = SseFramer::new();
        // Memory tools are injected whenever this turn carries a memory
        // context, so the same condition has to decide whether their blocks are
        // proxy-owned. Hardcoding false here advertised tools to the client
        // that only the proxy can run, and the client answered with
        // "No such tool available: memory_search".
        let mut rw = Rewriter::new(ctx.memory.is_some());

        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    // Upstream broke mid-turn. Pass the error on; the client's
                    // stream ends the way it would have without us.
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            framer.push(&chunk);
            while let Some(ev) = framer.next_event() {
                let Ok(ev) = ev else {
                    continue;
                };
                for out in rw.handle(ev) {
                    if tx.send(Ok(out)).await.is_err() {
                        // Client hung up. Nothing left to write to.
                        return;
                    }
                }
            }
        }

        // Nothing was retrieved: release the terminal events and the client
        // has had the upstream turn, unchanged.
        if !rw.saw_ccr {
            for ev in rw.withheld {
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            return;
        }

        let resolved = resolve_retrieval(&ctx, &rw, &round_usage).await;
        let content = resolved
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // Three kinds of block must not reach the client here:
        //
        // - A `headroom_retrieve` still standing means the continuation could
        //   not run it — mixed with a client tool call, out of rounds, or
        //   upstream refused. Emitting it would reproduce the bug this module
        //   exists to fix.
        // - A `thinking` block from the continuation carries a signature
        //   Anthropic issued for the *continuation* request. The client stores
        //   it against *this* conversation and replays it next turn, where it
        //   cannot verify: the API rejects with "thinking or redacted_thinking
        //   blocks in the latest assistant message cannot be modified". The
        //   live stream already gave the client this turn's real reasoning.
        // - A block the client already received live. When the continuation
        //   cannot run, the handler returns the turn unchanged, so every block
        //   would go out a second time under a new index — including a
        //   `tool_use` repeating an id the client is already acting on.
        let memory_enabled = ctx.memory.is_some();
        let live_blocks = rebuild_message(&rw.state)
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut emit: Vec<Value> = Vec::with_capacity(content.len());
        let mut dropped = [0usize; DropReason::ALL.len()];
        // The first one is enough to name in the message; a turn dropping two
        // different proxy tools has the same cause as one dropping either.
        let mut unresolved_tool: Option<String> = None;
        for block in content {
            match drop_reason(&block, memory_enabled, &live_blocks) {
                Some(reason) => {
                    dropped[reason as usize] += 1;
                    if matches!(reason, DropReason::UnresolvedProxyTool)
                        && unresolved_tool.is_none()
                    {
                        unresolved_tool = block
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
                None => emit.push(block),
            }
        }
        if dropped.iter().any(|&n| n > 0) {
            for (i, &count) in dropped.iter().enumerate() {
                if count > 0 {
                    crate::observability::ccr_splice::observe_dropped(
                        DropReason::ALL[i].label(),
                        count as u64,
                    );
                }
            }
            // Only an unresolved proxy tool is a fault. The other two are the
            // splice working: a continuation round's thinking and a block the
            // client already has must not go out twice. Measured over
            // 2026-08-23, 209 of 215 events were continuation thinking alone,
            // which drowned the 2 that mattered.
            if dropped[DropReason::UnresolvedProxyTool as usize] > 0 {
                tracing::warn!(
                    event = "ccr_tool_call_dropped",
                    request_id = %ctx.request_id,
                    unresolved_proxy_tool = dropped[DropReason::UnresolvedProxyTool as usize],
                    unresolved_tool_name = ?unresolved_tool,
                    continuation_thinking = dropped[DropReason::ContinuationThinking as usize],
                    already_streamed = dropped[DropReason::AlreadyStreamed as usize],
                    deferred_memory_answer = dropped[DropReason::DeferredMemoryAnswer as usize],
                    "ccr: dropped a proxy tool call the client expected; the turn \
                     promises a tool_use block that will not arrive"
                );
            } else {
                tracing::debug!(
                    request_id = %ctx.request_id,
                    continuation_thinking = dropped[DropReason::ContinuationThinking as usize],
                    already_streamed = dropped[DropReason::AlreadyStreamed as usize],
                    deferred_memory_answer = dropped[DropReason::DeferredMemoryAnswer as usize],
                    "ccr: dropping blocks the client must not receive from a streamed turn"
                );
            }
        }

        // A proxy tool we could not resolve is dropped above, but the turn
        // still carries `stop_reason: tool_use` from upstream. That pair —
        // a promised tool call with no `tool_use` block — is what the client
        // reports as "the model's tool call could not be parsed", killing the
        // whole turn. The buffered path derives the stop reason from surviving
        // content (see `resolved_message`); the streamed path has to do the
        // same, counting blocks already on their way to the client.
        let client_has_tool_call = rw.client_saw_tool_use
            || emit
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"));
        let mut resolved = resolved;
        if stop_reason_overclaims_tool_call(
            resolved.get("stop_reason").and_then(Value::as_str),
            client_has_tool_call,
        ) {
            tracing::warn!(
                event = "ccr_tool_call_dropped_stop_reason_downgraded",
                request_id = %ctx.request_id,
                unresolved_proxy_tool = dropped[DropReason::UnresolvedProxyTool as usize],
                unresolved_tool_name = ?unresolved_tool,
                "ccr: turn promised a tool call the client will not receive; \
                 downgrading stop_reason to end_turn"
            );
            resolved["stop_reason"] = json!("end_turn");
            // Without this the client renders an empty turn and the failure
            // looks like the model simply said nothing.
            //
            // Two guards, both learned from this firing wrongly. The turn has
            // to lack *visible text* to the client, not merely to this round
            // — text already streamed is content, and appending an apology
            // under it told the user a turn had failed when they had just
            // watched it succeed. Thinking is not visible text: on
            // 2026-09-22 a Zen memory continuation 403 left a thinking
            // block on the wire (`next_client_index > 0`) and the previous
            // `next_client_index == 0` guard skipped the notice, so the
            // client showed "Thought for 27s" then nothing. And the wording
            // has to match the cause: over 2026-08-28, five of the six
            // firings had `unresolved_proxy_tool == 0`, so five users were
            // told a retrieval had failed when no retrieval was dropped at
            // all. Bytes already on the wire still cannot become a 5xx; this
            // splices a text block at the next client index.
            // The client is told the call did not run whatever else the turn
            // carried. Gating this on "the turn has no visible text at all"
            // meant a turn that said "I'll search memory" and then lost the
            // call went out looking like the model had simply finished —
            // measured 2026-09-22 on Spark over Zen, where that is the common
            // case rather than the corner one. Only the wording depends on
            // whether anything else arrived.
            // ... except when nothing was actually dropped. An in-place
            // answer (e.g. a retrieval miss served from the failure text)
            // leaves no unresolved proxy tool; pushing the notice then claims
            // a drop that never happened and arms the retry hook on an
            // already-complete turn. Only an unresolved proxy tool is a fault.
            let nothing_dropped = dropped[DropReason::UnresolvedProxyTool as usize] == 0;
            let notice = if nothing_dropped && !emit.is_empty() {
                // Answered in place with visible text: nothing to say.
                String::new()
            } else if turn_lacks_visible_text(&emit, rw.client_saw_visible_text) {
                empty_turn_text(unresolved_tool.as_deref())
            } else {
                dropped_call_text(unresolved_tool.as_deref())
            };
            if !notice.is_empty() {
                emit.push(json!({"type": "text", "text": notice}));
            }
        }

        // Nothing to add. When the client has had blocks already, that is the
        // whole turn and the terminal events finish it; only a turn that was
        // *nothing but* a retrieval leaves the client with an empty message.
        // The continuation returned no blocks at all — nothing to drop, nothing
        // to emit — while the proxy had taken a tool call off the client's
        // hands. Left alone this streams a bare `end_turn` after the model's
        // "I'll look that up", which reads as the model choosing to stop.
        // Measured 2026-09-22 on Spark over Zen: the continuation spent its
        // whole output budget reasoning, came back `response.incomplete` with
        // `output: []`, and the memory answer vanished without a word in the
        // log or on the wire.
        //
        // `next_client_index == 0` is left to the branch below: a client that
        // has seen nothing at all gets the fuller notice there.
        if emit.is_empty()
            && rw.next_client_index != 0
            && dropped.iter().all(|&n| n == 0)
            && !rw.suppressed.is_empty()
        {
            tracing::warn!(
                event = "ccr_continuation_answer_lost",
                request_id = %ctx.request_id,
                suppressed_blocks = rw.suppressed.len(),
                unresolved_tool_name = ?unresolved_tool,
                "ccr: the continuation carried no content; the resolved tool answer never reached the client"
            );
            emit.push(json!({
                "type": "text",
                "text": lost_answer_text(unresolved_tool.as_deref()),
            }));
        }

        let mut events = if emit.is_empty() && rw.next_client_index == 0 {
            // Dropping left an empty assistant turn, which is not a thing the
            // client can render. Say what happened instead of sending nothing.
            synthesize_blocks(
                &[json!({
                    "type": "text",
                    "text": empty_turn_text(unresolved_tool.as_deref()),
                })],
                rw.next_client_index,
            )
        } else {
            synthesize_blocks(&emit, rw.next_client_index)
        };
        events.extend(synthesize_terminal(&resolved));

        for ev in events {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (stream, usage_handle)
}

/// De-stream a continuation for routed backends, whose SSE this code cannot
/// fold back into a turn: it synthesises the client's stream itself and has no
/// use for a second SSE body to splice. Anthropic upstreams stream instead —
/// see [`streamed_continuation_request`] for why.
///
/// `stream_options` is only valid with `stream: true` (the Responses API
/// rejects the combination with `400 stream_options requires stream to be
/// true`), and it only tunes streaming delivery (`reasoning_summary_delivery`,
/// `include_usage`), so it must go when streaming does. Leaving it behind is
/// what broke every `headroom_retrieve` continuation on routed Responses
/// models: the retrieval was fetched and then dropped on a 400.
fn non_streaming_continuation_request(forwarded_request: &Bytes) -> Bytes {
    match serde_json::from_slice::<Value>(forwarded_request) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("stream".into(), json!(false));
                obj.remove("stream_options");
            }
            serde_json::to_vec(&v)
                .map(Bytes::from)
                .unwrap_or_else(|_| forwarded_request.clone())
        }
        Err(_) => forwarded_request.clone(),
    }
}

/// Keep streaming on for backends that mandate it. The chatgpt codex gateway
/// answers a de-streamed continuation with `400 Stream must be set to true`.
///
/// OpenCode Zen mandates it too, and says so less plainly: a de-streamed
/// continuation comes back `403 FreeTierError: "OpenCode's free tier can only
/// be used from within OpenCode"`, which reads as an auth problem and is not.
/// The real OpenCode client always streams, so a `stream: false` body fails
/// the gate whatever its headers say. Measured 2026-09-22 on
/// `muse-spark-1.3-contributor-free`: a streaming client 403d on every memory
/// continuation while a non-streaming client on the same session, seconds
/// apart, got 200 — and the only difference between the two is this flag,
/// because the non-streaming path (`routed::ccr`) forwards the request body
/// untouched and never de-streams it.
///
/// An earlier comment here claimed Spark never reaches this path because its
/// compatibility profile suppresses Headroom's memory tools. With
/// `HEADROOM_MEMORY_INJECT_TOOLS=1` it does reach it, on every turn where the
/// model asks for memory.
fn restore_stream_when_mandated(request: Bytes, upstream_url: &url::Url) -> Bytes {
    if !matches!(
        upstream_url.host_str(),
        Some("chatgpt.com") | Some("opencode.ai")
    ) {
        return request;
    }
    streamed_continuation_request(request)
}

/// Stream a continuation, which is how Anthropic upstreams take it.
///
/// A buffered request holds its response headers until generation is done, so
/// the bounded headers wait in `handle_ccr_response` was really a bound on the
/// model's thinking time. Measured 2026-09-15: one retrieval spent 30s per
/// attempt three times over and died at 90.8s with the content already
/// fetched, while the 18 continuations that did land took 0.8s to 17.9s — the
/// 30s ceiling sat inside the working range. Streamed, headers arrive at once
/// and a stall there is transport, as the comment on that timeout always
/// claimed. The body is folded back into a turn by
/// [`anthropic_stream_to_turn`].
///
/// `stream_options` goes with it for the reason given above: it is only valid
/// on the Responses API and only tunes streaming delivery.
fn streamed_continuation_request(request: Bytes) -> Bytes {
    match serde_json::from_slice::<Value>(&request) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("stream".into(), json!(true));
                obj.remove("stream_options");
            }
            serde_json::to_vec(&v).map(Bytes::from).unwrap_or(request)
        }
        Err(_) => request,
    }
}

/// Run the buffered continuation logic against a rebuilt streamed turn.
async fn resolve_retrieval(
    ctx: &CcrStreamContext,
    rw: &Rewriter,
    round_usage: &Arc<Mutex<crate::proxy::CcrRoundUsage>>,
) -> Value {
    let rebuilt = rebuild_message(&rw.state);

    // The handler reads whatever shape its upstream speaks, so hand it the
    // turn in that shape and translate the answer back.
    let (mut turn_for_handler, provider) = match &ctx.shape {
        CcrShape::Anthropic => (rebuilt.clone(), "anthropic"),
        CcrShape::RoutedChat { .. } => (anthropic_turn_as_openai_response(&rebuilt), "openai"),
        CcrShape::RoutedResponses { .. } => (
            anthropic_turn_as_responses_output(&rebuilt),
            "openai_responses",
        ),
    };
    // The rebuilt turn speaks client tool names (the rewriter observes the
    // translated-back stream); continuations go back upstream, where the
    // Zen outbound rename still applies. Same tool list both directions,
    // so this is a no-op unless that pass renamed something.
    if !matches!(ctx.shape, CcrShape::Anthropic) {
        if let Some(anthropic_request) = match &ctx.shape {
            CcrShape::RoutedChat { anthropic_request } => Some(anthropic_request),
            CcrShape::RoutedResponses { anthropic_request } => Some(anthropic_request),
            CcrShape::Anthropic => None,
        } {
            crate::routed::tool_alias::ToolAlias::derive(
                anthropic_request.get("tools").and_then(|t| t.as_array()),
            )
            .forward_body(&mut turn_for_handler);
        }
    }
    // The rebuilt turn echoes first-response usage, which is already booked
    // through the first-round path on every streaming arm. Leaving the block
    // in place makes the resolver count it as a continuation round and the
    // turn's output books twice. Continuation responses keep their own usage;
    // only the seed turn is scrubbed.
    if let Some(obj) = turn_for_handler.as_object_mut() {
        obj.remove("usage");
    }

    let turn_bytes = match serde_json::to_vec(&turn_for_handler) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::warn!(
                event = "ccr_stream_rebuild_failed",
                request_id = %ctx.request_id,
                error = %e,
                "ccr: could not rebuild the streamed turn; leaving it unresolved"
            );
            return rebuilt;
        }
    };

    // An Anthropic continuation streams, so a slow round is not mistaken for a
    // stalled one. A routed one comes back as JSON, which this code can parse
    // without a second SSE body to fold, except on backends that mandate
    // streaming, where de-streaming 400s.
    let continuation_request = if matches!(ctx.shape, CcrShape::Anthropic) {
        streamed_continuation_request(ctx.forwarded_request.clone())
    } else {
        restore_stream_when_mandated(
            non_streaming_continuation_request(&ctx.forwarded_request),
            &ctx.upstream_url,
        )
    };

    // Memory tools run after retrieval, on whatever the retrieval left. A turn
    // can reach for both, and the client can run neither — but neither can the
    // answer to one be the end of it, because a resolved memory call can leave
    // the model asking for a retrieval that retrieval has already passed by.
    // So alternate rather than run each once, and stop as soon as a whole pass
    // changes nothing. See `MAX_RESOLVER_ALTERNATIONS`.
    let mut resolved_bytes = turn_bytes.clone();
    let mut usage = crate::proxy::CcrRoundUsage::default();
    for _ in 0..crate::proxy::MAX_RESOLVER_ALTERNATIONS {
        let before = resolved_bytes.clone();

        let (bytes, extra) = crate::proxy::handle_ccr_response(
            &resolved_bytes,
            &continuation_request,
            &ctx.upstream_url,
            &ctx.client,
            ctx.ccr_store.as_ref(),
            ctx.ccr_stores.as_ref(),
            &ctx.config,
            &ctx.request_id,
            &ctx.outgoing_headers,
            provider,
            ctx.redact.clone(),
        )
        .await;
        usage.absorb(extra);
        resolved_bytes = bytes;

        if let Some(memory) = &ctx.memory {
            let (bytes, extra) = crate::proxy::handle_memory_response(
                &resolved_bytes,
                &continuation_request,
                &ctx.upstream_url,
                &ctx.client,
                memory,
                &ctx.config,
                &ctx.request_id,
                &ctx.outgoing_headers,
                provider,
                ctx.redact.clone(),
            )
            .await;
            usage.absorb(extra);
            resolved_bytes = bytes;
        }

        if resolved_bytes == before {
            break;
        }
    }

    // Fold the terminal response's usage: the loop above adds every
    // superseded response, but the final one returns as the turn and was
    // never added — yet the provider billed it like every other round.
    // Without this, single-continuation turns (the common case) book zero
    // rounds on every streaming path, buffered included in spirit (which
    // instead reads the final usage block directly). A rebuilt turn with no
    // continuation behind it carries no usage block, so the add is a no-op
    // there rather than a double count.
    if let Ok(final_turn) = serde_json::from_slice::<Value>(&resolved_bytes) {
        usage.add_response(&final_turn);
    }

    if let Ok(mut guard) = round_usage.lock() {
        *guard = usage;
    }

    let Ok(resolved) = serde_json::from_slice::<Value>(&resolved_bytes) else {
        return rebuilt;
    };
    match &ctx.shape {
        CcrShape::Anthropic => resolved,
        CcrShape::RoutedChat { anthropic_request } => {
            let mut turn =
                crate::openai::response::openai_to_anthropic_response(&resolved, anthropic_request);
            // Continuation rounds called the upstream names; map them back
            // like every other inbound seam (same tool list, same rule).
            crate::routed::tool_alias::ToolAlias::derive(
                anthropic_request.get("tools").and_then(|t| t.as_array()),
            )
            .reverse_turn(&mut turn);
            turn
        }
        CcrShape::RoutedResponses { anthropic_request } => {
            let mut turn = responses_output_as_anthropic_turn(&resolved, anthropic_request);
            crate::routed::tool_alias::ToolAlias::derive(
                anthropic_request.get("tools").and_then(|t| t.as_array()),
            )
            .reverse_turn(&mut turn);
            turn
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn feed(rw: &mut Rewriter, name: &str, data: &str) -> Vec<Bytes> {
        rw.handle(SseEvent {
            event_name: Some(name.to_string()),
            data: Bytes::from(data.to_string()),
        })
    }

    fn joined(chunks: &[Bytes]) -> String {
        chunks
            .iter()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .collect()
    }

    /// A continuation is buffered JSON: it must force `stream: false` and drop
    /// `stream_options`, which the Responses API rejects with 400 alongside
    /// `stream: false` (`stream_options requires stream to be true`). That
    /// combination is what dropped every `headroom_retrieve` continuation on
    /// routed Responses models after the retrieval had already been fetched.
    #[test]
    fn continuation_request_drops_stream_options_with_stream() {
        let forwarded = Bytes::from(
            r#"{"model":"m","stream":true,"stream_options":{"reasoning_summary_delivery":"sequential_cutoff"},"input":[]}"#,
        );
        let out = non_streaming_continuation_request(&forwarded);
        let v: Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["stream"], json!(false));
        assert!(
            v.get("stream_options").is_none(),
            "stream_options must not survive on a non-streaming continuation"
        );
    }

    /// The ChatGPT Codex gateway mandates streaming; other routed backends
    /// keep the buffered continuation shape.
    #[test]
    fn mandating_backend_keeps_stream_on_continuations() {
        let destreamed = Bytes::from(r#"{"model":"m","stream":false,"input":[]}"#);
        let codex: url::Url = "https://chatgpt.com/backend-api/codex/responses"
            .parse()
            .unwrap();
        let out = restore_stream_when_mandated(destreamed.clone(), &codex);
        let v: Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["stream"], json!(true));

        // Zen rejects a de-streamed continuation with a 403 that reads like an
        // auth failure, so it belongs in the same set as the codex gateway.
        let zen: url::Url = "https://opencode.ai/zen/v1/responses".parse().unwrap();
        let out = restore_stream_when_mandated(destreamed.clone(), &zen);
        let v: Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["stream"], json!(true));

        // Everything else still takes the de-streamed body.
        let plain: url::Url = "https://api.x.ai/v1/responses".parse().unwrap();
        let out = restore_stream_when_mandated(destreamed.clone(), &plain);
        let v: Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["stream"], json!(false));

        let garbage = Bytes::from(b"not json".to_vec());
        assert_eq!(
            restore_stream_when_mandated(garbage.clone(), &codex),
            garbage,
            "unparseable bodies pass through untouched"
        );
    }

    /// An Anthropic continuation streams, so the 30s headers bound stops being
    /// a bound on how long the model may think.
    #[test]
    fn anthropic_continuation_streams() {
        let forwarded = Bytes::from(
            r#"{"model":"m","stream":true,"stream_options":{"include_usage":true},"messages":[]}"#,
        );
        let out = streamed_continuation_request(forwarded);
        let v: Value = serde_json::from_slice(&out).expect("valid JSON");
        assert_eq!(v["stream"], json!(true));
        assert!(v.get("stream_options").is_none());

        let garbage = Bytes::from(b"not json".to_vec());
        assert_eq!(
            streamed_continuation_request(garbage.clone()),
            garbage,
            "unparseable bodies pass through untouched"
        );
    }

    /// The fold reads a streamed continuation back into a turn, and refuses a
    /// stream that died partway rather than passing its first blocks off as
    /// the whole answer.
    #[test]
    fn anthropic_stream_folds_into_a_turn() {
        let body = b"event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\",\"usage\":{\"input_tokens\":9,\"cache_read_input_tokens\":4}}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n\
             event: content_block_stop\n\
             data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
             event: message_stop\n\
             data: {\"type\":\"message_stop\"}\n\n";
        let turn = anthropic_stream_to_turn(body).expect("a complete stream folds");
        assert_eq!(turn["content"][0]["text"], "done");
        assert_eq!(turn["stop_reason"], "end_turn");
        assert_eq!(turn["usage"]["cache_read_input_tokens"], 4);

        let errored = b"event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\"}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"half\"}}\n\n\
             event: error\n\
             data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n";
        assert!(
            anthropic_stream_to_turn(errored).is_none(),
            "a stream that errored mid-body is not an answer"
        );

        assert!(
            anthropic_stream_to_turn(b"data: not json\n\n").is_none(),
            "a body with no Anthropic events folds to nothing"
        );

        let cut_short = b"event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\"}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"half\"}}\n\n";
        assert!(
            anthropic_stream_to_turn(cut_short).is_none(),
            "a body that ended before message_delta has no stop_reason to trust"
        );
    }

    /// The whole point: the client must never see the tool it cannot run.
    #[test]
    fn ccr_block_events_are_suppressed() {
        let mut rw = Rewriter::new(false);
        feed(
            &mut rw,
            "message_start",
            r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        );
        let out = feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"headroom_retrieve","input":{}}}"#,
        );
        assert!(
            out.is_empty(),
            "the tool_use block must not reach the client"
        );
        let out = feed(
            &mut rw,
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"hash\":\"abc\"}"}}"#,
        );
        assert!(
            out.is_empty(),
            "its deltas must not reach the client either"
        );
        assert!(rw.saw_ccr);
    }

    /// Memory tools are the proxy's too, and the client cannot run them
    /// either. This is the case the streaming path used to miss: the rewriter
    /// was built with `false` regardless of the turn's memory context, so a
    /// `memory_search` block streamed through and the client answered "No such
    /// tool available".
    /// The other half of the same fact, and the shape of the routed-path bug:
    /// a rewriter told the turn has no memory context passes the block
    /// straight through. `handle_streaming_response` built its
    /// `CcrStreamContext` with a hardcoded `memory: None` on the belief that
    /// routed requests never carry memory tools — they do, injected at the
    /// `codex_memory_tools` site — so every Codex turn that called
    /// `memory_search` reached the client, which answered "No such tool
    /// available: memory_search".
    #[test]
    fn a_memory_block_reaches_the_client_when_the_turn_claims_no_memory() {
        const START: &str = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"memory_search","input":{}}}"#;

        let mut rw = Rewriter::new(false);
        feed(
            &mut rw,
            "message_start",
            r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        );
        assert!(
            !feed(&mut rw, "content_block_start", START).is_empty(),
            "with no memory context the block is not ours to own, and it reaches \
             the client — which is exactly the failure this documents"
        );
    }

    #[test]
    fn memory_block_events_are_suppressed_when_memory_is_enabled() {
        const START: &str = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"memory_search","input":{}}}"#;

        let mut rw = Rewriter::new(true);
        feed(
            &mut rw,
            "message_start",
            r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        );
        assert!(
            feed(&mut rw, "content_block_start", START).is_empty(),
            "a memory tool_use must not reach the client"
        );
        assert!(
            feed(
                &mut rw,
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"x\"}"}}"#,
            )
            .is_empty(),
            "nor may its deltas"
        );
        assert!(rw.saw_ccr, "the turn must be marked for resolution");

        // And the flag is what does it — with memory off, the same block is an
        // ordinary tool the client owns and must pass through untouched.
        let mut rw = Rewriter::new(false);
        feed(
            &mut rw,
            "message_start",
            r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        );
        assert!(
            !feed(&mut rw, "content_block_start", START).is_empty(),
            "with memory disabled the block belongs to the client"
        );
        assert!(!rw.saw_ccr);
    }

    /// A turn with no retrieval must come out the far side unchanged.
    #[test]
    fn ordinary_blocks_pass_through_with_their_indices() {
        let mut rw = Rewriter::new(false);
        feed(
            &mut rw,
            "message_start",
            r#"{"type":"message_start","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        );
        let out = feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        assert!(joined(&out).contains("\"index\":0"));
        let out = feed(
            &mut rw,
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        );
        assert!(joined(&out).contains("hi"));
        assert!(!rw.saw_ccr);
    }

    /// Text before the retrieval keeps index 0; the block after the
    /// suppressed one takes index 1, not the upstream's 2.
    #[test]
    fn indices_close_the_gap_left_by_a_suppressed_block() {
        let mut rw = Rewriter::new(false);
        feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"headroom_retrieve","input":{}}}"#,
        );
        let out = feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
        );
        assert!(
            joined(&out).contains("\"index\":1"),
            "client-side numbering must stay contiguous, got {}",
            joined(&out)
        );
        assert_eq!(rw.next_client_index, 2);
    }

    /// Terminal events are the signal that the turn is over. They cannot go
    /// out before a continuation has had its chance to add to it.
    #[test]
    fn terminal_events_are_withheld() {
        let mut rw = Rewriter::new(false);
        let out = feed(
            &mut rw,
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
        );
        assert!(out.is_empty());
        let out = feed(&mut rw, "message_stop", r#"{"type":"message_stop"}"#);
        assert!(out.is_empty());
        assert_eq!(rw.withheld.len(), 2);
    }

    /// The rebuilt shape is what the buffered CCR path expects to read.
    #[test]
    fn rebuild_produces_the_non_streaming_response_shape() {
        let mut rw = Rewriter::new(false);
        feed(
            &mut rw,
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-x","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        );
        feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        feed(
            &mut rw,
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"looking"}}"#,
        );
        feed(
            &mut rw,
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        );
        feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"headroom_retrieve","input":{}}}"#,
        );
        feed(
            &mut rw,
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"hash\":\"abc123\"}"}}"#,
        );
        feed(
            &mut rw,
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        );
        feed(
            &mut rw,
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
        );

        let msg = rebuild_message(&rw.state);
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["id"], "msg_1");
        assert_eq!(msg["stop_reason"], "tool_use");
        let content = msg["content"].as_array().expect("content array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "looking");
        assert_eq!(content[1]["name"], CCR_TOOL_NAME);
        // The fragment has to survive as a real object, or the store lookup
        // has no hash to look up.
        assert_eq!(content[1]["input"]["hash"], "abc123");
    }

    /// Synthesised events must be the shape a client already knows how to
    /// parse, and must continue the numbering rather than restart it.
    #[test]
    fn synthesized_blocks_continue_the_client_numbering() {
        let content = vec![json!({"type": "text", "text": "the answer"})];
        let out = synthesize_blocks(&content, 3);
        let text = joined(&out);
        assert!(text.contains("event: content_block_start"));
        assert!(text.contains("\"index\":3"));
        assert!(text.contains("\"type\":\"text_delta\""));
        assert!(text.contains("the answer"));
        assert!(text.contains("event: content_block_stop"));
        // The block opens empty and is filled by the delta, so the text must
        // appear exactly once — on the delta, not on the start event.
        assert_eq!(
            text.matches("the answer").count(),
            1,
            "content duplicated across start and delta: {text}"
        );
    }

    /// The splice's filter, run the way the stream runs it.
    fn spliceable(content: Vec<Value>, live: &[Value]) -> Vec<Value> {
        content
            .into_iter()
            .filter(|b| drop_reason(b, false, live).is_none())
            .collect()
    }

    /// The 2026-08-20 failure: three memory continuation rounds, the last
    /// tool call left unresolved and dropped, and `stop_reason: tool_use`
    /// forwarded regardless. Every client turn shaped like this died with
    /// "the model's tool call could not be parsed (retry also failed)".
    #[test]
    fn a_dropped_tool_call_must_not_leave_stop_reason_claiming_one() {
        assert!(stop_reason_overclaims_tool_call(Some("tool_use"), false));
    }

    #[test]
    fn a_surviving_tool_call_keeps_its_stop_reason() {
        assert!(!stop_reason_overclaims_tool_call(Some("tool_use"), true));
    }

    #[test]
    fn an_ordinary_turn_is_left_alone() {
        assert!(!stop_reason_overclaims_tool_call(Some("end_turn"), false));
        assert!(!stop_reason_overclaims_tool_call(None, false));
    }

    /// The client-facing half of the same decision: a tool block the client
    /// received counts, a proxy-owned one it never saw does not.
    #[test]
    fn only_tool_blocks_the_client_receives_count() {
        let mut rw = Rewriter::new(true);
        feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"memory_search","input":{}}}"#,
        );
        assert!(
            !rw.client_saw_tool_use,
            "a suppressed memory tool never reaches the client"
        );

        feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_2","name":"Bash","input":{}}}"#,
        );
        assert!(rw.client_saw_tool_use, "a client tool call does");
    }

    /// Anthropic signs a `thinking` block against the request that produced it.
    /// The continuation is a different request, so forwarding its reasoning
    /// hands the client a signature that cannot verify in the conversation it
    /// replays — and the next turn comes back 400 "thinking or
    /// redacted_thinking blocks in the latest assistant message cannot be
    /// modified".
    #[test]
    fn continuation_thinking_never_reaches_the_client() {
        let live = vec![json!({"type": "thinking", "thinking": "mine", "signature": "sig-a"})];
        let resolved = vec![
            json!({"type": "thinking", "thinking": "theirs", "signature": "sig-b"}),
            json!({"type": "redacted_thinking", "data": "opaque"}),
            json!({"type": "text", "text": "the answer"}),
        ];
        let emit = spliceable(resolved, &live);
        assert_eq!(emit.len(), 1, "only the text survives: {emit:?}");
        assert_eq!(emit[0]["text"], "the answer");
    }

    /// When the continuation cannot run — a `headroom_retrieve` mixed with a
    /// client tool call — the handler returns the turn unchanged. Every block
    /// in it has already gone out live, so splicing it again duplicates the
    /// turn and repeats a `tool_use` id the client is already acting on.
    #[test]
    fn blocks_already_streamed_are_not_sent_twice() {
        let live = vec![
            json!({"type": "text", "text": "looking"}),
            json!({"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "a"}}),
        ];
        let resolved = vec![
            json!({"type": "text", "text": "looking"}),
            // Rebuilt from deltas, so the input object need not match byte for
            // byte — the id is what makes it the same block.
            json!({"type": "tool_use", "id": "t1", "name": "Read", "input": {}}),
            json!({"type": "tool_use", "id": "ccr1", "name": CCR_TOOL_NAME, "input": {}}),
        ];
        assert!(
            spliceable(resolved, &live).is_empty(),
            "the client already has this turn"
        );
    }

    /// A genuinely new block from the continuation still gets through — the
    /// filter must not swallow the answer the retrieval was run to produce.
    #[test]
    fn a_new_continuation_block_still_reaches_the_client() {
        let live = vec![json!({"type": "text", "text": "looking"})];
        let resolved = vec![
            json!({"type": "text", "text": "looking"}),
            json!({"type": "text", "text": "found it"}),
        ];
        let emit = spliceable(resolved, &live);
        assert_eq!(emit.len(), 1);
        assert_eq!(emit[0]["text"], "found it");
    }

    #[test]
    fn synthesized_tool_use_carries_its_input_as_a_fragment() {
        let content = vec![json!({
            "type": "tool_use", "id": "t9", "name": "Read", "input": {"file_path": "/x"}
        })];
        let text = joined(&synthesize_blocks(&content, 0));
        assert!(text.contains("input_json_delta"));
        assert!(text.contains("file_path"));
    }

    #[test]
    fn terminal_carries_the_final_rounds_usage() {
        let msg = json!({
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 900, "output_tokens": 40},
        });
        let text = joined(&synthesize_terminal(&msg));
        assert!(text.contains("event: message_delta"));
        assert!(text.contains("end_turn"));
        assert!(text.contains("900"));
        assert!(text.contains("event: message_stop"));
    }
}

#[cfg(test)]
mod deferred_drop_reason_tests {
    use super::*;
    use crate::memory::deferred::{store, PendingMemoryResult};

    /// The deferred store is a process-wide static, so these run one at a time.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear() {
        if let Ok(mut held) = store().lock() {
            *held = crate::memory::deferred::DeferredMemory::new();
        }
    }

    fn memory_call(id: &str) -> Value {
        serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": "memory_search",
            "input": {"query": "q"},
        })
    }

    #[test]
    fn a_held_answer_is_not_a_lost_tool_call() {
        let _g = lock();
        clear();
        let block = memory_call("toolu_held");
        store().lock().unwrap().hold(PendingMemoryResult::new(
            block.clone(),
            serde_json::json!({"type": "tool_result", "tool_use_id": "toolu_held"}),
            vec!["toolu_client".to_string()],
        ));

        assert!(matches!(
            drop_reason(&block, true, &[]),
            Some(DropReason::DeferredMemoryAnswer)
        ));
        clear();
    }

    #[test]
    fn a_memory_call_with_no_held_answer_is_still_a_fault() {
        let _g = lock();
        clear();
        assert!(matches!(
            drop_reason(&memory_call("toolu_stranded"), true, &[]),
            Some(DropReason::UnresolvedProxyTool)
        ));
    }

    #[test]
    fn a_held_answer_for_another_call_does_not_excuse_this_one() {
        let _g = lock();
        clear();
        store().lock().unwrap().hold(PendingMemoryResult::new(
            memory_call("toolu_other"),
            serde_json::json!({"type": "tool_result", "tool_use_id": "toolu_other"}),
            vec!["toolu_client".to_string()],
        ));

        assert!(matches!(
            drop_reason(&memory_call("toolu_stranded"), true, &[]),
            Some(DropReason::UnresolvedProxyTool)
        ));
        clear();
    }

    /// The counters are indexed by discriminant, so a new variant that breaks
    /// `ALL[r as usize] == r` would silently mislabel every count.
    #[test]
    fn every_reason_indexes_to_itself() {
        for (i, reason) in DropReason::ALL.iter().enumerate() {
            assert_eq!(i, *reason as usize, "{} is out of order", reason.label());
        }
    }

    /// Buffered Responses twin of the chat-path downgrade: a `function_call`
    /// without `call_id` or `name` cannot round-trip, so it is dropped and
    /// the stop reason derives from surviving content instead of killing
    /// the turn.
    #[test]
    fn an_identity_less_function_call_downgrades_to_end_turn() {
        let resolved = json!({
            "id": "resp_1",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "here is what I found"}]},
                {"type": "function_call", "call_id": "", "name": "", "arguments": "{}"}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = responses_output_as_anthropic_turn(&resolved, &original);
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

        // Nothing survives: an empty `end_turn`, not a `tool_use` the client
        // would discard whole.
        let resolved = json!({
            "id": "resp_2",
            "output": [
                {"type": "function_call", "call_id": "call_1", "name": "", "arguments": "{}"}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let output = responses_output_as_anthropic_turn(&resolved, &original);
        assert_eq!(output["stop_reason"], "end_turn");
        assert!(
            output["content"].as_array().unwrap().is_empty(),
            "no playable block survived: {output}"
        );
    }
}

#[cfg(test)]
mod empty_turn_text_tests {
    use super::tests::feed;
    use super::*;

    /// A dropped call is named, so the reader knows which machinery to look
    /// at. "context retrieval" was true of `headroom_retrieve` and wrong of
    /// every memory tool, which the same branch also covers.
    #[test]
    fn a_dropped_tool_is_named() {
        let text = empty_turn_text(Some(CCR_TOOL_NAME));
        assert!(text.contains(CCR_TOOL_NAME), "tool not named: {text}");

        let text = empty_turn_text(Some("memory_search"));
        assert!(text.contains("memory_search"), "tool not named: {text}");
        assert!(
            !text.contains("retrieval"),
            "a dropped memory call is not a retrieval failure: {text}"
        );
    }

    /// The five-in-six case. Saying "context retrieval" here sent the reader
    /// after machinery that had done nothing wrong.
    #[test]
    fn an_empty_tool_call_is_not_called_a_retrieval_failure() {
        let text = empty_turn_text(None);
        assert!(!text.contains("retrieval"), "wrong cause named: {text}");
        assert!(text.contains("tool call"));
    }

    /// The marker rides along so the Stop hook can continue the turn.
    /// Both branches carry it: the hook cannot tell which fired.
    #[test]
    fn empty_turns_carry_the_hook_marker() {
        for text in [
            empty_turn_text(Some("memory_search")),
            empty_turn_text(None),
        ] {
            assert!(
                text.contains(RETRIEVAL_DROPPED_MARKER),
                "hook marker missing: {text}"
            );
        }
    }

    /// Rust↔shell contract: the hook greps transcripts for these literals,
    /// so a rename on either side silently disables continuation. Pin them
    /// together; the hook path is relative to the crate checkout.
    #[test]
    fn hook_detector_matches_wire_markers() {
        let hook = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../contrib/claude/hooks/retry-dropped-turn.sh");
        let hook = std::fs::read_to_string(&hook).expect("hook script readable from checkout");
        for marker in [
            RETRIEVAL_DROPPED_MARKER,
            "<retrieved_context>",
            crate::memory::deferred::DEFERRED_MEMORY_DROPPED_MARKER,
        ] {
            assert!(
                hook.contains(marker),
                "hook must grep for wire marker: {marker}"
            );
        }
    }

    /// Both notices carry the marker the hook greps for, and each says the
    /// thing that is true of its own case. A turn that already spoke must not
    /// be told it "came back empty" directly under the model's own sentence.
    #[test]
    fn each_notice_fits_the_turn_it_describes() {
        let dropped = dropped_call_text(Some("memory_search"));
        assert!(dropped.contains("memory_search"));
        assert!(dropped.contains(RETRIEVAL_DROPPED_MARKER));
        assert!(
            !dropped.contains("came back empty"),
            "a turn that spoke did not come back empty: {dropped}"
        );

        let empty = empty_turn_text(Some("memory_search"));
        assert!(empty.contains("came back empty"));
        assert!(empty.contains(RETRIEVAL_DROPPED_MARKER));

        // And the case where the call ran but its answer never arrived, which
        // is a different sentence again: nothing was dropped, the follow-up
        // was.
        let lost = lost_answer_text(Some("memory_search"));
        assert!(lost.contains("came back empty"), "{lost}");
        assert!(lost.contains("ran `memory_search`"), "{lost}");
        assert!(lost.contains(RETRIEVAL_DROPPED_MARKER));
    }

    /// Thinking on the wire is not visible text. The 2026-09-22 Zen 403
    /// turns streamed a thinking block, then nothing; the old
    /// `next_client_index == 0` guard treated that as a successful turn.
    #[test]
    fn thinking_only_turns_lack_visible_text() {
        let thinking = json!({"type": "thinking", "thinking": "hmm"});
        assert!(
            turn_lacks_visible_text(&[thinking.clone()], false),
            "thinking in emit is not visible text"
        );
        assert!(
            turn_lacks_visible_text(&[], false),
            "an empty emit with no live text is blank"
        );
        assert!(
            !turn_lacks_visible_text(&[json!({"type": "text", "text": "hi"})], false),
            "prose in emit is visible"
        );
        assert!(
            !turn_lacks_visible_text(&[], true),
            "text already streamed is visible"
        );
        assert!(
            turn_lacks_visible_text(&[json!({"type": "text", "text": "   "})], false),
            "whitespace-only text is not visible"
        );
    }

    #[test]
    fn rewriter_counts_text_not_thinking_as_visible() {
        let mut rw = Rewriter::new(false);
        feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        );
        assert!(
            !rw.client_saw_visible_text,
            "thinking must not count as visible text"
        );
        assert!(rw.next_client_index > 0, "thinking still occupies an index");
        feed(
            &mut rw,
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        );
        assert!(
            rw.client_saw_visible_text,
            "a text block the client received is visible"
        );
    }
}

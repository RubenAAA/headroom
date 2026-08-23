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
//! covers yet. Rather than leave them advertising an unanswerable tool, the
//! injection site in `proxy.rs` skips those tools when such a client asks for
//! a stream. They keep the feature on buffered requests, where the buffered
//! arm resolves it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::sse::anthropic::{AnthropicStreamState, BlockState};
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
    pub config: Arc<crate::config::Config>,
    pub request_id: String,
    pub shape: CcrShape,
    /// Present when memory tools were injected into this request. The proxy
    /// runs those too, for the same reason it runs `headroom_retrieve`.
    pub memory: Option<crate::proxy::MemoryToolContext>,
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
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect()
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
            }
            Some("function_call") => content.push(json!({
                "type": "tool_use",
                "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "name": item.get("name").cloned().unwrap_or(Value::Null),
                "input": item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or_else(|| json!({})),
            })),
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
}

impl DropReason {
    /// Indexed by the discriminant, so `ALL[r as usize] == r`.
    const ALL: [DropReason; 3] = [
        DropReason::UnresolvedProxyTool,
        DropReason::ContinuationThinking,
        DropReason::AlreadyStreamed,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::UnresolvedProxyTool => "unresolved_proxy_tool",
            Self::ContinuationThinking => "continuation_thinking",
            Self::AlreadyStreamed => "already_streamed",
        }
    }
}

/// The splice's filter. `None` means the block is safe to send on.
fn drop_reason(block: &Value, memory_enabled: bool, live: &[Value]) -> Option<DropReason> {
    if proxy_owned_tool(block, memory_enabled) {
        Some(DropReason::UnresolvedProxyTool)
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
        let kind = parsed
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Feed the state machine first; a parse failure there is not a reason
        // to drop the byte path.
        let _ = self.state.apply(SseEvent {
            event_name: ev.event_name.clone(),
            data: ev.data.clone(),
        });

        match kind.as_str() {
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
    let round_usage = Arc::new(Mutex::new(crate::proxy::CcrRoundUsage::default()));
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
        for block in content {
            match drop_reason(&block, memory_enabled, &live_blocks) {
                Some(reason) => dropped[reason as usize] += 1,
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
                    request_id = %ctx.request_id,
                    unresolved_proxy_tool = dropped[DropReason::UnresolvedProxyTool as usize],
                    continuation_thinking = dropped[DropReason::ContinuationThinking as usize],
                    already_streamed = dropped[DropReason::AlreadyStreamed as usize],
                    "ccr: dropped a proxy tool call the client expected; the turn \
                     promises a tool_use block that will not arrive"
                );
            } else {
                tracing::debug!(
                    request_id = %ctx.request_id,
                    continuation_thinking = dropped[DropReason::ContinuationThinking as usize],
                    already_streamed = dropped[DropReason::AlreadyStreamed as usize],
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
                "ccr: turn promised a tool call the client will not receive; \
                 downgrading stop_reason to end_turn"
            );
            resolved["stop_reason"] = json!("end_turn");
            // Without this the client renders an empty turn and the retrieval
            // failure looks like the model simply said nothing.
            if emit.is_empty() {
                emit.push(json!({
                    "type": "text",
                    "text": "The proxy could not complete a context retrieval for this turn.",
                }));
            }
        }

        // Nothing to add. When the client has had blocks already, that is the
        // whole turn and the terminal events finish it; only a turn that was
        // *nothing but* a retrieval leaves the client with an empty message.
        let mut events = if emit.is_empty() && rw.next_client_index == 0 {
            // Dropping left an empty assistant turn, which is not a thing the
            // client can render. Say what happened instead of sending nothing.
            synthesize_blocks(
                &[json!({
                    "type": "text",
                    "text": "The proxy could not complete a context retrieval for this turn.",
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

/// Run the buffered continuation logic against a rebuilt streamed turn.
async fn resolve_retrieval(
    ctx: &CcrStreamContext,
    rw: &Rewriter,
    round_usage: &Arc<Mutex<crate::proxy::CcrRoundUsage>>,
) -> Value {
    let rebuilt = rebuild_message(&rw.state);

    // The handler reads whatever shape its upstream speaks, so hand it the
    // turn in that shape and translate the answer back.
    let (turn_for_handler, provider) = match &ctx.shape {
        CcrShape::Anthropic => (rebuilt.clone(), "anthropic"),
        CcrShape::RoutedChat { .. } => (anthropic_turn_as_openai_response(&rebuilt), "openai"),
        CcrShape::RoutedResponses { .. } => (
            anthropic_turn_as_responses_output(&rebuilt),
            "openai_responses",
        ),
    };

    let turn_bytes = match serde_json::to_vec(&turn_for_handler) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::warn!(
                request_id = %ctx.request_id,
                error = %e,
                "ccr: could not rebuild the streamed turn; leaving it unresolved"
            );
            return rebuilt;
        }
    };

    // Continuation rounds must come back as JSON — this code synthesises the
    // client's stream itself and has no use for a second SSE body to splice.
    let continuation_request = match serde_json::from_slice::<Value>(&ctx.forwarded_request) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("stream".into(), json!(false));
            }
            serde_json::to_vec(&v)
                .map(Bytes::from)
                .unwrap_or_else(|_| ctx.forwarded_request.clone())
        }
        Err(_) => ctx.forwarded_request.clone(),
    };

    let (resolved_bytes, mut usage) = crate::proxy::handle_ccr_response(
        &turn_bytes,
        &continuation_request,
        &ctx.upstream_url,
        &ctx.client,
        ctx.ccr_store.as_ref(),
        &ctx.config,
        &ctx.request_id,
        &ctx.outgoing_headers,
        provider,
    )
    .await;

    // Memory tools run after retrieval, on whatever the retrieval left. A turn
    // can reach for both, and the client can run neither.
    let resolved_bytes = match &ctx.memory {
        Some(memory) => {
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
            )
            .await;
            usage.absorb(extra);
            bytes
        }
        None => resolved_bytes,
    };

    if let Ok(mut guard) = round_usage.lock() {
        *guard = usage;
    }

    let Ok(resolved) = serde_json::from_slice::<Value>(&resolved_bytes) else {
        return rebuilt;
    };
    match &ctx.shape {
        CcrShape::Anthropic => resolved,
        CcrShape::RoutedChat { anthropic_request } => {
            crate::handlers::local_model::openai_to_anthropic_response(&resolved, anthropic_request)
        }
        CcrShape::RoutedResponses { anthropic_request } => {
            responses_output_as_anthropic_turn(&resolved, anthropic_request)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(rw: &mut Rewriter, name: &str, data: &str) -> Vec<Bytes> {
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

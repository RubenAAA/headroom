//! Close out an Anthropic stream that dies after the client is committed.
//!
//! `stream_retry` covers the drops it can: while the opening bytes are still
//! held nothing has reached the client, so the request can simply be made
//! again. Once those bytes are flushed that door shuts. The old behaviour from
//! there on was to hand the transport error to the client, which kills the
//! response body mid-frame — the client sees a reset socket and reports the
//! turn as lost. Over one logged day that was 11 turns, against 6 the hold
//! caught.
//!
//! Nothing can recover the tokens the model never sent. What is recoverable is
//! the *shape* of the reply: a truncated message that is still well-formed ends
//! the turn, hands control back, and leaves the session usable. So on a drop
//! this synthesises the events the stream still owed — close the open block,
//! `message_delta` with a stop reason, `message_stop` — and ends the body
//! normally.
//!
//! Three things make that safe to do.
//!
//! **The reply says it was cut off.** A truncated answer dressed up as a
//! complete one is worse than an error, so the tail carries a marker in the
//! text. The user sees where the model stopped.
//!
//! **A `tool_use` block is never half-emitted.** Its input arrives as a stream
//! of JSON fragments, and a fragment closed off early is either unparseable or,
//! worse, parseable and wrong — `{"command":"rm -rf /home/user/proj` is a valid
//! prefix of a command nobody asked for. So `tool_use` blocks are withheld
//! until their `content_block_stop` arrives and released in one piece. A drop
//! part-way through discards them, and the client is left with the text, which
//! is exactly what it can safely act on. The cost is that tool input no longer
//! paints token by token.
//!
//! **A failure the provider reported is left alone.** Anthropic ends a failed
//! generation with `event: error`, and the in-band retry below acts on it.
//! Closing such a stream off as a finished turn would hide a real error and
//! strand the retry, so a stream that carried one gets no tail. The same goes
//! for a transport error that arrives after `message_stop`: the message is
//! complete, and the error is noise trailing it.
//!
//! The marker text is also what guarantees the message has content. An
//! assistant turn with no content blocks is rejected when the client sends the
//! history back, so a drop before any block opened would trade one dead turn
//! for a dead conversation.
//!
//! This sits closest to the client, above the telemetry tee, so cost and
//! savings accounting still sees the stream end short and books it as the
//! incomplete turn it was.

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use super::framing::SseFramer;

/// Depth of the hand-off queue to the client. Matches `stream_retry`.
const CLIENT_QUEUE_DEPTH: usize = 64;

/// Appended to the reply so a truncated answer never reads as a finished one.
const TRUNCATION_MARKER: &str = "\n\n[truncated: the connection to the API dropped mid-response]";

/// The kind of content block currently open on the wire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    /// `tool_use`, withheld until complete.
    ToolUse,
    /// `thinking`, `redacted_thinking`, or anything a future model adds.
    /// Closable, but not something the marker can be appended to.
    Opaque,
}

/// What the client has been told about this message so far.
#[derive(Default)]
struct Wire {
    saw_message_start: bool,
    saw_message_stop: bool,
    /// Index and kind of the block currently open, on the wire or withheld.
    open: Option<(u64, Kind)>,
    /// Highest block index the client has seen, for placing a new tail block.
    max_index: u64,
    /// Whether any block index has been seen at all.
    any_block: bool,
    /// Running output-token count, for the synthetic `message_delta`.
    output_tokens: u64,
    /// Whether an in-band `error` event has gone to the client.
    saw_error: bool,
}

impl Wire {
    /// Fold one event into the picture.
    fn observe(&mut self, v: &serde_json::Value) {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
        match ty {
            "message_start" => {
                // A second `message_start` is a second message: the CCR layer
                // below can answer a retrieval and drive another round through
                // the same body. State from the finished one must not decide
                // how this one ends.
                *self = Self::default();
                self.saw_message_start = true;
                if let Some(n) = v
                    .pointer("/message/usage/output_tokens")
                    .and_then(|n| n.as_u64())
                {
                    self.output_tokens = n;
                }
            }
            // Anthropic ends a stream this way when generation fails part-way.
            // The client has been told the truth, and the in-band retry below
            // owns what happens next, so the tail stays out of it.
            "error" => {
                self.saw_error = true;
            }
            "content_block_start" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let kind = match v
                    .pointer("/content_block/type")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                {
                    "text" => Kind::Text,
                    "tool_use" | "server_tool_use" | "mcp_tool_use" => Kind::ToolUse,
                    _ => Kind::Opaque,
                };
                self.open = Some((index, kind));
                self.max_index = self.max_index.max(index);
                self.any_block = true;
            }
            "content_block_stop" => {
                self.open = None;
            }
            "message_delta" => {
                if let Some(n) = v.pointer("/usage/output_tokens").and_then(|n| n.as_u64()) {
                    self.output_tokens = n;
                }
            }
            "message_stop" => {
                self.saw_message_stop = true;
            }
            _ => {}
        }
    }

    /// The events the stream still owed the client, in order.
    ///
    /// Empty when the message already ended properly, or when it never
    /// started — with no `message_start` on the wire there is no message to
    /// close, and the error belongs to the layer below.
    fn tail(&self) -> Vec<Bytes> {
        if !self.saw_message_start || self.saw_message_stop || self.saw_error {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Where the marker goes. An open text block can take it directly;
        // anything else has to be closed first and the marker given its own
        // block after it.
        let marker_index = match self.open {
            Some((index, Kind::Text)) => {
                out.push(delta_event(index, TRUNCATION_MARKER));
                out.push(stop_event(index));
                None
            }
            Some((index, Kind::Opaque)) => {
                out.push(stop_event(index));
                Some(index + 1)
            }
            // A withheld `tool_use` block never reached the wire, so there is
            // nothing to close — its index is simply reused.
            Some((index, Kind::ToolUse)) => Some(index),
            None if self.any_block => Some(self.max_index + 1),
            None => Some(0),
        };
        if let Some(index) = marker_index {
            out.push(start_event(index));
            out.push(delta_event(index, TRUNCATION_MARKER.trim_start()));
            out.push(stop_event(index));
        }
        // `end_turn` rather than a truer-sounding reason: it is the one stop
        // reason every client reads as "the model is done, the human has the
        // floor again", which is the whole point of synthesising a tail.
        out.push(frame(
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": self.output_tokens},
            }),
        ));
        out.push(frame(
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        ));
        out
    }
}

/// Rewrite a `message_delta`'s `stop_reason`. The client refuses a whole turn
/// that claims a tool call it never received — "Content block not found" on
/// the far side — so when the tool block is dropped the claim has to go too.
fn rewrite_stop_reason(v: &serde_json::Value, reason: &str) -> Bytes {
    let mut v = v.clone();
    if let Some(delta) = v.get_mut("delta") {
        if let Some(obj) = delta.as_object_mut() {
            obj.insert(
                "stop_reason".to_string(),
                serde_json::Value::String(reason.to_string()),
            );
        }
    }
    frame("message_delta", &v)
}

fn frame(name: &str, data: &serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

fn start_event(index: u64) -> Bytes {
    frame(
        "content_block_start",
        &serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""},
        }),
    )
}

fn delta_event(index: u64, text: &str) -> Bytes {
    frame(
        "content_block_delta",
        &serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text},
        }),
    )
}

fn stop_event(index: u64) -> Bytes {
    frame(
        "content_block_stop",
        &serde_json::json!({"type": "content_block_stop", "index": index}),
    )
}

/// The JSON payload of a raw SSE block, if it has one.
///
/// Comments and keepalives have no `data:` line; they are forwarded untouched
/// and need no parsing.
fn payload(raw: &[u8]) -> Option<serde_json::Value> {
    raw.split(|b| *b == b'\n')
        .filter_map(|line| line.strip_prefix(b"data:"))
        .find_map(|rest| serde_json::from_slice(rest).ok())
}

/// The client stopped reading. Log it before unwinding: returning here drops
/// `inner`, so the rest of the turn — `message_stop` included — is never
/// pulled, and never crosses the telemetry tee either. Without this line the
/// only trace is a `stream_incomplete` warning that reads like a provider
/// fault when the hangup was on our side of the wire.
fn client_gone(request_id: &str) {
    tracing::warn!(
        request_id = %request_id,
        event = "stream_finisher_client_gone",
        "client stopped reading; upstream tail left unread"
    );
}

/// Wrap an Anthropic SSE body so it always ends as a well-formed message.
pub(crate) fn finish_on_drop<S>(
    inner: S,
    request_id: String,
) -> impl Stream<Item = reqwest::Result<Bytes>>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<reqwest::Result<Bytes>>(CLIENT_QUEUE_DEPTH);

    tokio::spawn(async move {
        let mut inner = Box::pin(inner);
        let mut framer = SseFramer::new();
        let mut wire = Wire::default();
        // Raw blocks of a `tool_use` block that has not yet completed.
        let mut withheld: Vec<Bytes> = Vec::new();
        // Whether a complete `tool_use` block has actually reached the client.
        // `stop_reason: tool_use` is only honest if this is true.
        let mut delivered_tool_use = false;
        // Set when an open tool block's buffer was thrown away. The rest of
        // that block is dropped too, rather than reaching the client without
        // the `content_block_start` that opened it.
        let mut abandoned = false;
        // Set when the body died. Held as the error, not a string: when there
        // is no message to close, the client is owed the error itself.
        let mut drop_err: Option<reqwest::Error> = None;

        loop {
            let chunk = match inner.next().await {
                Some(Ok(b)) => b,
                Some(Err(e)) => {
                    drop_err = Some(e);
                    break;
                }
                None => break,
            };
            framer.push(&chunk);
            while let Some(raw) = framer.next_raw_block() {
                let Some(v) = payload(&raw) else {
                    // A keepalive or comment. It carries no data and no
                    // ordering, so it goes straight out even mid-tool-block —
                    // holding it back would risk spending the client's idle
                    // timeout on a block that may yet be discarded.
                    if tx.send(Ok(raw)).await.is_err() {
                        return client_gone(&request_id);
                    }
                    continue;
                };
                let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
                let was_open = matches!(wire.open, Some((_, Kind::ToolUse)));
                wire.observe(&v);
                if ty == "message_start" {
                    // A second message through the same body. Whatever the
                    // previous one left open says nothing about this one.
                    withheld.clear();
                    abandoned = false;
                }
                if matches!(
                    ty,
                    "content_block_start" | "content_block_delta" | "content_block_stop"
                ) {
                    // Read the state *after* folding the event in, so the
                    // `content_block_start` that opens a tool block is withheld
                    // along with the deltas that follow it.
                    if matches!(wire.open, Some((_, Kind::ToolUse))) {
                        if abandoned {
                            // A later fragment of a block already given up on.
                            continue;
                        }
                        withheld.push(raw);
                        continue;
                    }
                    if was_open {
                        if abandoned {
                            // The block was given up on while it was open, so
                            // its start never reached the client. Its
                            // `content_block_stop` must not either: a stop for
                            // an index the client never opened is the one
                            // thing it cannot parse.
                            abandoned = false;
                            continue;
                        }
                        if ty == "content_block_stop" {
                            // Complete: release the block in one piece, then
                            // the stop that completed it.
                            for held in withheld.drain(..) {
                                if tx.send(Ok(held)).await.is_err() {
                                    return client_gone(&request_id);
                                }
                            }
                            delivered_tool_use = true;
                        } else {
                            // A new block started over the top of it. Whatever
                            // was buffered is a partial tool call.
                            withheld.clear();
                        }
                    }
                } else if was_open && matches!(ty, "message_delta" | "message_stop" | "error") {
                    // The message is ending, so the tool block will never
                    // complete and its buffer is dropped. The event itself must
                    // still go out: withholding a `message_stop` would leave the
                    // client hanging on exactly the truncated stream this exists
                    // to prevent.
                    //
                    // Only these three end a message. Everything else —
                    // `ping` above all, which Anthropic sends on a timer and so
                    // lands inside a tool block routinely — says nothing about
                    // the block and must leave the buffer alone.
                    withheld.clear();
                    abandoned = true;
                }
                // The gate. A `message_delta` promising a tool call the
                // client never got makes it throw the whole turn away, losing
                // the text it already received and the tokens that bought it.
                // Downgrading the claim keeps the turn valid and its content
                // intact; the model simply ends this turn without a call.
                let raw = if ty == "message_delta"
                    && !delivered_tool_use
                    && v.pointer("/delta/stop_reason").and_then(|r| r.as_str()) == Some("tool_use")
                {
                    tracing::warn!(
                        request_id = %request_id,
                        event = "stop_reason_downgraded",
                        "dropped an incomplete tool block; downgrading \
                         stop_reason to end_turn so the client keeps the turn"
                    );
                    rewrite_stop_reason(&v, "end_turn")
                } else {
                    raw
                };
                if tx.send(Ok(raw)).await.is_err() {
                    return client_gone(&request_id);
                }
            }
        }

        // Anything still withheld belonged to a tool call the model never
        // finished describing. It is dropped on purpose.
        let discarded = withheld.len();
        let tail = wire.tail();
        if tail.is_empty() {
            let Some(e) = drop_err else {
                return;
            };
            if wire.saw_message_start {
                // The message already ended — `message_stop`, or an in-band
                // error the client has seen. A transport error trailing a
                // finished turn is noise, and forwarding it would break a
                // response that is actually complete.
                tracing::debug!(
                    request_id = %request_id,
                    error = %e,
                    "transport error after the message ended; not forwarded"
                );
                return;
            }
            // Nothing ever reached the client, so there is no message to close
            // and nothing to invent. An empty body would read as an empty
            // answer; the error at least says what happened, and it is what
            // this path handed down before.
            tracing::debug!(
                request_id = %request_id,
                error = %e,
                "stream dropped before any message started; passing the error down"
            );
            let _ = tx.send(Err(e)).await;
            return;
        }
        tracing::warn!(
            request_id = %request_id,
            event = "stream_tail_synthesised",
            error = drop_err.map_or("body ended early".to_string(), |e| e.to_string()),
            discarded_tool_blocks = discarded,
            output_tokens = wire.output_tokens,
            "upstream stream died mid-message; closing the turn cleanly so the \
             session survives, with the reply marked truncated"
        );
        for b in tail {
            if tx.send(Ok(b)).await.is_err() {
                return client_gone(&request_id);
            }
        }
    });

    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `chunks` through the finisher and return what the client saw.
    ///
    /// The source ends without erroring, which the finisher treats exactly as
    /// a transport drop — both leave the message unfinished, and only the log
    /// line differs. That keeps the tests free of a hand-built `reqwest::Error`,
    /// which has no public constructor.
    fn drive(chunks: &[&str]) -> String {
        let owned: Vec<reqwest::Result<Bytes>> = chunks
            .iter()
            .map(|c| Ok(Bytes::from(c.to_string())))
            .collect();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let out = finish_on_drop(futures_util::stream::iter(owned), "test".into());
            futures_util::pin_mut!(out);
            let mut s = String::new();
            while let Some(item) = out.next().await {
                s.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
            }
            s
        })
    }

    const START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":0}}}\n\n";
    const TEXT_START: &str = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
    const TEXT_DELTA: &str = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
    const TOOL_START: &str = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\"}}\n\n";
    const TOOL_DELTA: &str = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"rm -rf /ho\"}}\n\n";
    const TOOL_STOP: &str =
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n";

    #[test]
    fn complete_stream_passes_through_untouched() {
        const CLOSE: &str = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let chunks = [START, TEXT_START, TEXT_DELTA, CLOSE];
        assert_eq!(drive(&chunks), chunks.concat());
    }

    #[test]
    fn drop_inside_text_closes_the_block_and_marks_it() {
        let out = drive(&[START, TEXT_START, TEXT_DELTA]);
        assert!(out.contains("[truncated"), "marker missing: {out}");
        assert!(out.contains("\"content_block_stop\",\"index\":0"));
        assert!(out.contains("\"stop_reason\":\"end_turn\""));
        assert!(out.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }

    #[test]
    fn partial_tool_block_never_reaches_the_client() {
        let out = drive(&[START, TEXT_START, TEXT_DELTA, TOOL_START, TOOL_DELTA]);
        assert!(!out.contains("rm -rf"), "partial tool input leaked: {out}");
        assert!(!out.contains("tool_use"), "tool block leaked: {out}");
        // The text before it survives, and the turn still ends properly.
        assert!(out.contains("hi"));
        assert!(out.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }

    #[test]
    fn completed_tool_block_is_released_whole() {
        let out = drive(&[
            START, TEXT_START, TEXT_DELTA, TOOL_START, TOOL_DELTA, TOOL_STOP,
        ]);
        assert!(
            out.contains("rm -rf"),
            "completed tool input withheld: {out}"
        );
        assert!(out.contains("\"tool_use\""));
        assert!(out.contains("\"content_block_stop\",\"index\":1"));
    }

    const PING: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";

    /// Anthropic sends `ping` on a timer, so it lands inside a tool block on
    /// ordinary traffic. Treating it as the end of the message threw the
    /// block's opening away and let the rest through without it, which the
    /// client rejects with "Content block not found".
    #[test]
    fn a_ping_inside_a_tool_block_leaves_the_block_whole() {
        let out = drive(&[
            START, TOOL_START, TOOL_DELTA, PING, TOOL_STOP, MSG_DELTA, MSG_STOP,
        ]);
        let start_at = out.find("\"content_block_start\",\"index\":1");
        let delta_at = out.find("\"content_block_delta\",\"index\":1");
        let stop_at = out.find("\"content_block_stop\",\"index\":1");
        assert!(
            start_at.is_some() && start_at < delta_at && delta_at < stop_at,
            "tool block did not arrive in one piece, in order: {out}"
        );
        assert!(out.contains("rm -rf"), "tool input lost: {out}");
    }

    /// A block given up on mid-flight has no start on the wire, so its
    /// `content_block_stop` has nothing to close and must not be forwarded.
    #[test]
    fn an_abandoned_blocks_stop_is_not_forwarded_alone() {
        let out = drive(&[
            START, TOOL_START, TOOL_DELTA, MSG_DELTA, TOOL_STOP, MSG_STOP,
        ]);
        assert!(
            !out.contains("\"content_block_stop\",\"index\":1"),
            "orphan stop reached the client: {out}"
        );
        assert!(!out.contains("rm -rf"), "partial tool input leaked: {out}");
    }

    #[test]
    fn drop_before_any_block_still_yields_non_empty_content() {
        let out = drive(&[START]);
        assert!(out.contains("\"content_block_start\",\"index\":0"));
        assert!(out.contains("[truncated"));
        assert!(out.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }

    #[test]
    fn drop_before_message_start_synthesises_nothing() {
        assert_eq!(drive(&[]), "");
    }

    #[test]
    fn keepalives_are_forwarded() {
        let out = drive(&[START, ": ping\n\n", TEXT_START, TEXT_DELTA]);
        assert!(out.contains(": ping"), "keepalive dropped: {out}");
    }

    const ERROR_EV: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    const MSG_STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    const CB_STOP0: &str =
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
    const MSG_DELTA: &str = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n";

    #[test]
    fn an_in_band_error_is_never_papered_over() {
        // Anthropic ends a failed generation with `event: error`, and the
        // in-band retry below acts on it. Dressing that up as a finished turn
        // would hide a real failure and strand the retry.
        let out = drive(&[START, TEXT_START, TEXT_DELTA, ERROR_EV]);
        assert!(
            out.contains("overloaded_error"),
            "the error must reach the client: {out}"
        );
        assert!(
            !out.contains("[truncated"),
            "no tail should be synthesised after an error: {out}"
        );
        assert!(!out.contains("message_stop"), "{out}");
    }

    #[test]
    fn a_message_level_event_is_never_swallowed_with_a_tool_block() {
        // The tool block never completes, so it is discarded — but the
        // `message_stop` that ended the turn still has to reach the client, or
        // the stream is truncated by the very code meant to prevent that.
        let out = drive(&[START, TOOL_START, TOOL_DELTA, MSG_STOP]);
        assert!(
            out.contains("message_stop"),
            "message_stop swallowed: {out}"
        );
        assert!(!out.contains("rm -rf"), "partial tool input leaked: {out}");
    }

    #[test]
    fn a_second_round_gets_its_own_ending() {
        // The CCR layer below can answer a retrieval and drive another round
        // through the same body. A `message_stop` from the first round must
        // not convince the finisher the second one already ended.
        let out = drive(&[
            START, TEXT_START, TEXT_DELTA, CB_STOP0, MSG_DELTA, MSG_STOP, START, TEXT_START,
            TEXT_DELTA,
        ]);
        assert_eq!(
            out.matches("event: message_stop").count(),
            2,
            "the second round was left unterminated: {out}"
        );
        assert!(out.contains("[truncated"), "{out}");
    }

    #[test]
    fn a_split_chunk_boundary_does_not_lose_an_event() {
        let joined = [START, TEXT_START, TEXT_DELTA].concat();
        let (a, b) = joined.split_at(joined.len() / 2);
        let out = drive(&[a, b]);
        assert!(out.contains("hi"), "event lost across the split: {out}");
        assert!(out.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }

    /// An incomplete tool block is dropped, so the `stop_reason` that promised
    /// it must not survive: a client told to expect a tool call it never got
    /// rejects the entire turn and the text already sent goes with it.
    #[test]
    fn stop_reason_downgraded_when_tool_block_dropped() {
        let out = drive(&[
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            // A tool block that never completes: withheld, then dropped.
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        assert!(
            !out.contains("\"stop_reason\":\"tool_use\""),
            "tool_use claim survived a dropped tool block:\n{out}"
        );
        assert!(
            out.contains("\"stop_reason\":\"end_turn\""),
            "no downgrade:\n{out}"
        );
        assert!(
            out.contains("hi"),
            "text the client already paid for was lost:\n{out}"
        );
        assert!(
            !out.contains("\"index\":1"),
            "partial tool block leaked:\n{out}"
        );
    }

    /// A tool call that completes must keep its `stop_reason` untouched.
    #[test]
    fn stop_reason_preserved_when_tool_block_completes() {
        let out = drive(&[
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        assert!(
            out.contains("\"stop_reason\":\"tool_use\""),
            "downgraded a good turn:\n{out}"
        );
        assert!(out.contains("\"tool_use\""), "tool block missing:\n{out}");
    }
}

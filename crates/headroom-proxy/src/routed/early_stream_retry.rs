//! Re-issue a routed streaming request whose body dies before the client
//! is committed.
//!
//! The direct Anthropic path has this in `sse::stream_retry`: the opening
//! bytes are held back, and a transport drop inside the hold window
//! re-sends the request instead of killing the turn. The routed path had
//! no equivalent — a stream that died early went straight to
//! `stream_finisher`, which can only mark the turn truncated.
//!
//! This was first put down to Zen shedding load by closing streams cleanly
//! mid-turn (2026-09-24: Spark turns dying with no output tokens, several
//! in the same millisecond). That reading was wrong. Re-measured on
//! 2026-09-25, the drops were `ConnectionReset`s, and Codex streams died
//! with them. The zen-rotate watcher was rotating the device-wide VPN on
//! every Zen 429, and the translator ended each aborted stream cleanly,
//! so `stream_finisher` saw what looked like a clean EOF. Most deaths come
//! after the hold has committed, so this wrapper cannot save them; the
//! cure is the per-egress pool, where a 429 rotates one Zen lane and
//! leaves the shared route alone.
//!
//! The clean-EOF check stays as cheap insurance. A clean EOF carries no
//! transport error, so there is nothing to match on except the shape of
//! what arrived: an SSE stream that ends without its terminal event
//! (`response.completed` / `failed` / `incomplete`, or chat `[DONE]`) was
//! cut, not finished.
//!
//! So this holds the opening bytes instead of forwarding them. While the
//! held buffer is under `hold_bytes` the response is uncommitted: a
//! transport error there, or a clean EOF with SSE framing but no terminal
//! event, discards the buffer and re-sends through `send_with_retry` (so
//! the resend gets a fresh egress, gate, and nonce like any other send),
//! and the client sees one clean stream. Once the buffer fills it is
//! flushed, the response is committed, and any later error propagates
//! exactly as it did before.
//!
//! The cost is the same one the direct path pays: the first `hold_bytes`
//! of every routed stream arrive in one burst rather than token by token.
//! A body with no SSE framing at all (a JSON answer on the streaming path)
//! is never retried — without framing there is no truncation signature
//! to read, and re-sending a complete answer would only duplicate it.

use bytes::Bytes;
use futures_util::StreamExt;

use crate::proxy::AppState;

/// Everything needed to issue the routed request again.
#[derive(Clone)]
pub(crate) struct EarlyRetryCtx {
    pub state: AppState,
    pub url: String,
    pub headers: http::HeaderMap,
    pub body: Bytes,
    pub request_id: String,
    pub session_key: Option<String>,
    pub lane_key: Option<String>,
    pub is_chatgpt_auth: bool,
    pub is_zen: bool,
    /// Bytes held back before the response counts as committed. Mirrors
    /// `retry_stream_hold_bytes` on the direct path.
    pub hold_bytes: usize,
    /// Total re-sends after the first attempt, bounded by the same
    /// `retry_max_attempts` that bounds header retries.
    pub max_resends: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

/// Depth of the hand-off queue to the client. Matches `stream_retry`.
const CLIENT_QUEUE_DEPTH: usize = 64;

/// Terminal events of the two stream shapes this path translates. Matched
/// as whole SSE lines: an `event:` name (Responses) or the `data: [DONE]`
/// sentinel (chat). A substring match fired on model text: a delta saying
/// `[DONE]` committed the hold early, and a drop after that was never
/// retried. JSON escapes newlines, so no delta can start a line.
fn has_terminal_event(held: &[Bytes]) -> bool {
    // Joined per check; the hold window is bytes, not megabytes.
    let joined: Vec<u8> = held.iter().flat_map(|b| b.iter().copied()).collect();
    let text = String::from_utf8_lossy(&joined);
    text.lines().any(|line| {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.strip_prefix("event:") {
            return matches!(
                name.trim(),
                "response.completed" | "response.failed" | "response.incomplete"
            );
        }
        line.strip_prefix("data:")
            .is_some_and(|data| data.trim() == "[DONE]")
    })
}

/// Whether the held bytes look like an SSE stream at all. A complete JSON
/// answer mistakenly routed down the streaming path has no framing; without
/// this check its clean EOF would read as a truncation and re-send a
/// turn that already finished.
fn has_sse_framing(held: &[Bytes]) -> bool {
    held.iter().any(|b| {
        let text = String::from_utf8_lossy(b);
        text.contains("event:") || text.contains("data:")
    })
}

/// Whether a body that stopped part-way is worth asking for again.
///
/// Mirrors `stream_retry::is_retryable_drop`: reqwest calls a mid-body cut
/// `Decode` ("error decoding response body") because the chunked encoding
/// ended where it should not have.
fn is_retryable_drop(e: &reqwest::Error) -> bool {
    e.is_decode() || crate::proxy::is_retryable_transport_error(e)
}

/// Wrap an accepted routed body stream so an early drop re-issues the
/// request. `None` context passes the stream through untouched.
pub(crate) fn wrap_streaming_body(
    first: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>,
    ctx: Option<EarlyRetryCtx>,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>> {
    let Some(ctx) = ctx else {
        return first;
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<reqwest::Result<Bytes>>(CLIENT_QUEUE_DEPTH);

    tokio::spawn(async move {
        let mut stream = first;
        let mut resends: u32 = 0;

        loop {
            // Bytes read but not yet forwarded. Empty once the response commits.
            let mut held: Vec<Bytes> = Vec::new();
            let mut held_len: usize = 0;
            let mut committed = false;
            // Set when the body dropped early and a fresh attempt is due.
            // A complete turn (terminal seen, non-SSE body, or committed)
            // and an out-of-budget drop both flush below instead.
            enum Drop {
                Transport(reqwest::Error),
                Truncated,
            }
            let mut drop: Option<Drop> = None;

            while let Some(item) = stream.next().await {
                match item {
                    Ok(b) => {
                        if committed {
                            if tx.send(Ok(b)).await.is_err() {
                                return; // client hung up
                            }
                            continue;
                        }
                        held_len += b.len();
                        held.push(b);
                        if has_terminal_event(&held) || held_len >= ctx.hold_bytes {
                            for h in held.drain(..) {
                                if tx.send(Ok(h)).await.is_err() {
                                    return;
                                }
                            }
                            committed = true;
                        }
                    }
                    Err(e) => {
                        if !committed && resends < ctx.max_resends && is_retryable_drop(&e) {
                            drop = Some(Drop::Transport(e));
                            break;
                        }
                        // Committed, out of budget, or not a transport cut:
                        // the client keeps whatever it was owed, then the
                        // error, as before.
                        for h in held.drain(..) {
                            if tx.send(Ok(h)).await.is_err() {
                                return;
                            }
                        }
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }

            if drop.is_none() {
                if committed
                    || has_terminal_event(&held)
                    || !has_sse_framing(&held)
                    || resends >= ctx.max_resends
                {
                    // A complete turn (terminal seen, or a non-SSE body),
                    // or nothing left to spend: hand over every byte.
                    for h in held.drain(..) {
                        if tx.send(Ok(h)).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
                drop = Some(Drop::Truncated);
            }
            let (kind, err_detail, reason): (&str, String, &str) = match drop {
                Some(Drop::Transport(e)) => (
                    "transport",
                    e.to_string(),
                    crate::observability::retry_reason::TRANSPORT,
                ),
                Some(Drop::Truncated) => (
                    "clean-eof",
                    "body ended early".to_string(),
                    crate::observability::retry_reason::TRUNCATED,
                ),
                None => unreachable!("drop is always set here"),
            };

            // `held` is dropped here: those bytes belong to the dead attempt
            // and the replacement response brings its own preamble.
            resends += 1;
            let delay_ms = ctx
                .base_delay_ms
                .saturating_mul(1u64.checked_shl(resends - 1).unwrap_or(u64::MAX))
                .min(ctx.max_delay_ms);
            tracing::warn!(
                event = "routed_stream_early_retry",
                request_id = %ctx.request_id,
                kind = kind,
                error = %err_detail,
                resend = resends,
                max_resends = ctx.max_resends,
                delay_ms = delay_ms,
                held_bytes = held_len,
                "routed stream dropped before the client was committed; re-sending"
            );
            crate::observability::record_upstream_retry("routed", reason);
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

            // Same hygiene as every other repeat send: the real client
            // mints a request id per POST (no-op off zen routes).
            let mut headers = ctx.headers.clone();
            crate::routed::quirks::refresh_zen_request_id(&mut headers);
            let send = super::retry::send_with_retry(
                &ctx.state,
                &ctx.url,
                headers,
                ctx.body.clone(),
                &ctx.request_id,
                ctx.session_key.as_deref(),
                ctx.lane_key.as_deref(),
                ctx.is_chatgpt_auth,
                ctx.is_zen,
            )
            .await;
            match send {
                Ok(next) if next.resp.status().is_success() => {
                    stream = Box::pin(next.resp.bytes_stream());
                }
                Ok(next) => {
                    // A status the upstream chose is an answer, not a
                    // stumble. Serve what the first attempt delivered and
                    // let the layers above read the refusal.
                    tracing::warn!(
                        event = "routed_stream_early_retry_non_success",
                        request_id = %ctx.request_id,
                        status = next.resp.status().as_u16(),
                        "retry of a dropped routed stream came back non-success; giving up"
                    );
                    crate::observability::record_upstream_retry_exhausted("routed", reason);
                    for h in held.drain(..) {
                        if tx.send(Ok(h)).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
                Err(_) => {
                    // The resend never got off the ground; same give-up.
                    tracing::warn!(
                        event = "routed_stream_early_retry_send_failed",
                        request_id = %ctx.request_id,
                        "retry of a dropped routed stream failed to send; giving up"
                    );
                    crate::observability::record_upstream_retry_exhausted("routed", reason);
                    for h in held.drain(..) {
                        if tx.send(Ok(h)).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
            }
        }
    });

    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive held bytes through the commit/terminal/framing predicates the
    /// wrapper decides on. The resend loop itself is exercised by the
    /// integration test, which owns a real upstream.
    #[test]
    fn terminal_events_are_recognized() {
        let completed = vec![Bytes::from("event: response.completed\ndata: {}\n\n")];
        assert!(has_terminal_event(&completed));
        for ev in ["response.failed", "response.incomplete"] {
            let b = vec![Bytes::from(format!("event: {ev}\ndata: {{}}\n\n"))];
            assert!(has_terminal_event(&b), "{ev}");
        }
        let done = vec![Bytes::from("data: [DONE]\n\n")];
        assert!(has_terminal_event(&done));
        let mid = vec![Bytes::from("event: response.created\ndata: {}\n\n")];
        assert!(!has_terminal_event(&mid));
    }

    #[test]
    fn framing_check_passes_sse_and_json_through_correctly() {
        let sse = vec![Bytes::from("event: response.created\ndata: {}\n\n")];
        assert!(has_sse_framing(&sse));
        let json = vec![Bytes::from(r#"{"id":"resp_1","status":"completed"}"#)];
        assert!(!has_sse_framing(&json));
        assert!(!has_sse_framing(&[]));
    }
}

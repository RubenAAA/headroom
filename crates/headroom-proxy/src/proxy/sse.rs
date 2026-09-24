//! SSE plumbing: parser tee, Anthropic stream rewrite, buffered-response
//! reframing, stream classification, and the telemetry state machine.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Tee every streamed chunk toward the SSE state machine without ever holding
/// up the client: `try_send` failures (parser behind, queue full/closed) are
/// logged + counted, never awaited. Mid-stream transport errors are logged with
/// their `Debug`-only source chain — the only thing separating a TLS record
/// failure from an idle drop — and passed through unchanged.
pub(super) fn tee_stream_to_parser<S>(
    upstream_body: S,
    parser_tx: Option<tokio::sync::mpsc::Sender<bytes::Bytes>>,
    parser_telemetry: std::sync::Arc<ParserTelemetry>,
    request_id: String,
) -> futures_util::stream::Map<
    S,
    impl FnMut(Result<bytes::Bytes, reqwest::Error>) -> Result<bytes::Bytes, reqwest::Error>,
>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
{
    let rid = request_id;
    upstream_body.map(move |r| match r {
        Ok(b) => {
            if let Some(tx) = &parser_tx {
                if let Err(e) = tx.try_send(b.clone()) {
                    parser_telemetry
                        .dropped_chunks
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::debug!(
                        request_id = %rid,
                        error = %e,
                        "sse parser queue full or closed; skipping telemetry chunk"
                    );
                } else {
                    parser_telemetry
                        .sent_chunks
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Ok(b)
        }
        Err(e) => {
            // `cause` for the same reason as `stream_finisher`: the source
            // chain is `Debug`-only and it is the only thing that separates a
            // TLS record failure from an idle drop.
            tracing::warn!(
                event = "upstream_stream_mid_response_error",
                request_id = %rid,
                error = %e,
                cause = ?e,
                "upstream stream error mid-response"
            );
            Err(e)
        }
    })
}

/// Spawn the SSE state-machine tee: bytes flow to the client unchanged while a
/// spawned task sinks them into an mpsc the state machine drains. The parser is
/// detached from forwarding but its JoinHandle is kept: a waiter task makes
/// panics/cancellation operator-visible instead of erasing the only completion
/// record. The mpsc is bounded; when the parser falls behind `try_send` fails
/// and chunks are logged + dropped — the byte path is never blocked.
pub(super) fn spawn_sse_parser_tee(
    sse_kind: SseStreamKind,
    state: &AppState,
    outcome_ctx: Option<OutcomeContext>,
    ccr_round_usage: Option<Arc<Mutex<CcrRoundUsage>>>,
    status: StatusCode,
    request_id: &str,
    parser_telemetry: &std::sync::Arc<ParserTelemetry>,
) -> Option<tokio::sync::mpsc::Sender<bytes::Bytes>> {
    if !matches!(sse_kind, SseStreamKind::None) {
        let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(SSE_PARSER_QUEUE_DEPTH);
        let rid_for_parser = request_id.to_owned();
        // Freeze-replay: hand the state machine a store handle so the
        // Anthropic arm can feed the final usage's cache tokens back
        // into the session tracker (`SessionReplayStore::complete`) —
        // the request→response correlation the Python handler did
        // inline. `None` when the feature is off so the flag-off path
        // is observably unchanged.
        let replay_store_for_parser = if state.config.prefix_replay {
            Some(state.replay_store.clone())
        } else {
            None
        };
        let parser_task = tokio::spawn(run_sse_state_machine(
            sse_kind,
            rx,
            rid_for_parser.clone(),
            state.usage_observer.clone(),
            outcome_ctx,
            replay_store_for_parser,
            ccr_round_usage,
            status,
        ));
        // Keep the parser detached from response forwarding, but do not drop
        // its JoinHandle: a panic would otherwise erase the only completion
        // record for this request. The waiter preserves the streaming path and
        // makes task panics/cancellation operator-visible.
        let waiter_telemetry = parser_telemetry.clone();
        tokio::spawn(async move {
            let result = parser_task.await;
            let sent_chunks = waiter_telemetry
                .sent_chunks
                .load(std::sync::atomic::Ordering::Relaxed);
            let dropped_chunks = waiter_telemetry
                .dropped_chunks
                .load(std::sync::atomic::Ordering::Relaxed);
            match result {
                // A clean finish is already announced once per stream by
                // `sse stream closed`, so this stays quiet unless the chunk
                // counts say something that line cannot: a parser that missed
                // input because its queue was full or already closed. Logging
                // every clean finish at info would double the per-stream volume
                // of a log that is never rotated.
                Ok(()) if dropped_chunks > 0 => tracing::warn!(
                    event = "sse_missed_chunks",
                    request_id = %rid_for_parser,
                    sent_chunks,
                    dropped_chunks,
                    "sse state-machine task completed having missed chunks; \
                     its usage totals are short by whatever those carried"
                ),
                Ok(()) => tracing::debug!(
                    request_id = %rid_for_parser,
                    sent_chunks,
                    dropped_chunks,
                    "sse state-machine task completed"
                ),
                Err(error) => tracing::error!(
                    event = "sse_task_failed",
                    request_id = %rid_for_parser,
                    sent_chunks,
                    dropped_chunks,
                    task_panic = error.is_panic(),
                    task_cancelled = error.is_cancelled(),
                    error = %error,
                    "sse state-machine task failed"
                ),
            }
        });
        Some(tx)
    } else {
        None
    }
}

/// Streamed-path CCR retrieval: answer the offered `headroom_retrieve` tool call
/// inside the Anthropic SSE stream — suppressing the block, running the
/// continuation against `continuation_base`, splicing the result back in — so
/// the streamed turn behaves like the buffered one. When `ccr_stream_eligible`
/// is false the upstream body is handed on untouched.
//
// Ten request-context args for one call site; a params struct would churn
// both without buying clarity, so the lint stays off here by decision.
#[allow(clippy::too_many_arguments)]
pub(super) async fn maybe_rewrite_anthropic_stream(
    upstream_body: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
    >,
    ccr_stream_eligible: bool,
    continuation_base: bytes::Bytes,
    original_buffered: bytes::Bytes,
    state: &AppState,
    upstream_client: &reqwest::Client,
    upstream_url: &url::Url,
    outgoing_headers: &http::HeaderMap,
    headers_snapshot: &Option<http::HeaderMap>,
    request_id: &str,
) -> (
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    Option<Arc<Mutex<CcrRoundUsage>>>,
) {
    if ccr_stream_eligible {
        let ctx = crate::sse::ccr_stream::CcrStreamContext {
            client: upstream_client.clone(),
            upstream_url: upstream_url.clone(),
            outgoing_headers: outgoing_headers.clone(),
            forwarded_request: continuation_base.clone(),
            ccr_store: state
                .ctx_offload
                .as_ref()
                .expect("ctx_offload checked above")
                .store
                .ccr(),
            ccr_stores: state.ctx_offload.as_ref().map(|r| r.store.stores()),
            config: state.config.clone(),
            request_id: request_id.to_owned(),
            shape: crate::sse::ccr_stream::CcrShape::Anthropic,
            memory: memory_tool_context(
                state,
                headers_snapshot,
                Some("anthropic"),
                &original_buffered,
            )
            .await,
            // Anthropic path: redaction lives on routed translate paths only.
            redact: None,
            // Anthropic path: the caller folds the returned handle itself.
            rounds_sink: None,
        };
        let (stream, usage) = crate::sse::ccr_stream::rewrite_anthropic_stream(upstream_body, ctx);
        (Box::pin(stream), Some(usage))
    } else {
        (Box::pin(upstream_body), None)
    }
}

/// #2613 edge, port of `_openai_responses_from_sse`: some OpenAI-compatible
/// upstreams answer a `stream: false` request with a valid 200 SSE body. When
/// this request was buffered for Responses CCR, collect that stream and
/// reassemble the terminal JSON so the buffered arm below still resolves
/// retrieval. Without a terminal event the collected bytes stream through
/// unchanged (previous behaviour, error included).
pub(super) async fn reframe_buffered_responses_sse(
    mut upstream_body: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
    >,
    is_sse: bool,
    sse_kind: SseStreamKind,
    buffered_responses_ccr: bool,
    status: StatusCode,
    request_id: &str,
) -> (
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    bool,
    SseStreamKind,
) {
    let (mut is_sse, mut sse_kind) = (is_sse, sse_kind);
    if buffered_responses_ccr && is_sse && status.is_success() {
        use futures_util::StreamExt as _;
        let mut collected = bytes::BytesMut::new();
        let mut first_err: Option<reqwest::Error> = None;
        {
            let s = &mut upstream_body;
            while let Some(chunk) = s.next().await {
                match chunk {
                    Ok(b) => collected.extend_from_slice(&b),
                    Err(e) => {
                        first_err = Some(e);
                        break;
                    }
                }
            }
        }
        let reassembled: Option<bytes::Bytes> = if first_err.is_none() {
            // Whole stream collected cleanly: reassemble the terminal JSON
            // when one is present.
            crate::openai_buffered_ccr::responses_completed_from_sse(&String::from_utf8_lossy(
                &collected,
            ))
            .and_then(|completed| serde_json::to_vec(&completed).ok())
            .map(bytes::Bytes::from)
        } else {
            None
        };
        if let Some(json_bytes) = reassembled {
            tracing::info!(
                request_id = %request_id,
                event = "buffered_responses_ccr_sse_answer",
                "upstream answered stream:false with SSE; reassembled terminal JSON for buffered handling"
            );
            upstream_body = Box::pin(futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(
                json_bytes,
            )]));
            is_sse = false;
            sse_kind = SseStreamKind::None;
        } else {
            let mut items: Vec<reqwest::Result<bytes::Bytes>> = vec![Ok(collected.freeze())];
            if let Some(e) = first_err {
                items.push(Err(e));
            }
            upstream_body = Box::pin(futures_util::stream::iter(items));
        }
    }
    (upstream_body, is_sse, sse_kind)
}

/// PR-C1: classify the SSE flavor for the response state machine from the request
/// path. Bytes flow to the client unchanged; the state machine sinks bytes into
/// a channel in a spawned task that never blocks the byte path.
///
/// PR-C4: the OpenAI Responses arm is gated by `enable_responses_streaming`.
/// When false the tee short-circuits to `None` so the framer + state machine
/// don't spin up and bytes flow opaquely. Other providers' state machines are
/// unaffected.
pub(super) fn classify_sse_kind(
    is_sse: bool,
    request_path: &str,
    enable_responses_streaming: bool,
    request_id: &str,
) -> SseStreamKind {
    if is_sse {
        let kind = SseStreamKind::for_request_path(request_path);
        if matches!(kind, SseStreamKind::OpenAiResponses) && !enable_responses_streaming {
            tracing::info!(
                request_id = %request_id,
                path = %request_path,
                event = "responses_streaming_state_machine_skipped",
                reason = "enable_responses_streaming=false",
                "PR-C4 streaming pipeline disabled; SSE bytes pass through without telemetry"
            );
            SseStreamKind::None
        } else {
            kind
        }
    } else {
        SseStreamKind::None
    }
}

/// Bound on the in-flight queue between the byte-passthrough and the
/// SSE state-machine task. Picked so that under steady-state streaming
/// load (~5 events/100ms typical) the parser is never blocked on
/// queue space, yet a stalled parser can't grow memory unboundedly.
/// Tunable via `proxy.toml` if a deployment finds this insufficient.
pub(super) const SSE_PARSER_QUEUE_DEPTH: usize = 256;

/// Which provider's state machine should run on this stream. Picked
/// from the *request* path because the response content-type
/// (`text/event-stream`) is identical across providers.
#[derive(Debug, Clone, Copy)]
pub(super) enum SseStreamKind {
    None,
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
}

impl SseStreamKind {
    pub(super) fn for_request_path(path: &str) -> Self {
        match path {
            "/v1/messages" => Self::Anthropic,
            "/v1/chat/completions" => Self::OpenAiChat,
            "/v1/responses" => Self::OpenAiResponses,
            // No telemetry parser registered for this endpoint.
            // We still pass bytes through unchanged.
            _ => Self::None,
        }
    }
}

pub(super) fn is_sse_response(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let media_type = s.split(';').next().unwrap_or("").trim();
            media_type.eq_ignore_ascii_case("text/event-stream")
        })
        .unwrap_or(false)
}

/// Whether the client asked for a streamed answer.
///
/// Anthropic treats a missing `stream` as false, so an absent key means the
/// caller wants one JSON body. An unreadable body is the one case that reads
/// as `true`: the streaming path is what the proxy did before this check
/// existed, so a body we cannot parse keeps that behaviour rather than
/// converting a stream the client may well have wanted.
pub(super) fn client_wants_stream(client_body: &bytes::Bytes) -> bool {
    match serde_json::from_slice::<serde_json::Value>(client_body) {
        Ok(v) => v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
        Err(_) => true,
    }
}

/// Read an Anthropic event stream to its end and rebuild the single JSON reply
/// it describes.
///
/// `Err` when the stream never reached `message_stop`, which is the only
/// honest answer for a caller that cannot be handed a partial turn: it asked
/// for one complete message and there is no way to say "half" in that shape.
pub(super) async fn buffer_sse_as_message<S, E>(
    stream: S,
    request_id: &str,
) -> Result<Vec<u8>, String>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures_util::StreamExt;

    let mut framer = crate::sse::framing::SseFramer::new();
    let mut state = crate::sse::anthropic::AnthropicStreamState::new();
    let mut stream = stream;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream error: {e}"))?;
        framer.push(&chunk);
        while let Some(event) = framer.next_event() {
            let event = event.map_err(|e| format!("framing error: {e}"))?;
            if event.is_done_sentinel() {
                continue;
            }
            // A state error means the stream contradicted itself; the rebuilt
            // message would be a guess, so fail instead.
            state
                .apply(event)
                .map_err(|e| format!("stream state error: {e}"))?;
        }
    }

    if state.status != crate::sse::anthropic::StreamStatus::MessageStop {
        return Err(format!(
            "stream ended in {:?} without message_stop",
            state.status
        ));
    }

    let message = crate::sse::ccr_stream::rebuild_message(&state);
    tracing::debug!(
        request_id = %request_id,
        event = "sse_buffered_for_non_streaming_client",
        "rebuilt a non-streaming reply from an event stream"
    );
    serde_json::to_vec(&message).map_err(|e| format!("serialize error: {e}"))
}

/// Latch time-to-first-byte on the first upstream chunk. Every SSE arm calls
/// this from its receive loop; the value is written once and never overwritten.
pub(super) fn latch_ttfb(ttfb_ms: &mut f64, outcome_ctx: &Option<OutcomeContext>) {
    if *ttfb_ms == 0.0 {
        if let Some(ctx) = outcome_ctx.as_ref() {
            *ttfb_ms = ctx.started_at.elapsed().as_secs_f64() * 1000.0;
        }
    }
}

#[derive(Default)]
pub(super) struct ParserTelemetry {
    pub(super) sent_chunks: std::sync::atomic::AtomicU64,
    pub(super) dropped_chunks: std::sync::atomic::AtomicU64,
}

/// Drive the per-provider state machine over a stream of byte chunks.
/// Lives in its own task; the byte path never waits on it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_sse_state_machine(
    kind: SseStreamKind,
    rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    request_id: String,
    usage_observer: Arc<cache_stabilization::usage_observer::UsageObserver>,
    outcome_ctx: Option<OutcomeContext>,
    replay_store: Option<SessionReplayStore>,
    // Usage of CCR continuation rounds the client never saw, filled in by
    // `sse::ccr_stream` before this task's channel closes. `None` when the
    // rewriter did not run.
    ccr_round_usage: Option<Arc<Mutex<CcrRoundUsage>>>,
    // HTTP status the stream arrived with (upstream 4949cd55). The close
    // arms stamp it onto the outcome so a 5xx served as SSE books failed
    // instead of success. `Copy`, so the spawn site just moves it in.
    upstream_status: StatusCode,
) {
    use crate::sse::framing::SseFramer;

    let framer = SseFramer::new();
    // Time to first byte from upstream. Only the first chunk marks it, so it is
    // latched once and never overwritten. Declared outside the match because
    // every arm needs it — leaving it in one arm made the other providers
    // report a 0 that the histogram then silently dropped.
    let mut ttfb_ms: f64 = 0.0;
    // The state machines are different types; rather than introducing
    // a trait object dance, run each variant in its own arm. The dead
    // branches compile out cleanly and the hot path stays monomorphic.
    match kind {
        SseStreamKind::Anthropic => {
            let state = sse_anthropic::drive_anthropic_stream(
                rx,
                framer,
                &request_id,
                &outcome_ctx,
                &mut ttfb_ms,
            )
            .await;
            // Snapshot hidden continuation usage before any cache observer
            // runs. The final streamed usage belongs to the proxy's private
            // continuation request; the first discarded response below is the
            // cache footprint of the request the client actually sent.
            let ccr_rounds = ccr_round_usage
                .as_ref()
                .and_then(|u| u.lock().ok().map(|g| *g))
                .unwrap_or_default();
            let (cache_baseline_input, cache_baseline_read, cache_baseline_write) = ccr_rounds
                .client_cache_baseline(
                    state.usage.input_tokens,
                    state.usage.cache_read_input_tokens,
                    state.usage.cache_creation_input_tokens,
                );
            let close = sse_anthropic::AnthropicClose {
                state: &state,
                ccr_rounds,
                cache_baseline_input,
                cache_baseline_read,
                cache_baseline_write,
                usage_observer: &usage_observer,
                outcome_ctx: &outcome_ctx,
                replay_store: &replay_store,
                request_id: &request_id,
                ttfb_ms,
                upstream_status,
            };
            sse_anthropic::note_anthropic_billed_totals(&close);
            sse_anthropic::note_anthropic_hit_rate(&close);
            sse_anthropic::complete_anthropic_watchdog(&close);
            sse_anthropic::complete_anthropic_replay(&close);
            sse_anthropic::log_anthropic_close(&close);
            sse_anthropic::book_anthropic_incomplete(&close);
            sse_anthropic::emit_anthropic_outcome(&close);
        }
        SseStreamKind::OpenAiChat => {
            let state = sse_openai::drive_openai_chat_stream(
                rx,
                framer,
                &request_id,
                &outcome_ctx,
                &mut ttfb_ms,
            )
            .await;
            sse_openai::close_openai_chat_stream(
                &state,
                &request_id,
                ttfb_ms,
                &outcome_ctx,
                upstream_status,
            );
        }
        SseStreamKind::OpenAiResponses => {
            let state = sse_openai::drive_openai_responses_stream(
                rx,
                framer,
                &request_id,
                &outcome_ctx,
                &mut ttfb_ms,
            )
            .await;
            sse_openai::close_openai_responses_stream(
                &state,
                &request_id,
                ttfb_ms,
                &outcome_ctx,
                upstream_status,
            );
        }
        SseStreamKind::None => {}
    }
}

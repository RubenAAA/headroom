//! Request shaping and response arms for the routed paths.

use crate::openai::response::{openai_to_anthropic_response, responses_stream_to_turn};
use crate::openai::stream::{translate_openai_stream_to_anthropic, DeferredCcrBooking};
use crate::routed::ccr::{resolve_routed_proxy_tools, RoutedCcr};
use crate::routed::outcome::{
    book_routed_outcome, book_routed_outcome_with_ccr, RoutedOutcomeContext,
};
use crate::routed::redaction::{redact_table_for, restore_buffered, restore_streaming};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

/// Shared resolve-then-book envelope for the buffered arms (C6): CCR
/// continuations resolve first — their rounds are billed too, so booking the
/// first round's usage as the turn's would under-report the retrieval — then
/// the turn books once off the resolved body's usage. The arms differ only in
/// how the body was parsed and translated, never in this ordering.
async fn resolve_and_book(
    body: Value,
    ccr: Option<RoutedCcr>,
    outcome: Option<&RoutedOutcomeContext>,
    output_tokens: i64,
) -> Value {
    let (resolved, ccr_rounds) = match ccr {
        Some(ccr) => resolve_routed_proxy_tools(&body, &ccr).await,
        None => (body, crate::proxy::CcrRoundUsage::default()),
    };
    if let Some(ctx) = outcome {
        book_routed_outcome_with_ccr(
            ctx,
            resolved.get("usage"),
            output_tokens,
            0.0,
            200,
            ccr_rounds,
        );
    }
    resolved
}

pub(crate) fn apply_target_model_override(
    mut body: Value,
    target_model: Option<&str>,
    force_store_false: bool,
    force_stream_true: bool,
) -> Value {
    if let Some(target) = target_model {
        body["model"] = Value::String(target.to_string());
    }
    if force_store_false {
        body["store"] = Value::Bool(false);
    }
    if force_stream_true {
        body["stream"] = Value::Bool(true);
    }
    body
}

/// Return a routed upstream failure without translating its status or
/// `Retry-After`. This is especially important when the requested delay is
/// longer than our in-request cap: retrying early violates the upstream's
/// instruction, while converting a 429 stream into a 200 SSE body hides it
/// from the client that can schedule the next request correctly.
pub(crate) async fn handle_routed_error_response(
    upstream_resp: reqwest::Response,
    upstream_status: StatusCode,
    outcome: Option<RoutedOutcomeContext>,
) -> Response {
    let retry_after = upstream_resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .cloned();
    let body_text = upstream_resp.text().await.unwrap_or_default();
    if let Some(ctx) = outcome.as_ref() {
        book_routed_outcome(ctx, None, 0, 0.0, upstream_status.as_u16() as i64);
    }
    // An error body can echo the redacted request; restore before handing it
    // to the client like any other inbound body.
    let body_text =
        String::from_utf8_lossy(&restore_buffered(outcome.as_ref(), body_text.into_bytes()))
            .into_owned();
    // A 413 from Zen is a measured fact about the outbound body, not the
    // canned "32MB of images" note the client prints for any 413: its zod
    // error parse fails on Zen's plain-text body, so it falls back to blaming
    // attachments that were never there.
    let outbound_bytes = outcome.as_ref().map(|ctx| ctx.outbound_bytes);
    tracing::warn!(
        event = "local_model_upstream_error",
        status = upstream_status.as_u16(),
        body = %body_text.chars().take(200).collect::<String>(),
        body_len = body_text.len(),
        retry_after_preserved = retry_after.is_some(),
        outbound_bytes = outbound_bytes.unwrap_or(0),
        "local model upstream returned error"
    );
    if upstream_status == StatusCode::PAYLOAD_TOO_LARGE {
        let bytes = outbound_bytes.unwrap_or(0);
        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "request_too_large",
                "message": format!(
                    "Upstream refused the request body ({} bytes, {:.1}MB): no images or attachments were sent. Run /compact or drop tool results to shrink the turn.",
                    bytes,
                    bytes as f64 / 1_048_576.0,
                ),
            }
        })
        .to_string();
        return Response::builder()
            .status(upstream_status)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("static response");
    }
    let mut response = Response::builder().status(upstream_status);
    if let Some(value) = retry_after {
        response = response.header(http::header::RETRY_AFTER, value);
    }
    response
        .body(Body::from(body_text))
        .expect("static response")
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic → OpenAI
// ---------------------------------------------------------------------------

/// Buffered (non-streaming) Chat Completions reply, translated back to the
/// Anthropic response shape.
///
/// Resolves `headroom_retrieve` before translating, on the OpenAI shape the
/// upstream actually returned. The streaming arm does the same through
/// `sse::ccr_stream`; both are required, because this path injects the tool
/// and a tool the client cannot run must never leave the proxy.
/// Read a routed turn's body, or hand back the response the caller should
/// return instead.
///
/// A non-OK status books the turn against its own status code before it goes
/// back to the client — the spend is real whether or not the turn succeeded.
/// Both buffered arms need exactly this, and having written it twice is how the
/// two drifted the first time.
pub(crate) async fn read_routed_body(
    upstream_resp: reqwest::Response,
    upstream_status: StatusCode,
    outcome: Option<&RoutedOutcomeContext>,
) -> Result<String, Response> {
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if status != StatusCode::OK {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        if let Some(ctx) = outcome {
            book_routed_outcome(ctx, None, 0, 0.0, status.as_u16() as i64);
        }
        // Same measured 413 as the explicit error arm above: the buffered
        // fold reaches non-OK statuses through here, not through
        // `handle_routed_error_response`.
        let outbound_bytes = outcome.map(|ctx| ctx.outbound_bytes).unwrap_or(0);
        tracing::warn!(
            event = "local_model_upstream_error",
            status = status.as_u16(),
            body = %body_text.chars().take(200).collect::<String>(),
            body_len = body_text.len(),
            outbound_bytes,
            "local model upstream returned error"
        );
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            let body = serde_json::json!({
                "type": "error",
                "error": {
                    "type": "request_too_large",
                    "message": format!(
                        "Upstream refused the request body ({} bytes, {:.1}MB): no images or attachments were sent. Run /compact or drop tool results to shrink the turn.",
                        outbound_bytes,
                        outbound_bytes as f64 / 1_048_576.0,
                    ),
                }
            })
            .to_string();
            return Err(Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("static response"));
        }
        return Err(Response::builder()
            .status(status)
            .body(Body::from(body_text))
            .expect("static response"));
    }

    upstream_resp.text().await.map_err(|e| {
        tracing::warn!(
            event = "local_model_response_parse_error",
            error = %e,
            "failed to read upstream response body"
        );
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("failed to read upstream response"))
            .expect("static response")
    })
}

/// One buffered fold for both wire shapes (C6). Parse and translate branch
/// on the shape decided once in translation; the read, resolve-and-book
/// envelope, restore, and response envelope are shared. Log strings stay
/// per-shape so log bytes are unchanged by the merge.
pub(crate) async fn fold_buffered(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
    is_responses: bool,
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let body_text = match read_routed_body(upstream_resp, upstream_status, outcome.as_ref()).await {
        Ok(text) => text,
        Err(response) => return response,
    };

    let (parsed_body, output_tokens) = if is_responses {
        // A gateway honoring stream:false answers buffered JSON, which the
        // SSE fold below would flatten to an empty turn. Prefer a body that
        // already is a turn.
        match serde_json::from_str::<Value>(&body_text) {
            Ok(v) if v.get("output").and_then(|o| o.as_array()).is_some() => (v, 0),
            _ => {
                let (turn, tokens) = responses_stream_to_turn(&body_text);
                let empty = turn
                    .get("output")
                    .and_then(|o| o.as_array())
                    // `is_none_or` needs Rust 1.82; MSRV is 1.80.
                    .map_or(true, |o| o.is_empty());
                if empty
                    && crate::proxy::continuation_stream_terminal(
                        body_text.as_bytes(),
                        "openai_responses",
                    )
                    .is_none()
                {
                    // Cut stream (reasoning deltas then EOF, no terminal):
                    // serving it would be a quiet empty turn with end_turn.
                    // Fail loudly like the chat arm's parse failure below so
                    // the client retries the turn instead of stopping on one.
                    tracing::warn!(
                        event = "routed_responses_empty_fold_no_terminal",
                        body_len = body_text.len(),
                        body_head = %body_text.chars().take(200).collect::<String>(),
                        "routed Responses body folded to zero output blocks with no terminal event"
                    );
                    return Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .body(Body::from(
                            "upstream response ended before producing content",
                        ))
                        .expect("static response");
                }
                (turn, tokens as i64)
            }
        }
    } else {
        match serde_json::from_str(&body_text) {
            Ok(v) => (v, 0),
            Err(e) => {
                tracing::warn!(
                    event = "local_model_response_parse_error",
                    error = %e,
                    "failed to parse OpenAI response JSON"
                );
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("failed to parse upstream response"))
                    .expect("static response");
            }
        }
    };

    let resolved = resolve_and_book(parsed_body, ccr, outcome.as_ref(), output_tokens).await;

    let mut anthropic_response = if is_responses {
        crate::sse::ccr_stream::responses_output_as_anthropic_turn(&resolved, original)
    } else {
        openai_to_anthropic_response(&resolved, original)
    };
    // Undo the Zen outbound rename (translation.rs): the model called the
    // lowercased names, the client must get its own back. The map derives
    // from the same tool list both directions, so this is a no-op unless
    // the outbound pass actually renamed something — unknown names pass
    // through either way.
    crate::routed::tool_alias::ToolAlias::derive(original.get("tools").and_then(|t| t.as_array()))
        .reverse_turn(&mut anthropic_response);

    let mut body_bytes = match serde_json::to_vec(&anthropic_response) {
        Ok(b) => b,
        Err(e) => {
            // Log text stays per-shape so log bytes are unchanged by the merge.
            if is_responses {
                tracing::warn!(
                    event = "local_model_serialize_error",
                    error = %e,
                    "failed to serialize Anthropic responses translation"
                );
            } else {
                tracing::warn!(
                    event = "local_model_serialize_error",
                    error = %e,
                    "failed to serialize Anthropic response"
                );
            }
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };
    body_bytes = restore_buffered(outcome.as_ref(), body_bytes);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .expect("static response")
}

// ---------------------------------------------------------------------------
// Streaming response translation: OpenAI SSE → Anthropic SSE
// ---------------------------------------------------------------------------

/// Completion booking for streamed CCR turns.
///
/// The translator records first-round usage but never books when deferral is
/// attached; the rewriter populates continuation rounds asynchronously. This
/// guard books exactly once — first-round usage plus rounds, through the same
/// funnel the buffered arms use — when the finished stream is exhausted, or
/// from `Drop` when the client disconnects first. The second of those is a
/// no-op, so a turn is never booked twice.
struct StreamedCcrBooking {
    outcome: Option<RoutedOutcomeContext>,
    deferred: DeferredCcrBooking,
    booked: bool,
}

impl StreamedCcrBooking {
    fn book(&mut self) {
        if self.booked {
            return;
        }
        self.booked = true;
        let Some(ctx) = self.outcome.take() else {
            return;
        };
        let first = self
            .deferred
            .first
            .lock()
            .ok()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let rounds = self
            .deferred
            .rounds
            .lock()
            .ok()
            .map(|guard| *guard)
            .unwrap_or_default();
        book_routed_outcome_with_ccr(
            &ctx,
            first.usage.as_ref(),
            first.output_tokens,
            first.ttfb_ms,
            first.status_code,
            rounds,
        );
    }
}

struct BookingStream {
    inner: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send>,
    >,
    guard: StreamedCcrBooking,
}

impl futures_util::Stream for BookingStream {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // `BookingStream` is `Unpin` (every field is), so full access is safe.
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(None) => {
                this.guard.book();
                std::task::Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for BookingStream {
    fn drop(&mut self) {
        self.guard.book();
    }
}

/// Wrap a finished routed stream so a CCR turn books exactly once, with
/// continuation rounds folded in. Non-CCR turns (either argument `None`)
/// pass through untouched — same bytes, same timing, no extra allocation
/// beyond the box the caller already holds.
fn with_ccr_booking(
    stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send>,
    >,
    deferred: &Option<DeferredCcrBooking>,
    outcome: &Option<RoutedOutcomeContext>,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send>>
{
    match (deferred.clone(), outcome.clone()) {
        (Some(d), Some(ctx)) => Box::pin(BookingStream {
            inner: stream,
            guard: StreamedCcrBooking {
                outcome: Some(ctx),
                deferred: d,
                booked: false,
            },
        }),
        _ => stream,
    }
}

pub(crate) async fn handle_streaming_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    codex_limits: crate::codex_rate_limits::CodexRateLimitStore,
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Quota headers ride on the response envelope and are gone once the body
    // is taken, so read them first. Nothing downstream depends on this.
    let quota_seen_in_headers =
        codex_limits.record_headers(&original_model, upstream_resp.headers());

    let stream = upstream_resp.bytes_stream();
    // Snapshot before `outcome` moves into the translator below.
    let request_id = outcome
        .as_ref()
        .map(|c| c.request_id.clone())
        .unwrap_or_default();
    let redact_table = redact_table_for(outcome.as_ref());
    // CCR turns defer booking to the completion guard below, which folds
    // continuation rounds in. The translator keeps a clone for observation,
    // replay, TTFB, and quota only — it must not book.
    let deferred = ccr
        .as_ref()
        .map(|_| crate::openai::stream::DeferredCcrBooking::new());
    let guard_outcome = deferred.as_ref().and(outcome.clone());
    let translated_stream = translate_openai_stream_to_anthropic(
        stream,
        original_model,
        codex_limits,
        quota_seen_in_headers,
        outcome,
        deferred.clone(),
    );

    // The translator has already put the turn into the Anthropic event
    // vocabulary, which is the one the client reads and the one the stream
    // rewriter speaks — so the same rewriter that serves the Claude path
    // serves this one. Only the continuation differs: it has to go back to
    // the routed upstream in its own shape.
    //
    // A routed upstream can die mid-response exactly like a direct one, and
    // the translator leaves the turn unstopped on purpose (`abort_terminal`
    // emits no `message_stop` and never closes a half-streamed tool call)
    // so the finisher below owns the close: partial tool calls are
    // discarded with a named marker instead of reaching the client
    // truncated, and the turn ends `end_turn` instead of a reset socket.
    // Without this a routed drop ends as a clean-looking turn (silent cut)
    // or a bare connection error (dead session).
    let inner: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send>,
    > = match ccr {
        Some(ccr) => {
            let anthropic_request = original.clone();
            let ctx = crate::sse::ccr_stream::CcrStreamContext {
                ccr_stores: ccr.stores.clone(),
                client: ccr.client,
                upstream_url: match url::Url::parse(&ccr.upstream_url) {
                    Ok(u) => u,
                    Err(e) => {
                        // Unparseable upstream means no continuation is
                        // possible. Stream on untouched rather than fail the
                        // turn; the client is no worse off than before. The
                        // finisher still owns the close: translator output
                        // aborts unstopped on every path, not just the CCR one.
                        tracing::warn!(
                            event = "routed_ccr_bad_upstream_url",
                            error = %e,
                            url = %ccr.upstream_url,
                            "cannot resolve headroom_retrieve on this turn"
                        );
                        return streaming_body_response(restore_streaming(
                            redact_table,
                            &request_id,
                            with_ccr_booking(
                                Box::pin(crate::sse::stream_finisher::finish_on_drop(
                                    translated_stream,
                                    request_id.clone(),
                                )),
                                &deferred,
                                &guard_outcome,
                            ),
                        ));
                    }
                },
                outgoing_headers: ccr.headers,
                forwarded_request: ccr.request_body,
                ccr_store: ccr.store,
                config: ccr.config,
                request_id: ccr.request_id,
                shape: if ccr.responses_shape {
                    crate::sse::ccr_stream::CcrShape::RoutedResponses { anthropic_request }
                } else {
                    crate::sse::ccr_stream::CcrShape::RoutedChat { anthropic_request }
                },
                // Memory tools ARE injected into routed requests — see the
                // `codex_memory_tools` site in `apply_ctx_request_transforms`.
                // This said otherwise and passed `None`, so the rewriter did
                // not own the block, `memory_search` streamed through to a
                // client that has never heard of it, and the turn died with
                // `No such tool available: memory_search`.
                //
                // Nothing types the agreement between the two sites, so it
                // rests on a gate relation: `memory_tool_context` asks only
                // that the handler exist and be initialized, while injection
                // asks that *and* that the tool array grew. The rewriter's
                // view is therefore a superset of what was injected, and the
                // only way the two can disagree is the harmless way — the
                // rewriter watches for a tool the model was never handed.
                // Narrowing this gate would reopen the bug above.
                memory: ccr.memory,
                // Continuations inherit the turn's redaction (see
                // `RoutedCcr::redact`).
                redact: ccr.redact,
                // Booking-owned handle: the rewriter populates it and the
                // completion guard books it together with first-round usage.
                rounds_sink: deferred.as_ref().map(|d| d.rounds.clone()),
            };
            let (rewritten, usage_handle) =
                crate::sse::ccr_stream::rewrite_anthropic_stream(translated_stream, ctx);
            debug_assert!(
                deferred
                    .as_ref()
                    .is_some_and(|d| std::sync::Arc::ptr_eq(&usage_handle, &d.rounds)),
                "rewriter must populate the booking-owned handle"
            );
            Box::pin(rewritten)
        }
        None => Box::pin(translated_stream),
    };
    let finished = crate::sse::stream_finisher::finish_on_drop(inner, request_id.clone());
    // CCR turns book once here, with rounds folded in; every other turn
    // passes through exactly as before.
    let finished = with_ccr_booking(Box::pin(finished), &deferred, &guard_outcome);
    let body = restore_streaming(redact_table, &request_id, finished);

    streaming_body_response(body)
}

/// The SSE response envelope every routed streaming reply uses.
pub(crate) fn streaming_body_response(body: axum::body::Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .expect("static response")
}

/// Forward a no-translation route's Anthropic body straight to its upstream
/// and hand the upstream's reply back, dropping the hop-by-hop headers the
/// client must not see.
///
/// Bytes stay verbatim — the C7 passthrough row promises no transforms — but
/// the turn books through the shared funnel (C7 flip B): the spend is real
/// and unbooked passthrough traffic is invisible to `/stats`. Booking reads
/// the response usage read-only and never touches the bytes.
pub(crate) async fn handle_passthrough(
    state: &crate::proxy::AppState,
    upstream: &url::Url,
    uri: &axum::http::Uri,
    headers: HeaderMap,
    body: bytes::Bytes,
    body_model: &str,
    request_id: &str,
) -> Response {
    let started_at = std::time::Instant::now();
    // No translation needed — forward Anthropic format directly to the upstream.
    let upstream_url = format!("{}{}", upstream.as_str().trim_end_matches('/'), uri.path());
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let full_url = format!("{upstream_url}{query}");

    tracing::info!(
        event = "model_route_passthrough",
        model = %body_model,
        upstream = %full_url,
        "routing to upstream without translation"
    );

    let resp = match state
        .client
        .post(&full_url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                event = "model_route_upstream_error",
                error = %e,
                upstream = %full_url,
                "failed to connect to upstream"
            );
            crate::routed::outcome::book_passthrough_outcome(
                &crate::proxy::ProxyOutcomeSink::from_state(state),
                request_id,
                body_model,
                StatusCode::BAD_GATEWAY.as_u16() as i64,
                None,
                started_at,
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("upstream error: {e}")))
                .expect("static response");
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = resp.headers().clone();
    let body_bytes = resp.bytes().await.unwrap_or_default();

    // Read-only: usage for booking only, bytes forwarded untouched.
    let parsed: Option<Value> = serde_json::from_slice(&body_bytes).ok();
    crate::routed::outcome::book_passthrough_outcome(
        &crate::proxy::ProxyOutcomeSink::from_state(state),
        request_id,
        body_model,
        status.as_u16() as i64,
        parsed.as_ref().and_then(|v| v.get("usage")),
        started_at,
    );

    let mut response = Response::builder().status(status);
    for (name, value) in resp_headers.iter() {
        if !crate::headers::is_response_drop(name) {
            response = response.header(name.clone(), value.clone());
        }
    }
    response
        .body(Body::from(body_bytes))
        .expect("static response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Process-global isolation for booking tests: a `None`-path tracker and
    /// the ledger resolve the developer's live files, and every booking test
    /// would append test turns to real lifetime totals. Same pattern as the
    /// translator tests.
    fn redirect_test_savings() {
        static LEDGER: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let path = LEDGER.get_or_init(|| {
            let dir = std::mem::ManuallyDrop::new(tempfile::tempdir().expect("tempdir"));
            dir.path().join("savings_events.jsonl")
        });
        std::env::set_var("HEADROOM_SAVINGS_EVENTS_PATH", path);
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let path = DIR.get_or_init(|| {
            let dir = std::mem::ManuallyDrop::new(tempfile::tempdir().expect("tempdir"));
            dir.path().join("proxy_savings.json")
        });
        std::env::set_var("HEADROOM_SAVINGS_PATH", path);
    }

    /// Translator wired to real trackers, so a test can assert on what the
    /// outcome funnel recorded rather than on the events it emitted.
    #[test]
    fn apply_target_model_override_rewrites_model() {
        let body = json!({"model": "claude-codex-5.5", "input": "hi"});
        let output = apply_target_model_override(body, Some("gpt-5.5"), false, false);
        assert_eq!(output["model"], "gpt-5.5");
        assert_eq!(output["input"], "hi");
    }

    #[test]
    fn apply_target_model_override_leaves_model_when_absent() {
        let body = json!({"model": "claude-codex-5.5", "input": "hi"});
        let output = apply_target_model_override(body, None, false, false);
        assert_eq!(output["model"], "claude-codex-5.5");
        assert_eq!(output["input"], "hi");
    }

    #[test]
    fn apply_target_model_override_forces_store_false_when_requested() {
        let body = json!({"model": "claude-codex-5.5", "input": "hi", "store": true});
        let output = apply_target_model_override(body, Some("gpt-5.5"), true, false);
        assert_eq!(output["model"], "gpt-5.5");
        assert_eq!(output["store"], false);
    }

    #[test]
    fn apply_target_model_override_forces_stream_true_when_requested() {
        let body = json!({"model": "claude-codex-5.5", "input": "hi", "stream": false});
        let output = apply_target_model_override(body, Some("gpt-5.5"), false, true);
        assert_eq!(output["stream"], true);
    }

    /// The completion guard books first-round usage plus continuation rounds
    /// through one funnel call, exactly once — at exhaustion and at drop.
    /// First round reports 100 in / 20 out; one continuation round spent
    /// 1000 in / 50 out. The booked turn must carry 1100 in / 70 out: the
    /// spend the old code dropped on the floor.
    #[tokio::test]
    async fn ccr_completion_guard_books_rounds_exactly_once() {
        use futures_util::StreamExt as _;

        redirect_test_savings();
        let state = crate::test_support::test_state(|_| {});
        let logger = state.request_logger.clone();
        let parsed = json!({
            "model": "routed-test-model",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let ctx = crate::routed::outcome::build_routed_outcome_context(
            &state,
            &parsed,
            &axum::http::HeaderMap::new(),
            Some("gpt-5.5"),
            true,
            "routed-test-model",
            crate::routed::transforms::CtxTransformReport::default(),
            0.0,
            std::time::Instant::now(),
            "req-guard-test".to_string(),
            None,
            7,
            0,
        )
        .expect("context builds");

        let deferred = crate::openai::stream::DeferredCcrBooking::new();
        *deferred.first.lock().unwrap() = crate::openai::stream::DeferredFirstRound {
            usage: Some(json!({"input_tokens": 100, "output_tokens": 20})),
            output_tokens: 7,
            ttfb_ms: 1.0,
            status_code: 200,
        };
        *deferred.rounds.lock().unwrap() = crate::proxy::CcrRoundUsage {
            rounds: 1,
            input_tokens: 1000,
            output_tokens: 50,
            ..Default::default()
        };

        let inner = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from(
            "data: done\n\n",
        ))]);
        let stream = BookingStream {
            inner: Box::pin(inner),
            guard: StreamedCcrBooking {
                outcome: Some(ctx),
                deferred,
                booked: false,
            },
        };
        let collected: Vec<_> = stream.collect().await;
        assert_eq!(collected.len(), 1, "bytes still flow through the guard");
        drop(collected);

        let entries = logger.get_recent(10);
        assert_eq!(entries.len(), 1, "one turn books once: {entries:?}");
        assert_eq!(entries[0].input_tokens_optimized, 1100);
        assert_eq!(entries[0].output_tokens, 70);
    }

    /// A cut Responses stream (reasoning deltas, EOF, no terminal — the
    /// 2026-09-17 shape) must fail loudly, not serve a quiet empty turn.
    #[tokio::test]
    async fn fold_buffered_cut_responses_stream_is_bad_gateway() {
        let sse = "event: response.created\n\
                   data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"status\":\"in_progress\"}}\n\
                   \n\
                   event: response.reasoning_summary_text.delta\n\
                   data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"delta\":\"thinking\"}\n\
                   \n";
        let http_resp = axum::http::Response::builder()
            .status(200)
            .body(reqwest::Body::from(sse.to_string()))
            .unwrap();
        let out = fold_buffered(
            reqwest::Response::from(http_resp),
            &json!({"model": "m", "input": []}),
            StatusCode::OK,
            true,
            None,
            None,
        )
        .await;
        assert_eq!(out.status(), StatusCode::BAD_GATEWAY);
    }

    /// A buffered JSON Responses body is already a turn: it must pass
    /// through, not flatten to empty in the SSE fold.
    #[tokio::test]
    async fn fold_buffered_json_responses_body_passes_through() {
        let body = json!({
            "id": "resp_1",
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "hi"}]}],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        });
        let http_resp = axum::http::Response::builder()
            .status(200)
            .body(reqwest::Body::from(body.to_string()))
            .unwrap();
        let out = fold_buffered(
            reqwest::Response::from(http_resp),
            &json!({"model": "m", "input": []}),
            StatusCode::OK,
            true,
            None,
            None,
        )
        .await;
        assert_eq!(out.status(), StatusCode::OK);
    }
}

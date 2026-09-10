//! Request shaping and response arms for the routed paths.

use crate::openai::response::{openai_to_anthropic_response, responses_stream_to_turn};
use crate::openai::stream::translate_openai_stream_to_anthropic;
use crate::routed::ccr::{resolve_routed_proxy_tools, RoutedCcr};
use crate::routed::outcome::{
    book_routed_outcome, book_routed_outcome_with_ccr, RoutedOutcomeContext,
};
use crate::routed::redaction::{redact_table_for, restore_buffered, restore_streaming};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

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
    tracing::warn!(
        event = "local_model_upstream_error",
        status = upstream_status.as_u16(),
        body = %body_text,
        retry_after_preserved = retry_after.is_some(),
        "local model upstream returned error"
    );
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
        tracing::warn!(
            event = "local_model_upstream_error",
            status = status.as_u16(),
            body = %body_text,
            "local model upstream returned error"
        );
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

pub(crate) async fn handle_buffered_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let openai_text = match read_routed_body(upstream_resp, upstream_status, outcome.as_ref()).await
    {
        Ok(text) => text,
        Err(response) => return response,
    };
    let openai_body: Value = match serde_json::from_str(&openai_text) {
        Ok(v) => v,
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
    };

    // Resolve any `headroom_retrieve` the model asked for, before the outcome
    // is booked: the continuation rounds are billed too, and booking the first
    // round's usage as the turn's would under-report the retrieval.
    let (openai_body, ccr_rounds) = match ccr {
        Some(ccr) => resolve_routed_proxy_tools(&openai_body, &ccr).await,
        None => (openai_body, crate::proxy::CcrRoundUsage::default()),
    };

    if let Some(ctx) = outcome.as_ref() {
        book_routed_outcome_with_ccr(ctx, openai_body.get("usage"), 0, 0.0, 200, ccr_rounds);
    }

    let anthropic_response = openai_to_anthropic_response(&openai_body, original);

    let mut body_bytes = match serde_json::to_vec(&anthropic_response) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize Anthropic response"
            );
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

pub(crate) async fn handle_buffered_responses_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let responses_text =
        match read_routed_body(upstream_resp, upstream_status, outcome.as_ref()).await {
            Ok(text) => text,
            Err(response) => return response,
        };

    let (responses_turn, output_tokens) = responses_stream_to_turn(&responses_text);

    // Resolve before the outcome is booked: continuation rounds are billed
    // too, and booking the first round's usage as the turn's would under-report
    // the retrieval. Same ordering as the chat arm.
    let (resolved, ccr_rounds) = match ccr {
        Some(ccr) => resolve_routed_proxy_tools(&responses_turn, &ccr).await,
        None => (responses_turn, crate::proxy::CcrRoundUsage::default()),
    };

    if let Some(ctx) = outcome.as_ref() {
        book_routed_outcome_with_ccr(
            ctx,
            resolved.get("usage"),
            output_tokens as i64,
            0.0,
            200,
            ccr_rounds,
        );
    }

    let anthropic_response =
        crate::sse::ccr_stream::responses_output_as_anthropic_turn(&resolved, original);

    let mut body_bytes = match serde_json::to_vec(&anthropic_response) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize Anthropic responses translation"
            );
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
    let translated_stream = translate_openai_stream_to_anthropic(
        stream,
        original_model,
        codex_limits,
        quota_seen_in_headers,
        outcome,
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
                            crate::sse::stream_finisher::finish_on_drop(
                                translated_stream,
                                request_id.clone(),
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
            };
            let (rewritten, _usage) =
                crate::sse::ccr_stream::rewrite_anthropic_stream(translated_stream, ctx);
            Box::pin(rewritten)
        }
        None => Box::pin(translated_stream),
    };
    let finished = crate::sse::stream_finisher::finish_on_drop(inner, request_id.clone());
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
pub(crate) async fn handle_passthrough(
    client: &reqwest::Client,
    upstream: &url::Url,
    uri: &axum::http::Uri,
    headers: HeaderMap,
    body: bytes::Bytes,
    body_model: &str,
) -> Response {
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

    let resp = match client
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
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("upstream error: {e}")))
                .expect("static response");
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = resp.headers().clone();
    let body_bytes = resp.bytes().await.unwrap_or_default();

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
}

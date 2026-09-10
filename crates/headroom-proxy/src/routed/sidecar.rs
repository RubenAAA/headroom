//! Spinner-sidecar offload onto a routed Responses upstream.
//!
//! One bounded attempt per sidecar; any failure falls back to the direct
//! path, leaving no per-conversation state either way.

use crate::openai::request::anthropic_to_openai_responses_request;
use crate::openai::response::responses_stream_to_turn;
use crate::openai::stream::translate_openai_stream_to_anthropic;
use crate::proxy::AppState;
use crate::routed::auth::{inject_opencode_headers, upstream_auth_headers};
use crate::routed::response_arms::{apply_target_model_override, streaming_body_response};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};

/// A model route the spinner sidecar may take off-Claude.
///
/// Only Responses-shaped routes qualify: `translate` plus a target model id
/// select the `/v1/responses` endpoint the translator speaks. A passthrough
/// route (Anthropic in, Anthropic out, no model rewrite) or a `cursor:`
/// subprocess route cannot serve a translated sidecar; the sidecar keeps its
/// direct path for those, exactly as before.
pub(crate) fn sidecar_responses_route<'a>(
    routes: &'a [crate::config::ModelRoute],
    sidecar_model: &str,
) -> Option<&'a crate::config::ModelRoute> {
    routes.iter().find(|r| {
        r.matches(sidecar_model)
            && r.translate
            && r.target_model.is_some()
            && r.cursor_agent.is_none()
            && r.upstream.is_some()
    })
}

/// Output budget for a routed sidecar, overriding the client's 64-token cap.
///
/// A reasoning model spends its budget on thinking first: measured 2026-09-06
/// at 240-1000 reasoning tokens for a four-word summary (effort- and
/// model-dependent), so forwarding the 64 cap starves the text every time.
/// 512 leaves room for ~300 reasoning tokens plus the summary at ~100 tok/s,
/// about 5s on Zen's free pool. Billed cost is zero on the free tier, so the
/// cap is purely a latency bound; anything unused is simply not generated.
pub(crate) const SIDECAR_ROUTED_MAX_TOKENS: u64 = 512;

/// Force the reasoning effort and output budget a routed sidecar is served at.
///
/// The shrunk sidecar carries no thinking or effort fields, so the translator
/// leaves `reasoning` unset and the backend falls back to its default
/// (`high` on Zen: 1001 reasoning tokens measured for four words). `minimal`
/// is the floor — killing reasoning entirely is a 400 on every surface, per
/// Meta's docs — and `muse-spark-1.2` at `minimal` completes inside the
/// budget above in ~5s, verified live.
pub(crate) fn shape_sidecar_request(openai_body: &mut Value) {
    if let Some(obj) = openai_body.as_object_mut() {
        obj.insert("reasoning".to_string(), json!({"effort": "minimal"}));
        obj.insert(
            "max_output_tokens".to_string(),
            json!(SIDECAR_ROUTED_MAX_TOKENS),
        );
    }
}

/// True when a translated sidecar reply carries usable text.
///
/// A 200 whose output budget went to reasoning is a dead spinner; treating it
/// as a failure sends the sidecar down the direct path, which answers it.
pub(crate) fn sidecar_text_present(anthropic: &Value) -> bool {
    anthropic
        .get("content")
        .and_then(|c| c.as_array())
        .is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("text")
                    && b.get("text")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| !t.trim().is_empty())
            })
        })
}

/// Try the spinner sidecar on a routed Responses upstream, failing fast.
///
/// `Some` means the route answered with usable text. `None` means carry on:
/// either the sidecar model names no Responses route (the direct path,
/// today's behavior) or the routed attempt failed — a non-OK status, a
/// transport error or timeout, an unparseable body, or empty text — and the
/// direct path should answer instead.
///
/// One attempt, bounded by `sidecar_route_timeout`, no retry: spending a
/// retry budget here would hold the status line through exactly the window
/// the Haiku fallback needs. Deliberately lean — no CTX transforms, no
/// compression, no replay parking, no outcome booking — so a routed sidecar
/// leaves no per-conversation state, the invariant the direct path upholds.
/// The only trace is the `sidecar_detected` line with `routed: true`.
pub(crate) async fn try_routed_sidecar(
    state: &AppState,
    headers: &HeaderMap,
    parsed: &Value,
    request_id: &str,
) -> Option<Response> {
    let sidecar_model = state
        .config
        .sidecar_model
        .clone()
        .unwrap_or_else(|| crate::sidecar::DEFAULT_SIDECAR_MODEL.to_string());
    // A routed sidecar would carry the client's raw text to a routed upstream
    // past the redaction stage, which runs later. Skip it while redaction is
    // on; the direct sidecar path answers instead, on the default upstream.
    if state.config.redact_sensitive {
        tracing::info!(
            event = "sidecar_routed_skipped_redacted",
            request_id = %request_id,
            "routed sidecar disabled while redaction is on"
        );
        return None;
    }
    let route = sidecar_responses_route(&state.config.model_routes, &sidecar_model)?;
    let target = route.target_model.clone()?;
    let upstream = route.upstream.clone()?;

    // The same shrink the direct path sends: tail messages, no tools,
    // one-line system, 64 output tokens.
    let shrunk = crate::sidecar::rewrite_sidecar(parsed, &sidecar_model);
    let downstream_stream = shrunk
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // `false`: the client's 64-token cap is not forwarded — a reasoning
    // model spends the budget on thinking first (see
    // [`SIDECAR_ROUTED_MAX_TOKENS`]). The routed budget is set explicitly
    // below.
    let mut openai_body = anthropic_to_openai_responses_request(&shrunk, false).ok()?;
    openai_body = apply_target_model_override(openai_body, Some(&target), true, true);
    shape_sidecar_request(&mut openai_body);
    let openai_bytes = serde_json::to_vec(&openai_body).ok()?;

    let (mut upstream_headers, _) = upstream_auth_headers(
        route.auth_env.as_deref(),
        headers,
        state.config.codex_auth_file.as_deref(),
    )
    .ok()?;
    if upstream.host_str() == Some("opencode.ai") {
        // `session_key` is not material here — the sidecar is a stateless
        // one-shot, so the request_id-derived session is sufficient.
        inject_opencode_headers(&mut upstream_headers, request_id, None);
    }

    let base = upstream.as_str().trim_end_matches('/');
    let upstream_url = format!("{}/v1/responses", base.trim_end_matches("/v1"));

    tracing::info!(
        event = "sidecar_routed_attempt",
        request_id = %request_id,
        model = %sidecar_model,
        target = %target,
        upstream = %upstream_url,
        "trying the spinner sidecar on a routed Responses upstream"
    );

    let upstream_resp = state
        .client
        .post(&upstream_url)
        .headers(upstream_headers)
        .body(openai_bytes)
        .timeout(state.config.sidecar_route_timeout)
        .send()
        .await
        .ok()?;
    if upstream_resp.status() != StatusCode::OK {
        tracing::warn!(
            event = "sidecar_routed_fallback",
            request_id = %request_id,
            status = upstream_resp.status().as_u16(),
            "routed sidecar failed; falling back to the direct path"
        );
        return None;
    }

    let shape = crate::sidecar::SidecarShape {
        original_messages: parsed
            .get("messages")
            .and_then(|m| m.as_array())
            .map_or(0, |m| m.len()),
        forwarded_messages: shrunk
            .get("messages")
            .and_then(|m| m.as_array())
            .map_or(0, |m| m.len()),
        model_from: parsed
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string(),
        model_to: sidecar_model.clone(),
        routed: true,
    };

    if downstream_stream {
        let stream = upstream_resp.bytes_stream();
        // A fresh quota store, not the shared one: Zen's rate-limit headers
        // must never pollute Codex quota tracking.
        let translated = translate_openai_stream_to_anthropic(
            stream,
            sidecar_model,
            crate::codex_rate_limits::CodexRateLimitStore::new(),
            false,
            None,
        );
        // Same close-on-drop as the main routed path: a mid-response death
        // ends `end_turn` with a marker instead of a reset socket.
        let finished =
            crate::sse::stream_finisher::finish_on_drop(translated, request_id.to_string());
        crate::sidecar::record_sidecar(request_id, &shape);
        return Some(streaming_body_response(axum::body::Body::from_stream(
            finished,
        )));
    }

    let text = upstream_resp.text().await.ok()?;
    let (turn, _) = responses_stream_to_turn(&text);
    let anthropic_response =
        crate::sse::ccr_stream::responses_output_as_anthropic_turn(&turn, &shrunk);
    if !sidecar_text_present(&anthropic_response) {
        tracing::warn!(
            event = "sidecar_routed_fallback",
            request_id = %request_id,
            reason = "empty_text",
            "routed sidecar returned no text; falling back to the direct path"
        );
        return None;
    }
    crate::sidecar::record_sidecar(request_id, &shape);

    let body_bytes = serde_json::to_vec(&anthropic_response).ok()?;
    Some(
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(body_bytes))
            .expect("static response"),
    )
}

/// Answer Claude Code's spinner-text sidecar without touching route matching,
/// the replay store, or the cache tracker: the whole point is that it leaves
/// no per-conversation state for the next real turn to be measured against.
/// See `crate::sidecar`.
///
/// Runs ahead of the route table on purpose. A sidecar is a throwaway
/// four-word summary; whichever model the client named, it is answered on
/// the sidecar model against the default upstream with the client's own
/// credentials — unless the sidecar model names a Responses route, in
/// which case one bounded attempt goes there first (free-tier offload)
/// and any failure lands back here.
///
/// `None` means either that this was not a sidecar or that the shrunk
/// request failed. Both want the same thing from the caller: fall through
/// with `parsed` exactly as the client sent it. Neither the predicate
/// nor the rewrite mutates it, so the normal path cannot tell this ran.
pub(crate) async fn handle_sidecar(
    state: &AppState,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    parsed: &Value,
    request_id: &str,
) -> Option<Response> {
    if !crate::sidecar::is_describe_action_sidecar(parsed) {
        return None;
    }
    if let Some(resp) = try_routed_sidecar(state, headers, parsed, request_id).await {
        return Some(resp);
    }
    let base = state.effective_upstream().await;
    if let Ok(url) = crate::proxy::build_upstream_url(&base, uri) {
        let sidecar_model = state
            .config
            .sidecar_model
            .clone()
            .unwrap_or_else(|| crate::sidecar::DEFAULT_SIDECAR_MODEL.to_string());
        if let Some(resp) = crate::sidecar::try_handle(
            &state.client,
            &url,
            request_id,
            headers,
            parsed,
            &sidecar_model,
            crate::sidecar::SidecarRetry::from_config(&state.config),
        )
        .await
        {
            return Some(resp);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Only Responses-shaped routes can serve a translated sidecar: the
    /// shuttle speaks `/v1/responses` and rewrites the model id, so it needs
    /// `translate`, a target, and an HTTP upstream.
    #[test]
    fn sidecar_route_qualifies_only_responses_routes() {
        fn route(
            prefix: &str,
            translate: bool,
            target: Option<&str>,
            cursor: Option<&str>,
            upstream: Option<&str>,
        ) -> crate::config::ModelRoute {
            crate::config::ModelRoute {
                model_prefix: prefix.to_string(),
                prefix_match: false,
                upstream: upstream.map(|u| url::Url::parse(u).expect("valid url")),
                translate,
                cursor_agent: cursor.map(str::to_string),
                target_model: target.map(str::to_string),
                auth_env: None,
            }
        }
        let zen = route(
            "claude-muse-spark-1.3",
            true,
            Some("muse-spark-1.3-contributor-free"),
            None,
            Some("https://opencode.ai/zen/v1"),
        );
        assert!(
            sidecar_responses_route(std::slice::from_ref(&zen), "claude-muse-spark-1.3").is_some()
        );

        // Passthrough: Anthropic in, Anthropic out, no model rewrite.
        let passthrough = route(
            "claude-passthrough",
            false,
            None,
            None,
            Some("https://api.meta.ai"),
        );
        assert!(
            sidecar_responses_route(std::slice::from_ref(&passthrough), "claude-passthrough")
                .is_none()
        );

        // A `cursor:` route is a subprocess transport with no HTTP upstream.
        let cursor = route(
            "claude-grok-4.6",
            false,
            None,
            Some("cursor-grok-4.6-high"),
            None,
        );
        assert!(
            sidecar_responses_route(std::slice::from_ref(&cursor), "claude-grok-4.6").is_none()
        );

        // Translate without a target selects chat-completions, not Responses.
        let chat = route(
            "codex-5.5",
            true,
            None,
            None,
            Some("https://api.openai.com/v1"),
        );
        assert!(sidecar_responses_route(std::slice::from_ref(&chat), "codex-5.5").is_none());

        // A name that matches nothing.
        assert!(sidecar_responses_route(std::slice::from_ref(&zen), "claude-opus-5").is_none());
    }

    /// The forced effort and budget land on the translated body without
    /// touching the rest.
    #[test]
    fn sidecar_request_shaping() {
        let mut body = json!({"model": "m", "input": [], "max_output_tokens": 64});
        shape_sidecar_request(&mut body);
        assert_eq!(body["reasoning"], json!({"effort": "minimal"}));
        assert_eq!(body["max_output_tokens"], 512);
        assert_eq!(body["model"], "m");
        assert_eq!(body["input"], json!([]));
    }

    /// Empty, whitespace-only, thinking-only and missing content all read as
    /// "no text" and send the sidecar down the direct path.
    #[test]
    fn sidecar_empty_text_detection() {
        assert!(sidecar_text_present(
            &json!({"content": [{"type": "text", "text": "Reading foo.rs"}]})
        ));
        assert!(!sidecar_text_present(
            &json!({"content": [{"type": "text", "text": "   "}]})
        ));
        assert!(!sidecar_text_present(
            &json!({"content": [{"type": "thinking", "thinking": "hmm"}]})
        ));
        assert!(!sidecar_text_present(&json!({"content": []})));
        assert!(!sidecar_text_present(&json!({})));
    }
}

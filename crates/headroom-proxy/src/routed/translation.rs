//! Anthropic → OpenAI translation for a prepared routed turn.
//!
//! Runs on the prepared body: shape translation, target-model override,
//! the OpenAI-side `prompt_cache_key`, stream-flag split, and the upstream
//! URL. The one cache-stabilization stage whose natural home is the
//! post-translation body.

use crate::openai::request::{anthropic_to_openai_request, anthropic_to_openai_responses_request};
use crate::routed::response_arms::apply_target_model_override;
use crate::routed::transforms::apply_bytes_stage;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

/// A prepared turn translated and addressed: ready to serialize and send.
pub(crate) struct TranslatedRequest {
    pub openai_body: Value,
    pub upstream_url: String,
    pub downstream_is_stream: bool,
}

/// Translate the prepared body and resolve where to send it. `Err` is the
/// response to return directly when translation itself fails.
///
/// The `Response` error keeps the convention of every sibling arm on this
/// path (cf. `auth.rs`); boxing it would save nothing measurable and diverge
/// from all of them.
#[allow(clippy::result_large_err)]
pub(crate) fn translate_routed_request(
    parsed: &Value,
    headers: &HeaderMap,
    target_model: Option<&str>,
    upstream: &url::Url,
    is_chatgpt_auth: bool,
    body_model: &str,
    request_id: &str,
) -> Result<TranslatedRequest, Response> {
    // Translation path: Anthropic → OpenAI.
    let openai_body = match if target_model.is_some() {
        anthropic_to_openai_responses_request(parsed, false)
    } else {
        anthropic_to_openai_request(parsed, true, true)
    } {
        Ok(v) => apply_target_model_override(
            v,
            target_model,
            target_model.is_some(),
            target_model.is_some(),
        ),
        Err(e) => {
            tracing::warn!(
                event = "local_model_translate_error",
                error = %e,
                "failed to translate Anthropic request to OpenAI format"
            );
            return Err(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("translation error"))
                .expect("static response"));
        }
    };

    // PR-E4: OpenAI `prompt_cache_key`. Injected *after* translation, because
    // the field belongs to the OpenAI request shape — before translation there
    // is nowhere valid to put it, and Anthropic has no equivalent. This is the
    // one cache-stabilization stage whose natural home on this path is the
    // post-translation body.
    //
    // Same gating as the Claude path's OpenAI arm: PAYG only, and it self-skips
    // when the caller already set a key. A ChatGPT-subscription codex route
    // classifies as subscription, not PAYG, so this is a no-op there by
    // design — those clients are fingerprinted upstream and a synthesised key
    // works against them.
    let mut openai_body = openai_body;
    apply_bytes_stage(&mut openai_body, |body| {
        crate::proxy::maybe_inject_openai_prompt_cache_key(
            body,
            if target_model.is_some() {
                crate::cache_stabilization::openai_cache_key::OpenAiShape::Responses
            } else {
                crate::cache_stabilization::openai_cache_key::OpenAiShape::ChatCompletions
            },
            headroom_core::auth_mode::classify(headers),
            request_id,
            if target_model.is_some() {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            },
        )
    });

    let is_stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let upstream_is_stream = is_stream || target_model.is_some();
    let downstream_is_stream = is_stream;

    // Upstream may be configured either as the API root (e.g.
    // `https://api.openai.com`) or already including `/v1` (e.g.
    // `https://api.openai.com/v1`, per the --extra-model-route example in
    // config.rs) — strip a trailing `/v1` so we don't double it up into
    // `.../v1/v1/chat/completions`, which OpenAI 404s on.
    let upstream_base = upstream.as_str().trim_end_matches('/');
    let upstream_url = if target_model.is_some() {
        if is_chatgpt_auth && upstream.host_str() == Some("api.openai.com") {
            "https://chatgpt.com/backend-api/codex/responses".to_string()
        } else {
            let base = upstream_base.trim_end_matches("/v1");
            format!("{base}/v1/responses")
        }
    } else {
        let base = upstream_base.trim_end_matches("/v1");
        format!("{base}/v1/chat/completions")
    };

    tracing::info!(
        event = "model_route_translate",
        request_id = %request_id,
        model = %body_model,
        upstream = %upstream_url,
        stream = upstream_is_stream,
        "routing to upstream with format translation"
    );

    Ok(TranslatedRequest {
        openai_body,
        upstream_url,
        downstream_is_stream,
    })
}

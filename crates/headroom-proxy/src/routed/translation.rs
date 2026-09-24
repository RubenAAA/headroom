//! Anthropic → OpenAI translation for a prepared routed turn.
//!
//! Runs on the prepared body: shape translation, target-model override,
//! the OpenAI-side `prompt_cache_key`, stream-flag split, and the upstream
//! URL. The one cache-stabilization stage whose natural home is the
//! post-translation body.

use crate::openai::request::{
    anthropic_to_openai_request, anthropic_to_openai_responses_request, shape_for, RouteShape,
};
use crate::routed::quirks::classify_upstream;
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
    /// The wire shape, decided once by [`shape_for`]. Downstream consumers
    /// (buffered-arm pick, CCR shape flag, outcome provider label) match on
    /// this instead of re-deriving it from `target_model`.
    pub is_responses: bool,
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
    // Translation path: Anthropic → OpenAI. The shape is decided once here;
    // everything below matches on it.
    let shape = shape_for(target_model);
    let is_responses = shape == RouteShape::Responses;
    let openai_body = translate_shaped_body(
        parsed,
        shape,
        target_model,
        upstream,
        is_chatgpt_auth,
        is_responses,
        request_id,
    )?;

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
            if is_responses {
                crate::cache_stabilization::openai_cache_key::OpenAiShape::Responses
            } else {
                crate::cache_stabilization::openai_cache_key::OpenAiShape::ChatCompletions
            },
            headroom_core::auth_mode::classify(headers),
            request_id,
            if is_responses {
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
    let upstream_is_stream = is_stream || is_responses;
    let downstream_is_stream = is_stream;

    // Endpoint selection is the provider quirk (P6): ChatGPT-subscription
    // Codex traffic speaks the Codex endpoint, everything else appends one
    // `/v1/...` path onto the stripped base. The sniff lives in
    // `routed::quirks`; this keeps only the per-occurrence misroute alarm.
    let upstream_url =
        resolve_upstream_url(shape, upstream, is_chatgpt_auth, body_model, request_id);

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
        is_responses,
    })
}

/// Translate the prepared body into the route's OpenAI shape, including the
/// target-model override, reasoning-strip, and Zen tool-alias passes.
/// `Err` is the response to return directly when translation itself fails.
///
/// The `Response` error keeps the convention of every sibling arm on this
/// path (cf. `auth.rs`); boxing it would save nothing measurable and diverge
/// from all of them.
///
/// Extracted from `translate_routed_request`, then extended: the Zen
/// Responses arm below clamps the output budget, moves `instructions` into a
/// `developer` message, injects `prompt_cache_key` and strips
/// `parallel_tool_calls`. Those are new behaviour, not part of the move.
#[allow(clippy::result_large_err)]
fn translate_shaped_body(
    parsed: &Value,
    shape: RouteShape,
    target_model: Option<&str>,
    upstream: &url::Url,
    is_chatgpt_auth: bool,
    is_responses: bool,
    request_id: &str,
) -> Result<Value, Response> {
    let kind = classify_upstream(upstream, is_chatgpt_auth);
    let translated = match shape {
        RouteShape::Responses => {
            // No Responses route forwards the client's output budget. On Zen
            // it was actively harmful: see [`lift_zen_output_ceiling`].
            anthropic_to_openai_responses_request(parsed, false)
        }
        RouteShape::Chat => anthropic_to_openai_request(parsed, true, true),
    };
    match translated {
        Ok(v) => {
            let mut v = apply_target_model_override(v, target_model, is_responses, is_responses);
            if kind == crate::routed::quirks::UpstreamKind::OpenCodeZen && is_responses {
                lift_zen_output_ceiling(&mut v, parsed, request_id);
                move_zen_instructions_to_developer(&mut v, request_id);
                // Match OpenCode's Responses request defaults. Its AI SDK
                // sends the real OpenCode session as the cache key and leaves
                // parallel tool calls at the provider default; forcing false
                // here serializes tool work and makes this path slower than
                // the native client.
                v["prompt_cache_key"] =
                    serde_json::json!(crate::routed::quirks::resolve_zen_session(request_id));
                if let Some(obj) = v.as_object_mut() {
                    obj.remove("parallel_tool_calls");
                }
            }
            kind.strip_unreplayable_reasoning(&mut v);
            // Zen's free-tier gate reads tool names: they must be
            // OpenCode-native lowercase (`read`, not `Read`). Rename the
            // translated body (definitions, history calls, forced choice)
            // and map the model's calls back before delivery — see
            // `routed::tool_alias`. Responses shape only: Chat routes
            // have no Zen free-tier configuration today.
            if kind == crate::routed::quirks::UpstreamKind::OpenCodeZen && is_responses {
                let alias = crate::routed::tool_alias::ToolAlias::derive(
                    parsed.get("tools").and_then(|t| t.as_array()),
                );
                let renamed = alias.forward_body(&mut v);
                // Tool-poor turns cannot clear the gate on renamed names
                // alone (there is nothing to rename, or extras like memory
                // tools inflate the count without adding known names): top
                // up the missing core names with marked shadow copies.
                // Already-present names are never duplicated.
                let shadowed = crate::routed::tool_alias::ensure_gate_tools(&mut v);
                if renamed > 0 || shadowed > 0 {
                    tracing::debug!(
                        event = "zen_tool_alias_applied",
                        request_id = %request_id,
                        renamed,
                        shadowed,
                        "lowercased tool names for the Zen gate; mapped back before delivery"
                    );
                }
            }
            Ok(v)
        }
        Err(e) => {
            tracing::warn!(
                event = "routed_translate_error",
                error = %e,
                "failed to translate Anthropic request to OpenAI format"
            );
            Err(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("translation error"))
                .expect("static response"))
        }
    }
}

/// Zen gets the whole budget and the top effort tier.
///
/// Two ceilings bounded a Zen turn and neither was deliberate. The client's
/// `max_tokens` was copied into `max_output_tokens`, which on the Responses
/// API covers reasoning *and* visible output together, so a reasoning model
/// could spend the lot thinking and return `output: []` with
/// `incomplete_details.reason: max_output_tokens`. Measured on Spark: 597 of
/// 600 tokens burned on reasoning, nothing emitted, the turn lost. The other
/// ceiling folded `xhigh` and `max` down to `high`, which is right for the
/// OpenAI Responses API and wrong for Zen.
///
/// So the output budget goes, and the effort the client asked for reaches the
/// backend at full strength. With no effort from the client, `xhigh` — Zen's
/// own default is `high`.
///
/// The spinner sidecar is untouched: it builds its request in
/// [`crate::routed::sidecar`], not here, and still pins `minimal` and a small
/// budget for the reason documented there.
fn lift_zen_output_ceiling(body: &mut Value, anthropic: &Value, request_id: &str) {
    // `max` is Claude Code vocabulary; Zen's top tier is `xhigh`, which it is
    // measured to accept. Anything else the client names goes through as sent.
    let effort = match crate::output_shaper::requested_effort(anthropic) {
        Some("max") | None => "xhigh",
        Some(other) => other,
    };
    body["reasoning"] = serde_json::json!({"effort": effort, "summary": "auto"});
    body["stream_options"] = serde_json::json!({"reasoning_summary_delivery": "sequential_cutoff"});

    tracing::debug!(
        event = "zen_output_ceiling_lifted",
        request_id = %request_id,
        effort = effort,
        "zen: no output ceiling, effort passed through"
    );
}

fn move_zen_instructions_to_developer(body: &mut Value, request_id: &str) {
    let Some(instructions) = body
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        tracing::warn!(
            event = "zen_instructions_drop",
            request_id = %request_id,
            "zen: translated body is not an object; dropping instructions"
        );
        return;
    };
    obj.remove("instructions");
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        tracing::warn!(
            event = "zen_instructions_drop",
            request_id = %request_id,
            "zen: Responses translation has no input array; dropping instructions"
        );
        return;
    };
    input.insert(
        0,
        serde_json::json!({
            "role": "developer",
            "content": instructions,
        }),
    );
}

/// Endpoint selection is the provider quirk (P6): ChatGPT-subscription
/// Codex traffic speaks the Codex endpoint, everything else appends one
/// `/v1/...` path onto the stripped base. The sniff lives in
/// `routed::quirks`; this keeps only the per-occurrence misroute alarm.
/// Extracted from `translate_routed_request` without behavior change.
fn resolve_upstream_url(
    shape: RouteShape,
    upstream: &url::Url,
    is_chatgpt_auth: bool,
    body_model: &str,
    request_id: &str,
) -> String {
    let kind = crate::routed::quirks::classify_upstream(upstream, is_chatgpt_auth);
    match shape {
        RouteShape::Responses => kind.responses_url(upstream),
        RouteShape::Chat => {
            if kind.warn_ambiguous_codex() {
                // Ambiguous Codex route: translate without a target on a
                // codex-bound upstream speaks chat-completions today (the
                // startup `warn_on_ambiguous_codex_routes` also fires). Loud
                // per occurrence so dashboards see a misroute, not just logs.
                tracing::warn!(
                    event = "model_route_ambiguous_codex",
                    request_id = %request_id,
                    model = %body_model,
                    "translate route on api.openai.com has no target model; serving chat-completions"
                );
            }
            kind.chat_url(upstream)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn anthropic_body(model: &str, stream: bool) -> Value {
        json!({
            "model": model,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": stream,
        })
    }

    /// Zen rotates its exit IP on every 429, which invalidates the reasoning
    /// blobs it issued earlier in the same conversation. The translated body
    /// must carry none of the caller-bound envelope (`id` + `encrypted_content`),
    /// must not ask for a new one, but keeps the visible summary text.
    #[test]
    fn zen_route_sends_no_encrypted_reasoning() {
        use crate::handlers::reasoning_signature::{encode_reasoning_signature, ReasoningReplay};

        let signature = encode_reasoning_signature(&ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "STALE".to_string(),
        })
        .expect("encodes");
        let parsed = json!({
            "model": "claude-muse-spark-1.3",
            "max_tokens": 16,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "...", "signature": signature},
                    {"type": "text", "text": "there"}
                ]},
                {"role": "user", "content": "continue"}
            ],
        });
        let reasoning_items = |out: &TranslatedRequest| {
            out.openai_body["input"]
                .as_array()
                .expect("input array")
                .iter()
                .filter(|i| i["type"] == json!("reasoning"))
                .count()
        };

        let zen: url::Url = "https://opencode.ai/zen/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            Some("muse-spark-1.3-contributor-free"),
            &zen,
            false,
            "claude-muse-spark-1.3",
            "req-zen",
        )
        .expect("translates");
        assert_eq!(reasoning_items(&out), 1);
        let reasoning = out.openai_body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == json!("reasoning"))
            .expect("reasoning item survives, stripped");
        assert!(reasoning.get("id").is_none());
        assert!(reasoning.get("encrypted_content").is_none());
        assert_eq!(
            reasoning["summary"],
            json!([{ "type": "summary_text", "text": "..." }])
        );
        assert!(out.openai_body.get("include").is_none());

        // Same conversation on a stable-identity upstream: replay intact.
        let openai: url::Url = "https://api.openai.com/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            Some("gpt-5.5"),
            &openai,
            false,
            "claude-codex-5.5",
            "req-openai",
        )
        .expect("translates");
        assert_eq!(reasoning_items(&out), 1);
        assert_eq!(
            out.openai_body["include"],
            json!(["reasoning.encrypted_content"])
        );
    }

    /// A Zen turn carries no output ceiling, and `xhigh` by default.
    ///
    /// The client's `max_tokens` used to become `max_output_tokens`, which on
    /// the Responses API is reasoning *plus* visible output — so a reasoning
    /// model could spend it all thinking and answer with nothing.
    #[test]
    fn zen_route_sends_no_output_ceiling_and_defaults_to_xhigh() {
        let parsed = json!({
            "model": "claude-muse-spark-1.3",
            "max_tokens": 600,
            "messages": [{"role": "user", "content": "hello"}],
        });
        let zen: url::Url = "https://opencode.ai/zen/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            Some("muse-spark-1.3-contributor-free"),
            &zen,
            false,
            "claude-muse-spark-1.3",
            "req-zen-budget",
        )
        .expect("translates");
        assert!(out.openai_body.get("max_output_tokens").is_none());
        assert_eq!(out.openai_body["reasoning"]["effort"], json!("xhigh"));
        assert!(out.openai_body["prompt_cache_key"]
            .as_str()
            .is_some_and(|key| key.starts_with("ses_")));
        assert!(out.openai_body.get("parallel_tool_calls").is_none());
    }

    /// The effort the client picked reaches Zen as picked. `max` is the one
    /// rewrite: it is Claude Code's word for the top tier, Zen's is `xhigh`.
    #[test]
    fn zen_route_passes_the_clients_effort_through() {
        let zen: url::Url = "https://opencode.ai/zen/v1".parse().unwrap();
        for (asked, sent) in [("low", "low"), ("high", "high"), ("max", "xhigh")] {
            let parsed = json!({
                "model": "claude-muse-spark-1.3",
                "max_tokens": 600,
                "output_config": {"effort": asked},
                "messages": [{"role": "user", "content": "hello"}],
            });
            let out = translate_routed_request(
                &parsed,
                &HeaderMap::new(),
                Some("muse-spark-1.3-contributor-free"),
                &zen,
                false,
                "claude-muse-spark-1.3",
                "req-zen-effort",
            )
            .expect("translates");
            assert_eq!(
                out.openai_body["reasoning"]["effort"],
                json!(sent),
                "client asked for {asked}"
            );
        }
    }

    /// Zen's free-tier gate reads tool names: the translated body carries
    /// OpenCode-native lowercase names upstream, while every other
    /// provider keeps the client's names verbatim.
    #[test]
    fn zen_route_lowercases_tool_names_only_there() {
        let parsed = json!({
            "model": "claude-muse-spark-1.3",
            "max_tokens": 16,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "c1", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "c1", "content": "ok"}
                ]},
            ],
            "tools": [
                {"name": "Read", "description": "read", "input_schema": {"type": "object"}},
                {"name": "Bash", "description": "run", "input_schema": {"type": "object"}},
            ],
        });
        let tool_names = |out: &TranslatedRequest| {
            out.openai_body["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .map(|t| t["name"].as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        };
        let history_names = |out: &TranslatedRequest| {
            out.openai_body["input"]
                .as_array()
                .expect("input array")
                .iter()
                .filter(|i| i["type"] == json!("function_call"))
                .map(|i| i["name"].as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        };

        let zen: url::Url = "https://opencode.ai/zen/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            Some("muse-spark-1.3-contributor-free"),
            &zen,
            false,
            "claude-muse-spark-1.3",
            "req-zen",
        )
        .expect("translates");
        // Client names lowered, plus shadow copies of the missing core
        // names (two client tools only — the gate needs the five code
        // tools, re-probed 2026-09-24).
        let names = tool_names(&out);
        assert_eq!(&names[..2], ["read", "bash"]);
        assert_eq!(names.len(), 5);
        assert_eq!(history_names(&out), vec!["read"]);

        // Same turn on a generic upstream: names verbatim.
        let openai: url::Url = "https://api.x.ai/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            Some("grok-4.6"),
            &openai,
            false,
            "claude-grok-4.6",
            "req-xai",
        )
        .expect("translates");
        assert_eq!(tool_names(&out), vec!["Read", "Bash"]);
        assert_eq!(history_names(&out), vec!["Read"]);
    }

    /// The Zen turn the client actually receives back: renamed calls are
    /// restored to client names, while a call to a shadow the client never
    /// declared passes through visibly — and the client rejects it. (That
    /// is the 2026-09-24 `create_goal` incident; the gate filler no longer
    /// emits goal shadows, but code-tool shadows on tool-poor turns keep
    /// this behavior.)
    #[test]
    fn zen_response_restores_names_but_leaves_undeclared_shadow_calls_visible() {
        use crate::routed::tool_alias::ToolAlias;
        let parsed = json!({
            "model": "claude-muse-spark-1.3",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "Read", "description": "read", "input_schema": {"type": "object"}},
                {"name": "Bash", "description": "run", "input_schema": {"type": "object"}},
            ],
        });
        let zen: url::Url = "https://opencode.ai/zen/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            Some("muse-spark-1.3-contributor-free"),
            &zen,
            false,
            "claude-muse-spark-1.3",
            "req-zen-shadows",
        )
        .expect("translates");
        // Five names went out (2 renamed + 3 shadows); the alias map derives
        // from the client's two.
        let alias = ToolAlias::derive(parsed.get("tools").and_then(|t| t.as_array()));
        assert!(alias.active());
        let names: Vec<String> = out.openai_body["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(names.len(), 5);

        // The model answers with upstream names, including a call to a
        // shadow the client never declared.
        let mut turn = serde_json::json!({
            "content": [
                {"type": "tool_use", "id": "c1", "name": "read", "input": {}},
                {"type": "tool_use", "id": "c2", "name": "grep", "input": {}},
                {"type": "text", "text": "done"},
            ],
        });
        let restored = alias.reverse_turn(&mut turn);
        // Only the real client tool is restored; the shadow call passes
        // through visibly so the client sees (and rejects) it instead of
        // it silently executing somewhere.
        assert_eq!(restored, 1);
        assert_eq!(turn["content"][0]["name"], json!("Read"));
        assert_eq!(turn["content"][1]["name"], json!("grep"));
        assert_eq!(turn["content"][2]["type"], json!("text"));
    }

    /// Retroactive lock: Responses shape forces upstream streaming while
    /// downstream follows the client. A non-stream client on a target route
    /// still sends `stream:true` upstream but answers buffered.
    #[test]
    fn responses_target_forces_upstream_stream_only() {
        let parsed = anthropic_body("claude-codex-5.5", false);
        let upstream: url::Url = "https://api.openai.com/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            Some("gpt-5.5"),
            &upstream,
            false,
            "claude-codex-5.5",
            "req-test",
        )
        .expect("translates");
        assert!(!out.downstream_is_stream);
        assert_eq!(
            out.openai_body.get("stream").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            out.upstream_url.ends_with("/v1/responses"),
            "{}",
            out.upstream_url
        );
    }

    /// Chat shape keeps the client flag on both sides and targets chat.
    #[test]
    fn chat_shape_preserves_client_stream_flag() {
        let parsed = anthropic_body("codex-5.5", false);
        let upstream: url::Url = "https://api.openai.com/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            None,
            &upstream,
            false,
            "codex-5.5",
            "req-test",
        )
        .expect("translates");
        assert!(!out.downstream_is_stream);
        assert!(
            out.upstream_url.ends_with("/v1/chat/completions"),
            "{}",
            out.upstream_url
        );
    }

    /// C3 decision table: ChatGPT-auth + api.openai.com + target goes to the
    /// Codex responses endpoint, not the generic one.
    #[test]
    fn chatgpt_auth_target_uses_codex_endpoint() {
        let parsed = anthropic_body("claude-codex-5.5", false);
        let upstream: url::Url = "https://api.openai.com/v1".parse().unwrap();
        let out = translate_routed_request(
            &parsed,
            &HeaderMap::new(),
            Some("gpt-5.5"),
            &upstream,
            true,
            "claude-codex-5.5",
            "req-test",
        )
        .expect("translates");
        assert_eq!(
            out.upstream_url,
            crate::codex::codex_endpoint(),
            "the ChatGPT arm must stay on the single endpoint fn"
        );
    }

    /// C3 verify: the ambiguous case (translate on api.openai.com with no
    /// target) serves chat AND fires a per-occurrence warn, so dashboards see
    /// a misroute instead of only the startup log.
    #[test]
    fn ambiguous_codex_route_warns_per_occurrence() {
        use tracing_subscriber::layer::SubscriberExt;
        let capture = crate::test_support::EventCapture::default();
        let lines = capture.0.clone();
        let subscriber = tracing_subscriber::registry().with(capture);
        tracing::subscriber::with_default(subscriber, || {
            let parsed = anthropic_body("codex-5.5", false);
            let upstream: url::Url = "https://api.openai.com/v1".parse().unwrap();
            let out = translate_routed_request(
                &parsed,
                &HeaderMap::new(),
                None,
                &upstream,
                true,
                "codex-5.5",
                "req-amb",
            )
            .expect("translates");
            assert!(
                out.upstream_url.ends_with("/v1/chat/completions"),
                "{}",
                out.upstream_url
            );
        });
        let joined = lines.lock().unwrap().join("\n");
        assert!(
            joined.contains("model_route_ambiguous_codex"),
            "alarm must fire per occurrence: {joined}"
        );
    }
}

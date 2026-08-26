//! Booking a routed turn through the shared outcome funnel.
//!
//! The routed paths (codex, local models, cursor) do not pass through
//! `forward_http`, so nothing else would touch the cost tracker, savings
//! tracker, or request logger. Without this module their spend is real but
//! invisible to `/stats` and the dashboard.

use crate::proxy::AppState;
use crate::routed::transforms::CtxTransformReport;
use axum::http::HeaderMap;
use serde_json::Value;

/// Assemble the outcome context for a routed request.
///
/// `target_model` present means the Responses API; absent means Chat
/// Completions. The provider labels match the ones `forward_http` uses for the
/// same two wire formats, so a `/stats` filter behaves the same either way.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_routed_outcome_context(
    state: &AppState,
    parsed: &Value,
    headers: &HeaderMap,
    target_model: Option<&str>,
    body_model: &str,
    report: CtxTransformReport,
    overhead_ms: f64,
    started_at: std::time::Instant,
    request_id: String,
    replay_store: Option<crate::cache_stabilization::prefix_replay::SessionReplayStore>,
    forwarded_tokens_estimate: i64,
) -> Option<RoutedOutcomeContext> {
    // Resolve the project the same way the Claude path does, so routed turns
    // land in the same per-project buckets rather than an "unknown" pile.
    let hdrs = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|val| (k.as_str().to_lowercase(), val.to_string()))
        })
        .collect();
    let project_ctx = crate::memory::router::RequestContext {
        headers: hdrs,
        system_prompt: crate::memory::router::extract_system_prompt(parsed),
        base_user_id: String::new(),
        project_root_override: None,
    };
    let project =
        crate::memory::router::ProjectResolver::resolve(&project_ctx).map(|(key, _display)| key);

    Some(RoutedOutcomeContext {
        sink: std::sync::Arc::new(crate::proxy::ProxyOutcomeSink::from_state(state)),
        request_id,
        replay_store,
        usage_observer: Some(state.usage_observer.clone()),
        session_key: report.session_key,
        // The upstream model. `body_model` is the client-facing alias, which is
        // deliberately named `claude-*` so Claude Code will offer it in
        // `/model` — booking that would price OpenAI tokens off the `claude-`
        // row in the pricing table.
        model: target_model.unwrap_or(body_model).to_string(),
        provider: if target_model.is_some() {
            "openai_responses".to_string()
        } else {
            "openai_chat".to_string()
        },
        // `None`, matching the Claude path — neither identifies the client
        // today, and inventing a value here would make the two paths report
        // differently for the same caller.
        client: None,
        project,
        tokens_saved: report.tokens_saved,
        transforms_applied: report.transforms_applied,
        num_messages: parsed
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|a| a.len() as i64)
            .unwrap_or(0),
        started_at,
        overhead_ms,
        forwarded_tokens_estimate,
        upstream_attempts: 1,
    })
}

/// Tokens in the request's `tools` array, or 0 when it carries none.
pub(crate) fn count_tools_tokens(body: &Value) -> i64 {
    let Some(tools) = body.get("tools").filter(|t| !t.is_null()) else {
        return 0;
    };
    let Ok(text) = serde_json::to_string(tools) else {
        return 0;
    };
    headroom_core::tokenizer::get_tokenizer(
        body.get("model").and_then(|m| m.as_str()).unwrap_or(""),
    )
    .count_text(&text) as i64
}

/// Metadata threaded into [`StreamTranslator`] so a completed routed turn books
/// the same [`RequestOutcome`] a Claude turn does.
///
/// Without this the translate path never touches the cost tracker, savings
/// tracker, or request logger, so codex traffic is absent from `/stats`,
/// `/stats-history`, and the dashboard — the spend is real but invisible.
#[derive(Clone)]
pub(crate) struct RoutedOutcomeContext {
    pub(crate) sink: std::sync::Arc<crate::proxy::ProxyOutcomeSink>,
    pub(crate) request_id: String,
    /// The *upstream* model, never the client-facing alias. Pricing resolves by
    /// name prefix alone, so booking `claude-codex-5.6` would silently bill
    /// OpenAI tokens at Sonnet rates via the `claude-` family fallback.
    pub(crate) model: String,
    /// `openai_responses` or `openai_chat`, matching the labels `forward_http`
    /// uses for the same wire formats.
    pub(crate) provider: String,
    pub(crate) client: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) tokens_saved: i64,
    pub(crate) transforms_applied: Vec<String>,
    pub(crate) num_messages: i64,
    pub(crate) started_at: std::time::Instant,
    /// Time spent in headroom's own transforms, as distinct from waiting on
    /// upstream.
    pub(crate) overhead_ms: f64,
    /// Request-side estimate used when an error body carries no usage block.
    pub(crate) forwarded_tokens_estimate: i64,
    pub(crate) upstream_attempts: i64,
    /// `Some` when the prefix-replay stage parked this turn. The store needs
    /// the response's cache-token counts to decide how much of the prefix the
    /// provider actually held, so a parked turn must be completed.
    pub(crate) replay_store: Option<crate::cache_stabilization::prefix_replay::SessionReplayStore>,
    /// Carried alongside `replay_store` so a parked turn can be completed
    /// against the right session. Nothing reads it back yet.
    #[allow(dead_code)]
    pub(crate) session_key: String,
    /// CTX-7 observer, to close out the entry parked at request time.
    pub(crate) usage_observer:
        Option<std::sync::Arc<crate::cache_stabilization::usage_observer::UsageObserver>>,
}

/// Book a finished routed turn through the shared outcome funnel.
///
/// `usage` is the provider's own block, in whichever shape the endpoint uses.
/// Cache accounting follows the OpenAI convention the Claude path already
/// encodes for these providers: `input_tokens` *includes* the cached prefix,
/// so uncached is the difference. (Anthropic's own `input_tokens` already
/// excludes it — getting this backwards would double-count the prefix.)
pub(crate) fn book_routed_outcome(
    ctx: &RoutedOutcomeContext,
    usage: Option<&Value>,
    fallback_output_tokens: i64,
    ttfb_ms: f64,
    status_code: i64,
) {
    book_routed_outcome_with_ccr(
        ctx,
        usage,
        fallback_output_tokens,
        ttfb_ms,
        status_code,
        crate::proxy::CcrRoundUsage::default(),
    )
}

/// As [`book_routed_outcome`], plus the usage of CCR continuation rounds the
/// client never saw. Those rounds are billed upstream, so leaving them out
/// books the turn at a fraction of what it cost.
pub(crate) fn book_routed_outcome_with_ccr(
    ctx: &RoutedOutcomeContext,
    usage: Option<&Value>,
    fallback_output_tokens: i64,
    ttfb_ms: f64,
    status_code: i64,
    ccr_rounds: crate::proxy::CcrRoundUsage,
) {
    let get = |key: &str| -> i64 {
        usage
            .and_then(|u| u.get(key))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    // Responses uses `input_tokens`/`output_tokens`; Chat Completions uses
    // `prompt_tokens`/`completion_tokens`. Take whichever is present.
    let provider_reported_input = get("input_tokens").max(get("prompt_tokens"));
    let input_tokens = if usage.is_some() {
        provider_reported_input + ccr_rounds.input_tokens
    } else {
        ctx.forwarded_tokens_estimate.max(0)
    };
    let output_tokens = get("output_tokens")
        .max(get("completion_tokens"))
        .max(fallback_output_tokens)
        + ccr_rounds.output_tokens;
    let cached = usage
        .and_then(|u| {
            u.get("input_tokens_details")
                .or_else(|| u.get("prompt_tokens_details"))
        })
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let outcome = headroom_core::request_outcome::RequestOutcome {
        request_id: ctx.request_id.clone(),
        provider: ctx.provider.clone(),
        model: ctx.model.clone(),
        status_code,
        upstream_attempts: ctx.upstream_attempts,
        provider_input_tokens: usage.map(|_| provider_reported_input + ccr_rounds.input_tokens),
        provider_output_tokens: usage.map(|_| output_tokens),
        // What we forwarded is what upstream counted; the pre-transform size is
        // that plus whatever the transforms removed.
        original_tokens: input_tokens + ctx.tokens_saved.max(0),
        optimized_tokens: input_tokens,
        output_tokens,
        tokens_saved: ctx.tokens_saved.max(0),
        // Same denominator as `original_tokens` above: the material the
        // transforms were asked to work on, not the provider's billing count.
        // See `OutcomeContext::attempted` on the Claude path for why.
        attempted_input_tokens: input_tokens + ctx.tokens_saved.max(0),
        cache_read_tokens: cached,
        uncached_input_tokens: (input_tokens - cached).max(0),
        total_latency_ms: ctx.started_at.elapsed().as_secs_f64() * 1000.0,
        overhead_ms: ctx.overhead_ms,
        ttfb_ms,
        transforms_applied: ctx.transforms_applied.clone(),
        num_messages: ctx.num_messages,
        client: ctx.client.clone(),
        project: ctx.project.clone(),
        ..Default::default()
    };
    headroom_core::request_outcome::emit_request_outcome(ctx.sink.as_ref(), &outcome);
}

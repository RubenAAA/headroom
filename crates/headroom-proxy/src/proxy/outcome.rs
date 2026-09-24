//! Request outcomes: the outcome sink, savings ledger, wire footprint and
//! failure/stream outcome emission, and waste signals.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Phase 2: proxy-side implementation of [`headroom_core::request_outcome::OutcomeSink`].
/// Fans out per-request bookkeeping to the cost tracker, savings tracker,
/// and output-savings recorder.
///
/// Visible crate-wide so handlers that run outside `forward_http` (the routed
/// model translate path) book their traffic through the same funnel rather
/// than a private near-copy. A second sink was easy to let drift: the one in
/// `websocket_codex` silently omits the Prometheus families below.
pub(crate) struct ProxyOutcomeSink {
    pub(crate) cost_tracker: Arc<headroom_core::cost_tracker::CostTracker>,
    pub(crate) savings_tracker: Arc<headroom_core::savings_tracker::SavingsTracker>,
    pub(crate) request_logger: Arc<crate::request_logger::RequestLogger>,
}

/// Tokens the upstream actually processed for billing, when its usage block
/// supplied the cache breakdown. Anthropic reports uncached input, cache reads,
/// and cache creation as disjoint values, so their sum is the post-transform
/// request the proxy sent — not the pre-compression baseline used to measure
/// savings.
///
/// A provider that returns no input usage leaves us without a billable source
/// of truth. In that exceptional case use the post-transform compression
/// estimate rather than the original pre-transform size.
pub(super) fn provider_billed_input_tokens(
    outcome: &headroom_core::request_outcome::RequestOutcome,
) -> i64 {
    let billed = outcome
        .uncached_input_tokens
        .max(0)
        .saturating_add(outcome.cache_read_tokens.max(0))
        .saturating_add(outcome.cache_write_tokens.max(0));
    if billed > 0 {
        billed
    } else {
        outcome.optimized_tokens.max(0)
    }
}

impl ProxyOutcomeSink {
    /// Build a sink from the shared trackers on [`AppState`].
    pub(crate) fn from_state(state: &AppState) -> Self {
        Self {
            cost_tracker: state.cost_tracker.clone(),
            savings_tracker: state.savings_tracker.clone(),
            request_logger: state.request_logger.clone(),
        }
    }
}

impl headroom_core::request_outcome::OutcomeSink for ProxyOutcomeSink {
    fn record_request(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        let billed_input_tokens = provider_billed_input_tokens(outcome);
        // Headline leg: tool-schema tokens never entered context (deferral /
        // hook shrink), additive to `tokens_saved`. Zero on turns without
        // deferral, so every existing number reproduces exactly there.
        let tool_schema_saved =
            crate::tool_schema_savings::tool_schema_saved_from_tags(&outcome.tags).max(0);
        let rec = headroom_core::savings_tracker::RequestRecord {
            model: &outcome.model,
            // `original_tokens` is a savings baseline. Cost and usage must
            // instead follow the request Anthropic received after every proxy
            // transform, as reported in the response usage breakdown.
            input_tokens: billed_input_tokens,
            tokens_saved: outcome.tokens_saved,
            tool_schema_saved,
            compression_savings_cost_usd: Some(outcome.compression_savings_cost_usd()),
            provider: Some(&outcome.provider),
            project: outcome.project.as_deref(),
            cache_read_tokens: outcome.cache_read_tokens,
            cache_write_tokens: outcome.cache_write_tokens,
            uncached_input_tokens: outcome.uncached_input_tokens,
            total_input_tokens: None,
            total_input_cost_usd: None,
            timestamp: None,
            // Read-only counterfactual estimate; `record_output_savings` did
            // the ledger mutation, so the two compose without double-counting.
            output_tokens_saved: headroom_core::output_savings::get_recorder()
                .estimate_request_savings(&outcome.transforms_applied, outcome.output_tokens),
            // Durable lifetime metrics only. The outcome has carried these all
            // along; forwarding them is what lets the persisted blob say
            // whether the cache is working, not just whether we compressed.
            output_tokens: outcome.output_tokens,
            attempted_input_tokens: outcome.attempted_input_tokens,
            cache_write_5m_tokens: outcome.cache_write_5m_tokens,
            cache_write_1h_tokens: outcome.cache_write_1h_tokens,
            cached: outcome.cache_hit(),
            stack: outcome.client.as_deref(),
            waste_signals: outcome.waste_signals.clone(),
            // Router offload: the bill the requested model never saw because
            // the turn was served cheaper. Same value the ledger books as the
            // `model_offload` line; the tracker accumulates it into lifetime
            // totals and `savings_percent`.
            offload_savings_usd: offload_savings(outcome).map(|o| o.saved_usd).unwrap_or(0.0),
        };
        self.savings_tracker.record_request(&rec);

        // Provider-cache Prometheus families. Same place Python emits them
        // (step 1 of the outcome funnel, inside `metrics.record_request`), so
        // every handler that reaches an outcome reports cache usage exactly
        // once.
        crate::observability::proxy_counters::record_provider_cache_observation(
            &outcome.provider,
            &outcome.model,
            outcome.cache_read_tokens.max(0) as u64,
            outcome.cache_write_tokens.max(0) as u64,
            outcome.cache_write_5m_tokens.max(0) as u64,
            outcome.cache_write_1h_tokens.max(0) as u64,
            outcome.uncached_input_tokens.max(0) as u64,
        );

        // Request-level families: counts, tokens, and the latency/overhead/ttfb
        // histograms plus their min/max gauges.
        //
        crate::observability::proxy_counters::record_request(
            &outcome.provider,
            &outcome.model,
            billed_input_tokens as u64,
            outcome.output_tokens.max(0) as u64,
            outcome.tokens_saved.max(0) as u64,
            outcome.total_latency_ms,
            outcome.cache_hit(),
            outcome.overhead_ms,
            outcome.ttfb_ms,
        );
        // Split counter for the headline leg; `tokens_saved_total` above
        // keeps its meaning so existing PromQL is unaffected.
        crate::observability::proxy_counters::record_tool_schema_saved(
            tool_schema_saved.max(0) as u64
        );

        if let Some(signals) = &outcome.waste_signals {
            for (signal, tokens) in signals {
                if *tokens > 0 {
                    crate::observability::proxy_counters::record_waste_signal_tokens(
                        signal,
                        *tokens as u64,
                    );
                }
            }
        }
    }

    fn record_tokens(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        let rec = headroom_core::cost_tracker::TokenRecord {
            tokens_saved: outcome.tokens_saved,
            tool_schema_saved: crate::tool_schema_savings::tool_schema_saved_from_tags(
                &outcome.tags,
            )
            .max(0),
            tokens_sent: provider_billed_input_tokens(outcome),
            cache_read_tokens: outcome.cache_read_tokens,
            cache_write_tokens: outcome.cache_write_tokens,
            cache_write_5m_tokens: outcome.cache_write_5m_tokens,
            cache_write_1h_tokens: outcome.cache_write_1h_tokens,
            uncached_tokens: outcome.uncached_input_tokens,
            output_tokens: outcome.output_tokens,
            cache_inferred: outcome.cache_inferred,
        };
        self.cost_tracker.record_tokens(&outcome.model, &rec);
    }

    fn log_request(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        let entry = crate::request_logger::RequestLogEntry::from_outcome(outcome);
        self.request_logger.log(entry);
    }

    fn record_output_savings(&self, transforms: &[String], output_tokens: i64) {
        // Every `flush_every`th call writes the ledger to disk, and this runs on
        // the task handling the request. `std::fs::write` + `rename` on a tokio
        // worker stalls every other request that thread is driving, so hand it
        // to the blocking pool the way `record_savings_ledger` already does.
        let transforms = transforms.to_vec();
        tokio::task::spawn_blocking(move || {
            headroom_core::output_savings::get_recorder()
                .record_from_labels(&transforms, output_tokens);
        });
    }

    fn record_failed(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        crate::observability::proxy_counters::record_failed(&outcome.provider);
        self.savings_tracker.record_failed_work(
            &headroom_core::savings_tracker::FailedWorkRecord {
                provider: Some(outcome.provider.clone()),
                status_code: outcome.status_code,
                upstream_attempts: outcome.upstream_attempts,
                forwarded_tokens: outcome.optimized_tokens,
                provider_input_tokens: outcome.provider_input_tokens,
                provider_output_tokens: outcome.provider_output_tokens,
                timestamp: None,
            },
        );
    }

    fn record_rate_limited(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        // source="upstream": this funnel only ever sees a 429 the provider
        // returned. Our own limiter rejects before a request is ever sent
        // and records source="headroom" from its own call site.
        crate::observability::proxy_counters::record_rate_limited("upstream");
        self.savings_tracker
            .record_rate_limited(Some(outcome.provider.as_str()));
        // A 429 is failed work too: it reached the failure bucket, not the
        // success funnel, and the ledger must see the wasted tokens.
        self.record_failed(outcome);
    }

    fn record_cache_outcome(&self, provider: &str, reason: &str, wasted_tokens: i64) {
        self.savings_tracker
            .record_cache_miss(Some(provider), Some(reason));
        if wasted_tokens > 0 {
            self.savings_tracker.record_cache_bust(wasted_tokens);
        }
    }

    fn record_savings_ledger(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        // `optimized_tokens` is what we forwarded; the helper reconstructs the
        // pre-compression original. Offloaded to a blocking thread because the
        // append takes a cross-process flock — the same reason Python wraps it
        // in `asyncio.to_thread`. Fire-and-forget: a ledger write must never
        // delay or fail a served request.
        let forwarded = outcome.optimized_tokens;
        // Headline: deferral/hook-shrink tags are additive savings the
        // message count never saw. The ledger helper gates on `> 0` itself,
        // so this stays a no-op for turns without savings.
        let saved =
            crate::tool_schema_savings::headline_tokens_saved(outcome.tokens_saved, &outcome.tags);
        let model = outcome.model.clone();
        let client = outcome.client.clone();
        // Price the booked (headline) count, not the message-only count, so
        // the ledger's token and dollar columns share one basis. The
        // cache-aware rate selection is unchanged.
        let priced_cost = outcome.compression_savings_cost_usd_for(saved);
        // `"free"` (zero-rate tier) makes the basis self-describing: the
        // counterfactual dollars below are 0.0 because the model costs
        // nothing, not because nothing was saved — do not read them
        // against priced rows.
        let priced_basis = outcome.compression_savings_cost_basis().to_string();
        let pricing = headroom_core::pricing::lookup(&model);
        let fresh_rate = pricing
            .map(|p| p.input_cost_per_token)
            .unwrap_or(headroom_core::savings_ledger::DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN);
        let cache_read_rate = pricing
            .and_then(|p| p.cache_read_cost_per_token)
            .unwrap_or(fresh_rate);
        let fresh_counterfactual = saved.max(0) as f64 * fresh_rate;
        let cache_counterfactual = saved.max(0) as f64 * cache_read_rate;
        tracing::info!(
            event = "savings_pricing_counterfactual",
            request_id = %outcome.request_id,
            model = %model,
            tokens_saved = saved,
            cache_read_tokens = outcome.cache_read_tokens,
            cache_write_tokens = outcome.cache_write_tokens,
            fresh_input_rate = fresh_rate,
            cache_read_rate,
            fresh_input_usd = fresh_counterfactual,
            cache_read_usd = cache_counterfactual,
            priced_cost_basis = %priced_basis,
            priced_cost_usd = priced_cost,
            "savings ledger pricing counterfactuals"
        );
        let offload = offload_savings(outcome);
        // Widening the gate above brought turns here that never reached this
        // line before, including ones booked outside a runtime. `spawn_blocking`
        // panics off-runtime, and a ledger append must never be the thing that
        // takes a request down — write inline when there is no pool to hand it
        // to. The counterfactuals are already logged either way.
        if tokio::runtime::Handle::try_current().is_err() {
            write_savings_ledger(
                forwarded,
                saved,
                &model,
                client.as_deref(),
                priced_cost,
                &priced_basis,
                offload,
            );
            return;
        }
        tokio::task::spawn_blocking(move || {
            write_savings_ledger(
                forwarded,
                saved,
                &model,
                client.as_deref(),
                priced_cost,
                &priced_basis,
                offload,
            );
        });
    }
}

/// Append this turn's compression saving and, when the router moved it, the
/// bill its original model never saw. Takes a cross-process flock, so callers
/// hand it to the blocking pool when there is one.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_savings_ledger(
    forwarded: i64,
    saved: i64,
    model: &str,
    client: Option<&str>,
    priced_cost: f64,
    priced_basis: &str,
    offload: Option<OffloadSavings>,
) {
    headroom_core::savings_ledger::record_from_forwarded_with_cost(
        forwarded,
        saved,
        Some(model),
        client,
        Some(priced_cost),
        Some(priced_basis),
    );
    if let Some(offload) = offload {
        headroom_core::savings_ledger::record_savings_event(
            headroom_core::savings_ledger::SavingsEvent {
                tokens_before: offload.tokens,
                tokens_after: 0,
                model: Some(&offload.from_model),
                client: None,
                source: Some("proxy"),
                timestamp: None,
                cost_usd: Some(offload.saved_usd),
                cost_basis: Some("model_offload"),
                fallback_rate: None,
                path: None,
            },
        );
    }
}

/// What a rerouted turn saved by not running on the model the client asked
/// for: the tokens that model never billed, and the money they would have
/// cost there minus what they cost where the turn actually ran.
///
/// Both legs price through the same helper on the same token counts, so an
/// unknown model on either side cannot make them disagree — it moves both.
/// `uncached_input_tokens` rather than `optimized_tokens` because the
/// routed path folds the cached prefix into its input count, and adding
/// `cache_read_tokens` on top would bill that prefix twice.
///
/// `None` for a turn the client's own model served, and for a failed one:
/// a reroute that 502s saved nothing, it just cost the user a turn.
pub(super) fn offload_savings(
    outcome: &headroom_core::request_outcome::RequestOutcome,
) -> Option<OffloadSavings> {
    let from_model = outcome.routed_from_model.clone()?;
    if !(200..300).contains(&outcome.status_code) {
        return None;
    }
    let fresh = outcome.uncached_input_tokens.max(0);
    let out = outcome.output_tokens.max(0);
    let read = outcome.cache_read_tokens.max(0);
    let write = outcome.cache_write_tokens.max(0);
    let tokens = fresh + out + read + write;
    if tokens <= 0 {
        return None;
    }
    // The counterfactual has to price the write at the TTL the provider
    // actually billed. The client's model would have been charged 2.0x input
    // on a 1-hour write against 1.25x on a 5-minute one, and the 1h tier is the
    // common case here, so pricing both legs flat at 5m understated what the
    // reroute avoided.
    let write_1h = outcome.cache_write_1h_tokens;
    let fallback = headroom_core::savings_ledger::DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN;
    let would_have_cost = headroom_core::pricing::estimate_cost_usd_split(
        &from_model,
        fresh,
        out,
        read,
        write,
        write_1h,
        fallback,
    );
    let did_cost = headroom_core::pricing::estimate_cost_usd_split(
        &outcome.model,
        fresh,
        out,
        read,
        write,
        write_1h,
        fallback,
    );
    let saved_usd = (would_have_cost - did_cost).max(0.0);
    tracing::info!(
        event = "model_offload_savings",
        request_id = %outcome.request_id,
        from_model = %from_model,
        to_model = %outcome.model,
        tokens,
        fresh_input_tokens = fresh,
        cache_read_tokens = read,
        cache_write_tokens = write,
        output_tokens = out,
        would_have_cost_usd = would_have_cost,
        did_cost_usd = did_cost,
        saved_usd,
        "turn served off the client's model: tokens and cost it never billed"
    );
    Some(OffloadSavings {
        from_model,
        tokens,
        saved_usd,
    })
}

/// One rerouted turn's contribution to the ledger, computed on the request
/// thread and moved to the blocking pool with the compression entry.
pub(super) struct OffloadSavings {
    pub(super) from_model: String,
    pub(super) tokens: i64,
    pub(super) saved_usd: f64,
}

/// Tool definitions (name → serialized bytes) and the calls the model made
/// (name → count), for the durable tool inventory.
///
/// Calls come from `tool_use` blocks in the history the client just resent, so
/// a name accumulates across the turns it survives in that history rather than
/// once per call — the inventory is read as "used / never used", not as an
/// exact call count.
#[allow(clippy::type_complexity)]
pub(super) fn tool_inventory_of(
    value: &serde_json::Value,
) -> (Vec<(String, i64)>, Vec<(String, i64)>) {
    let mut definitions = Vec::new();
    if let Some(tools) = value.get("tools").and_then(serde_json::Value::as_array) {
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| tool.get("function")?.get("name")?.as_str());
            if let Some(name) = name {
                let bytes = serde_json::to_string(tool).map(|s| s.len()).unwrap_or(0);
                definitions.push((name.to_string(), bytes as i64));
            }
        }
    }

    let mut calls: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    if let Some(messages) = value.get("messages").and_then(serde_json::Value::as_array) {
        for message in messages {
            let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for block in blocks {
                if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
                    continue;
                }
                if let Some(name) = block.get("name").and_then(serde_json::Value::as_str) {
                    *calls.entry(name.to_string()).or_default() += 1;
                }
            }
        }
    }
    (definitions, calls.into_iter().collect())
}

/// Record what the proxy added to this request and which tools it carried.
///
/// Costs two JSON parses of the request body, which is why it runs once, last,
/// and only on the buffered Anthropic path. Against an upstream call measured
/// in seconds it does not register; on a passthrough request it never runs.
pub(super) fn record_request_footprint(
    tracker: &headroom_core::savings_tracker::SavingsTracker,
    request_id: &str,
    original: &bytes::Bytes,
    on_the_wire: &bytes::Bytes,
) {
    let Ok(before) = serde_json::from_slice::<serde_json::Value>(original) else {
        return;
    };
    let Ok(after) = serde_json::from_slice::<serde_json::Value>(on_the_wire) else {
        return;
    };
    audit_tool_pairing(request_id, &before, &after);
    tracker.record_proxy_overhead(prefix_head_bytes(&before), prefix_head_bytes(&after));
    let (definitions, calls) = tool_inventory_of(&after);
    tracker.record_tools(&definitions, &calls);
}

/// `record_request_footprint` on the blocking pool, so the request does not
/// wait for it. The handle is for tests; the proxy drops it.
pub(super) fn spawn_request_footprint(
    tracker: Arc<headroom_core::savings_tracker::SavingsTracker>,
    request_id: String,
    original: bytes::Bytes,
    on_the_wire: bytes::Bytes,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        record_request_footprint(&tracker, &request_id, &original, &on_the_wire)
    })
}

/// Metadata threaded from `forward_http` into the SSE state-machine task so
/// it can build a [`headroom_core::request_outcome::RequestOutcome`] and call
/// [`headroom_core::request_outcome::emit_request_outcome`] at stream close.
#[derive(Clone)]
pub(super) struct OutcomeContext {
    pub(super) sink: Arc<ProxyOutcomeSink>,
    pub(super) model: String,
    pub(super) provider: String,
    pub(super) tags: std::collections::HashMap<String, String>,
    pub(super) client: Option<String>,
    pub(super) project: Option<String>,
    pub(super) original_tokens: i64,
    pub(super) tokens_saved: i64,
    pub(super) transforms_applied: Vec<String>,
    pub(super) num_messages: i64,
    pub(super) total_latency_ms: f64,
    /// Time headroom itself spent on this request (compression and transforms),
    /// as distinct from time waiting on the upstream. Filled in after the
    /// compression stage completes, so it is 0 on paths that never compress.
    pub(super) overhead_ms: f64,
    /// When the request entered the proxy. Used to derive TTFB at the moment
    /// the first upstream byte arrives, which is the only place that is
    /// observable.
    pub(super) started_at: std::time::Instant,
    /// Per-signal waste token counts for this request, if the message body
    /// could be parsed. `None` means "not measured", which is distinct from
    /// "measured and found nothing".
    pub(super) waste_signals: Option<Vec<(String, i64)>>,
    /// True only on the request that inserted the one-time expansion tail.
    /// Its provider cache creation usage is a separate cost signal from the
    /// raw bytes injected on the request path.
    pub(super) proactive_expansion_applied: bool,
    /// Whole-body bytes received from the client and put on the wire. Carried
    /// here because the sizes are only knowable at the send point while the
    /// provider's usage only arrives at stream close, and the pair is worth
    /// nothing apart: bytes alone cannot say what the provider billed.
    pub(super) wire_bytes: Option<(i64, i64)>,
    /// Request-side estimate used only when an error response omits provider
    /// usage. It remains separate from `provider_*_tokens` in the failed-work
    /// bucket so it cannot be mistaken for actual billing.
    pub(super) forwarded_tokens_estimate: i64,
    /// Number of upstream transmissions made for this client turn.
    pub(super) upstream_attempts: i64,
    /// Conversation identity for novel-vs-repeat savings attribution
    /// (upstream `427fa76f`). `Some` only when the request carries a whole
    /// transcript under an explicit conversation id (`/v1/responses`
    /// shape); `None` keeps ordinary per-request accounting, which is
    /// already novel-only on frozen-prefix paths.
    pub(super) conversation_key: Option<String>,
}

/// Book this request's wire bytes against the usage the provider reported for
/// it.
///
/// Both halves have to come from the same request or the ratio is meaningless,
/// which is why this takes the byte pair off the context rather than from any
/// running total. A request whose bytes were never measured (passthrough, or a
/// stream that ended before usage arrived) is skipped entirely: booking bytes
/// with no tokens, or tokens with no bytes, would quietly bias the ratio in
/// whichever direction the missing half went.
pub(super) fn record_wire_footprint(
    ctx: &OutcomeContext,
    input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
) {
    let Some((bytes_in, bytes_out)) = ctx.wire_bytes else {
        return;
    };
    if input_tokens + cache_read_tokens + cache_write_tokens <= 0 {
        return; // no usage reported; nothing to reconcile against
    }
    ctx.sink.savings_tracker.record_wire_footprint(
        bytes_in,
        bytes_out,
        input_tokens,
        cache_read_tokens,
        cache_write_tokens,
    );
}

pub(super) fn observe_proactive_expansion_cache_write(ctx: &OutcomeContext, write_tokens: u64) {
    if ctx.proactive_expansion_applied {
        crate::observability::ctx_metrics::observe_proactive_expansion_cache_write_tokens(
            write_tokens,
        );
    }
}

/// Book a terminal non-SSE upstream rejection into the failure-only bucket. The ordinary
/// success body path builds the same fields later, but upstream rejections take
/// the small buffered-error branch and used to bypass `RequestOutcome`
/// entirely. A usage block is accepted when present; absent usage stays
/// `None`, distinct from the request-side forwarded-token estimate.
pub(super) fn emit_failed_http_outcome(
    ctx: &OutcomeContext,
    request_id: &str,
    status: StatusCode,
    body: Option<&bytes::Bytes>,
) {
    let parsed = body.and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok());
    let usage = parsed.as_ref().and_then(|value| value.get("usage"));
    let get = |key: &str| -> i64 {
        usage
            .and_then(|value| value.get(key))
            .and_then(|value| value.as_i64())
            .unwrap_or(0)
    };
    let (provider_input, output_tokens, cache_read, cache_write) = match ctx.provider.as_str() {
        "anthropic" => (
            get("input_tokens")
                .saturating_add(get("cache_read_input_tokens"))
                .saturating_add(get("cache_creation_input_tokens")),
            get("output_tokens"),
            get("cache_read_input_tokens"),
            get("cache_creation_input_tokens"),
        ),
        "openai_responses" => (
            get("input_tokens"),
            get("output_tokens"),
            usage
                .and_then(|value| value.get("input_tokens_details"))
                .and_then(|value| value.get("cached_tokens"))
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            0,
        ),
        _ => (
            get("prompt_tokens"),
            get("completion_tokens"),
            usage
                .and_then(|value| value.get("prompt_tokens_details"))
                .and_then(|value| value.get("cached_tokens"))
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            0,
        ),
    };
    let provider_input_for_sizes = if ctx.provider == "anthropic" {
        get("input_tokens")
    } else {
        provider_input
    };
    let (original_tokens, optimized_tokens) = ctx.sizes(provider_input_for_sizes);
    let outcome = headroom_core::request_outcome::RequestOutcome {
        request_id: request_id.to_string(),
        provider: ctx.provider.clone(),
        model: ctx.model.clone(),
        status_code: i64::from(status.as_u16()),
        upstream_attempts: ctx.upstream_attempts,
        provider_input_tokens: usage.map(|_| provider_input),
        provider_output_tokens: usage.map(|_| output_tokens),
        original_tokens,
        optimized_tokens,
        output_tokens,
        tokens_saved: ctx.tokens_saved,
        attempted_input_tokens: ctx.attempted(provider_input_for_sizes),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        uncached_input_tokens: if ctx.provider == "anthropic" {
            get("input_tokens")
        } else {
            provider_input.saturating_sub(cache_read)
        },
        total_latency_ms: ctx.started_at.elapsed().as_secs_f64() * 1000.0,
        overhead_ms: ctx.overhead_ms,
        transforms_applied: ctx.transforms_applied.clone(),
        waste_signals: ctx.waste_signals.clone(),
        num_messages: ctx.num_messages,
        tags: ctx.tags.clone(),
        client: ctx.client.clone(),
        project: ctx.project.clone(),
        ..Default::default()
    };
    headroom_core::request_outcome::emit_failed_request_outcome(ctx.sink.as_ref(), &outcome);
}

impl OutcomeContext {
    /// Resolve `(original_tokens, optimized_tokens)` for this request.
    ///
    /// `original_tokens` is only populated when the compression pipeline ran —
    /// it comes from `Outcome::Compressed { tokens_before }`. A transform that
    /// shrinks the body outside that pipeline (ctx_offload) therefore produced
    /// a real `tokens_saved` against a zero baseline, which reads as a 0%
    /// saving and contributes nothing to the savings tracker.
    ///
    /// Fall back to the provider's own input count: that is, by definition, the
    /// size we forwarded, so the pre-transform size is it plus what we removed.
    pub(super) fn sizes(&self, attempted_input_tokens: i64) -> (i64, i64) {
        if self.original_tokens > 0 {
            return (
                self.original_tokens,
                self.original_tokens.saturating_sub(self.tokens_saved),
            );
        }
        let forwarded = if attempted_input_tokens > 0 {
            attempted_input_tokens
        } else {
            self.forwarded_tokens_estimate.max(0)
        };
        (forwarded + self.tokens_saved.max(0), forwarded)
    }

    /// The denominator `RequestOutcome::attempted_input_tokens` is documented
    /// to carry: the size of the material compression was asked to work on.
    ///
    /// Every outcome site used to fill that field from the provider's
    /// `usage.input_tokens` instead. On Anthropic that number excludes cache
    /// reads and writes, so on a warm session it collapses to the uncached
    /// remainder — a live session reported 8,059 against 3.66M of actual
    /// compressible input, and the two fields `attempted_input_tokens` and
    /// `uncached_input_tokens` held byte-identical values, which is the tell.
    ///
    /// The compressible portion is exactly what [`Self::sizes`] already
    /// resolves as `original_tokens`, so read it from there. Note this is NOT
    /// the whole prompt: the frozen cached prefix is not compression's to
    /// touch, and folding it in would make the denominator a sum of the same
    /// prefix re-read every turn.
    pub(super) fn attempted(&self, provider_input_tokens: i64) -> i64 {
        self.sizes(provider_input_tokens).0
    }
}

/// Book a completed OpenAI-shaped stream.
///
/// Chat Completions and the Responses API report the same three numbers under
/// different names; once they have been read, the outcome is identical, and the
/// two copies of this literal were byte-for-byte the same bar a comment. The
/// Anthropic arm is deliberately not folded in — it also carries cache-write
/// tiers, a wire footprint, and CCR continuation totals that neither OpenAI
/// shape has.
pub(super) fn emit_openai_stream_outcome(
    ctx: &OutcomeContext,
    request_id: &str,
    ttfb_ms: f64,
    input_tok: i64,
    cached_tok: i64,
    output_tok: i64,
    // Upstream 4949cd55: the HTTP status the stream arrived with. The old
    // code left this at the `Default` 0 (success), so an exhausted 529
    // served as `text/event-stream` booked a success at stream close. A
    // real >= 500 diverts through the failure funnel in
    // `emit_request_outcome`; anything else behaves exactly as before.
    status_code: i64,
) {
    let outcome = headroom_core::request_outcome::RequestOutcome {
        request_id: request_id.to_string(),
        provider: ctx.provider.clone(),
        model: ctx.model.clone(),
        original_tokens: ctx.sizes(input_tok).0,
        optimized_tokens: ctx.sizes(input_tok).1,
        output_tokens: output_tok,
        tokens_saved: ctx.tokens_saved,
        conversation_key: ctx.conversation_key.clone(),
        conversation_tokens_saved: Some(ctx.tokens_saved),
        attempted_input_tokens: ctx.attempted(input_tok),
        cache_read_tokens: cached_tok,
        // Both providers report a total that includes the cached prefix, unlike
        // Anthropic's `input_tokens`, so the uncached remainder is a subtraction.
        uncached_input_tokens: input_tok.saturating_sub(cached_tok),
        total_latency_ms: ctx.total_latency_ms,
        overhead_ms: ctx.overhead_ms,
        ttfb_ms,
        transforms_applied: ctx.transforms_applied.clone(),
        num_messages: ctx.num_messages,
        tags: ctx.tags.clone(),
        client: ctx.client.clone(),
        project: ctx.project.clone(),
        status_code,
        ..Default::default()
    };
    headroom_core::request_outcome::emit_request_outcome(ctx.sink.as_ref(), &outcome);
}

/// Message array for an inbound request body, across the shapes the proxy sees.
///
/// Anthropic and OpenAI Chat both use `messages`; the OpenAI Responses API uses
/// `input`. Returns `None` when the body carries neither, which is the signal to
/// skip waste measurement rather than measure an empty conversation.
pub(super) fn request_message_array(
    parsed_body: &serde_json::Value,
) -> Option<&Vec<serde_json::Value>> {
    parsed_body
        .get("messages")
        .or_else(|| parsed_body.get("input"))
        .and_then(|v| v.as_array())
}

/// Measure waste signals for one request body.
///
/// Returns `None` when there is nothing to measure — no message array, or a
/// parse that produced no signals. `None` means "not measured"; an empty vec
/// would claim "measured, found nothing".
pub(super) fn waste_signals_for_request(
    parsed_body: &serde_json::Value,
    model: &str,
) -> Option<Vec<(String, i64)>> {
    let messages = request_message_array(parsed_body)?;
    let tokenizer = headroom_core::tokenizer::get_tokenizer(model);
    let (_blocks, _breakdown, waste) =
        headroom_core::parser::parse_messages(messages, tokenizer.as_ref(), None);
    let signals = waste.non_zero();
    if signals.is_empty() {
        None
    } else {
        Some(signals)
    }
}

/// Emit one request's waste signals to Prometheus.
///
/// Only signals that fired are emitted — a zero for every signal on every
/// request would inflate the label space without adding information.
// The signals are computed and logged; this emits them to Prometheus, which
// no caller has asked for yet.
#[allow(dead_code)]
pub(super) fn record_waste_signals(signals: &[(String, i64)]) {
    for (signal, tokens) in signals {
        if *tokens > 0 {
            crate::observability::proxy_counters::record_waste_signal_tokens(
                signal,
                *tokens as u64,
            );
        }
    }
}

#[cfg(test)]
mod waste_signal_wiring_tests {
    use super::*;
    use serde_json::json;

    /// Prometheus state is process-global; these read counter deltas.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Anthropic and OpenAI Chat carry `messages`; OpenAI Responses uses
    /// `input`. Both have to be found, or waste goes unmeasured on that route.
    #[test]
    fn both_request_shapes_expose_their_messages() {
        let chat = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(request_message_array(&chat).map(|m| m.len()), Some(1));

        let responses = json!({"input": [{"role": "user", "content": "hi"}]});
        assert_eq!(request_message_array(&responses).map(|m| m.len()), Some(1));

        assert!(request_message_array(&json!({"model": "x"})).is_none());
    }

    /// A body with no message array is "not measured", not "measured zero".
    #[test]
    fn a_body_without_messages_is_not_measured() {
        assert!(
            waste_signals_for_request(&json!({"model": "x"}), "claude-3-5-sonnet-20241022")
                .is_none()
        );
    }

    /// Clean prose has no waste, and that must read as `None` rather than a
    /// vector of zeros — otherwise every request would emit every signal.
    #[test]
    fn a_clean_request_reports_no_signals() {
        let body = json!({"messages": [{"role": "user", "content": "hello there friend"}]});
        assert!(waste_signals_for_request(&body, "claude-3-5-sonnet-20241022").is_none());
    }

    /// A base64 blob is waste, and only the signals that fired are returned.
    #[test]
    fn a_blob_is_detected_and_only_fired_signals_are_returned() {
        let body = json!({
            "messages": [{"role": "user", "content": format!("data: {}==", "A".repeat(400))}]
        });
        let signals = waste_signals_for_request(&body, "claude-3-5-sonnet-20241022")
            .expect("a base64 blob should register as waste");

        assert!(signals
            .iter()
            .any(|(name, tokens)| name == "base64" && *tokens > 0));
        assert!(
            signals.iter().all(|(_, tokens)| *tokens > 0),
            "only fired signals should be present, got {signals:?}"
        );
    }

    #[test]
    fn recorded_signals_reach_the_counter() {
        let _g = lock();
        let before = crate::observability::proxy_counters::waste_signal_tokens_for_test("base64");

        record_waste_signals(&[("base64".to_string(), 25)]);

        assert_eq!(
            crate::observability::proxy_counters::waste_signal_tokens_for_test("base64"),
            before + 25
        );
    }

    /// A zero or negative count must not be emitted at all.
    #[test]
    fn non_positive_counts_are_not_recorded() {
        let _g = lock();
        let before =
            crate::observability::proxy_counters::waste_signal_tokens_for_test("html_noise");

        record_waste_signals(&[
            ("html_noise".to_string(), 0),
            ("html_noise".to_string(), -5),
        ]);

        assert_eq!(
            crate::observability::proxy_counters::waste_signal_tokens_for_test("html_noise"),
            before
        );
    }
}

#[cfg(test)]
mod offload_savings_tests {
    use super::*;
    use headroom_core::request_outcome::RequestOutcome;

    /// A tool-less Opus turn the router sent to the free Spark tier: 26,015
    /// fresh input tokens, a 1,000-token cache read, 500 out.
    fn spark_turn() -> RequestOutcome {
        RequestOutcome {
            model: "muse-spark-1.3-contributor-free".into(),
            routed_from_model: Some("claude-opus-5".into()),
            status_code: 200,
            uncached_input_tokens: 26_015,
            cache_read_tokens: 1_000,
            cache_write_tokens: 0,
            output_tokens: 500,
            ..Default::default()
        }
    }

    #[test]
    fn a_rerouted_turn_is_credited_the_bill_it_never_sent() {
        let s = offload_savings(&spark_turn()).expect("a served reroute saves something");
        assert_eq!(s.from_model, "claude-opus-5");
        assert_eq!(s.tokens, 27_515);
        // Opus 5: $5/MTok in, $25/MTok out, $0.50/MTok cache read. Spark is
        // free, so the whole counterfactual is the saving.
        let expected = 26_015.0 * 5.0 / 1e6 + 500.0 * 25.0 / 1e6 + 1_000.0 * 0.5 / 1e6;
        assert!(
            (s.saved_usd - expected).abs() < 1e-12,
            "{} != {expected}",
            s.saved_usd
        );
    }

    #[test]
    fn a_turn_the_clients_own_model_served_is_not_an_offload() {
        let mut o = spark_turn();
        o.routed_from_model = None;
        assert!(offload_savings(&o).is_none());
    }

    #[test]
    fn a_failed_reroute_saves_nothing() {
        // The Zen upstream 503s and, with no fallback wired, the turn dies.
        // Booking a saving there would pay us for losing the user's turn.
        let mut o = spark_turn();
        o.status_code = 503;
        assert!(offload_savings(&o).is_none());
    }

    #[test]
    fn a_reroute_to_a_dearer_model_never_books_a_negative_saving() {
        let mut o = spark_turn();
        o.model = "claude-opus-5".into();
        o.routed_from_model = Some("claude-haiku-4-5".into());
        assert_eq!(offload_savings(&o).unwrap().saved_usd, 0.0);
    }
}

//! Session identity and buffered-session analysis.
//!
//! Moved out of `forward.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Return bundle for [`derive_session_identity`]: volatile findings plus the
/// derived session identity `(api-kind, session key, conversation key, lane key)`.
#[allow(clippy::type_complexity)]
pub(crate) type SessionIdentityOut = (
    Vec<cache_stabilization::volatile_detector::VolatileFinding>,
    Option<(ApiKind, String, String, String)>,
);

/// Derives volatile findings and the session identity for a buffered body.
///
/// First half of `analyze_buffered_session`: volatile detection plus the
/// shared session-identity derivation. Returns `(findings, identity)`.
/// Extracted to keep both halves under the complexity threshold; no behavior
/// change.
pub(crate) fn derive_session_identity(
    parsed: &serde_json::Value,
    endpoint: compression::CompressibleEndpoint,
    headers_snapshot: &Option<HeaderMap>,
    client_addr: &std::net::SocketAddr,
) -> SessionIdentityOut {
    // PR-E5: volatile-content detector. Emits one WARN per
    // finding (capped at 10) for content that busts cache
    // (timestamps, UUIDs, ID-named fields).
    let volatile_kind = cache_stabilization::volatile_detector::ApiKind::from_endpoint(endpoint);
    let findings =
        cache_stabilization::volatile_detector::detect_volatile_content(parsed, volatile_kind);
    // PR-E6: cache-bust drift detector. SHA-256 fingerprints
    // the cache hot zone (system / tools / first 3 messages);
    // a mismatch between consecutive turns of the same session
    // emits a `cache_drift_observed` event so operators see
    // invisible cache busts.
    let drift_kind = match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages => Some(ApiKind::Anthropic),
        compression::CompressibleEndpoint::OpenAiChatCompletions => Some(ApiKind::OpenAiChat),
        compression::CompressibleEndpoint::OpenAiResponses => Some(ApiKind::OpenAiResponses),
    };

    // Derived once and shared by the volatile warnings below and the
    // drift detector after them. `derive_session_key` costs up to six
    // SHA-256 digests over canonicalized subtree clones, so deriving it
    // per consumer would put that on the hot path of every request
    // twice over — for a log field.
    let session_identity = match (drift_kind, headers_snapshot.as_ref()) {
        (Some(kind), Some(headers)) => {
            let key = derive_session_key(headers, client_addr, parsed, kind);
            // Lane before the previews: the pins key by it, and the
            // drift baseline, replay store, and observer below take it
            // too. One extra structural hash on the unmutated body;
            // the previews restore byte-identical, so this is the same
            // lane the post-preview hash below would derive.
            let lane = stream_lane_key(&key, &compute_structural_hash(parsed, kind));
            // Lane, not session: the usage observer files this turn
            // under the lane (begin_request below), and the volatile
            // warnings and turn fingerprint join on this key — a
            // session key would silently split one lane into two
            // conversations offline.
            let conversation = cache_stabilization::usage_observer::conversation_key(parsed, &lane);
            Some((kind, key, conversation, lane))
        }
        _ => None,
    };
    (findings, session_identity)
}

/// Parks session observations: usage identity, TTL, shed, ctx, betas.
///
/// Second half of `analyze_buffered_session`'s session arm. Returns
/// `Some(response)` on a concurrency-cap shed; `None` otherwise.
/// Extracted to keep both halves under the complexity threshold; no behavior
/// change.
pub(crate) fn park_session_observations(
    parsed: &serde_json::Value,
    scope: RequestScope<'_>,
    keys: RequestKeys<'_>,
    drift_dims: Option<String>,
    outgoing_headers: &mut HeaderMap,
) -> Option<Response<Body>> {
    let RequestScope {
        state,
        endpoint,
        request_id,
        headers_snapshot,
    } = scope;
    let RequestKeys {
        lane_key: request_lane_key,
        session_key,
        conversation_key: conversation,
        ..
    } = keys;
    // CTX-7: park conversation identity + drift dims under
    // the request id so the response-side usage observer
    // can classify this turn's billed usage against the
    // conversation's previous turn. Keyed by lane, not session:
    // same-opener streams must not share usage baselines.
    state.usage_observer.begin_request(
        request_id,
        cache_stabilization::usage_observer::conversation_key(parsed, request_lane_key),
        Some(session_key),
        drift_dims,
        Some(cache_stabilization::usage_observer::prefix_fingerprint(
            parsed,
        )),
    );
    // Presence for /debug/active-conversations: the canonical
    // project dir parks alongside the usage identity, before the
    // shed check below can pop the entry.
    state.usage_observer.note_project(
        request_id,
        resolve_ctx_project(
            headers_snapshot.as_ref(),
            parsed,
            state.config.memory_project_root.as_deref(),
        ),
    );
    // Price the stock arm at the tier the client actually bought:
    // `parsed` still carries its own markers here, before the
    // pipeline adds or rewrites any. Main-loop traffic arrives
    // on 1h, subagent traffic on the 5-minute default.
    state.usage_observer.note_client_cache_ttl(
        request_id,
        cache_stabilization::cache_ttl::client_ttl_shape(parsed),
    );
    // The stock arm is priced in Anthropic input-equivalents.
    // Turns forwarded to another cache universe (OpenAI chat /
    // Responses) bill without a creation counter, TTL split, or
    // Anthropic horizons, so comparing them at 1.25x/2.0x would
    // invent a premium the provider never charged.
    if !matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        state.usage_observer.note_stock_ineligible(request_id);
    }
    // Conversation-concurrency cap (`--max-conversation-concurrency`):
    // shed fan-out overlap with the client's own retry instead of
    // racing the provider's cache commit on every turn. The pending
    // entry just parked is popped by the shed call, so the turn
    // neither flags later turns concurrent nor lingers as
    // abandoned. Disabled at 0. Ordinary interactive overlap never
    // reaches a cap worth setting; only storms trip it.
    if let Some(in_flight) = state.usage_observer.shed_if_over_conversation_cap(
        request_id,
        conversation,
        state.config.max_conversation_concurrency,
    ) {
        crate::observability::proxy_counters::record_concurrency_shed();
        tracing::warn!(
            event = "conversation_concurrency_shed",
            request_id = %request_id,
            conversation_key = %conversation,
            in_flight,
            cap = state.config.max_conversation_concurrency,
            "conversation over its concurrency cap; shed with 429 so the client retries against a committed prefix"
        );
        return Some(conversation_concurrency_shed_response(
            in_flight,
            state.config.max_conversation_concurrency,
        ));
    }
    // Read off the client's body here, once: the observer has no
    // messages by the time usage comes back, and a first turn
    // that writes cache needs them to say why.
    state.usage_observer.note_first_turn_context(
        request_id,
        cache_stabilization::usage_observer::first_turn_context(parsed),
    );

    // PR-J0: env-gated request-body capture for the offload
    // simulator. Pure observer (no body mutation); no-op unless
    // HEADROOM_CAPTURE_DIR is set. Reuses the hashed session key
    // so the simulator can group + order turns per session.
    let endpoint_label = match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages => "anthropic",
        compression::CompressibleEndpoint::OpenAiChatCompletions => "openai_chat",
        compression::CompressibleEndpoint::OpenAiResponses => "openai_responses",
    };
    cache_stabilization::capture::maybe_capture(parsed, endpoint_label, session_key, request_id);

    // CTX-2: passive session capture. Same spot + inputs as
    // maybe_capture; same never-block rule — `observe` clones the
    // body once and hands it to a detached worker. No-op unless
    // `ctx_capture` is enabled (then `ctx_observer` is `Some`).
    if let Some(observer) = state.ctx_observer.as_ref() {
        let project_dir = resolve_ctx_project(
            headers_snapshot.as_ref(),
            parsed,
            state.config.memory_project_root.as_deref(),
        );
        observer.observe(parsed, session_key, &project_dir);
    }

    // Session-sticky provider beta headers — port of the
    // Python PR-A6 `SessionBetaTracker`. Beta headers are
    // part of the bytes that determine the upstream
    // prefix-cache key; a client dropping a token between
    // turns rotates the key and re-writes the whole
    // prefix at the customer's cost. Forward the
    // per-conversation union instead. See
    // `cache_stabilization::beta_sticky` for the behavior
    // contract, the auth-mode rationale (applies to every
    // mode, like the Python handler), and the one
    // documented divergence from Python (per-conversation
    // keying). Reuses the drift detector's `session_key`
    // so both cache-stability subsystems agree on
    // conversation identity. Mutates upstream-bound
    // HEADERS only; body bytes stay untouched (Phase-A
    // cache-safety invariant).
    if state.config.beta_header_sticky.is_enabled() {
        let provider = match endpoint {
            compression::CompressibleEndpoint::AnthropicMessages => BetaProvider::Anthropic,
            compression::CompressibleEndpoint::OpenAiChatCompletions
            | compression::CompressibleEndpoint::OpenAiResponses => BetaProvider::OpenAi,
        };
        cache_stabilization::beta_sticky::apply_sticky_betas(
            &state.beta_sticky,
            provider,
            request_lane_key,
            outgoing_headers,
            request_id,
        );
    }
    None
}
/// What the session analysis writes back into the caller's locals.
///
/// Every one of these is an output: the request arrives without a lane or a
/// session, and the analysis is what decides them, along with whether the
/// prefix boundary has to be rebuilt and which headers go out.
pub(crate) struct SessionAnalysisOut<'a> {
    pub(crate) session_key: &'a mut String,
    pub(crate) lane_key: &'a mut String,
    pub(crate) conversation_key: &'a mut String,
    pub(crate) api_kind: &'a mut Option<ApiKind>,
    pub(crate) rebuild_boundary: &'a mut bool,
    pub(crate) pre_boundary_agreement: &'a mut Option<usize>,
    pub(crate) outgoing_headers: &'a mut HeaderMap,
}

/// Session/volatile/drift analysis over the parsed buffered body.
///
/// Runs the volatile detector, derives the session identity, emits drift
/// events, previews working-dir/role-sentence pins, runs the replay-store
/// boundary logic, and parks usage-observer entries. Writes the derived
/// session/lane/conversation/api-kind keys plus boundary flags through
/// `&mut` out-params. Returns `Some(response)` on a concurrency-cap shed
/// (caller returns it directly); `None` otherwise.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn analyze_buffered_session(
    parsed: &mut serde_json::Value,
    scope: RequestScope<'_>,
    client_addr: &std::net::SocketAddr,
    out: SessionAnalysisOut<'_>,
) -> Option<Response<Body>> {
    let RequestScope {
        state,
        endpoint,
        request_id,
        headers_snapshot,
    } = scope;
    let SessionAnalysisOut {
        session_key: request_session_key,
        lane_key: request_lane_key,
        conversation_key: request_conversation_key,
        api_kind: request_api_kind,
        rebuild_boundary,
        pre_boundary_agreement,
        outgoing_headers,
    } = out;
    let (findings, session_identity) =
        derive_session_identity(parsed, endpoint, headers_snapshot, client_addr);
    if !findings.is_empty() {
        // Same identity the drift and recache events carry, so a
        // volatile finding can be joined to the bust it is suspected of
        // causing. Item 4 cannot be settled without it: the warning
        // fires on static sample text as readily as on real per-request
        // churn, and only a per-conversation join tells the two apart.
        let session_hash = session_identity
            .as_ref()
            .map(|(_, key, _, _)| cache_stabilization::drift_detector::session_key_log_prefix(key));
        cache_stabilization::volatile_detector::emit_volatile_warnings(
            &findings,
            request_id,
            session_hash.as_deref(),
            session_identity
                .as_ref()
                .map(|(_, _, conv, _)| conv.as_str()),
        );
    }

    if let Some((kind, session_key, conversation, lane)) = session_identity {
        *request_session_key = session_key.clone();
        *request_conversation_key = conversation.clone();
        *request_api_kind = Some(kind);
        *request_lane_key = lane;
        // A lane switch that continues another lane's message lineage
        // inherits its hold pins before the previews below read them;
        // without this the fresh lane latches the live form and the
        // `cd` the holds exist to mask costs a full rewrite.
        if let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()) {
            inherit_lane_pins(state, request_lane_key, messages, request_id);
        }
        // Hash the body the way it will be forwarded. The
        // working-directory hold rewrites `system` further down, so
        // hashing the client's own form calls a `cd` a hot-zone change
        // and drops the stored prefix — the re-cache the hold exists to
        // stop. Put the client's `system` back straight after: nothing
        // below here may see the pinned view.
        let previewed = if state.config.hold_working_directory
            && state.config.prefix_replay
            && matches!(kind, ApiKind::Anthropic)
        {
            state.working_dir_pins.preview(parsed, request_lane_key)
        } else {
            None
        };
        // Same again for the opening sentence: hash the held form so a
        // client-side flip does not read as a hot-zone change.
        let sentence_previewed = if state.config.hold_role_sentence
            && state.config.prefix_replay
            && matches!(kind, ApiKind::Anthropic)
        {
            state.role_sentence_pins.preview(parsed, request_lane_key)
        } else {
            None
        };
        let hash = compute_structural_hash(parsed, kind);
        // Restore in reverse order: the sentence preview saw the
        // directory-held view, so its copy goes back first.
        if let (Some(original), Some(slot)) = (sentence_previewed, parsed.get_mut("system")) {
            *slot = original;
        }
        if let (Some(original), Some(slot)) = (previewed, parsed.get_mut("system")) {
            *slot = original;
        }
        let (drift_dims, lane_birth) =
            observe_drift_with_birth(&state.drift_state, request_lane_key, hash);
        *rebuild_boundary = drift_dims.is_some();

        // Cross-session gate seeding: a newborn lane whose SESSION the
        // gate never saw (model switch, resume) inherits the same
        // conversation's conversions, so known blocks convert on
        // first sight instead of stalling Deferred. Known-session new
        // lanes (same session, new system) hit the shared gate and
        // refuse inside `seed_if_absent` — benign. Runs before the
        // offload policy below reads the gate (S1a ordering), on the
        // client's restored body, and only when flagged on.
        if lane_birth {
            if let (Some(runtime), Some(headers)) =
                (state.ctx_offload.as_ref(), headers_snapshot.as_ref())
            {
                if runtime.config.cross_session_seed {
                    crate::compression::ctx_offload::seed_newborn_session(
                        &runtime.gate,
                        headers,
                        client_addr,
                        parsed,
                        kind,
                        request_session_key,
                        request_id,
                    );
                }
            }
        }

        // The hot zone changed, so every prefix this lane had
        // cached shares a preamble the provider no longer holds —
        // including the alternates, which are prefixes for the same
        // dead cache. Drop them before `apply_prefix_replay` runs
        // below, so the next turn opens a fresh chain instead of
        // splicing bytes against a cache that is gone. A lane switch
        // (same session, new system) does NOT land here — it mints a
        // fresh baseline with no warn — so sibling streams stop
        // invalidating each other.
        if *rebuild_boundary {
            *pre_boundary_agreement = parsed
                .get("messages")
                .and_then(serde_json::Value::as_array)
                .and_then(|messages| {
                    state
                        .replay_store
                        .forwarded_agreement_len(request_lane_key, messages)
                });
            state.replay_store.invalidate(request_lane_key);
            tracing::info!(
                event = "prefix_replay_invalidated_on_rebuild",
                request_id = %request_id,
                session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(&session_key),
                "dropped stored prefix at a drift/rebuild boundary"
            );
        }

        return park_session_observations(
            parsed,
            scope,
            RequestKeys {
                lane_key: request_lane_key,
                session_key: &session_key,
                conversation_key: &conversation,
                api_kind: Some(kind),
            },
            drift_dims,
            outgoing_headers,
        );
    }
    None
}

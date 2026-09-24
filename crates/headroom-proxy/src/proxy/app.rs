//! The axum router: route mounting, the catch-all handler, identity
//! header trust, and inbound request tracking.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Build the axum app. `/healthz`, `/healthz/upstream`, `/livez`, `/readyz`,
/// and `/health` are intercepted; everything else hits the catch-all
/// forwarder. WebSocket upgrades are handled inside the catch-all handler
/// when an `Upgrade: websocket` header is present.
/// PR-D1: native AWS Bedrock InvokeModel routes. Mounts only when
/// `enable_bedrock_native` is on (default). Merged as a sub-router with
/// ONLY the Bedrock routes carrying the auth-mode layer, so it fires
/// before the handler runs and is scoped to these routes alone.
/// Extracted from `build_app` without behavior change.
pub(super) fn mount_bedrock_routes(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    // PR-D1: native AWS Bedrock InvokeModel route. Mounts only when
    // `enable_bedrock_native` is on (default). The handler runs the
    // live-zone compressor over Anthropic-shape bodies, signs with
    // SigV4, and forwards to the configured Bedrock endpoint. The
    // `/converse` route mounts the same handler — the wire shape is
    // identical for `anthropic.claude-*` model IDs (Bedrock just
    // accepts both legacy `invoke` and modern `converse` paths).
    if !state.config.enable_bedrock_native {
        tracing::warn!(
            event = "bedrock_native_disabled",
            "Bedrock native InvokeModel route disabled by \
             --enable-bedrock-native=false; Bedrock requests will fall \
             through to the catch-all (no SigV4 re-signing — fails closed)"
        );
        return router;
    }
    // PR-D3: Bedrock-scoped auth-mode middleware. Build a
    // sub-router with ONLY the Bedrock routes, attach the
    // auth-mode layer (so it fires before the handler runs and
    // is scoped to these routes alone — `/v1/messages`,
    // `/healthz`, etc. do NOT run through this middleware), and
    // merge it into the parent router. The merge composes
    // routes without changing their layer stacks; the parent's
    // `with_state` (applied at the end) hands `AppState` to the
    // Bedrock handlers identically.
    let bedrock_router: Router<AppState> = Router::new()
        .route(
            "/model/{model_id}/invoke",
            post(crate::bedrock::invoke::handle_invoke),
        )
        .route(
            "/model/{model_id}/converse",
            post(crate::bedrock::invoke::handle_invoke),
        )
        // PR-D2/PR-D5: streaming counterparts. Bedrock's protocol is
        // binary EventStream; the handler parses incrementally,
        // optionally translates each chunk to an SSE frame, and
        // tees translated frames into AnthropicStreamState for
        // telemetry. `invoke-with-response-stream` and
        // `converse-stream` share the same wire framing and
        // processing pipeline, so both route to the same handler.
        // See `bedrock::invoke_streaming`.
        .route(
            "/model/{model_id}/invoke-with-response-stream",
            post(crate::bedrock::invoke_streaming::handle_invoke_streaming),
        )
        .route(
            "/model/{model_id}/converse-stream",
            post(crate::bedrock::invoke_streaming::handle_invoke_streaming),
        )
        .route_layer(axum::middleware::from_fn(
            crate::bedrock::classify_and_attach_auth_mode,
        ))
        // Match the explicit body-size cap used by the other proxy handlers.
        // The `Bytes` extractor axum uses for Bedrock would otherwise cap
        // at axum's built-in 2 MiB default, rejecting valid large payloads.
        .layer(DefaultBodyLimit::max(state.config.max_body_bytes as usize));
    let router = router.merge(bedrock_router);
    if !state.config.bedrock_validate_eventstream_crc {
        tracing::warn!(
            event = "bedrock_eventstream_crc_validation_disabled",
            "Bedrock EventStream CRC validation is DISABLED — \
             only safe for debugging; production must keep \
             --bedrock-validate-eventstream-crc=true"
        );
    }
    router
}

/// PR-C4: Conversations API (passthrough-with-instrumentation).
/// The flag is read once at app-build time so router shape
/// matches the configured policy. When disabled, requests still
/// reach upstream via `catch_all`'s streaming forwarder, but the
/// per-route handlers (and their structured-log breadcrumbs) are
/// NOT mounted — operators flip the toggle to silence logs, not
/// to break the surface. The catch-all preserves byte equivalence.
/// Extracted from `build_app` without behavior change.
pub(super) fn mount_conversation_routes(
    router: Router<AppState>,
    state: &AppState,
) -> Router<AppState> {
    if !state.config.enable_conversations_passthrough {
        // Mirror the WARN we use elsewhere when a default-on guard
        // is flipped off. Logged at app-build time, not per-request.
        tracing::warn!(
            event = "conversations_passthrough_disabled",
            "Conversations API per-route handlers disabled by \
             --enable-conversations-passthrough=false; requests will \
             still reach upstream via the catch-all (no per-route logs)"
        );
        return router;
    }
    router
        .route(
            "/v1/conversations",
            post(crate::handlers::conversations::handle_conversations_create),
        )
        .route(
            "/v1/conversations/{conversation_id}",
            get(crate::handlers::conversations::handle_conversations_get)
                .post(crate::handlers::conversations::handle_conversations_update)
                .delete(crate::handlers::conversations::handle_conversations_delete),
        )
        .route(
            "/v1/conversations/{conversation_id}/items",
            post(crate::handlers::conversations::handle_conversations_items_create)
                .get(crate::handlers::conversations::handle_conversations_items_list),
        )
        .route(
            "/v1/conversations/{conversation_id}/items/{item_id}",
            get(crate::handlers::conversations::handle_conversations_item_get)
                .delete(crate::handlers::conversations::handle_conversations_item_delete),
        )
}

/// Batch API routes. Gated on `enable_batch_api` — when disabled,
/// batch requests fall through to the catch-all (byte-equal passthrough).
/// NOTE: Google batch (`:batchGenerateContent`) is NOT registered here —
/// the Gemini dispatcher below owns `/v1beta/models/*model_action` and
/// delegates batch actions itself; registering it twice would panic axum
/// at startup ("Overlapping method route").
/// Extracted from `build_app` without behavior change.
pub(super) fn mount_batch_routes(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    if !state.config.enable_batch_api {
        return router;
    }
    router
        .route(
            "/v1/batches",
            post(crate::handlers::batch::openai_batch_create)
                .get(crate::handlers::batch::openai_batch_list),
        )
        .route(
            "/v1/batches/{batch_id}",
            get(crate::handlers::batch::openai_batch_get),
        )
        .route(
            "/v1/batches/{batch_id}/cancel",
            post(crate::handlers::batch::openai_batch_cancel),
        )
        // Anthropic batch (`/v1/messages/batches*`). Create compresses
        // each request's messages; passthrough (list/get/cancel) forwards
        // verbatim; results runs CCR post-processing.
        .route(
            "/v1/messages/batches",
            post(crate::handlers::batch_anthropic::anthropic_batch_create)
                .get(crate::handlers::batch_anthropic::anthropic_batch_list),
        )
        .route(
            "/v1/messages/batches/{batch_id}",
            get(crate::handlers::batch_anthropic::anthropic_batch_get),
        )
        .route(
            "/v1/messages/batches/{batch_id}/cancel",
            post(crate::handlers::batch_anthropic::anthropic_batch_cancel),
        )
        .route(
            "/v1/messages/batches/{batch_id}/results",
            get(crate::handlers::batch_anthropic::anthropic_batch_results),
        )
}

/// Model routing: intercept /v1/messages only when a local model
/// or extra model routes are configured. When disabled, /v1/messages
/// falls through to the catch-all and streams normally (zero overhead).
/// Extracted from `build_app` without behavior change.
pub(super) fn mount_model_routes(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    if state.config.local_model.is_none() && state.config.model_routes.is_empty() {
        return router;
    }
    let router = router.route(
        "/v1/messages",
        post(crate::handlers::local_model::handle_messages),
    );
    // Token counting for translated routes: a routed alias forwarded
    // verbatim is unknown to Anthropic (404 + a non-JSON page that also
    // pollutes the upstream-health refusal window), so the handler
    // answers those locally. Unrouted models forward byte-identical and
    // keep the exact upstream count.
    let router = router.route(
        "/v1/messages/count_tokens",
        post(crate::handlers::count_tokens::handle_count_tokens),
    );
    // Gateway model discovery (CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1):
    // Claude Code queries this at startup to populate the /model picker
    // with routed models. See handlers::local_model::handle_models.
    router.route(
        "/v1/models",
        get(crate::handlers::local_model::handle_models),
    )
}

/// /debug/* endpoints: loopback-only introspection. The loopback guard
/// layer rejects non-loopback callers with 404 (not 403 — invisible to
/// external scanners). Matches Python's `require_loopback` dependency.
/// Extracted from `build_app` without behavior change.
pub(super) fn mount_debug_routes(router: Router<AppState>) -> Router<AppState> {
    use axum::extract::ConnectInfo;
    use axum::http::StatusCode;
    use axum::middleware::{self, Next};
    use axum::response::{IntoResponse, Response};

    async fn loopback_guard(req: axum::extract::Request, next: Next) -> Response {
        // Gate 1: client IP must be loopback.
        let client_ip = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip());
        let client_ip_str = client_ip.map(|ip| ip.to_string());
        if !crate::loopback_guard::is_loopback_host(client_ip_str.as_deref()) {
            return StatusCode::NOT_FOUND.into_response();
        }

        // Gate 2: Host header must name loopback (DNS-rebinding defense).
        let host_header = req
            .headers()
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok());
        if !crate::loopback_guard::is_loopback_host_header(host_header) {
            return StatusCode::NOT_FOUND.into_response();
        }

        next.run(req).await
    }

    async fn debug_tasks(
        axum::extract::State(state): axum::extract::State<AppState>,
    ) -> axum::response::Json<serde_json::Value> {
        let warmup = crate::warmup::WarmupRegistry::default();
        let ws = state.ws_sessions.lock().unwrap();
        axum::response::Json(crate::debug_introspection::serialize_tasks_debug(
            &warmup, &ws,
        ))
    }

    async fn debug_ws_sessions(
        axum::extract::State(state): axum::extract::State<AppState>,
    ) -> axum::response::Json<serde_json::Value> {
        let ws = state.ws_sessions.lock().unwrap();
        axum::response::Json(crate::debug_introspection::serialize_ws_sessions_debug(&ws))
    }

    async fn debug_warmup(
        axum::extract::State(state): axum::extract::State<AppState>,
    ) -> axum::response::Json<serde_json::Value> {
        let warmup = crate::warmup::WarmupRegistry::default();
        let ws = state.ws_sessions.lock().unwrap();
        axum::response::Json(crate::debug_introspection::serialize_warmup_debug(
            &warmup, &ws,
        ))
    }

    async fn debug_active_conversations(
        axum::extract::State(state): axum::extract::State<AppState>,
    ) -> axum::response::Json<serde_json::Value> {
        let conversations = state.usage_observer.active_conversations();
        axum::response::Json(
            crate::debug_introspection::serialize_active_conversations_debug(conversations),
        )
    }

    async fn debug_zen_egresses(
        axum::extract::State(state): axum::extract::State<AppState>,
    ) -> axum::response::Json<serde_json::Value> {
        let egresses = state.zen_egress_inventory();
        axum::response::Json(serde_json::json!({
            "pool_enabled": !egresses.is_empty(),
            "egresses": egresses,
        }))
    }

    async fn debug_zen_egress_maintenance(
        axum::extract::State(state): axum::extract::State<AppState>,
        axum::extract::Path(egress_id): axum::extract::Path<String>,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> Response {
        let Some(rotating) = body.get("rotating").and_then(serde_json::Value::as_bool) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if !state.set_zen_egress_maintenance(&egress_id, rotating) {
            return StatusCode::NOT_FOUND.into_response();
        }
        axum::Json(serde_json::json!({"ok": true, "rotating": rotating})).into_response()
    }

    let debug_router = Router::new()
        .route("/debug/tasks", get(debug_tasks))
        .route("/debug/ws-sessions", get(debug_ws_sessions))
        .route("/debug/warmup", get(debug_warmup))
        .route("/debug/zen-egresses", get(debug_zen_egresses))
        .route(
            "/debug/zen-egresses/{egress_id}/maintenance",
            post(debug_zen_egress_maintenance),
        )
        .route(
            "/debug/active-conversations",
            get(debug_active_conversations),
        )
        .route(
            "/debug/inflight",
            get(
                |axum::extract::State(state): axum::extract::State<AppState>| async move {
                    // `zen_held` turns are parked in the Zen 429 hold: no
                    // generation is running for them, so the rotation
                    // watcher's drain must not wait on them (they are
                    // waiting on it). `egress_in_flight` already leaves
                    // them out: a parked turn gives its egress back.
                    axum::response::Json(serde_json::json!({
                        "in_flight": InflightGuard::count_global(),
                        "zen_held": crate::routed::zen_hold::held_count(),
                        "egress_in_flight": state.zen_egress_in_flight(),
                    }))
                },
            ),
        )
        .layer(middleware::from_fn(loopback_guard));

    let router = router.merge(debug_router);

    // The MCP endpoint Cursor's agent calls back into, sharing the guard
    // above. It hands out whatever tools a conversation advertised and then
    // blocks on them, so it belongs behind the same loopback gate.
    //
    // Mounted unconditionally: it answers nothing for a conversation that
    // was never opened, and only a `cursor:` model route opens one, so an
    // installation not using Cursor carries a dead path and no state.
    router.merge(
        Router::new()
            .route(
                "/mcp/{conversation}",
                post(crate::cursor::endpoint::handle_mcp),
            )
            .layer(middleware::from_fn(loopback_guard)),
    )
}

pub fn build_app(state: AppState) -> Router {
    // Point `headroom-core`'s Kompress size gate at the Prometheus counter.
    // The core crate has no dependency on this one, so it reports through a
    // hook instead. First call wins; a second `build_app` (as in tests) is a
    // harmless no-op.
    headroom_core::transforms::observability::set_kompress_size_gate_hook(Box::new(|outcome| {
        crate::observability::proxy_counters::record_kompress_size_gate(outcome);
    }));

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/healthz/upstream", get(healthz_upstream))
        // Orchestrator probes (port of upstream `/livez` + `/readyz`).
        // Auth-exempt like `/healthz`, and mounted (not forwarded): before
        // this they fell through to the catch-all and leaked upstream.
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/health", get(health))
        .route("/rollout/status", get(rollout_status))
        // PR-D3: Prometheus scrape endpoint. Renders the global
        // registry in text format. The handler is stateless — no
        // `AppState` needed — and idempotent across concurrent
        // scrapes (`prometheus`'s registry uses internal locking).
        // Mounted unconditionally because it has no dependencies on
        // any feature flag; an operator who doesn't want it scraped
        // simply firewalls the path.
        .route("/metrics", get(crate::observability::handle_metrics))
        // CTX-7: re-cache watchdog snapshot. Cheap (one in-memory
        // snapshot, no I/O) so a statusline script can poll it every
        // few seconds. See `cache_stabilization::usage_observer`.
        .route("/cache-health", get(cache_health))
        .route(
            "/stats",
            get(crate::handlers::stats::handle_stats),
        )
        // Polled by the statusline on every prompt, so it stays separate from
        // the heavier /stats payload.
        .route(
            "/codex-limits",
            get(crate::handlers::stats::handle_codex_limits),
        )
        // Same split-out rationale as /codex-limits: the statusline polls
        // this on every prompt for routed Spark models, whose own
        // `context_window` never arrives from Claude Code.
        .route(
            "/spark-context",
            get(crate::handlers::stats::handle_spark_context),
        )
        .route(
            "/stats/reset",
            post(crate::handlers::stats::handle_stats_reset),
        )
        .route(
            "/stats-history",
            get(crate::handlers::stats::handle_stats_history),
        )
        .route(
            "/stats-lifetime",
            get(crate::handlers::stats::handle_stats_lifetime),
        )
        // PR-C2: explicit POST route for /v1/chat/completions. The
        // handler buffers the body and re-injects it into
        // `forward_http`, which runs the OpenAI live-zone gate
        // alongside the existing Anthropic dispatcher. Non-POST
        // methods (and other paths) still fall through to
        // `catch_all` so the proxy stays a transparent reverse
        // proxy for everything else.
        .route(
            "/v1/chat/completions",
            post(crate::handlers::chat_completions::handle_chat_completions),
        )
        // PR-C3: explicit POST route for /v1/responses. Same forward
        // pattern as /v1/chat/completions — the handler buffers the
        // body, then `forward_http`'s gate dispatches to the
        // Responses live-zone walker via `compress_openai_responses_request`.
        .route(
            "/v1/responses",
            // GET on this path is the Codex Responses WebSocket upgrade —
            // route it through `catch_all`, whose upgrade branch dispatches
            // to `websocket_codex::ws_codex_handler`. (The other three
            // Codex WS aliases have no explicit route and reach `catch_all`
            // via the fallback.) A plain non-upgrade GET forwards as HTTP.
            post(crate::handlers::responses::handle_responses).get(catch_all),
        )
        // PR-D4: native Vertex publisher path. The Vertex AI Anthropic
        // publisher endpoints look like
        // `POST /v1beta1/projects/{p}/locations/{l}/publishers/anthropic/models/{m}:rawPredict`
        // (and `:streamRawPredict`). The trailing `:<verb>` is awkward
        // in axum's `{param}` syntax, so we capture the entire trailing
        // segment as `{model_action}` and split on the last `:` inside
        // the dispatcher. Both verbs share the same axum route shape
        // — matchit can't distinguish two patterns that overlap on the
        // literal parameter. The verb dispatch lives in
        // [`crate::vertex::handle_vertex_predict_dispatch`].
        .route(
            "/v1beta1/projects/{project}/locations/{location}/publishers/anthropic/models/{model_action}",
            post(crate::vertex::handle_vertex_predict_dispatch),
        );

    // PR-D1: native AWS Bedrock InvokeModel routes.
    let router = mount_bedrock_routes(router, &state);

    // PR-C4: Conversations API (passthrough-with-instrumentation).
    let router = mount_conversation_routes(router, &state);

    // Batch API routes.
    let router = mount_batch_routes(router, &state);

    // Gemini native API routes. These handle the Gemini-specific format
    // (contents[] with parts[], systemInstruction) and apply compression
    // via the OpenAI pipeline after format conversion.
    let router = router.route(
        "/v1beta/models/{*model_action}",
        post(crate::handlers::gemini::handle_gemini_action),
    );

    // Azure AI Foundry: Claude Code in Foundry mode points the
    // Anthropic SDK at `ANTHROPIC_FOUNDRY_BASE_URL` (which carries
    // an `/anthropic` path component to mirror the real Azure AI
    // Services URL shape), so requests arrive as
    // `POST /anthropic/v1/messages`. The handler normalizes the
    // path to `/v1/messages` and forwards through the same
    // `forward_http` pipeline, targeting `Config::foundry_base_url`
    // when configured. Mounted unconditionally to match the Python
    // route registration (`headroom/providers/proxy_routes.py`,
    // `foundry_anthropic_messages`).
    let router = router.route(
        "/anthropic/v1/messages",
        post(crate::foundry::handle_foundry_messages),
    );

    // Model routing: intercept /v1/messages only when configured.
    let router = mount_model_routes(router, &state);

    // /debug/* endpoints: loopback-only introspection.
    let router = mount_debug_routes(router);

    // CTX-5/6: mount /ctx/* endpoints when the offload store is available.
    // The endpoints share the proxy's listener — no separate bind.
    let router = if state.config.ctx_offload {
        router.nest("/ctx", crate::ctx::endpoints::router())
    } else {
        router
    };

    // Everything below is layered AFTER the fallback is set, and the order is
    // load-bearing: `Router::layer` wraps the routes registered up to the call,
    // so a layer added above `fallback` does not see the catch-all at all.
    // `/v1/messages` is not a registered route — it falls through — so layering
    // earlier leaves the proxy's busiest path unwrapped.
    //
    // Innermost to outermost: identity strip, request counter, auth gate.
    let router = router.fallback(any(catch_all));

    // WEB-02: drop a caller-supplied memory identity unless the caller is on
    // loopback. Done here rather than at each reader so a new reader cannot
    // miss it — and the readers that matter (`handle_memory_response`, the
    // routed transforms) sit behind the catch-all.
    let router = router.layer(axum::middleware::from_fn(strip_untrusted_identity));

    // Count every inbound request, including ones that fall through to the
    // catch-all.
    let router = router.layer(axum::middleware::from_fn(track_inbound_request));

    // Raise the body cap for every route, not just Bedrock. axum defaults to
    // 2 MiB, and a conversation of ~100k tokens serializes past that, so the
    // proxy answered 413 "Failed to buffer the request body: length limit
    // exceeded" — which Claude Code reports as "Request too large (max 32MB).
    // Accumulated images and attachments...", blaming images that do not
    // exist. Observed 2026-09-14: bodies topping out at 1.90 MB in the log
    // with nothing bigger ever recorded, and 9 such 413s across 6 sessions.
    let router = router.layer(DefaultBodyLimit::max(state.config.max_body_bytes as usize));

    // Require `HEADROOM_PROXY_TOKEN` from non-loopback callers. Outermost, so
    // one gate covers both transports: a WebSocket upgrade arrives as an
    // ordinary HTTP GET and only becomes a socket inside the handler. A
    // rejected caller is not counted as proxy traffic, because it never
    // reached the proxy.
    router
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::proxy_auth::proxy_auth_gate,
        ))
        .with_state(state)
}

/// Whether a caller at `client_ip` may choose its own memory partition.
///
/// `None` means the peer address is unknown, which is not evidence of loopback,
/// so it fails closed — unlike [`crate::loopback_guard::is_loopback_host`],
/// which treats `None` as local for the benefit of test clients.
pub(super) fn identity_header_is_trusted(client_ip: Option<&str>) -> bool {
    client_ip.is_some_and(|ip| crate::loopback_guard::is_loopback_host(Some(ip)))
}

/// Remove `x-headroom-user-id` unless the caller is on loopback.
///
/// The header picks the memory partition, and a caller cannot prove its own
/// authority to select one, so honoring it from a remote caller lets anyone
/// read or write another user's memories. Mirrors Python's
/// `resolve_memory_identity`, which trusts the header only from loopback.
/// Remote callers fall back to the reader's own default partition.
///
/// Missing peer metadata is not evidence of loopback, so it fails closed —
/// unlike [`crate::loopback_guard::is_loopback_host`], which treats `None` as
/// local for the benefit of test clients.
pub(super) async fn strip_untrusted_identity(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    const USER_ID_HEADER: &str = "x-headroom-user-id";
    if req.headers().contains_key(USER_ID_HEADER) {
        let client_ip = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip().to_string());
        if !identity_header_is_trusted(client_ip.as_deref()) {
            req.headers_mut().remove(USER_ID_HEADER);
            tracing::warn!(
                event = "identity_header_stripped",
                header = USER_ID_HEADER,
                client_ip = client_ip.as_deref().unwrap_or("unknown"),
                "ignoring x-headroom-user-id from a non-loopback caller"
            );
        }
    }
    next.run(req).await
}

/// Count an inbound request for the lifetime of its handler.
///
/// `headroom_inbound_requests_active` is a balance, so the decrement has to
/// happen on every exit path. Awaiting the inner service and decrementing after
/// covers handler errors and early returns; a client that disconnects mid-flight
/// drops the future here, which is the one case the counter cannot observe.
pub(super) async fn track_inbound_request(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    crate::observability::proxy_counters::record_inbound_request();
    let response = next.run(request).await;
    crate::observability::proxy_counters::record_inbound_request_completed();
    response
}

/// Catch-all handler. If the request is a WebSocket upgrade, hand off to the
/// ws module; otherwise forward as plain HTTP.
pub(super) async fn catch_all(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response<Body> {
    let (mut parts, body) = req.into_parts();
    if is_websocket_upgrade(&parts.headers) {
        // axum 0.8 requires optional extractors to opt in explicitly, and
        // WebSocketUpgrade intentionally does not. Extract it only after the
        // upgrade headers have identified this as a WebSocket request.
        if let Ok(ws) = WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            let req = Request::from_parts(parts, body);
            return ws_handler(ws, state, client_addr, req).await;
        }
        // Header says websocket but axum didn't extract it (likely missing
        // Sec-WebSocket-Key) — fall through to HTTP forwarding which will
        // surface the upstream error.
    }
    // Codex Live call creation (port of upstream `53982aef`): a multipart
    // POST carrying `sdp` + `session` that the generic forwarder would relay
    // to the default upstream unconverted. The handler gates on headers
    // alone first and hands an unauthenticated request back with its body
    // unread, so the fall-through below forwards exactly what arrived.
    // Non-POST methods keep existing behaviour (the upgrade branch above
    // owns GET-with-upgrade; plain GET forwards as HTTP).
    if parts.method == http::Method::POST
        && crate::codex_live_http::is_live_call_path(parts.uri.path())
    {
        let request_id = ensure_request_id(&parts.headers);
        let inbound_path = parts.uri.path().to_string();
        match crate::codex_live_http::handle_live_call(
            &state.client,
            state.config.strip_internal_headers.is_enabled(),
            state.config.max_body_bytes as usize,
            &request_id,
            &inbound_path,
            parts,
            body,
        )
        .await
        {
            crate::codex_live_http::LiveHttpDecision::Handled(resp) => return resp,
            crate::codex_live_http::LiveHttpDecision::Fallthrough(p, b) => {
                let req = Request::from_parts(p, b);
                return forward_http(state, client_addr, req)
                    .await
                    .unwrap_or_else(|e| e.into_response());
            }
        }
    }
    let req = Request::from_parts(parts, body);
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}

/// True if `Content-Type` is `application/json` (with any optional
/// parameters like `; charset=utf-8`). Compression only inspects JSON
/// bodies — multipart uploads, form-encoded posts, and binary
/// payloads stream through untouched.
/// CTX-7: serve the re-cache watchdog snapshot as JSON.
///
/// The upstream-rejection summary rides along under `upstream`. A refused turn
/// costs more than any cache miss, so it belongs on the endpoint the statusline
/// already polls rather than on one nobody watches.
pub(super) async fn cache_health(State(state): State<AppState>) -> impl IntoResponse {
    let mut snapshot = serde_json::to_value(state.usage_observer.snapshot())
        .unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = snapshot.as_object_mut() {
        obj.insert(
            "upstream".to_string(),
            crate::observability::upstream_health::snapshot(),
        );
    }
    axum::Json(snapshot)
}

pub(super) fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    upgrade && connection
}

#[cfg(test)]
mod identity_trust_tests {
    use super::*;

    #[test]
    fn loopback_callers_may_choose_their_partition() {
        for ip in ["127.0.0.1", "::1", "127.0.0.53", "localhost"] {
            assert!(
                identity_header_is_trusted(Some(ip)),
                "{ip} should be trusted"
            );
        }
    }

    #[test]
    fn remote_callers_may_not() {
        for ip in ["10.0.0.5", "192.168.1.20", "8.8.8.8", "2606:4700::1111"] {
            assert!(
                !identity_header_is_trusted(Some(ip)),
                "{ip} must not be trusted"
            );
        }
    }

    #[test]
    fn unknown_peer_fails_closed() {
        // The loopback guard treats None as local; partition selection must not.
        assert!(crate::loopback_guard::is_loopback_host(None));
        assert!(!identity_header_is_trusted(None));
    }
}

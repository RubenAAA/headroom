//! Local model routing handler.
//!
//! Intercepts `/v1/messages` requests whose `model` field matches the
//! configured local model name. Translates Anthropic Messages API format
//! to OpenAI Chat Completions format, forwards to the local upstream,
//! and translates the response back.

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bytes::Bytes;
use serde_json::{json, Value};
use std::net::SocketAddr;

use crate::proxy::{forward_http, AppState};
use headroom_core::parser::extract_tool_result_text;

use super::reasoning_signature::{
    decode_reasoning_signature, encode_reasoning_signature, reasoning_input_item, PendingReasoning,
};

/// Values mirrored from the Codex CLI source (codex-rs/login/src/auth):
/// the codex backend gates and buckets traffic by originator/user-agent,
/// and token refresh uses the CLI's public OAuth client id.
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Resolve a file that lives alongside auth.json in the codex home dir.
fn codex_home_sibling(auth_file: Option<&str>, name: &str) -> Option<std::path::PathBuf> {
    Some(std::path::Path::new(auth_file?).parent()?.join(name))
}

/// The installed Codex CLI version, read from version.json next to the auth
/// file, so our user-agent tracks whatever CLI release the user actually has.
fn codex_cli_version(auth_file: Option<&str>) -> String {
    codex_home_sibling(auth_file, "version.json")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("latest_version")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| "0.144.1".to_string())
}

/// The CLI's persistent installation id (a UUID codex writes on first run).
fn codex_installation_id(auth_file: Option<&str>) -> Option<String> {
    let content =
        std::fs::read_to_string(codex_home_sibling(auth_file, "installation_id")?).ok()?;
    let id = content.trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// User-agent matching the Codex CLI's format:
/// `{originator}/{version} ({os} {version}; {arch}) {terminal}`.
fn codex_user_agent(auth_file: Option<&str>) -> String {
    format!(
        "{CODEX_ORIGINATOR}/{} (Ubuntu 24.04; {}) WindowsTerminal",
        codex_cli_version(auth_file),
        std::env::consts::ARCH,
    )
}

/// W3C trace context header with random trace/span ids, as sent per-request
/// by the Codex CLI's instrumented HTTP client.
fn generate_traceparent() -> String {
    let trace = uuid::Uuid::new_v4().simple().to_string();
    let span = &uuid::Uuid::new_v4().simple().to_string()[..16];
    format!("00-{trace}-{span}-01")
}

/// Last `x-codex-turn-state` value per session key. The codex backend uses
/// this for sticky routing within a turn; the real CLI echoes it back on
/// follow-up requests, so we do the same across our stateless proxy calls.
static CODEX_TURN_STATE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::OnceLock::new();

fn turn_state_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    CODEX_TURN_STATE.get_or_init(Default::default)
}

// Encrypted reasoning items used to be cached here, keyed by session and
// anchored to the call_id that followed them. That cache is gone: the items now
// ride back to us inside the `thinking` block signature we hand the client.
// See `super::reasoning_signature` for why.

/// Derive a stable UUID-shaped session id from Claude Code's metadata.user_id
/// so `session-id`/`thread-id` headers stay constant within a session.
fn derive_session_uuid(user_id: &str) -> String {
    // FNV-1a over the input, expanded to 128 bits via two passes with
    // different offsets; shape the result like a UUID.
    fn fnv1a(data: &[u8], mut hash: u64) -> u64 {
        for b in data {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
    let h1 = fnv1a(user_id.as_bytes(), 0xcbf29ce484222325);
    let h2 = fnv1a(user_id.as_bytes(), h1 | 1);
    let bytes = [h1.to_be_bytes(), h2.to_be_bytes()].concat();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-8{:01x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6] & 0x0f, bytes[7],
        bytes[8] & 0x0f, bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// Refresh the Codex OAuth token using the refresh_token in the auth file,
/// mirroring codex-rs/login/src/auth/manager.rs. Persists the new tokens back
/// to the auth file and returns the fresh access token.
async fn refresh_codex_token(client: &reqwest::Client, auth_file: &str) -> Option<String> {
    let data = std::fs::read_to_string(auth_file).ok()?;
    let mut parsed: Value = serde_json::from_str(&data).ok()?;
    let refresh_token = parsed
        .get("tokens")?
        .get("refresh_token")?
        .as_str()?
        .to_string();

    let resp = client
        .post(CODEX_REFRESH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&json!({
            "client_id": CODEX_OAUTH_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!(
            event = "codex_token_refresh_failed",
            status = resp.status().as_u16(),
            "codex OAuth token refresh rejected"
        );
        return None;
    }

    let refreshed: Value = resp.json().await.ok()?;
    let access_token = refreshed.get("access_token")?.as_str()?.to_string();

    let tokens = parsed.get_mut("tokens")?;
    tokens["access_token"] = json!(access_token.clone());
    if let Some(rt) = refreshed.get("refresh_token").and_then(|v| v.as_str()) {
        tokens["refresh_token"] = json!(rt);
    }
    if let Some(idt) = refreshed.get("id_token").and_then(|v| v.as_str()) {
        tokens["id_token"] = json!(idt);
    }
    parsed["last_refresh"] = json!(chrono::Utc::now().to_rfc3339());
    if let Ok(serialized) = serde_json::to_string_pretty(&parsed) {
        if let Err(e) = std::fs::write(auth_file, serialized) {
            tracing::warn!(
                event = "codex_token_persist_failed",
                error = %e,
                "refreshed codex token could not be written back to auth file"
            );
        }
    }
    tracing::info!(
        event = "codex_token_refreshed",
        "codex OAuth access token refreshed"
    );
    Some(access_token)
}

/// Read the Codex access token from the auth JSON file.
/// Returns the token string, or None if the file doesn't exist or is invalid.
/// Re-reads on every call so Codex's token refresh cycle is picked up.
fn read_codex_access_token(path: &str) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&data).ok()?;
    parsed
        .get("tokens")?
        .get("access_token")?
        .as_str()
        .map(String::from)
}

fn decode_openai_bearer_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let payload = URL_SAFE_NO_PAD.decode(payload.as_bytes()).ok()?;
    serde_json::from_slice(&payload).ok()
}

fn resolve_codex_routing_headers(
    headers: &HeaderMap,
    auth_file: Option<&str>,
) -> (HeaderMap, bool) {
    let mut upstream_headers = HeaderMap::new();
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        "application/json".parse().expect("valid header"),
    );
    // Identify as a Codex client; the backend gates/buckets by these.
    upstream_headers.insert(
        "originator",
        CODEX_ORIGINATOR.parse().expect("valid header"),
    );
    if let Ok(ua) = codex_user_agent(auth_file).parse() {
        upstream_headers.insert(http::header::USER_AGENT, ua);
    }
    if let Some(id) = codex_installation_id(auth_file) {
        if let Ok(val) = http::HeaderValue::from_str(&id) {
            upstream_headers.insert("x-codex-installation-id", val);
        }
    }
    if let Ok(tp) = generate_traceparent().parse() {
        upstream_headers.insert("traceparent", tp);
    }

    // Prefer an explicit ChatGPT account id if the caller supplied one.
    if let Some(account_id) = headers.get("ChatGPT-Account-ID") {
        upstream_headers.insert("ChatGPT-Account-ID", account_id.clone());
        if let Some(auth) = headers.get(http::header::AUTHORIZATION) {
            upstream_headers.insert(http::header::AUTHORIZATION, auth.clone());
        }
        return (upstream_headers, true);
    }

    if let Some(auth_file) = auth_file {
        if let Some(token) = read_codex_access_token(auth_file) {
            if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
                upstream_headers.insert(http::header::AUTHORIZATION, val);
            }

            if let Some(payload) = decode_openai_bearer_payload(&token) {
                if let Some(account_id) = payload
                    .get("https://api.openai.com/auth")
                    .and_then(|auth| auth.get("chatgpt_account_id"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if let Ok(val) = http::HeaderValue::from_str(account_id) {
                        upstream_headers.insert("ChatGPT-Account-ID", val);
                        return (upstream_headers, true);
                    }
                }
            }

            return (upstream_headers, false);
        }
    }

    if let Some(auth) = headers.get(http::header::AUTHORIZATION) {
        upstream_headers.insert(http::header::AUTHORIZATION, auth.clone());
    }

    (upstream_headers, false)
}

/// Apply headroom's CTX request-side transforms to a routed model's parsed
/// Anthropic body, reusing the same flags/state as the Claude passthrough path
/// (`forward_http`). Runs the passive session capture (read-only) and, when
/// `ctx_offload` is enabled, the tool_result offload — which both feeds
/// `headroom ctx search` and shrinks the request. Mutates `parsed` in place.
///
/// Note: offload rewrites frozen history only on rebuild boundaries (the gate
/// prevents cache thrash), exactly as the Claude path does.
fn apply_ctx_request_transforms(
    state: &AppState,
    parsed: &mut Value,
    headers: &HeaderMap,
    client_addr: &SocketAddr,
) {
    use crate::cache_stabilization::drift_detector::{
        compute_structural_hash, derive_session_key, observe_drift, ApiKind,
    };

    // Derived from the body as received — this runs before any transform
    // mutates `parsed`, which matters because `derive_session_key`
    // fingerprints the conversation's first message when no
    // `x-headroom-session-id` header is present.
    let session_key = derive_session_key(headers, client_addr, parsed, ApiKind::Anthropic);

    // Observe cache-prefix drift on the incoming body (before any transform),
    // matching the Claude path's ordering. Runs unconditionally so the
    // `cache_drift_observed` signal (which axis of system/tools/early_messages
    // changed turn-to-turn) is available regardless of which CTX flags are on.
    // A drift means the codex prompt-cache prefix moved this turn.
    let hash = compute_structural_hash(parsed, ApiKind::Anthropic);
    let rebuild_boundary = observe_drift(&state.drift_state, &session_key, hash).is_some();

    // CTX-2: passive session capture. Read-only — clones the body onto a
    // detached worker; never mutates and never blocks.
    if let Some(observer) = state.ctx_observer.as_ref() {
        observer.observe(parsed, &session_key);
    }

    // CTX-4: recall/resume injection. Runs BEFORE offload (matching the
    // Claude path order). Cache-safe by construction — the engine decides
    // once per conversation and replays the exact same bytes into the first
    // user message on every later turn (nothing volatile), so the codex
    // prompt-cache prefix stays byte-stable after the one-time introduction.
    // It never touches `system`/`tools`.
    if let Some(engine) = state.ctx_inject.as_ref() {
        if engine.maybe_inject(parsed, &session_key) {
            tracing::debug!(
                event = "codex_ctx_inject",
                "injected recall/resume block into routed-model request"
            );
        }
    }

    // CTX-3: tool_result offload. Feeds the FTS search store and shrinks the
    // body. Gated on the same `ctx_offload` flag as the Claude path.
    if let Some(runtime) = state.ctx_offload.as_ref() {
        let policy = crate::compression::ctx_offload::OffloadPolicy {
            gate: &runtime.gate,
            session_key: &session_key,
            rebuild_boundary,
        };
        let out = crate::compression::ctx_offload::offload_anthropic_request(
            parsed,
            &runtime.config,
            Some(&policy),
        );
        if out.changed() {
            tracing::debug!(
                event = "codex_ctx_offload",
                blocks_offloaded = out.blocks_offloaded,
                blocks_deferred = out.blocks_deferred,
                rebuild_boundary,
                "offloaded tool_result blocks on routed-model request"
            );
            runtime.store.persist(out.records);
        }
    }
}

fn apply_target_model_override(
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

/// Handle GET `/v1/models` for Claude Code's gateway model-discovery feature
/// (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`).
///
/// Claude Code reads `id`/`display_name` from `data[]` and drops any entry
/// whose `id` doesn't start with `claude` or `anthropic`, so only exact-match
/// routes (never `*`-suffixed prefix routes, which aren't a single
/// selectable model) with a qualifying `model_prefix` are listed. Operators
/// who want a routed model (MiMo, Codex, ...) to show up in `/model` name
/// its route with a `claude-`/`anthropic-` prefix, e.g.
/// `--extra-model-route "claude-mimo-v2.5=mimo:MiMo-V2.5"`.
pub async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    fn discoverable(id: &str) -> bool {
        id.starts_with("claude") || id.starts_with("anthropic")
    }

    let mut data: Vec<Value> = Vec::new();

    if let Some(local_model) = &state.config.local_model {
        if discoverable(local_model) {
            data.push(json!({
                "id": local_model,
                "display_name": format!("{local_model} (headroom local model)"),
            }));
        }
    }

    for route in &state.config.model_routes {
        if route.prefix_match || !discoverable(&route.model_prefix) {
            continue;
        }
        let display_name = if let Some(mimo_model) = &route.mimo_run {
            format!("{} (via mimo)", mimo_model)
        } else if let Some(upstream) = &route.upstream {
            format!("{} (via {})", route.model_prefix, upstream.authority())
        } else {
            route.model_prefix.clone()
        };
        data.push(json!({
            "id": route.model_prefix,
            "display_name": display_name,
        }));
    }

    axum::Json(json!({ "data": data }))
}

/// Handle POST `/v1/messages` with local model routing.
///
/// 1. Buffer the body
/// 2. Check if `model` matches the configured local model
/// 3. If yes: translate request -> forward to local upstream -> translate response
/// 4. If no: delegate to `forward_http()` (transparent passthrough)
pub async fn handle_messages(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Parse body to extract model name.
    let mut parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            // Not JSON — can't be a model-routed request, delegate to forward_http.
            let mut builder = Request::builder().method(method).uri(uri);
            if let Some(hs) = builder.headers_mut() {
                *hs = headers;
            }
            let req = builder.body(Body::from(body)).expect("valid request");
            return forward_http(state, client_addr, req)
                .await
                .unwrap_or_else(|e| e.into_response());
        }
    };

    let body_model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body_model = body_model.as_str();

    // Find a matching route: first check local_model (backward compat),
    // then check model_routes table.
    // Check for mimo_run route first (highest priority)
    let mimo_run_model = state
        .config
        .model_routes
        .iter()
        .find(|r| r.matches(body_model))
        .and_then(|r| r.mimo_run.clone());

    if let Some(ref mimo_model) = mimo_run_model {
        return handle_mimo_run(state, &parsed, body_model, mimo_model).await;
    }

    let matched = if let (Some(model), Some(upstream)) =
        (&state.config.local_model, &state.config.local_upstream)
    {
        if body_model == model.as_str() {
            Some((upstream.clone(), true, None))
        } else {
            None
        }
    } else {
        None
    };

    let matched = matched.or_else(|| {
        state
            .config
            .model_routes
            .iter()
            .find(|r| r.matches(body_model))
            .filter(|r| r.mimo_run.is_none())
            .and_then(|r| Some((r.upstream.clone()?, r.translate, r.target_model.clone())))
    });

    let (upstream, translate, target_model) = match matched {
        Some((u, t, tm)) => (u.clone(), t, tm),
        None => {
            // No route matched — delegate to standard forwarder.
            let mut builder = Request::builder().method(method).uri(uri);
            if let Some(hs) = builder.headers_mut() {
                *hs = headers;
            }
            let req = match builder.body(Body::from(body)) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        event = "handler_error",
                        handler = "messages_local_model",
                        error = %e,
                        "failed to reconstruct request"
                    );
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::from("internal handler error"))
                        .expect("static response");
                }
            };
            return forward_http(state, client_addr, req)
                .await
                .unwrap_or_else(|e| e.into_response());
        }
    };

    let (upstream_headers, is_chatgpt_auth) =
        resolve_codex_routing_headers(&headers, state.config.codex_auth_file.as_deref());

    if !translate {
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
            .headers(upstream_headers)
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

        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let resp_headers = resp.headers().clone();
        let body_bytes = resp.bytes().await.unwrap_or_default();

        let mut response = Response::builder().status(status);
        for (name, value) in resp_headers.iter() {
            if !crate::headers::is_response_drop(name) {
                response = response.header(name.clone(), value.clone());
            }
        }
        return response
            .body(Body::from(body_bytes))
            .expect("static response");
    }

    // Apply headroom's CTX request-side transforms (session capture +
    // tool_result offload) so routed models get the same optimizations and
    // searchable archive as the Claude passthrough path, gated on the same
    // flags. Mutates `parsed` before translation.
    apply_ctx_request_transforms(&state, &mut parsed, &headers, &client_addr);

    // Translation path: Anthropic → OpenAI.
    let openai_body = match if target_model.is_some() {
        anthropic_to_openai_responses_request(&parsed, false)
    } else {
        anthropic_to_openai_request(&parsed, true, true)
    } {
        Ok(v) => apply_target_model_override(
            v,
            target_model.as_deref(),
            target_model.is_some(),
            target_model.is_some(),
        ),
        Err(e) => {
            tracing::warn!(
                event = "local_model_translate_error",
                error = %e,
                "failed to translate Anthropic request to OpenAI format"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("translation error"))
                .expect("static response");
        }
    };

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
        model = %body_model,
        upstream = %upstream_url,
        stream = upstream_is_stream,
        "routing to upstream with format translation"
    );

    let openai_body_bytes = match serde_json::to_vec(&openai_body) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize OpenAI request"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };

    let mut upstream_headers = upstream_headers;

    // Session correlation headers and turn-state echo, mirroring the real
    // Codex client (codex-api/src/requests/headers.rs, client.rs).
    let session_key = parsed
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    if is_chatgpt_auth {
        if let Some(key) = &session_key {
            let session_uuid = derive_session_uuid(key);
            if let Ok(val) = http::HeaderValue::from_str(&session_uuid) {
                upstream_headers.insert("session-id", val.clone());
                upstream_headers.insert("thread-id", val);
            }
            let stored = turn_state_map()
                .lock()
                .ok()
                .and_then(|m| m.get(key).cloned());
            if let Some(ts) = stored {
                if let Ok(val) = http::HeaderValue::from_str(&ts) {
                    upstream_headers.insert("x-codex-turn-state", val);
                }
            }
        }
    }

    // Send with retry: refresh the OAuth token once on 401, back off on
    // 429/5xx/transport errors (honoring Retry-After), like the Codex CLI.
    const MAX_ATTEMPTS: u32 = 3;
    let mut refreshed = false;
    let mut attempt: u32 = 0;
    let upstream_resp = loop {
        attempt += 1;
        let result = state
            .client
            .post(&upstream_url)
            .headers(upstream_headers.clone())
            .body(openai_body_bytes.clone())
            .send()
            .await;

        match result {
            Ok(r) => {
                let status = r.status();
                if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed && is_chatgpt_auth {
                    if let Some(auth_file) = state.config.codex_auth_file.as_deref() {
                        if let Some(token) = refresh_codex_token(&state.client, auth_file).await {
                            if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}"))
                            {
                                upstream_headers.insert(http::header::AUTHORIZATION, val);
                            }
                            refreshed = true;
                            continue;
                        }
                    }
                    break r;
                }
                if (status.as_u16() == 429 || status.is_server_error()) && attempt < MAX_ATTEMPTS {
                    let retry_after = r
                        .headers()
                        .get(http::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());
                    let backoff = retry_after
                        .map(std::time::Duration::from_secs)
                        .unwrap_or_else(|| {
                            std::time::Duration::from_millis(250 * 2u64.pow(attempt - 1))
                        });
                    tracing::warn!(
                        event = "local_model_upstream_retry",
                        status = status.as_u16(),
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        "retrying transient upstream error"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                break r;
            }
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    let backoff = std::time::Duration::from_millis(250 * 2u64.pow(attempt - 1));
                    tracing::warn!(
                        event = "local_model_upstream_retry",
                        error = %e,
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        "retrying failed upstream connection"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                tracing::warn!(
                    event = "local_model_upstream_error",
                    error = %e,
                    upstream = %upstream_url,
                    "failed to connect to local model upstream"
                );
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!("local upstream error: {e}")))
                    .expect("static response");
            }
        }
    };

    // Capture the turn-state token for sticky routing on follow-up requests.
    if let Some(key) = &session_key {
        if let Some(ts) = upstream_resp
            .headers()
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok())
        {
            if let Ok(mut map) = turn_state_map().lock() {
                map.insert(key.clone(), ts.to_string());
            }
        }
    }

    let upstream_status = upstream_resp.status();

    if downstream_is_stream {
        handle_streaming_response(upstream_resp, &parsed).await
    } else if target_model.is_some() {
        handle_buffered_responses_response(upstream_resp, &parsed, upstream_status).await
    } else {
        handle_buffered_response(upstream_resp, &parsed, upstream_status).await
    }
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic → OpenAI
// ---------------------------------------------------------------------------

fn anthropic_to_openai_request(
    anthropic: &Value,
    include_max_output_tokens: bool,
    include_tool_calls: bool,
) -> Result<Value, String> {
    let model = anthropic
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("missing model field")?;

    let max_tokens = anthropic.get("max_tokens").and_then(|v| v.as_u64());

    let stream = anthropic
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let temperature = anthropic.get("temperature");

    let mut messages: Vec<Value> = Vec::new();

    // System prompt → system message.
    if let Some(system) = anthropic.get("system") {
        match system {
            Value::String(s) => {
                messages.push(json!({"role": "system", "content": s}));
            }
            Value::Array(arr) => {
                let text: String = arr
                    .iter()
                    .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(json!({"role": "system", "content": text}));
            }
            _ => {}
        }
    }

    if let Some(msgs) = anthropic.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            match role {
                "user" => translate_user_message(msg, &mut messages),
                "assistant" => translate_assistant_message(msg, &mut messages, include_tool_calls),
                _ => {}
            }
        }
    }

    let tools = anthropic
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|anthropic_tools| {
            anthropic_tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name")?.as_str()?;
                    let description = tool.get("description").and_then(|d| d.as_str());
                    let input_schema = tool.get("input_schema");
                    let default_schema = json!({});
                    let params = input_schema.unwrap_or(&default_schema);
                    Some(json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description.unwrap_or(""),
                            "parameters": params
                        }
                    }))
                })
                .collect::<Vec<_>>()
        });

    let mut openai = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });

    if include_max_output_tokens {
        if let Some(mt) = max_tokens {
            // Chat Completions uses `max_tokens`; `max_output_tokens` is a
            // Responses-API field and would be silently ignored here.
            openai["max_tokens"] = json!(mt);
        }
    }
    if let Some(t) = temperature {
        openai["temperature"] = t.clone();
    }
    if let Some(tools) = tools {
        if !tools.is_empty() {
            openai["tools"] = json!(tools);
        }
    }

    Ok(openai)
}

fn anthropic_to_openai_responses_request(
    anthropic: &Value,
    include_max_output_tokens: bool,
) -> Result<Value, String> {
    let model = anthropic
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("missing model field")?;

    let max_tokens = anthropic.get("max_tokens").and_then(|v| v.as_u64());
    let stream = anthropic
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut input: Vec<Value> = Vec::new();
    let mut instructions: Vec<String> = Vec::new();

    if let Some(system) = anthropic.get("system") {
        match system {
            Value::String(s) => {
                if !s.is_empty() {
                    instructions.push(s.clone());
                }
            }
            Value::Array(arr) => {
                let text = arr
                    .iter()
                    .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            _ => {}
        }
    }

    if let Some(msgs) = anthropic.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            match role {
                "user" => translate_user_message_to_responses(msg, &mut input),
                "assistant" => translate_assistant_message_to_responses(msg, &mut input),
                "system" | "developer" => {
                    let text = plain_message_text(msg);
                    if !text.is_empty() {
                        instructions.push(text);
                    }
                }
                _ => {}
            }
        }
    }

    // Reasoning items are already back in `input`: they were decoded from the
    // thinking blocks the client echoed, in the position they originally held.

    // Responses API uses a flat tool shape, unlike Chat Completions where
    // the fields are nested under `function`.
    let tools = anthropic
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|anthropic_tools| {
            anthropic_tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name")?.as_str()?;
                    let description = tool.get("description").and_then(|d| d.as_str());
                    let input_schema = tool.get("input_schema");
                    let default_schema = json!({});
                    let mut params = input_schema.unwrap_or(&default_schema).clone();
                    // Non-Claude models tend to fill every optional field.
                    // Strip the Agent tool's `mode` so spawned subagents
                    // inherit the session's permission mode instead of
                    // getting an explicit override.
                    if name == "Agent" {
                        if let Some(props) =
                            params.get_mut("properties").and_then(|p| p.as_object_mut())
                        {
                            props.remove("mode");
                        }
                    }
                    Some(json!({
                        "type": "function",
                        "name": name,
                        "description": description.unwrap_or(""),
                        "parameters": params
                    }))
                })
                .collect::<Vec<_>>()
        });

    let mut openai = json!({
        "model": model,
        "input": input,
        "stream": stream,
    });

    let has_tools = tools.as_ref().is_some_and(|t| !t.is_empty());
    if let Some(tools) = tools {
        if !tools.is_empty() {
            openai["tools"] = json!(tools);
        }
    }
    if let Some(tool_choice) = anthropic.get("tool_choice") {
        let translated = match tool_choice.get("type").and_then(|t| t.as_str()) {
            Some("auto") => Some(json!("auto")),
            Some("any") => Some(json!("required")),
            Some("none") => Some(json!("none")),
            Some("tool") => tool_choice
                .get("name")
                .and_then(|n| n.as_str())
                .map(|n| json!({"type": "function", "name": n})),
            _ => None,
        };
        if let Some(tc) = translated {
            openai["tool_choice"] = tc;
        }
    }
    // The Codex client always sends these explicitly rather than relying on
    // server defaults (codex-api/src/common.rs: ResponsesApiRequest).
    if has_tools && openai.get("tool_choice").is_none() {
        openai["tool_choice"] = json!("auto");
    }
    openai["parallel_tool_calls"] = json!(false);
    openai["include"] = json!(["reasoning.encrypted_content"]);
    if !instructions.is_empty() {
        openai["instructions"] = json!(instructions.join("\n\n"));
    }
    if include_max_output_tokens {
        if let Some(mt) = max_tokens {
            openai["max_output_tokens"] = json!(mt);
        }
    }
    // Note: `temperature` is deliberately NOT forwarded — the Codex
    // ResponsesApiRequest has no such field and the real CLI never sends it.
    // Map Anthropic's thinking budget onto Responses reasoning effort so the
    // client's thinking setting isn't silently upgraded to the backend default.
    if let Some(thinking) = anthropic.get("thinking") {
        if thinking.get("type").and_then(|t| t.as_str()) == Some("enabled") {
            let effort = match thinking.get("budget_tokens").and_then(|v| v.as_u64()) {
                Some(b) if b <= 4096 => "low",
                Some(b) if b <= 16384 => "medium",
                Some(_) => "high",
                None => "medium",
            };
            // `summary: auto` + sequential delivery makes the backend stream
            // reasoning summaries, which we translate into thinking blocks.
            openai["reasoning"] = json!({"effort": effort, "summary": "auto"});
            openai["stream_options"] = json!({"reasoning_summary_delivery": "sequential_cutoff"});
        }
    }
    // Stable per-session cache key enables upstream prompt caching; Claude
    // Code's metadata.user_id includes the session id.
    if let Some(user_id) = anthropic
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
    {
        openai["prompt_cache_key"] = json!(user_id);
    }

    Ok(openai)
}

fn plain_message_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

fn translate_user_message_to_responses(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"type": "message", "role": "user", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        Some(Value::Null) | None => {
            out.push(json!({"type": "message", "role": "user", "content": ""}));
            return;
        }
        Some(other) => {
            out.push(json!({"type": "message", "role": "user", "content": other.to_string()}));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut emitted_message = false;
    let mut emitted_tool_result = false;

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_result") => {
                if !text_parts.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": text_parts.join("\n")
                    }));
                    text_parts.clear();
                    emitted_message = true;
                }
                emitted_tool_result = true;
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // `content` is a plain string OR a list of blocks; the naive
                // `as_str()` silently blanked every array-shaped result.
                let result_content = extract_tool_result_text(block);
                let is_error = block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": if is_error { format!("Error: {result_content}") } else { result_content.to_string() }
                }));
            }
            _ => {}
        }
    }

    if !text_parts.is_empty() || (!emitted_message && !emitted_tool_result) {
        out.push(json!({
            "type": "message",
            "role": "user",
            "content": text_parts.join("\n")
        }));
    }
}

fn translate_assistant_message_to_responses(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"type": "message", "role": "assistant", "content": s}));
            return;
        }
        Some(Value::Null) => {
            out.push(json!({"type": "message", "role": "assistant", "content": ""}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            out.push(json!({"type": "message", "role": "assistant", "content": ""}));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                if !text_parts.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": text_parts.join("\n")
                    }));
                    text_parts.clear();
                }
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let default_input = json!({});
                let input = block.get("input").unwrap_or(&default_input);
                let arguments = serde_json::to_string(input).unwrap_or_default();
                out.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments
                }));
            }
            // A thinking block carrying our envelope is a reasoning item on the
            // way home. Anything else in that signature slot — a genuine
            // Anthropic signature, another proxy's envelope — decodes to None
            // and the block is dropped, which is what the client would expect
            // of history the backend never produced.
            Some("thinking") => {
                let Some(replay) = block
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .and_then(decode_reasoning_signature)
                else {
                    continue;
                };
                // Reasoning must sit ahead of the text it preceded, so flush
                // first rather than letting the message swallow the ordering.
                if !text_parts.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": text_parts.join("\n")
                    }));
                    text_parts.clear();
                }
                out.push(reasoning_input_item(replay));
            }
            _ => {}
        }
    }

    if !text_parts.is_empty() {
        out.push(json!({
            "type": "message",
            "role": "assistant",
            "content": text_parts.join("\n")
        }));
    }
}

fn translate_user_message(msg: &Value, out: &mut Vec<Value>) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"role": "user", "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            out.push(json!({"role": "user", "content": ""}));
            return;
        }
    };

    // Single tool_result block → tool message.
    if content.len() == 1 {
        if let Some(block) = content.first() {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let result_content = extract_tool_result_text(block);
                let is_error = block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": if is_error { format!("Error: {result_content}") } else { result_content.to_string() }
                }));
                return;
            }
        }
    }

    // Mixed content: text blocks + tool_result blocks.
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_results: Vec<&Value> = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_result") => {
                tool_results.push(block);
            }
            _ => {}
        }
    }

    if !text_parts.is_empty() {
        out.push(json!({
            "role": "user",
            "content": text_parts.join("\n")
        }));
    }

    for tr in tool_results {
        let tool_use_id = tr.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
        let result_content = extract_tool_result_text(tr);
        let is_error = tr
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        out.push(json!({
            "role": "tool",
            "tool_call_id": tool_use_id,
            "content": if is_error { format!("Error: {result_content}") } else { result_content.to_string() }
        }));
    }
}

fn translate_assistant_message(msg: &Value, out: &mut Vec<Value>, include_tool_calls: bool) {
    let content = match msg.get("content") {
        Some(Value::String(s)) => {
            out.push(json!({"role": "assistant", "content": s}));
            return;
        }
        Some(Value::Null) => {
            out.push(json!({"role": "assistant", "content": ""}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => {
            out.push(json!({"role": "assistant", "content": ""}));
            return;
        }
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let default_input = json!({});
                let input = block.get("input").unwrap_or(&default_input);
                let arguments = serde_json::to_string(input).unwrap_or_default();
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }));
            }
            _ => {}
        }
    }

    let mut assistant_msg = json!({"role": "assistant"});
    if text_parts.is_empty() {
        assistant_msg["content"] = json!("");
    } else {
        assistant_msg["content"] = json!(text_parts.join("\n"));
    }
    if include_tool_calls && !tool_calls.is_empty() {
        assistant_msg["tool_calls"] = json!(tool_calls);
    }

    out.push(assistant_msg);
}

// ---------------------------------------------------------------------------
// Response translation: OpenAI → Anthropic (non-streaming)
// ---------------------------------------------------------------------------

async fn handle_buffered_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
) -> Response {
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if status != StatusCode::OK {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        tracing::warn!(
            event = "local_model_upstream_error",
            status = status.as_u16(),
            body = %body_text,
            "local model upstream returned error"
        );
        return Response::builder()
            .status(status)
            .body(Body::from(body_text))
            .expect("static response");
    }

    let openai_text = match upstream_resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                event = "local_model_response_parse_error",
                error = %e,
                "failed to read upstream response body"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("failed to read upstream response"))
                .expect("static response");
        }
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

    let anthropic_response = openai_to_anthropic_response(&openai_body, original);

    let body_bytes = match serde_json::to_vec(&anthropic_response) {
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

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .expect("static response")
}

async fn handle_buffered_responses_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
) -> Response {
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if status != StatusCode::OK {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        tracing::warn!(
            event = "local_model_upstream_error",
            status = status.as_u16(),
            body = %body_text,
            "local model upstream returned error"
        );
        return Response::builder()
            .status(status)
            .body(Body::from(body_text))
            .expect("static response");
    }

    let responses_text = match upstream_resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                event = "local_model_response_parse_error",
                error = %e,
                "failed to read upstream responses stream"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("failed to read upstream response"))
                .expect("static response");
        }
    };

    let mut current_event: Option<String> = None;
    let mut current_data: Vec<String> = Vec::new();
    let mut assistant_text = String::new();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;

    let mut flush_frame = |event_name: Option<&str>, data: &str| {
        if data.trim().is_empty() {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match event_name {
            Some("response.output_text.delta") | Some("output_text.delta") => {
                if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str()) {
                    assistant_text.push_str(delta);
                }
            }
            Some("response.output_text.done") | Some("output_text.done") => {
                if let Some(text) = chunk
                    .get("text")
                    .and_then(|v| v.as_str())
                    .or_else(|| chunk.get("delta").and_then(|v| v.as_str()))
                {
                    if assistant_text.is_empty() {
                        assistant_text.push_str(text);
                    }
                }
            }
            Some("response.completed") => {
                if let Some(usage) = chunk.get("response").and_then(|v| v.get("usage")) {
                    if let Some(tokens) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        input_tokens = tokens;
                    }
                    if let Some(tokens) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        output_tokens = tokens;
                    }
                }
            }
            _ => {}
        }
    };

    for line in responses_text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            let data = current_data.join("\n");
            flush_frame(current_event.as_deref(), &data);
            current_event = None;
            current_data.clear();
            continue;
        }

        if let Some(event) = line.strip_prefix("event:") {
            current_event = Some(event.trim().to_string());
            continue;
        }

        if let Some(data) = line.strip_prefix("data:") {
            current_data.push(data.trim_start().to_string());
            continue;
        }
    }
    let data = current_data.join("\n");
    flush_frame(current_event.as_deref(), &data);

    let model_name = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let anthropic_response = json!({
        "type": "message",
        "role": "assistant",
        "model": model_name,
        "content": [{
            "type": "text",
            "text": assistant_text
        }],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    });

    let body_bytes = match serde_json::to_vec(&anthropic_response) {
        Ok(b) => Bytes::from(b),
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

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .expect("static response")
}

/// Extract a single text message from Anthropic request body for mimo run.
fn extract_text_message(body: &Value) -> String {
    let messages = match body.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return String::new(),
    };

    // Build context from system prompt
    let mut parts: Vec<String> = Vec::new();
    if let Some(system) = body.get("system") {
        match system {
            Value::String(s) => parts.push(format!("[System: {s}]")),
            Value::Array(arr) => {
                let text: String = arr
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    parts.push(format!("[System: {text}]"));
                }
            }
            _ => {}
        }
    }

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        if !content.is_empty() {
            parts.push(format!("[{role}]: {content}"));
        }
    }

    parts.join("\n")
}

/// Handle a request by routing through `mimo run` subprocess.
async fn handle_mimo_run(
    state: AppState,
    original: &Value,
    model_name: &str,
    mimo_model: &str,
) -> Response {
    let is_stream = original
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let message = extract_text_message(original);
    if message.is_empty() {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("no message content to send"))
            .expect("static response");
    }

    tracing::info!(
        event = "mimo_run_route",
        model = %model_name,
        mimo_model = %mimo_model,
        stream = is_stream,
        message_len = message.len(),
        "routing through mimo run"
    );

    let _max_tokens = original
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096);

    // Build mimo run command
    let mut cmd = std::process::Command::new("mimo");
    cmd.arg("run")
        .arg("-m")
        .arg(mimo_model)
        .arg("--format")
        .arg("json")
        .arg(&message);

    // Run in a blocking thread to avoid blocking the async runtime
    let output = match tokio::task::spawn_blocking(move || cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(
                event = "mimo_run_error",
                error = %e,
                "failed to execute mimo run"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("mimo run failed: {e}")))
                .expect("static response");
        }
        Err(e) => {
            tracing::warn!(
                event = "mimo_run_error",
                error = %e,
                "mimo run task failed"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("mimo run task failed"))
                .expect("static response");
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            event = "mimo_run_error",
            status = %output.status,
            stderr = %stderr,
            "mimo run exited with error"
        );
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from(format!("mimo run failed: {stderr}")))
            .expect("static response");
    }

    // Parse JSON lines output
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut text_parts: Vec<String> = Vec::new();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<Value>(line) {
            match event.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = event
                        .get("part")
                        .and_then(|p| p.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        text_parts.push(text.to_string());
                    }
                }
                Some("step_finish") => {
                    if let Some(tokens) = event.get("part").and_then(|p| p.get("tokens")) {
                        input_tokens = tokens.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
                        output_tokens = tokens.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                }
                _ => {}
            }
        }
    }

    let response_text = text_parts.join("");

    if is_stream {
        // For streaming, emit a single SSE message with the full response
        let raw_id = uuid::Uuid::new_v4().to_string().replace('-', "");
        let msg_id = format!("msg_{}", &raw_id[..raw_id.len().min(24)]);

        let events = format!(
            "event: message_start\ndata: {}\n\n\
             event: content_block_start\ndata: {}\n\n\
             event: content_block_delta\ndata: {}\n\n\
             event: content_block_stop\ndata: {}\n\n\
             event: message_delta\ndata: {}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            json!({"type":"message_start","message":{"id":msg_id,"type":"message","role":"assistant","content":[],"model":model_name,"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":input_tokens,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":response_text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":output_tokens}}),
        );

        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from(events))
            .expect("static response")
    } else {
        // Non-streaming: return full Anthropic response
        let raw_id = uuid::Uuid::new_v4().to_string().replace('-', "");
        let msg_id = format!("msg_{}", &raw_id[..raw_id.len().min(24)]);

        let mut content: Vec<Value> = Vec::new();
        if !response_text.is_empty() {
            content.push(json!({"type":"text","text":response_text}));
        }

        let response = json!({
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "content": content,
            "model": model_name,
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens
            }
        });

        let body_bytes = serde_json::to_vec(&response).unwrap_or_default();
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(body_bytes))
            .expect("static response")
    }
}

fn openai_to_anthropic_response(openai: &Value, original: &Value) -> Value {
    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let choice = openai
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first());

    let message = choice.and_then(|c| c.get("message"));

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|r| r.as_str())
        .unwrap_or("stop");

    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        _ => "end_turn",
    };

    let mut content: Vec<Value> = Vec::new();

    if let Some(msg) = message {
        // Handle reasoning_content (thinking tokens from models like Qwen).
        if let Some(reasoning) = msg.get("reasoning_content").and_then(|v| v.as_str()) {
            if !reasoning.is_empty() {
                content.push(json!({"type": "thinking", "thinking": reasoning}));
            }
        }

        if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
        }

        if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let arguments = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let input: Value = serde_json::from_str(arguments).unwrap_or(json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input
                }));
            }
        }
    }

    let default_usage = json!({});
    let usage = openai.get("usage").unwrap_or(&default_usage);
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let raw_id = uuid::Uuid::new_v4().to_string().replace('-', "");
    let msg_id = format!("msg_{}", &raw_id[..raw_id.len().min(24)]);

    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": original_model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    })
}

// ---------------------------------------------------------------------------
// Streaming response translation: OpenAI SSE → Anthropic SSE
// ---------------------------------------------------------------------------

async fn handle_streaming_response(upstream_resp: reqwest::Response, original: &Value) -> Response {
    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let stream = upstream_resp.bytes_stream();
    let translated_stream = translate_openai_stream_to_anthropic(stream, original_model);

    let body = axum::body::Body::from_stream(translated_stream);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .expect("static response")
}

struct StreamTranslator {
    model: String,
    content_block_index: usize,
    started: bool,
    in_text_block: bool,
    in_tool_block: bool,
    in_thinking_block: bool,
    current_tool_id: String,
    current_tool_name: String,
    total_output_tokens: u64,
    saw_tool_use: bool,
    /// Identity of the reasoning item currently streaming, assembled from the
    /// `output_item.added`/`.done` pair that describes it.
    pending_reasoning: PendingReasoning,
}

impl StreamTranslator {
    fn new(model: String) -> Self {
        Self {
            model,
            content_block_index: 0,
            started: false,
            in_text_block: false,
            in_tool_block: false,
            in_thinking_block: false,
            current_tool_id: String::new(),
            current_tool_name: String::new(),
            total_output_tokens: 0,
            saw_tool_use: false,
            pending_reasoning: PendingReasoning::default(),
        }
    }

    #[cfg(test)]
    fn process_line(&mut self, line: &str) -> Vec<String> {
        self.process_frame(None, line)
    }

    fn process_frame(&mut self, event_name: Option<&str>, data: &str) -> Vec<String> {
        let mut events = Vec::new();

        if data.trim().is_empty() || data.trim() == "[DONE]" {
            if data.trim() == "[DONE]"
                && (self.in_text_block || self.in_tool_block || self.in_thinking_block)
            {
                if self.in_thinking_block {
                    events.push(self.emit_content_block_stop());
                    self.in_thinking_block = false;
                }
                if self.in_text_block {
                    events.push(self.emit_content_block_stop());
                    self.in_text_block = false;
                }
                if self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                }
                events.push(self.emit_message_delta("end_turn"));
                events.push(self.emit_message_stop());
            }
            return events;
        }

        if let Some(name) = event_name {
            if name.starts_with("response.") || name.starts_with("output_") {
                return self.process_responses_frame(name, data);
            }
        }

        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return events,
        };

        self.process_chat_chunk(chunk)
    }

    fn process_chat_chunk(&mut self, chunk: Value) -> Vec<String> {
        let mut events = Vec::new();

        if !self.started {
            events.push(self.emit_message_start());
            self.started = true;
        }

        let delta = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"));

        let finish_reason = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("finish_reason"))
            .and_then(|r| r.as_str());

        if let Some(usage) = chunk.get("usage") {
            if let Some(tokens) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                self.total_output_tokens = tokens;
            }
        }

        if let Some(delta) = delta {
            // Handle reasoning_content (thinking tokens from models like Qwen).
            if let Some(thinking) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                if !self.in_thinking_block && !self.in_text_block && !self.in_tool_block {
                    events.push(self.emit_content_block_start_thinking());
                    self.in_thinking_block = true;
                }
                if self.in_text_block {
                    events.push(self.emit_content_block_stop());
                    self.in_text_block = false;
                    self.content_block_index += 1;
                    events.push(self.emit_content_block_start_thinking());
                    self.in_thinking_block = true;
                }
                if self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                    self.content_block_index += 1;
                    events.push(self.emit_content_block_start_thinking());
                    self.in_thinking_block = true;
                }
                events.push(self.emit_thinking_delta(thinking));
            }

            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                if !self.in_text_block && !self.in_tool_block {
                    if self.in_thinking_block {
                        events.push(self.emit_content_block_stop());
                        self.in_thinking_block = false;
                        self.content_block_index += 1;
                    }
                    events.push(self.emit_content_block_start_text());
                    self.in_text_block = true;
                }
                if self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                    self.content_block_index += 1;
                    events.push(self.emit_content_block_start_text());
                    self.in_text_block = true;
                }
                events.push(self.emit_text_delta(text));
            }

            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        if self.in_thinking_block {
                            events.push(self.emit_content_block_stop());
                            self.in_thinking_block = false;
                            self.content_block_index += 1;
                        }
                        if self.in_text_block {
                            events.push(self.emit_content_block_stop());
                            self.in_text_block = false;
                            self.content_block_index += 1;
                        }
                        if self.in_tool_block {
                            events.push(self.emit_content_block_stop());
                            self.in_tool_block = false;
                            self.content_block_index += 1;
                        }

                        self.current_tool_id = id.to_string();
                        self.current_tool_name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();

                        events.push(self.emit_content_block_start_tool(
                            &self.current_tool_id.clone(),
                            &self.current_tool_name.clone(),
                        ));
                        self.in_tool_block = true;
                    }

                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        if !args.is_empty() {
                            if !self.in_tool_block {
                                events.push(self.emit_content_block_start_tool(
                                    &self.current_tool_id.clone(),
                                    &self.current_tool_name.clone(),
                                ));
                                self.in_tool_block = true;
                            }
                            events.push(self.emit_input_json_delta(args));
                        }
                    }
                }
            }
        }

        if let Some(reason) = finish_reason {
            if self.in_thinking_block {
                events.push(self.emit_content_block_stop());
                self.in_thinking_block = false;
            }
            if self.in_text_block {
                events.push(self.emit_content_block_stop());
                self.in_text_block = false;
            }
            if self.in_tool_block {
                events.push(self.emit_content_block_stop());
                self.in_tool_block = false;
            }

            let stop_reason = match reason {
                "stop" => "end_turn",
                "tool_calls" => "tool_use",
                "length" => "max_tokens",
                _ => "end_turn",
            };
            events.push(self.emit_message_delta(stop_reason));
            events.push(self.emit_message_stop());
        }

        events
    }

    fn process_responses_frame(&mut self, event_name: &str, data: &str) -> Vec<String> {
        let mut events = Vec::new();

        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return events,
        };

        if !self.started && event_name == "response.created" {
            if let Some(model) = chunk
                .get("response")
                .and_then(|resp| resp.get("model"))
                .and_then(|v| v.as_str())
            {
                self.model = model.to_string();
            }
        }

        if !self.started {
            events.push(self.emit_message_start());
            self.started = true;
        }

        match event_name {
            "response.output_text.delta" | "output_text.delta" => {
                let delta = chunk.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if !delta.is_empty() {
                    if !self.in_text_block && !self.in_tool_block {
                        if self.in_thinking_block {
                            events.push(self.emit_content_block_stop());
                            self.in_thinking_block = false;
                            self.content_block_index += 1;
                        }
                        events.push(self.emit_content_block_start_text());
                        self.in_text_block = true;
                    }
                    events.push(self.emit_text_delta(delta));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str()) {
                    if !delta.is_empty() {
                        if !self.in_thinking_block {
                            if self.in_text_block {
                                events.push(self.emit_content_block_stop());
                                self.in_text_block = false;
                                self.content_block_index += 1;
                            }
                            if self.in_tool_block {
                                events.push(self.emit_content_block_stop());
                                self.in_tool_block = false;
                                self.content_block_index += 1;
                            }
                            events.push(self.emit_content_block_start_thinking());
                            self.in_thinking_block = true;
                        }
                        events.push(self.emit_thinking_delta(delta));
                    }
                }
            }
            "response.reasoning_summary_part.added" => {
                // Part boundary: close the current thinking block so the next
                // summary part starts a fresh one.
                if self.in_thinking_block {
                    events.push(self.emit_content_block_stop());
                    self.in_thinking_block = false;
                    self.content_block_index += 1;
                }
            }
            "response.output_item.added" => {
                let item = chunk.get("item");
                let item_type = item.and_then(|i| i.get("type")).and_then(|t| t.as_str());
                if item_type == Some("function_call") {
                    if self.in_thinking_block {
                        events.push(self.emit_content_block_stop());
                        self.in_thinking_block = false;
                        self.content_block_index += 1;
                    }
                    if self.in_text_block {
                        events.push(self.emit_content_block_stop());
                        self.in_text_block = false;
                        self.content_block_index += 1;
                    }
                    if self.in_tool_block {
                        events.push(self.emit_content_block_stop());
                        self.in_tool_block = false;
                        self.content_block_index += 1;
                    }
                    // `call_id` is what must round-trip back as
                    // function_call_output; fall back to `id` if absent.
                    self.current_tool_id = item
                        .and_then(|i| i.get("call_id").or_else(|| i.get("id")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.current_tool_name = item
                        .and_then(|i| i.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    events.push(self.emit_content_block_start_tool(
                        &self.current_tool_id.clone(),
                        &self.current_tool_name.clone(),
                    ));
                    self.in_tool_block = true;
                    self.saw_tool_use = true;
                }
                // A reasoning item may announce its id here and carry the blob
                // on `.done`, so start assembling as soon as it appears.
                if item_type == Some("reasoning") {
                    if let Some(item) = item {
                        self.pending_reasoning.capture(item);
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if self.in_tool_block {
                    if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str()) {
                        if !delta.is_empty() {
                            events.push(self.emit_input_json_delta(delta));
                        }
                    }
                }
            }
            "response.output_item.done" => {
                let item_type = chunk
                    .get("item")
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str());
                if item_type == Some("function_call") && self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                    self.content_block_index += 1;
                }
                // The reasoning item is complete: seal its identity into the
                // thinking block's signature so the client hands it back next
                // turn. Without a usable pair there is nothing to replay and
                // the block stays a plain summary.
                if item_type == Some("reasoning") {
                    if let Some(item) = chunk.get("item") {
                        self.pending_reasoning.capture(item);
                    }
                    let signature = self
                        .pending_reasoning
                        .replay()
                        .as_ref()
                        .and_then(encode_reasoning_signature);
                    self.pending_reasoning.reset();
                    if let Some(signature) = signature {
                        if !self.in_thinking_block {
                            // Reasoning summaries can be off entirely, in which
                            // case no block was ever opened. Open an empty one
                            // rather than drop the only copy of the item — but
                            // only once whatever else is open has been closed,
                            // or two blocks would share an index.
                            if self.in_text_block {
                                events.push(self.emit_content_block_stop());
                                self.in_text_block = false;
                                self.content_block_index += 1;
                            }
                            if self.in_tool_block {
                                events.push(self.emit_content_block_stop());
                                self.in_tool_block = false;
                                self.content_block_index += 1;
                            }
                            events.push(self.emit_content_block_start_thinking());
                            self.in_thinking_block = true;
                        }
                        events.push(self.emit_signature_delta(&signature));
                        events.push(self.emit_content_block_stop());
                        self.in_thinking_block = false;
                        self.content_block_index += 1;
                    }
                }
            }
            "response.completed" => {
                if let Some(usage) = chunk.get("response").and_then(|v| v.get("usage")) {
                    if let Some(tokens) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        self.total_output_tokens = tokens;
                    }
                    // Ground-truth cache effectiveness: how many input tokens
                    // the codex backend served from its prompt cache this turn.
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cached = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let hit_pct = if input_tokens > 0 {
                        (cached as f64 / input_tokens as f64) * 100.0
                    } else {
                        0.0
                    };
                    tracing::debug!(
                        event = "codex_cache_usage",
                        input_tokens,
                        cached_tokens = cached,
                        fresh_tokens = input_tokens.saturating_sub(cached),
                        cache_hit_pct = format!("{hit_pct:.1}"),
                        "codex prompt-cache effectiveness for this turn"
                    );
                }
                if self.in_thinking_block {
                    events.push(self.emit_content_block_stop());
                    self.in_thinking_block = false;
                }
                if self.in_text_block {
                    events.push(self.emit_content_block_stop());
                    self.in_text_block = false;
                }
                if self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                }
                let stop_reason = if self.saw_tool_use {
                    "tool_use"
                } else {
                    "end_turn"
                };
                events.push(self.emit_message_delta(stop_reason));
                events.push(self.emit_message_stop());
            }
            "response.failed" => {
                if self.in_thinking_block {
                    events.push(self.emit_content_block_stop());
                    self.in_thinking_block = false;
                }
                if self.in_text_block {
                    events.push(self.emit_content_block_stop());
                    self.in_text_block = false;
                }
                if self.in_tool_block {
                    events.push(self.emit_content_block_stop());
                    self.in_tool_block = false;
                }
            }
            "response.incomplete" => {
                if let Some(reason) = chunk
                    .get("response")
                    .and_then(|v| v.get("incomplete_details"))
                    .and_then(|v| v.get("reason"))
                    .and_then(|v| v.as_str())
                {
                    self.total_output_tokens = self.total_output_tokens.max(0);
                    if self.in_thinking_block {
                        events.push(self.emit_content_block_stop());
                        self.in_thinking_block = false;
                    }
                    if self.in_text_block {
                        events.push(self.emit_content_block_stop());
                        self.in_text_block = false;
                    }
                    if self.in_tool_block {
                        events.push(self.emit_content_block_stop());
                        self.in_tool_block = false;
                    }
                    let stop_reason = match reason {
                        "max_output_tokens" => "max_tokens",
                        _ => "end_turn",
                    };
                    events.push(self.emit_message_delta(stop_reason));
                    events.push(self.emit_message_stop());
                }
            }
            _ => {}
        }

        events
    }

    fn emit_message_start(&self) -> String {
        let raw = uuid::Uuid::new_v4().to_string().replace('-', "");
        let msg_id = format!("msg_{}", &raw[..raw.len().min(24)]);
        let event = json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        format!("event: message_start\ndata: {event}\n\n")
    }

    fn emit_content_block_start_text(&mut self) -> String {
        let event = json!({
            "type": "content_block_start",
            "index": self.content_block_index,
            "content_block": {"type": "text", "text": ""}
        });
        format!("event: content_block_start\ndata: {event}\n\n")
    }

    fn emit_content_block_start_thinking(&mut self) -> String {
        let event = json!({
            "type": "content_block_start",
            "index": self.content_block_index,
            "content_block": {"type": "thinking", "thinking": ""}
        });
        format!("event: content_block_start\ndata: {event}\n\n")
    }

    fn emit_content_block_start_tool(&mut self, id: &str, name: &str) -> String {
        let event = json!({
            "type": "content_block_start",
            "index": self.content_block_index,
            "content_block": {"type": "tool_use", "id": id, "name": name}
        });
        format!("event: content_block_start\ndata: {event}\n\n")
    }

    fn emit_text_delta(&self, text: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "text_delta", "text": text}
        });
        format!("event: content_block_delta\ndata: {event}\n\n")
    }

    fn emit_thinking_delta(&self, thinking: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "thinking_delta", "thinking": thinking}
        });
        format!("event: content_block_delta\ndata: {event}\n\n")
    }

    /// Closes a thinking block by handing the client the reasoning envelope it
    /// will echo back to us next turn.
    fn emit_signature_delta(&self, signature: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "signature_delta", "signature": signature}
        });
        format!("event: content_block_delta\ndata: {event}\n\n")
    }

    fn emit_input_json_delta(&self, json_str: &str) -> String {
        let event = json!({
            "type": "content_block_delta",
            "index": self.content_block_index,
            "delta": {"type": "input_json_delta", "partial_json": json_str}
        });
        format!("event: content_block_delta\ndata: {event}\n\n")
    }

    fn emit_content_block_stop(&self) -> String {
        let event = json!({
            "type": "content_block_stop",
            "index": self.content_block_index
        });
        format!("event: content_block_stop\ndata: {event}\n\n")
    }

    fn emit_message_delta(&self, stop_reason: &str) -> String {
        let event = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": self.total_output_tokens}
        });
        format!("event: message_delta\ndata: {event}\n\n")
    }

    fn emit_message_stop(&self) -> String {
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string()
    }
}

fn translate_openai_stream_to_anthropic(
    stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::StreamExt;

    let mut translator = StreamTranslator::new(model);
    let mut buffer = String::new();
    let mut current_event: Option<String> = None;
    let mut current_data: Vec<String> = Vec::new();

    stream.filter_map(move |chunk| {
        let translated = match chunk {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                buffer.push_str(&text);

                let mut output = Vec::new();
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.is_empty() {
                        let data = current_data.join("\n");
                        let events = translator.process_frame(current_event.as_deref(), &data);
                        for event in events {
                            output.extend_from_slice(event.as_bytes());
                        }
                        current_event = None;
                        current_data.clear();
                        continue;
                    }

                    if let Some(event) = line.strip_prefix("event:") {
                        current_event = Some(event.trim().to_string());
                        continue;
                    }

                    if let Some(data) = line.strip_prefix("data:") {
                        current_data.push(data.trim_start().to_string());
                        continue;
                    }
                }

                if output.is_empty() {
                    None
                } else {
                    Some(Ok(bytes::Bytes::from(output)))
                }
            }
            Err(e) => Some(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))),
        };
        async { translated }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

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

    #[test]
    fn anthropic_to_openai_responses_request_serializes_tool_turns() {
        let input = json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "stream": false,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "file1\nfile2"}
                ]}
            ]
        });
        let output = anthropic_to_openai_responses_request(&input, true).unwrap();
        assert_eq!(output["instructions"], "You are helpful.");
        assert_eq!(output["input"][0]["type"], "message");
        assert_eq!(output["input"][0]["role"], "user");
        assert_eq!(output["input"][1]["type"], "function_call");
        assert_eq!(output["input"][1]["call_id"], "call_1");
        assert_eq!(output["input"][1]["name"], "bash");
        assert_eq!(output["input"][2]["type"], "function_call_output");
        assert_eq!(output["input"][2]["call_id"], "call_1");
        assert_eq!(output["input"][2]["output"], "file1\nfile2");
        assert_eq!(output["max_output_tokens"], 100);
        assert!(!output["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["role"] == "tool"));
        assert!(!output["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["role"] == "system"));
    }

    /// Anthropic sends `tool_result.content` as a plain string *or* a list of
    /// blocks. Reading it with `as_str()` blanked every array-shaped result,
    /// so the model saw each tool call answered by an empty string.
    #[test]
    fn anthropic_to_openai_responses_request_preserves_block_shaped_tool_result() {
        let input = json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": [
                        {"type": "text", "text": "file1"},
                        {"type": "image", "source": {"type": "base64", "data": "aGk="}},
                        {"type": "text", "text": "file2"}
                    ]}
                ]}
            ]
        });
        let output = anthropic_to_openai_responses_request(&input, true).unwrap();
        assert_eq!(output["input"][1]["type"], "function_call_output");
        assert_eq!(output["input"][1]["call_id"], "call_1");
        // Text survives and keeps its order; the image block is skipped.
        assert_eq!(output["input"][1]["output"], "file1\nfile2");
    }

    #[test]
    fn anthropic_to_openai_responses_request_marks_block_shaped_tool_error() {
        let input = json!({
            "model": "claude-codex-5.5",
            "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "is_error": true, "content": [
                        {"type": "text", "text": "boom"}
                    ]}
                ]}
            ]
        });
        let output = anthropic_to_openai_responses_request(&input, true).unwrap();
        assert_eq!(output["input"][1]["output"], "Error: boom");
    }

    /// Same bug, Chat Completions path — both the single-block fast path and
    /// the mixed text+tool_result path read `content` with `as_str()`.
    #[test]
    fn anthropic_to_openai_request_preserves_block_shaped_tool_result() {
        let sole = json!({
            "model": "local",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": [
                        {"type": "text", "text": "only"}
                    ]}
                ]}
            ]
        });
        let output = anthropic_to_openai_request(&sole, true, true).unwrap();
        let tool_msg = output["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool message");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
        assert_eq!(tool_msg["content"], "only");

        let mixed = json!({
            "model": "local",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "context"},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": [
                        {"type": "text", "text": "mixed"}
                    ]}
                ]}
            ]
        });
        let output = anthropic_to_openai_request(&mixed, true, true).unwrap();
        let tool_msg = output["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool message");
        assert_eq!(tool_msg["tool_call_id"], "call_2");
        assert_eq!(tool_msg["content"], "mixed");
    }

    /// Pull the signature out of whatever SSE the translator emitted.
    fn signature_from_stream(sse: &str) -> Option<String> {
        for line in sse.lines() {
            // Skip the `event:` and blank lines that frame each SSE record.
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            if event["delta"]["type"] == "signature_delta" {
                return event["delta"]["signature"].as_str().map(String::from);
            }
        }
        None
    }

    fn drive(t: &mut StreamTranslator, frames: &[(&str, &str)]) -> String {
        let mut all = String::new();
        for (event, data) in frames {
            for e in t.process_frame(Some(event), data) {
                all.push_str(&e);
            }
        }
        all
    }

    /// The whole point of the envelope: a reasoning item leaves in the thinking
    /// block's signature and comes back from the client's own history, with no
    /// proxy-side state in between.
    #[test]
    fn reasoning_envelope_round_trips_through_the_client() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[
                (
                    "response.reasoning_summary_text.delta",
                    r#"{"delta":"weighing it"}"#,
                ),
                (
                    "response.output_item.done",
                    r#"{"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC_BLOB"}}"#,
                ),
            ],
        );
        assert!(sse.contains(r#""thinking":"weighing it""#));
        let signature = signature_from_stream(&sse).expect("signature delta emitted");

        // Next turn: the client echoes that thinking block back verbatim.
        let request = json!({
            "model": "claude-codex-5.6",
            "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "weighing it", "signature": signature},
                    {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "ok"}
                ]}
            ]
        });
        let out = anthropic_to_openai_responses_request(&request, true).unwrap();
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "ENC_BLOB");
        assert_eq!(input[0]["summary"], json!([]));
        // Reasoning has to stay ahead of the call it preceded.
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
    }

    /// With reasoning summaries disabled no thinking block is ever opened by a
    /// summary delta, so the item's only carrier is a signature-only block.
    #[test]
    fn reasoning_envelope_survives_when_summaries_are_disabled() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_2","summary":[],"encrypted_content":"ENC_2"}}"#,
            )],
        );
        assert!(sse.contains(r#""type":"thinking","thinking":"""#));
        let signature = signature_from_stream(&sse).expect("signature emitted without summary");

        let request = json!({
            "model": "claude-codex-5.6",
            "max_tokens": 100,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": signature}
            ]}]
        });
        let out = anthropic_to_openai_responses_request(&request, true).unwrap();
        assert_eq!(out["input"][0]["type"], "reasoning");
        assert_eq!(out["input"][0]["encrypted_content"], "ENC_2");
    }

    /// A signature-only block must not collide with a block already open, or
    /// two content blocks share an index and the client sees a torn stream.
    #[test]
    fn signature_only_block_closes_open_text_first() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[
                ("response.output_text.delta", r#"{"delta":"partial"}"#),
                (
                    "response.output_item.done",
                    r#"{"item":{"type":"reasoning","id":"rs_4","summary":[],"encrypted_content":"ENC_4"}}"#,
                ),
            ],
        );
        assert!(signature_from_stream(&sse).is_some());
        let indices: Vec<i64> = sse
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|p| serde_json::from_str::<Value>(p).ok())
            .filter(|e| e["type"] == "content_block_start")
            .filter_map(|e| e["index"].as_i64())
            .collect();
        let mut unique = indices.clone();
        unique.dedup();
        assert_eq!(indices, unique, "two content blocks opened on one index");
    }

    /// An id with no blob (or the reverse) is not replayable, so no signature
    /// is minted and the summary stays a plain thinking block.
    #[test]
    fn incomplete_reasoning_item_emits_no_signature() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_3","summary":[]}}"#,
            )],
        );
        assert!(signature_from_stream(&sse).is_none());
    }

    /// Thinking blocks we did not mint must never become reasoning items: a
    /// real Anthropic signature, or none at all, is dropped on the way out.
    #[test]
    fn foreign_thinking_blocks_are_dropped_not_replayed() {
        for block in [
            json!({"type": "thinking", "thinking": "hm", "signature": "ErUBCkYIBRgCKkDzS1nT"}),
            json!({"type": "thinking", "thinking": "hm"}),
            json!({"type": "redacted_thinking", "data": "opaque"}),
        ] {
            let request = json!({
                "model": "claude-codex-5.6",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    block, json!({"type": "text", "text": "answer"})
                ]}]
            });
            let out = anthropic_to_openai_responses_request(&request, true).unwrap();
            let input = out["input"].as_array().unwrap();
            assert!(
                !input.iter().any(|i| i["type"] == "reasoning"),
                "foreign thinking block became a reasoning item"
            );
            assert_eq!(input[0]["content"], "answer");
        }
    }

    /// Reasoning now travels with the conversation, so a `/model` switch cannot
    /// leak one model's items into another's request — there is no shared store
    /// to leak through. Only what the client echoes is replayed.
    #[test]
    fn reasoning_does_not_leak_across_models() {
        let mut first = StreamTranslator::new("model-a".to_string());
        let sse = drive(
            &mut first,
            &[(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_a","summary":[],"encrypted_content":"ENC_A"}}"#,
            )],
        );
        let signature = signature_from_stream(&sse).unwrap();

        // A turn on another model that does not echo the block gets nothing.
        let clean = json!({
            "model": "model-b",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "next"}]
        });
        let out = anthropic_to_openai_responses_request(&clean, true).unwrap();
        assert!(!out["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "reasoning"));

        // And what is echoed is carried by the request itself, not a cache.
        let echoed = json!({
            "model": "model-a",
            "max_tokens": 100,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": signature}
            ]}]
        });
        let out = anthropic_to_openai_responses_request(&echoed, true).unwrap();
        assert_eq!(out["input"][0]["encrypted_content"], "ENC_A");
    }

    #[test]
    fn anthropic_to_openai_responses_request_forwards_tools() {
        let input = json!({
            "model": "claude-codex-5.6",
            "max_tokens": 100,
            "stream": false,
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [
                {"name": "Bash", "description": "Run a command", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}},
                {"name": "Read", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {"type": "auto"}
        });
        let output = anthropic_to_openai_responses_request(&input, true).unwrap();
        let tools = output["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "Bash");
        assert_eq!(tools[0]["description"], "Run a command");
        assert_eq!(
            tools[0]["parameters"]["properties"]["command"]["type"],
            "string"
        );
        // Flat Responses-API shape, not nested under `function`.
        assert!(tools[0].get("function").is_none());
        assert_eq!(tools[1]["name"], "Read");
        assert_eq!(output["tool_choice"], "auto");
    }

    #[test]
    fn anthropic_to_openai_responses_request_sends_codex_defaults() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert_eq!(output["tool_choice"], "auto");
        assert_eq!(output["parallel_tool_calls"], false);
        assert_eq!(output["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn stream_translator_translates_reasoning_summary_to_thinking() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let mut all = String::new();
        for (event, data) in [
            (
                "response.reasoning_summary_text.delta",
                r#"{"delta":"Consider the"}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"delta":" edge cases"}"#,
            ),
            ("response.reasoning_summary_part.added", r#"{}"#),
            ("response.output_text.delta", r#"{"delta":"Answer"}"#),
            (
                "response.completed",
                r#"{"response":{"usage":{"output_tokens":5}}}"#,
            ),
        ] {
            for e in t.process_frame(Some(event), data) {
                all.push_str(&e);
            }
        }
        assert!(all.contains(r#""type":"thinking","thinking":"""#));
        assert!(all.contains(r#""thinking":"Consider the""#));
        assert!(all.contains(r#""text":"Answer""#));
        assert!(all.contains(r#""stop_reason":"end_turn""#));
    }

    #[test]
    fn reasoning_item_id_and_blob_may_arrive_on_separate_events() {
        // `added` announces the id, `done` carries the blob; neither event is
        // complete on its own but the pair is.
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[
                (
                    "response.output_item.added",
                    r#"{"item":{"type":"reasoning","id":"rs_split","summary":[]}}"#,
                ),
                (
                    "response.output_item.done",
                    r#"{"item":{"type":"reasoning","summary":[],"encrypted_content":"ENC_SPLIT"}}"#,
                ),
            ],
        );
        let signature = signature_from_stream(&sse).expect("signature from the merged pair");
        let request = json!({
            "model": "claude-codex-5.6",
            "max_tokens": 100,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": signature}
            ]}]
        });
        let out = anthropic_to_openai_responses_request(&request, true).unwrap();
        assert_eq!(out["input"][0]["id"], "rs_split");
        assert_eq!(out["input"][0]["encrypted_content"], "ENC_SPLIT");
    }

    #[test]
    fn injected_recall_block_keeps_front_position_through_translation() {
        // Mimics what the inject engine produces: a recall text block
        // prepended to the first user message. It must stay at the very
        // front of the translated Responses input so the codex prompt-cache
        // prefix is byte-stable across turns.
        let parsed = json!({
            "model": "claude-codex-5.6",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "<<<RECALL>>> prior context digest"},
                    {"type": "text", "text": "build a parser"}
                ]
            }]
        });
        let out = anthropic_to_openai_responses_request(&parsed, false).unwrap();
        let first = &out["input"][0];
        assert_eq!(first["type"], "message");
        assert_eq!(first["role"], "user");
        let text = serde_json::to_string(&first["content"]).unwrap();
        let recall_pos = text.find("<<<RECALL>>>").expect("recall block present");
        let query_pos = text.find("build a parser").expect("query present");
        assert!(recall_pos < query_pos, "recall must precede the user query");
    }

    #[test]
    fn offload_transform_shrinks_large_tool_result_before_translation() {
        // A large tool_result should be offloaded to a digest, and the
        // digest must survive translation into the Responses input.
        let big = "X".repeat(80_000);
        let mut parsed = json!({
            "model": "claude-codex-5.6",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_big", "name": "Bash", "input": {"command": "cat huge.log"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_big", "content": big.clone()}
                ]},
                {"role": "user", "content": "done?"}
            ]
        });
        let cfg = crate::compression::ctx_offload::CtxOffloadConfig { min_bytes: 50_000 };
        let out =
            crate::compression::ctx_offload::offload_anthropic_request(&mut parsed, &cfg, None);
        assert!(out.changed(), "expected a large tool_result to offload");
        let translated = anthropic_to_openai_responses_request(&parsed, false).unwrap();
        let serialized = serde_json::to_string(&translated).unwrap();
        assert!(!serialized.contains(&big), "raw payload must not survive");
        assert!(
            serialized.contains("headroom ctx get"),
            "digest pointer expected"
        );
    }

    #[test]
    fn anthropic_to_openai_responses_request_drops_temperature() {
        let input = json!({
            "model": "claude-codex-5.6",
            "temperature": 1.0,
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert!(output.get("temperature").is_none());
    }

    #[test]
    fn codex_user_agent_matches_cli_format() {
        let ua = codex_user_agent(None);
        assert!(ua.starts_with("codex_cli_rs/"));
        assert!(ua.contains('('));
        let tp = generate_traceparent();
        assert_eq!(tp.len(), 55);
        assert!(tp.starts_with("00-"));
        assert!(tp.ends_with("-01"));
    }

    #[test]
    fn derive_session_uuid_is_stable_and_uuid_shaped() {
        let a = derive_session_uuid("user_abc_session_123");
        let b = derive_session_uuid("user_abc_session_123");
        let c = derive_session_uuid("user_abc_session_456");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|&ch| ch == '-').count(), 4);
    }

    #[test]
    fn anthropic_to_openai_responses_request_maps_thinking_and_cache_key() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}],
            "thinking": {"type": "enabled", "budget_tokens": 10000},
            "metadata": {"user_id": "user_abc_session_123"}
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert_eq!(output["reasoning"]["effort"], "medium");
        assert_eq!(output["prompt_cache_key"], "user_abc_session_123");

        let low = json!({
            "model": "m", "messages": [],
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        });
        let output = anthropic_to_openai_responses_request(&low, false).unwrap();
        assert_eq!(output["reasoning"]["effort"], "low");

        let high = json!({
            "model": "m", "messages": [],
            "thinking": {"type": "enabled", "budget_tokens": 32000}
        });
        let output = anthropic_to_openai_responses_request(&high, false).unwrap();
        assert_eq!(output["reasoning"]["effort"], "high");

        let disabled = json!({
            "model": "m", "messages": [],
            "thinking": {"type": "disabled"}
        });
        let output = anthropic_to_openai_responses_request(&disabled, false).unwrap();
        assert!(output.get("reasoning").is_none());
        assert!(output.get("prompt_cache_key").is_none());
    }

    #[test]
    fn anthropic_to_openai_responses_request_strips_agent_mode_param() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{
                "name": "Agent",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string"},
                        "mode": {"type": "string", "enum": ["default", "plan"]}
                    },
                    "required": ["prompt"]
                }
            }]
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        let props = &output["tools"][0]["parameters"]["properties"];
        assert!(props.get("mode").is_none());
        assert!(props.get("prompt").is_some());
    }

    #[test]
    fn anthropic_to_openai_responses_request_translates_forced_tool_choice() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "Bash"}
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert_eq!(output["tool_choice"]["type"], "function");
        assert_eq!(output["tool_choice"]["name"], "Bash");
    }

    #[test]
    fn anthropic_to_openai_responses_request_omits_tools_when_absent() {
        let input = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let output = anthropic_to_openai_responses_request(&input, false).unwrap();
        assert!(output.get("tools").is_none());
        assert!(output.get("tool_choice").is_none());
    }

    #[test]
    fn stream_translator_translates_responses_function_call_frames() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let mut all = String::new();
        for (event, data) in [
            (
                "response.created",
                r#"{"response":{"model":"gpt-5.6-terra"}}"#,
            ),
            (
                "response.output_item.added",
                r#"{"item":{"type":"function_call","call_id":"call_1","name":"Bash","arguments":""}}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"delta":"{\"command\":"}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"delta":"\"ls\"}"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"function_call","call_id":"call_1","name":"Bash"}}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ),
        ] {
            for e in t.process_frame(Some(event), data) {
                all.push_str(&e);
            }
        }
        assert!(all.contains(r#""type":"tool_use","id":"call_1","name":"Bash""#));
        assert!(all.contains(r#""type":"input_json_delta""#));
        assert!(all.contains(r#"\"command\":"#));
        assert!(all.contains(r#""stop_reason":"tool_use""#));
        assert!(all.contains("message_stop"));
    }

    #[test]
    fn resolve_codex_routing_headers_detects_chatgpt_jwt_auth() {
        let payload = json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-from-jwt",
            }
        });
        let token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
        );
        let temp_dir = tempfile::tempdir().unwrap();
        let auth_file = temp_dir.path().join("auth.json");
        std::fs::write(
            &auth_file,
            json!({
                "tokens": {
                    "access_token": token,
                }
            })
            .to_string(),
        )
        .unwrap();

        let headers = HeaderMap::new();
        let (upstream_headers, is_chatgpt_auth) =
            resolve_codex_routing_headers(&headers, auth_file.to_str());

        assert!(is_chatgpt_auth);
        assert_eq!(
            upstream_headers.get(http::header::AUTHORIZATION),
            Some(&HeaderValue::from_str(&format!("Bearer {}", token)).unwrap())
        );
        assert_eq!(
            upstream_headers
                .get("ChatGPT-Account-ID")
                .and_then(|v| v.to_str().ok()),
            Some("acct-from-jwt")
        );
    }

    #[test]
    fn anthropic_to_openai_simple_text() {
        let input = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "stream": false,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let output = anthropic_to_openai_request(&input, true, true).unwrap();
        assert_eq!(output["messages"][0]["role"], "system");
        assert_eq!(output["messages"][0]["content"], "You are helpful.");
        assert_eq!(output["messages"][1]["role"], "user");
        assert_eq!(output["messages"][1]["content"], "Hello");
        assert_eq!(output["max_tokens"], 1024);
        assert!(output.get("max_output_tokens").is_none());
        assert_eq!(output["stream"], false);
    }

    #[test]
    fn anthropic_to_openai_codex_route_omits_max_output_tokens() {
        let input = json!({
            "model": "claude-codex-5.5",
            "max_tokens": 1024,
            "stream": false,
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let output = anthropic_to_openai_request(&input, false, false).unwrap();
        assert!(output.get("max_output_tokens").is_none());
    }

    #[test]
    fn anthropic_to_openai_codex_route_strips_tool_calls() {
        let input = json!({
            "model": "claude-codex-5.5",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "file1\nfile2"}
                ]}
            ]
        });
        let output = anthropic_to_openai_request(&input, false, false).unwrap();
        assert_eq!(output["messages"][1]["role"], "assistant");
        assert_eq!(output["messages"][1]["content"], "");
        assert!(output["messages"][1].get("tool_calls").is_none());
        assert_eq!(output["messages"][2]["role"], "tool");
        assert_eq!(output["messages"][2]["tool_call_id"], "call_1");
    }

    #[test]
    fn anthropic_to_openai_tool_use() {
        let input = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Run ls"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "file1\nfile2"}
                ]}
            ],
            "tools": [
                {"name": "bash", "description": "Run bash", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}}
            ]
        });
        let output = anthropic_to_openai_request(&input, true, true).unwrap();
        let assistant = &output["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["tool_calls"].is_array());
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "bash");
        let tool_msg = &output["messages"][2];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
        assert_eq!(tool_msg["content"], "file1\nfile2");
    }

    #[test]
    fn anthropic_to_openai_assistant_empty_content_uses_empty_string() {
        let input = json!({
            "model": "claude-codex-5.5",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": null}
            ]
        });
        let output = anthropic_to_openai_request(&input, true, true).unwrap();
        assert_eq!(output["messages"][1]["role"], "assistant");
        assert_eq!(output["messages"][1]["content"], "");
        assert!(output["messages"][1].get("tool_calls").is_none());
    }

    #[test]
    fn anthropic_to_openai_assistant_tool_calls_with_null_content_uses_empty_string() {
        let input = json!({
            "model": "claude-codex-5.5",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}
                ]}
            ]
        });
        let output = anthropic_to_openai_request(&input, true, true).unwrap();
        assert_eq!(output["messages"][1]["role"], "assistant");
        assert_eq!(output["messages"][1]["content"], "");
        assert!(output["messages"][1]["tool_calls"].is_array());
        assert_eq!(
            output["messages"][1]["tool_calls"][0]["function"]["name"],
            "bash"
        );
    }

    #[test]
    fn openai_to_anthropic_text_response() {
        let openai = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["type"], "message");
        assert_eq!(output["role"], "assistant");
        assert_eq!(output["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(output["stop_reason"], "end_turn");
        assert_eq!(output["content"][0]["type"], "text");
        assert_eq!(output["content"][0]["text"], "Hello!");
        assert_eq!(output["usage"]["input_tokens"], 10);
        assert_eq!(output["usage"]["output_tokens"], 5);
    }

    #[test]
    fn openai_to_anthropic_tool_calls() {
        let openai = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let original = json!({"model": "claude-3-5-sonnet-20241022"});
        let output = openai_to_anthropic_response(&openai, &original);
        assert_eq!(output["stop_reason"], "tool_use");
        assert_eq!(output["content"][0]["type"], "tool_use");
        assert_eq!(output["content"][0]["id"], "call_123");
        assert_eq!(output["content"][0]["name"], "bash");
        assert_eq!(output["content"][0]["input"]["command"], "ls");
    }

    #[test]
    fn stream_translator_text_only() {
        let mut translator = StreamTranslator::new("test-model".to_string());

        let chunk1 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        let chunk2 = r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk3 = r#"{"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#;
        let chunk4 = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;

        let mut all_events = Vec::new();
        all_events.extend(translator.process_line(chunk1));
        all_events.extend(translator.process_line(chunk2));
        all_events.extend(translator.process_line(chunk3));
        all_events.extend(translator.process_line(chunk4));

        let output = all_events.join("");
        assert!(output.contains("event: message_start"));
        assert!(output.contains("event: content_block_start"));
        assert!(output.contains("\"text_delta\""));
        assert!(output.contains("Hello"));
        assert!(output.contains(" world"));
        assert!(output.contains("event: content_block_stop"));
        assert!(output.contains("event: message_delta"));
        assert!(output.contains("event: message_stop"));
    }

    #[test]
    fn stream_translator_tool_calls() {
        let mut translator = StreamTranslator::new("test-model".to_string());

        let chunk1 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        let chunk2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}"#;
        let chunk3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"co"}}]},"finish_reason":null}]}"#;
        let chunk4 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"mmand\"}"}}]},"finish_reason":null}]}"#;
        let chunk5 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut all_events = Vec::new();
        all_events.extend(translator.process_line(chunk1));
        all_events.extend(translator.process_line(chunk2));
        all_events.extend(translator.process_line(chunk3));
        all_events.extend(translator.process_line(chunk4));
        all_events.extend(translator.process_line(chunk5));

        let output = all_events.join("");
        assert!(output.contains("event: message_start"));
        assert!(output.contains("\"tool_use\""));
        assert!(output.contains("bash"));
        assert!(output.contains("\"input_json_delta\""));
        assert!(output.contains("\"stop_reason\":\"tool_use\""));
        assert!(output.contains("event: message_stop"));
    }
}

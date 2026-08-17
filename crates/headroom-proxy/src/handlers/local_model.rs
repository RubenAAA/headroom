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
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use bytes::Bytes;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;

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

/// What [`apply_ctx_request_transforms`] did, for the request outcome.
///
/// `transforms_applied` uses the same label strings the Claude path feeds to
/// `RequestOutcome`, so one transform reads identically in `/stats` whichever
/// path served it.
#[derive(Debug, Default)]
struct CtxTransformReport {
    transforms_applied: Vec<String>,
    tokens_saved: i64,
    /// The session key the drift detector and offload gate used. Prefix replay
    /// must key off the same one — the Claude path shares a single key across
    /// all three deliberately, so they agree on what "this conversation" is.
    session_key: String,
}

/// Apply headroom's CTX request-side transforms to a routed model's parsed
/// Anthropic body, reusing the same flags/state as the Claude passthrough path
/// (`forward_http`). Runs the passive session capture (read-only) and, when
/// `ctx_offload` is enabled, the tool_result offload — which both feeds
/// `headroom ctx search` and shrinks the request. Mutates `parsed` in place.
///
/// Note: offload rewrites frozen history only on rebuild boundaries (the gate
/// prevents cache thrash), exactly as the Claude path does.
async fn apply_ctx_request_transforms(
    state: &AppState,
    parsed: &mut Value,
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    request_id: &str,
) -> CtxTransformReport {
    let mut report = CtxTransformReport::default();
    use crate::cache_stabilization::drift_detector::{
        compute_structural_hash, derive_session_key, observe_drift, ApiKind,
    };

    // PR-E5: volatile-content detector. Pure observer — one WARN per finding
    // for content that busts the cache (timestamps, UUIDs, ID-named fields).
    // Runs on the body as received so the warning names what the client sent,
    // not what our own transforms left behind.
    let findings = crate::cache_stabilization::volatile_detector::detect_volatile_content(
        parsed,
        crate::cache_stabilization::volatile_detector::ApiKind::Anthropic,
    );
    if !findings.is_empty() {
        crate::cache_stabilization::volatile_detector::emit_volatile_warnings(
            &findings, request_id, None, None,
        );
    }

    // Derived from the body as received — this runs before any transform
    // mutates `parsed`, which matters because `derive_session_key`
    // fingerprints the conversation's first message when no
    // `x-headroom-session-id` header is present.
    let session_key = derive_session_key(headers, client_addr, parsed, ApiKind::Anthropic);
    report.session_key = session_key.clone();

    // Observe cache-prefix drift on the incoming body (before any transform),
    // matching the Claude path's ordering. Runs unconditionally so the
    // `cache_drift_observed` signal (which axis of system/tools/early_messages
    // changed turn-to-turn) is available regardless of which CTX flags are on.
    // A drift means the codex prompt-cache prefix moved this turn.
    let hash = compute_structural_hash(parsed, ApiKind::Anthropic);
    let drift_dims = observe_drift(&state.drift_state, &session_key, hash);
    let rebuild_boundary = drift_dims.is_some();

    // CTX-7: park conversation identity + drift dims under the request id so
    // the response side can classify this turn's billed usage against the
    // conversation's previous turn. This is what feeds the re-cache watchdog
    // that `scripts/statusline-cache-health.sh` renders — without it the cache
    // segment simply has nothing to say about routed turns.
    state.usage_observer.begin_request(
        request_id,
        crate::cache_stabilization::usage_observer::conversation_key(parsed, &session_key),
        // Same hash the drift detector just logged for this session, so routed
        // recache events join to their drift events like the Claude path's do.
        Some(crate::cache_stabilization::drift_detector::session_key_log_prefix(&session_key)),
        drift_dims,
        Some(crate::cache_stabilization::usage_observer::prefix_fingerprint(parsed)),
    );

    // CTX-2: passive session capture. Read-only — clones the body onto a
    // detached worker; never mutates and never blocks.
    // Which project's ctx stores this turn is captured into and recalled from.
    let ctx_project = crate::proxy::resolve_ctx_project(Some(headers), parsed);
    if let Some(observer) = state.ctx_observer.as_ref() {
        observer.observe(parsed, &session_key, &ctx_project);
    }

    // CCR identity for this turn. All three helpers read the Anthropic
    // `messages` shape, which is exactly what `parsed` still is here.
    let ccr_workspace = crate::proxy::resolve_ccr_workspace(Some(headers), parsed);
    let user_query = crate::proxy::latest_user_query(parsed);
    let turn_number = crate::proxy::anthropic_turn_number(parsed);

    // One ceiling shared by every stage that appends to this turn, same as
    // the Claude path. The routed path runs the same three appenders, so it
    // needs the same combined bound.
    let injection_budget = crate::injection_budget::InjectionBudget::for_request(
        state.config.max_injection_bytes,
        request_id,
    );

    // CCR proactive expansion: pull back previously-offloaded content the
    // query looks like it needs, before anything else touches the body. First
    // in the block on the Claude path too.
    if let Some((workspace_key, workspace_label)) = ccr_workspace.as_ref() {
        if crate::proxy::maybe_append_ccr_proactive_expansion(
            state,
            parsed,
            &user_query,
            workspace_key,
            workspace_label.as_deref(),
            turn_number,
            request_id,
            &injection_budget,
        ) {
            report
                .transforms_applied
                .push("ccr_proactive_expansion".to_string());
        }
    }

    // CTX-4: recall/resume injection. Runs BEFORE offload (matching the
    // Claude path order). Cache-safe by construction — the engine decides
    // once per conversation and replays the exact same bytes into the first
    // user message on every later turn (nothing volatile), so the codex
    // prompt-cache prefix stays byte-stable after the one-time introduction.
    // It never touches `system`/`tools`.
    if let Some(engine) = state.ctx_inject.as_ref() {
        if engine.maybe_inject_for_request(
            parsed,
            &session_key,
            &ctx_project,
            &injection_budget,
            request_id,
        ) {
            report.transforms_applied.push("ctx_inject".to_string());
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
            report.transforms_applied.push("ctx_offload".to_string());
            report.tokens_saved += out.tokens_saved;
            tracing::debug!(
                event = "codex_ctx_offload",
                blocks_offloaded = out.blocks_offloaded,
                blocks_deferred = out.blocks_deferred,
                tokens_saved = out.tokens_saved,
                rebuild_boundary,
                "offloaded tool_result blocks on routed-model request"
            );
            // Record what was offloaded against the workspace so a later turn's
            // proactive expansion can find it. Without this the expansion above
            // has an empty index to consult and can never fire.
            if let Some((workspace_key, _)) = ccr_workspace.as_ref() {
                crate::proxy::track_ccr_context_records(
                    state,
                    &out.records,
                    workspace_key,
                    &user_query,
                    turn_number,
                    request_id,
                );
            } else if state.ccr_context_tracker.is_some() {
                tracing::info!(
                    event = "codex_ccr_workspace_unresolved",
                    "CCR: workspace unresolved; skipping compression tracking"
                );
            }
            runtime.store.persist(out.records, &ctx_project);
        }
    }

    // The routed body is still Anthropic-shaped here — translation runs after
    // — so every stage below uses the Anthropic provider and the Anthropic
    // tool shape, exactly as `forward_http` does for `/v1/messages`.
    const PROVIDER: crate::memory::tool_adapter::Provider =
        crate::memory::tool_adapter::Provider::Anthropic;

    // Memory: inject tool definitions. Without this, a routed model has no way
    // to write memories at all — `--memory` looked enabled and silently did
    // nothing.
    if let Some(handler) = state.memory_handler.as_ref() {
        let handler = handler.lock().await;
        if handler.is_initialized() {
            // A request with no `tools` array still gets the memory tools; the
            // array is created on demand, matching the Claude path.
            let existing: Vec<Value> = parsed
                .get("tools")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let (new_tools, injected) = handler.inject_memory_tools(Some(&existing), PROVIDER);
            if injected {
                if let Some(obj) = parsed.as_object_mut() {
                    obj.insert("tools".to_string(), Value::Array(new_tools));
                    report.transforms_applied.push("memory_tools".to_string());
                    tracing::debug!(
                        event = "codex_memory_tools",
                        "injected memory tool definitions into routed-model request"
                    );
                }
            }
        }
    }

    // CCR: the `headroom_retrieve` tool, so the model can pull back original
    // content by hash from a compression marker. Only extends an existing
    // `tools` array — same as the Claude path, which does not create one here.
    if state.config.ccr_inject_tool {
        if let Some(tools) = parsed.get_mut("tools").and_then(|v| v.as_array_mut()) {
            let already_has = tools
                .iter()
                .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("headroom_retrieve"));
            if !already_has {
                tools.push(json!({
                    "name": "headroom_retrieve",
                    "description": "Retrieve original uncompressed content that was compressed to save tokens. Use this when you need more data than what's shown in compressed tool results. The hash is provided in compression markers like [N items compressed... hash=abc123].",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "hash": {
                                "type": "string",
                                "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
                            }
                        },
                        "required": ["hash"]
                    }
                }));
                report.transforms_applied.push("ccr_tool".to_string());
                tracing::debug!(
                    event = "codex_ccr_tool",
                    "injected headroom_retrieve tool into routed-model request"
                );
            }
        }
    }

    // Output shaping: verbosity steering and effort routing. Idempotent — the
    // steering text carries a sentinel prefix — so replaying a prefix that
    // already contains it does not stack.
    if state.config.output_shaper_enabled {
        let shaped = crate::output_shaper::shape_request(
            parsed,
            true,
            state.config.verbosity_level,
            true,
            &state.config.mechanical_effort,
        );
        if shaped.changed {
            report.transforms_applied.extend(shaped.labels.clone());
            tracing::debug!(
                event = "codex_output_shaper",
                labels = ?shaped.labels,
                "shaped routed-model request"
            );
        }
    }

    // Memory: search and append recalled context to the latest user message.
    if let Some(handler) = state.memory_handler.as_ref() {
        let handler = handler.lock().await;
        if handler.is_initialized() {
            if let Some(messages) = parsed.get("messages").and_then(|v| v.as_array()).cloned() {
                let user_id = headers
                    .get("x-headroom-user-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("default");
                if let Some(context) = handler
                    .search_and_format_context(user_id, &messages, None, None, None, None)
                    .await
                {
                    let frozen = parsed
                        .get("system")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let (new_msgs, bytes) =
                        crate::memory::handler::MemoryHandler::append_to_latest_user_tail(
                            &messages, &context, PROVIDER, frozen,
                        );
                    if bytes > 0 {
                        if let Some(msgs) = parsed.get_mut("messages") {
                            *msgs = Value::Array(new_msgs);
                            report.transforms_applied.push("memory_context".to_string());
                            tracing::debug!(
                                event = "codex_memory_context",
                                bytes_appended = bytes,
                                "injected recalled memory into routed-model request"
                            );
                        }
                    }
                }
            }
        }
    }

    report
}

/// Run a `forward_http` stage that works on serialized bytes against the
/// routed path's parsed body.
///
/// The Claude path threads `Bytes` from stage to stage; this one carries a
/// `Value` because it has to hand the body to the translator at the end.
/// Rather than fork each stage, adapt around them — they stay the single
/// implementation, which is what keeps the two paths honest.
///
/// Any serialize/parse failure leaves `parsed` untouched. Every one of these
/// stages already returns its input unchanged when it cannot parse, so
/// preserving that is the same contract.
fn apply_bytes_stage(parsed: &mut Value, stage: impl FnOnce(bytes::Bytes) -> bytes::Bytes) {
    let Ok(body) = serde_json::to_vec(parsed) else {
        return;
    };
    let out = stage(bytes::Bytes::from(body));
    if let Ok(v) = serde_json::from_slice::<Value>(&out) {
        *parsed = v;
    }
}

/// Tool schema compaction: strips `$schema`/`title`/examples from tool
/// definitions.
///
/// Runs after compression and after every tool-injecting stage, which is where
/// the Claude path runs it — so the memory and CCR tools get compacted too
/// rather than being added behind its back.
///
/// Token counts are taken around the call rather than derived from the byte
/// deltas it reports: a bytes/4 rule of thumb is wrong by enough on JSON
/// (punctuation-dense, so tokens run well ahead of bytes/4) that the savings
/// figure would be fiction. Only the `tools` array is counted, not the whole
/// body, and only when the request actually carries tools.
fn apply_tool_schema_compaction(parsed: &mut Value) -> (bool, i64) {
    let tools_tokens_before = count_tools_tokens(parsed);
    let (compacted, modified, before_bytes, after_bytes) =
        crate::tool_schema_compaction::compact_tools(std::mem::take(parsed));
    *parsed = compacted;
    if !modified {
        return (false, 0);
    }
    let saved = (tools_tokens_before - count_tools_tokens(parsed)).max(0);
    tracing::debug!(
        event = "codex_tool_schema_compaction",
        tools_before_bytes = before_bytes,
        tools_after_bytes = after_bytes,
        tokens_saved = saved,
        "compacted tool schemas on routed-model request"
    );
    (true, saved)
}

/// What the compression + replay stage did, for the request outcome.
#[derive(Debug, Default)]
struct CompressionReport {
    transforms_applied: Vec<String>,
    tokens_saved: i64,
    /// Set when the prefix-replay stage parked this turn, so the response side
    /// knows to feed cache tokens back with [`SessionReplayStore::complete`].
    replay_parked: bool,
}

/// Merge routed live-zone compression into the report that is eventually
/// booked. CTX offload has its own scope and telemetry, so its saving must not
/// be carried forward as though the live-zone dispatcher produced it on this
/// turn. This replacement (rather than addition) is what prevents a prior
/// conversation-sized CTX value from being re-emitted in `tok_saved`.
fn merge_routed_compression_report(
    ctx_report: &mut CtxTransformReport,
    compression_report: CompressionReport,
) -> i64 {
    let ctx_tokens_saved = ctx_report.tokens_saved;
    ctx_report.tokens_saved = compression_report.tokens_saved;
    ctx_report
        .transforms_applied
        .extend(compression_report.transforms_applied);
    ctx_tokens_saved
}

/// Live-zone compression and freeze-replay for a routed request, mirroring the
/// `AnthropicMessages` arm of `forward_http`.
///
/// The routed body is still in Anthropic shape at this point — translation to
/// the OpenAI wire format happens after — so the same dispatcher applies, and
/// gating reads the same config fields rather than anything routed-specific.
/// A routed model therefore compresses exactly when a Claude model would:
/// `--compression` (implied by any `--ctx-*` flag) with a `--compression-mode`
/// other than `off`, no `x-headroom-bypass`, and a non-empty `messages`.
///
/// Replay runs after compression and is gated on `--prefix-replay`
/// independently, matching the Claude path. That ordering is the point of the
/// stage: compression rewrites bytes inside the prompt-cache prefix, and replay
/// puts the previously-forwarded bytes back so the provider's cache still hits.
/// Turning compression on without replay moves the prefix every turn — true on
/// both paths, and worth knowing before enabling one without the other.
fn apply_compression_and_replay(
    state: &AppState,
    parsed: &mut Value,
    headers: &HeaderMap,
    request_id: &str,
    session_key: &str,
) -> CompressionReport {
    let mut report = CompressionReport::default();

    let has_messages = parsed
        .get("messages")
        .and_then(|m| m.as_array())
        .is_some_and(|a| !a.is_empty());
    let decision = crate::compression_decision::CompressionDecision::decide(
        headers,
        state.config.compression,
        true, // license_allows — same TODO(license) stub as the Claude path
        has_messages,
    );

    // Nothing to do at all: no compression and no replay. Skip the
    // serialize/reparse round trip entirely so the flags-off path stays free.
    if !decision.should_compress && !state.config.prefix_replay {
        return report;
    }

    let body = match serde_json::to_vec(parsed) {
        Ok(b) => bytes::Bytes::from(b),
        Err(e) => {
            tracing::warn!(
                event = "routed_compression_skipped",
                request_id = %request_id,
                error = %e,
                "could not serialize routed body; skipping compression and replay"
            );
            return report;
        }
    };

    // Snapshot the messages as they stand *before* compression: they are the
    // append-only guard's comparison source and next turn's replay key. Taken
    // after the CTX transforms, which is where the Claude path takes it too —
    // `buffered` there has already been rewritten by them.
    let replay_original_messages: Option<Vec<Value>> = if state.config.prefix_replay {
        parsed.get("messages").and_then(|m| m.as_array()).cloned()
    } else {
        None
    };

    let body = if decision.should_compress {
        // PR-E3: the Phase E byte-mutating passes gate on PAYG, with the same
        // enforcement-flag override the Claude path applies.
        let auth_mode = if state.config.auth_mode_policy_enforcement.is_enabled() {
            headroom_core::auth_mode::classify(headers)
        } else {
            headroom_core::auth_mode::AuthMode::Payg
        };
        let routed_ccr_store = state.ccr_store();
        let outcome = crate::compression::compress_anthropic_request(
            &body,
            state.config.compression_mode,
            state.config.cache_control_auto_frozen,
            auth_mode,
            request_id,
            &state.config.exclude_tools,
            // This path injects headroom_retrieve and resolves it on both
            // response arms (`handle_streaming_response` through
            // `sse::ccr_stream`, `handle_buffered_response` directly), so the
            // marker points at a recovery route the model can actually take.
            routed_ccr_store.as_deref(),
        );
        let outcome = crate::compression::apply_cross_turn_dedup(
            outcome,
            &body,
            &state.config,
            "/v1/messages",
            request_id,
        );
        match outcome {
            crate::compression::Outcome::Compressed {
                body: compressed,
                tokens_before,
                tokens_after,
                strategies_applied,
                ..
            } => {
                report.tokens_saved += (tokens_before as i64 - tokens_after as i64).max(0);
                report
                    .transforms_applied
                    .extend(strategies_applied.iter().map(|s| s.to_string()));
                tracing::debug!(
                    event = "routed_compression_applied",
                    request_id = %request_id,
                    tokens_before,
                    tokens_after,
                    "compressed routed-model request"
                );
                compressed
            }
            _ => body,
        }
    } else {
        body
    };

    let body = match replay_original_messages {
        Some(original_messages) => {
            report.replay_parked = true;
            crate::proxy::apply_prefix_replay(
                &state.replay_store,
                session_key,
                request_id,
                original_messages,
                body,
                Some(&state.usage_observer),
                state.started_at.elapsed().as_secs(),
                state.config.cache_tail_breakpoints as usize,
                state.config.strip_system_cache_breakpoints,
            )
        }
        None => body,
    };

    match serde_json::from_slice::<Value>(&body) {
        Ok(v) => *parsed = v,
        Err(e) => {
            // Leave `parsed` as it was — forwarding the pre-compression body is
            // always safe, and is what every failure arm above already does.
            tracing::warn!(
                event = "routed_compression_reparse_failed",
                request_id = %request_id,
                error = %e,
                "compressed routed body did not re-parse; forwarding uncompressed"
            );
            report.tokens_saved = 0;
            report.transforms_applied.clear();
            report.replay_parked = false;
        }
    }

    report
}

/// Assemble the outcome context for a routed request.
///
/// `target_model` present means the Responses API; absent means Chat
/// Completions. The provider labels match the ones `forward_http` uses for the
/// same two wire formats, so a `/stats` filter behaves the same either way.
#[allow(clippy::too_many_arguments)]
fn build_routed_outcome_context(
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
fn count_tools_tokens(body: &Value) -> i64 {
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
    // Clock for the request outcome, started before any work so the recorded
    // latency covers what the client actually waited for.
    let request_started = std::time::Instant::now();
    // One id for this turn, shared by the request outcome and the prefix-replay
    // store — `begin_request` parks under it and the response side hands it
    // back to `complete`, so the two must be the same value.
    let request_id = uuid::Uuid::new_v4().to_string();
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
    let transform_started = std::time::Instant::now();
    let mut ctx_report =
        apply_ctx_request_transforms(&state, &mut parsed, &headers, &client_addr, &request_id)
            .await;

    // Live-zone compression + freeze-replay, on the same flags as the Claude
    // path and in the same order (compress, then replay the cached prefix).
    let session_key = ctx_report.session_key.clone();
    let compression_report =
        apply_compression_and_replay(&state, &mut parsed, &headers, &request_id, &session_key);
    let compression_tokens_saved = compression_report.tokens_saved;
    let replay_parked = compression_report.replay_parked;
    let ctx_tokens_saved = merge_routed_compression_report(&mut ctx_report, compression_report);
    tracing::info!(
        event = "routed_compression_accounting",
        request_id = %request_id,
        compression_tokens_saved,
        ctx_transform_tokens_saved = ctx_tokens_saved,
        "routed-model savings split by transform scope"
    );

    // Tool pruning, schema compaction, then order stabilization — the Claude
    // path's closing sequence, and order matters within it: compaction runs
    // once tools are final, and stabilization must follow every other tool
    // mutation so the order recorded is the order the provider caches.
    //
    // Both prune and stabilize are shape-agnostic (they match a tool by name
    // in either the Anthropic or the OpenAI wrapper) and run here on the
    // pre-translation body, which is Anthropic-shaped. `forward_http` gates
    // them to `AnthropicMessages`, but that is a call-site choice rather than
    // a limitation of either function.
    apply_bytes_stage(&mut parsed, |body| {
        if state.config.tool_prune_policy.is_noop() {
            body
        } else {
            crate::proxy::maybe_prune_tools(body, &state.config.tool_prune_policy, &request_id)
        }
    });

    let (compacted, compaction_saved) = apply_tool_schema_compaction(&mut parsed);
    if compacted {
        ctx_report
            .transforms_applied
            .push("tool_schema_compaction".to_string());
        ctx_report.tokens_saved += compaction_saved;
    }

    if state.config.cache_stable_tool_order {
        apply_bytes_stage(&mut parsed, |body| {
            crate::proxy::maybe_stabilize_tool_order(
                body,
                &state.tool_order_state,
                &session_key,
                &request_id,
            )
        });
    }

    // Two stages from the Claude path's closing sequence are deliberately
    // absent, because they do not apply rather than because they were missed:
    //
    // - `--context-edit` injects Anthropic's `context_management` block and its
    //   `context-management-2025-06-27` beta header. That is a server-side
    //   feature of the Anthropic API; this path always talks to OpenAI, which
    //   has no equivalent to translate it into.
    // - `--force-1h-cache-ttl` rewrites `cache_control.ttl`, an Anthropic
    //   prompt-caching control. The Responses API has no TTL knob.
    //
    // The OpenAI-side counterpart, `prompt_cache_key`, is injected after
    // translation instead — see below.
    let overhead_ms = transform_started.elapsed().as_secs_f64() * 1000.0;

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
            headroom_core::auth_mode::classify(&headers),
            &request_id,
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

    // Book this turn through the same outcome funnel `forward_http` uses, so
    // routed spend shows up in /stats, /stats-history, and the dashboard
    // alongside Claude traffic.
    let forwarded_tokens_estimate = serde_json::to_string(&openai_body)
        .ok()
        .map(|body| {
            headroom_core::tokenizer::get_tokenizer(target_model.as_deref().unwrap_or(body_model))
                .count_text(&body) as i64
        })
        .unwrap_or(0);
    let mut outcome_ctx = build_routed_outcome_context(
        &state,
        &parsed,
        &headers,
        target_model.as_deref(),
        body_model,
        ctx_report,
        overhead_ms,
        request_started,
        request_id.clone(),
        // Only hand the response side a store when this turn was actually
        // parked; `complete` on an unparked id is a no-op, but passing `None`
        // keeps the flag-off path from cloning a handle it will never use.
        replay_parked.then(|| state.replay_store.clone()),
        forwarded_tokens_estimate,
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
    //
    // Bounds come from the same config the Claude path uses, so
    // `--retry-max-attempts` and the backoff window mean one thing across both
    // paths. The 401-refresh is codex-specific and sits outside the budget:
    // it is a credential fix, not a transient failure, and always gets its one
    // shot regardless of how retries are configured.
    let max_attempts = if state.config.retry_enabled {
        state.config.retry_max_attempts.max(1)
    } else {
        1
    };
    let base_delay_ms = state.config.retry_base_delay_ms;
    let max_delay_ms = state.config.retry_max_delay_ms;
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
                if (status.as_u16() == 429 || status.is_server_error()) && attempt < max_attempts {
                    let retry_after_uncapped = r
                        .headers()
                        .get(http::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(headroom_core::retry::retry_after_ms_uncapped);
                    if retry_after_uncapped.is_some_and(|delay| delay > max_delay_ms as f64) {
                        tracing::warn!(
                            event = "local_model_retry_after_exceeds_cap",
                            status = status.as_u16(),
                            attempt,
                            max_attempts,
                            retry_after_ms = retry_after_uncapped.unwrap_or_default(),
                            retry_max_delay_ms = max_delay_ms,
                            request_id = %request_id,
                            session_key_hash = %session_key.as_deref().map(crate::cache_stabilization::drift_detector::session_key_log_prefix).unwrap_or_default(),
                            "upstream Retry-After exceeds the internal wait cap; returning the response without an early retry"
                        );
                        break r;
                    }
                    let retry_after = retry_after_uncapped;
                    let backoff = retry_after
                        .map(|delay| {
                            std::time::Duration::from_millis(
                                delay.ceil().min(u64::MAX as f64) as u64
                            )
                        })
                        .unwrap_or_else(|| {
                            std::time::Duration::from_millis(
                                base_delay_ms.saturating_mul(2u64.saturating_pow(attempt - 1)),
                            )
                        })
                        .min(std::time::Duration::from_millis(max_delay_ms));
                    tracing::warn!(
                        event = "local_model_upstream_retry",
                        status = status.as_u16(),
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        retry_after_header = r.headers().contains_key(http::header::RETRY_AFTER),
                        delay_source = if retry_after.is_some() { "header" } else { "backoff" },
                        retry_after_clamped = false,
                        request_id = %request_id,
                        session_key_hash = %session_key.as_deref().map(crate::cache_stabilization::drift_detector::session_key_log_prefix).unwrap_or_default(),
                        "retrying transient upstream error"
                    );
                    crate::observability::record_upstream_retry(
                        "local_model",
                        crate::observability::retry_reason::from_status(status.as_u16()),
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                break r;
            }
            Err(e) => {
                if attempt < max_attempts {
                    let backoff = std::time::Duration::from_millis(250 * 2u64.pow(attempt - 1));
                    tracing::warn!(
                        event = "local_model_upstream_retry",
                        error = %e,
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        delay_source = "transport_backoff",
                        request_id = %request_id,
                        "retrying failed upstream connection"
                    );
                    crate::observability::record_upstream_retry(
                        "local_model",
                        crate::observability::retry_reason::TRANSPORT,
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
    if let Some(ctx) = outcome_ctx.as_mut() {
        ctx.upstream_attempts = i64::from(attempt.max(1));
    }

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

    // This path injects `headroom_retrieve` (above) and hands compression a
    // CCR store, so it owns resolving the calls the model makes. Present only
    // when the store exists; without it there is nothing to look a hash up in.
    let ccr = state.ccr_store().map(|store| RoutedCcr {
        store,
        client: state.client.clone(),
        upstream_url: upstream_url.clone(),
        headers: upstream_headers.clone(),
        request_body: openai_body_bytes.clone(),
        config: state.config.clone(),
        request_id: request_id.to_string(),
        responses_shape: target_model.is_some(),
    });

    if upstream_status != StatusCode::OK {
        handle_routed_error_response(upstream_resp, upstream_status, outcome_ctx).await
    } else if downstream_is_stream {
        handle_streaming_response(
            upstream_resp,
            &parsed,
            state.codex_rate_limits.clone(),
            outcome_ctx,
            ccr,
        )
        .await
    } else if target_model.is_some() {
        handle_buffered_responses_response(
            upstream_resp,
            &parsed,
            upstream_status,
            outcome_ctx,
            ccr,
        )
        .await
    } else {
        handle_buffered_response(upstream_resp, &parsed, upstream_status, outcome_ctx, ccr).await
    }
}

/// Return a routed upstream failure without translating its status or
/// `Retry-After`. This is especially important when the requested delay is
/// longer than our in-request cap: retrying early violates the upstream's
/// instruction, while converting a 429 stream into a 200 SSE body hides it
/// from the client that can schedule the next request correctly.
async fn handle_routed_error_response(
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

/// Resolve `headroom_retrieve` on a buffered routed reply, in the upstream's
/// own shape. Returns the resolved response and the usage of the rounds the
/// client never saw.
async fn resolve_routed_ccr(
    response: &Value,
    ccr: &RoutedCcr,
) -> (Value, crate::proxy::CcrRoundUsage) {
    let provider = if ccr.responses_shape {
        "openai_responses"
    } else {
        "openai"
    };
    let Ok(url) = url::Url::parse(&ccr.upstream_url) else {
        tracing::warn!(
            event = "routed_ccr_bad_upstream_url",
            url = %ccr.upstream_url,
            "cannot resolve headroom_retrieve on this turn"
        );
        return (response.clone(), crate::proxy::CcrRoundUsage::default());
    };
    let body = match serde_json::to_vec(response) {
        Ok(b) => Bytes::from(b),
        Err(_) => return (response.clone(), crate::proxy::CcrRoundUsage::default()),
    };
    let (resolved, usage) = crate::proxy::handle_ccr_response(
        &body,
        &ccr.request_body,
        &url,
        &ccr.client,
        ccr.store.as_ref(),
        &ccr.config,
        &ccr.request_id,
        &ccr.headers,
        provider,
    )
    .await;
    match serde_json::from_slice(&resolved) {
        Ok(v) => (v, usage),
        Err(_) => (response.clone(), usage),
    }
}

/// What the routed response arms need to resolve a `headroom_retrieve` call.
///
/// Assembled once at the dispatch point because the request shape, URL and
/// headers are only in scope there.
pub(crate) struct RoutedCcr {
    pub store: Arc<dyn headroom_core::ccr::CcrStore>,
    pub client: reqwest::Client,
    pub upstream_url: String,
    pub headers: HeaderMap,
    /// The request as translated and sent upstream, in the upstream's shape.
    pub request_body: Bytes,
    pub config: Arc<crate::config::Config>,
    pub request_id: String,
    /// True when the upstream is the Responses API rather than
    /// chat-completions. The two disagree about where tool calls live.
    pub responses_shape: bool,
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
                        "parameters": params,
                        // Must be present and false, not merely absent: codex
                        // turns on strict constrained decoding for translated
                        // function tools that omit the field, which forces
                        // every optional parameter into every call. That is
                        // the same "fills every optional field" behaviour the
                        // `Agent`/`mode` strip above works around, so the
                        // strip may be redundant once this is live.
                        // Behaviour confirmed in raine/claude-code-proxy.
                        "strict": false
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
                // `content` is a plain string OR a list of blocks. Images that
                // the backend accepts survive as `input_image`; everything
                // else collapses to text, with a placeholder where a block
                // could not be carried.
                let is_error = block
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": tool_result_output_value(block.get("content"), is_error)
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

/// One piece of a rendered tool result: either text, or an image the Responses
/// API can actually accept.
enum ToolResultPart {
    Text(String),
    Image(String),
}

/// Media types the Responses API accepts as `input_image`.
const RESPONSES_IMAGE_MEDIA_TYPES: [&str; 4] =
    ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Turn an Anthropic `image` block into a data URL, or `None` if the Responses
/// API could not accept it.
///
/// The payload is decoded and re-encoded rather than passed through: Anthropic
/// tolerates whitespace and missing padding in base64 where the data-URL form
/// does not, and shipping a blob the backend rejects loses the whole turn.
fn image_block_to_data_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    if source.get("type").and_then(|v| v.as_str()) != Some("base64") {
        return None;
    }
    let media_type = source.get("media_type").and_then(|v| v.as_str())?;
    if !RESPONSES_IMAGE_MEDIA_TYPES.contains(&media_type) {
        return None;
    }
    let data = source.get("data").and_then(|v| v.as_str())?;
    let compact: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if compact.is_empty() {
        return None;
    }
    let decoded = STANDARD
        .decode(&compact)
        .or_else(|_| STANDARD_NO_PAD.decode(&compact))
        .ok()?;
    Some(format!(
        "data:{media_type};base64,{}",
        STANDARD.encode(decoded)
    ))
}

/// Render `tool_result.content` into ordered parts.
///
/// Anything that cannot be carried leaves a placeholder in the position it
/// occupied, so the model is told something was there. A silent drop reads to
/// the model as if the tool returned less than it did.
fn render_tool_result_parts(content: Option<&Value>) -> Vec<ToolResultPart> {
    match content {
        Some(Value::String(s)) => vec![ToolResultPart::Text(s.clone())],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => ToolResultPart::Text(
                    block
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                ),
                Some("image") => match image_block_to_data_url(block) {
                    Some(url) => ToolResultPart::Image(url),
                    None => ToolResultPart::Text(
                        "[unsupported content block omitted: image]".to_string(),
                    ),
                },
                Some(other) => {
                    ToolResultPart::Text(format!("[unsupported content block omitted: {other}]"))
                }
                None => {
                    ToolResultPart::Text("[unsupported content block omitted: unknown]".to_string())
                }
            })
            .collect(),
        Some(other) => vec![ToolResultPart::Text(other.to_string())],
        None => Vec::new(),
    }
}

/// Build the `function_call_output.output` value.
///
/// Stays a plain string unless an image survived — the string form is what the
/// backend sees for the overwhelming majority of turns, and switching shape
/// only when necessary keeps those bytes identical to before.
fn tool_result_output_value(content: Option<&Value>, is_error: bool) -> Value {
    let parts = render_tool_result_parts(content);
    let has_image = parts.iter().any(|p| matches!(p, ToolResultPart::Image(_)));

    if !has_image {
        let joined = parts
            .iter()
            .filter_map(|p| match p {
                ToolResultPart::Text(t) => Some(t.as_str()),
                ToolResultPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Value::String(if is_error {
            format!("Error: {joined}")
        } else {
            joined
        });
    }

    let mut items: Vec<Value> = parts
        .into_iter()
        .map(|p| match p {
            ToolResultPart::Text(text) => json!({"type": "input_text", "text": text}),
            ToolResultPart::Image(image_url) => {
                json!({"type": "input_image", "image_url": image_url})
            }
        })
        .collect();
    if is_error {
        items.insert(0, json!({"type": "input_text", "text": "Error:"}));
    }
    Value::Array(items)
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

/// Buffered (non-streaming) Chat Completions reply, translated back to the
/// Anthropic response shape.
///
/// Resolves `headroom_retrieve` before translating, on the OpenAI shape the
/// upstream actually returned. The streaming arm does the same through
/// `sse::ccr_stream`; both are required, because this path injects the tool
/// and a tool the client cannot run must never leave the proxy.
async fn handle_buffered_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if status != StatusCode::OK {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        if let Some(ctx) = outcome.as_ref() {
            book_routed_outcome(ctx, None, 0, 0.0, status.as_u16() as i64);
        }
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

    // Resolve any `headroom_retrieve` the model asked for, before the outcome
    // is booked: the continuation rounds are billed too, and booking the first
    // round's usage as the turn's would under-report the retrieval.
    let (openai_body, ccr_rounds) = match ccr {
        Some(ccr) => resolve_routed_ccr(&openai_body, &ccr).await,
        None => (openai_body, crate::proxy::CcrRoundUsage::default()),
    };

    if let Some(ctx) = outcome.as_ref() {
        book_routed_outcome_with_ccr(ctx, openai_body.get("usage"), 0, 0.0, 200, ccr_rounds);
    }

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
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if status != StatusCode::OK {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        if let Some(ctx) = outcome.as_ref() {
            book_routed_outcome(ctx, None, 0, 0.0, status.as_u16() as i64);
        }
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

    let (responses_turn, output_tokens) = responses_stream_to_turn(&responses_text);

    // Resolve before the outcome is booked: continuation rounds are billed
    // too, and booking the first round's usage as the turn's would under-report
    // the retrieval. Same ordering as the chat arm.
    let (resolved, ccr_rounds) = match ccr {
        Some(ccr) => resolve_routed_ccr(&responses_turn, &ccr).await,
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

/// Fold a Responses SSE stream into the buffered `output[]` turn the rest of
/// the CCR machinery speaks.
///
/// Tool calls ride this stream as `output[]` items and never as text deltas, so
/// a reader that accumulates only `output_text` sees none of them — which is
/// how every call on this path, `headroom_retrieve` included, used to vanish.
/// Returns the turn and the output-token count the outcome is booked with.
fn responses_stream_to_turn(responses_text: &str) -> (Value, u64) {
    let mut current_event: Option<String> = None;
    let mut current_data: Vec<String> = Vec::new();
    let mut assistant_text = String::new();
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    // The whole usage block, kept for the request outcome — the two counters
    // above drop the cache details the funnel wants.
    let mut usage_seen: Option<Value> = None;
    let mut output_items: Vec<Value> = Vec::new();
    let mut response_id: Option<String> = None;

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
            Some("response.output_item.done") => {
                if let Some(item) = chunk.get("item").filter(|v| v.is_object()) {
                    output_items.push(item.clone());
                }
            }
            Some("response.completed") => {
                let response = chunk.get("response");
                // `response.completed` carries the finished `output[]`, so it
                // wins over the items gathered frame by frame: a call whose
                // `output_item.done` never arrived is still in here.
                if let Some(items) = response
                    .and_then(|v| v.get("output"))
                    .and_then(Value::as_array)
                {
                    output_items = items.clone();
                }
                if let Some(id) = response.and_then(|v| v.get("id")).and_then(Value::as_str) {
                    response_id = Some(id.to_string());
                }
                if let Some(usage) = response.and_then(|v| v.get("usage")) {
                    if let Some(tokens) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        input_tokens = tokens;
                    }
                    if let Some(tokens) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        output_tokens = tokens;
                    }
                    usage_seen = Some(usage.clone());
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

    // Deltas and items are two views of one turn. A stream that sent its text
    // only as deltas still needs it carried; one that already sent a `message`
    // item must not have it carried twice. Keying this off "no items at all"
    // instead would drop the text of any turn that also made a tool call. Text
    // leads the calls it introduces, so it goes first.
    let has_message = output_items
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("message"));
    if !has_message && !assistant_text.is_empty() {
        output_items.insert(
            0,
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": assistant_text}],
            }),
        );
    }
    let mut responses_turn = json!({
        "output": output_items,
        "usage": usage_seen.clone().unwrap_or_else(|| json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        })),
    });
    if let Some(id) = response_id {
        responses_turn["id"] = json!(id);
    }

    (responses_turn, output_tokens)
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

pub(crate) fn openai_to_anthropic_response(openai: &Value, original: &Value) -> Value {
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

async fn handle_streaming_response(
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
    let body = match ccr {
        Some(ccr) => {
            let anthropic_request = original.clone();
            let ctx = crate::sse::ccr_stream::CcrStreamContext {
                client: ccr.client,
                upstream_url: match url::Url::parse(&ccr.upstream_url) {
                    Ok(u) => u,
                    Err(e) => {
                        // Unparseable upstream means no continuation is
                        // possible. Stream on untouched rather than fail the
                        // turn; the client is no worse off than before.
                        tracing::warn!(
                            event = "routed_ccr_bad_upstream_url",
                            error = %e,
                            url = %ccr.upstream_url,
                            "cannot resolve headroom_retrieve on this turn"
                        );
                        return streaming_body_response(axum::body::Body::from_stream(
                            translated_stream,
                        ));
                    }
                },
                outgoing_headers: ccr.headers,
                original_request: ccr.request_body,
                ccr_store: ccr.store,
                config: ccr.config,
                request_id: ccr.request_id,
                shape: if ccr.responses_shape {
                    crate::sse::ccr_stream::CcrShape::RoutedResponses { anthropic_request }
                } else {
                    crate::sse::ccr_stream::CcrShape::RoutedChat { anthropic_request }
                },
                // Routed requests are translated before they leave, so the
                // memory tools were never injected into them — nothing here
                // to own.
                memory: None,
            };
            let (rewritten, _usage) =
                crate::sse::ccr_stream::rewrite_anthropic_stream(translated_stream, ctx);
            axum::body::Body::from_stream(rewritten)
        }
        None => axum::body::Body::from_stream(translated_stream),
    };

    streaming_body_response(body)
}

/// The SSE response envelope every routed streaming reply uses.
fn streaming_body_response(body: axum::body::Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .expect("static response")
}

/// Metadata threaded into [`StreamTranslator`] so a completed routed turn books
/// the same [`RequestOutcome`] a Claude turn does.
///
/// Without this the translate path never touches the cost tracker, savings
/// tracker, or request logger, so codex traffic is absent from `/stats`,
/// `/stats-history`, and the dashboard — the spend is real but invisible.
#[derive(Clone)]
struct RoutedOutcomeContext {
    sink: std::sync::Arc<crate::proxy::ProxyOutcomeSink>,
    request_id: String,
    /// The *upstream* model, never the client-facing alias. Pricing resolves by
    /// name prefix alone, so booking `claude-codex-5.6` would silently bill
    /// OpenAI tokens at Sonnet rates via the `claude-` family fallback.
    model: String,
    /// `openai_responses` or `openai_chat`, matching the labels `forward_http`
    /// uses for the same wire formats.
    provider: String,
    client: Option<String>,
    project: Option<String>,
    tokens_saved: i64,
    transforms_applied: Vec<String>,
    num_messages: i64,
    started_at: std::time::Instant,
    /// Time spent in headroom's own transforms, as distinct from waiting on
    /// upstream.
    overhead_ms: f64,
    /// Request-side estimate used when an error body carries no usage block.
    forwarded_tokens_estimate: i64,
    upstream_attempts: i64,
    /// `Some` when the prefix-replay stage parked this turn. The store needs
    /// the response's cache-token counts to decide how much of the prefix the
    /// provider actually held, so a parked turn must be completed.
    replay_store: Option<crate::cache_stabilization::prefix_replay::SessionReplayStore>,
    session_key: String,
    /// CTX-7 observer, to close out the entry parked at request time.
    usage_observer:
        Option<std::sync::Arc<crate::cache_stabilization::usage_observer::UsageObserver>>,
}

/// Safety net for turns that never reach a terminal event — a client
/// disconnect, or an upstream that drops the connection mid-stream. Those
/// tokens were still spent and still cost money, and the Claude path books
/// them too (its state machine emits when the channel closes, however it
/// closed). `emit_outcome` is idempotent, so this is a no-op for the ordinary
/// case where `response.completed` or `[DONE]` already booked the turn.
impl Drop for StreamTranslator {
    fn drop(&mut self) {
        self.finish_rate_limit_observation();
        if self.outcome.is_some() && !self.outcome_emitted {
            let usage = self.last_usage.clone();
            self.emit_outcome(usage.as_ref(), 200);
        }
    }
}

/// Book a finished routed turn through the shared outcome funnel.
///
/// `usage` is the provider's own block, in whichever shape the endpoint uses.
/// Cache accounting follows the OpenAI convention the Claude path already
/// encodes for these providers: `input_tokens` *includes* the cached prefix,
/// so uncached is the difference. (Anthropic's own `input_tokens` already
/// excludes it — getting this backwards would double-count the prefix.)
fn book_routed_outcome(
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
fn book_routed_outcome_with_ccr(
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
    /// Where to file a `rate_limits` object if one appears in the stream.
    /// `None` in unit tests, which do not exercise quota reporting.
    codex_limits: Option<crate::codex_rate_limits::CodexRateLimitStore>,
    /// True when either the response headers or any SSE frame carried quota.
    /// The negative signal is emitted once from `Drop`, which is the actual end
    /// of the upstream stream rather than one ordinary frame that lacked it.
    codex_rate_limits_seen: bool,
    codex_rate_limits_finished: bool,
    /// Where to book the turn once usage arrives. `None` in unit tests, which
    /// assert on translated events rather than metrics.
    outcome: Option<RoutedOutcomeContext>,
    /// Guards against booking one turn twice. A stream can carry a terminal
    /// event *and* a trailing `[DONE]`, and the buffered fallback can fire on
    /// top of that.
    outcome_emitted: bool,
    /// Latched on the first upstream frame — the only point where TTFB is
    /// observable.
    ttfb_ms: f64,
    /// Most recent provider `usage` block seen. Chat Completions delivers it on
    /// a chunk of its own rather than a terminal event, so it has to be held
    /// until the stream ends.
    last_usage: Option<Value>,
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
            codex_limits: None,
            codex_rate_limits_seen: false,
            codex_rate_limits_finished: false,
            outcome: None,
            outcome_emitted: false,
            ttfb_ms: 0.0,
            last_usage: None,
        }
    }

    fn with_codex_limits(mut self, store: crate::codex_rate_limits::CodexRateLimitStore) -> Self {
        self.codex_limits = Some(store);
        self
    }

    fn with_initial_rate_limits_seen(mut self, seen: bool) -> Self {
        self.codex_rate_limits_seen = seen;
        self
    }

    fn finish_rate_limit_observation(&mut self) {
        if self.codex_limits.is_none()
            || self.codex_rate_limits_seen
            || self.codex_rate_limits_finished
        {
            return;
        }
        self.codex_rate_limits_finished = true;
        let request_id = self
            .outcome
            .as_ref()
            .map(|ctx| ctx.request_id.as_str())
            .unwrap_or("unknown");
        tracing::warn!(
            event = "codex_rate_limits_missing",
            request_id = %request_id,
            model = %self.model,
            "routed Codex stream ended without quota in response headers or SSE frames"
        );
    }

    fn with_outcome(mut self, ctx: Option<RoutedOutcomeContext>) -> Self {
        self.outcome = ctx;
        self
    }

    /// Close out the CTX-7 usage observation parked at request time.
    ///
    /// `begin_request` leaves a pending entry keyed by request id; without a
    /// matching `complete` the turn is never classified and the re-cache
    /// watchdog (and the cache-health statusline segment) stays blank.
    ///
    /// The observer takes Anthropic-named counters. The Responses API reports
    /// cache reads but has no cache-creation counter, so zero goes in for
    /// writes — the same mapping used elsewhere on this path.
    fn complete_usage_observation(&self, usage: Option<&Value>) {
        let Some(ctx) = self.outcome.as_ref() else {
            return;
        };
        let Some(observer) = ctx.usage_observer.as_ref() else {
            return;
        };
        let get = |key: &str| -> u64 {
            usage
                .and_then(|u| u.get(key))
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        };
        let cache_read = usage
            .and_then(|u| {
                u.get("input_tokens_details")
                    .or_else(|| u.get("prompt_tokens_details"))
            })
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let class = observer.complete(
            &ctx.request_id,
            get("input_tokens").max(get("prompt_tokens")),
            cache_read,
            0,
            // The Responses API publishes no cache-creation counter at all, so
            // there is no TTL breakdown to split — `None`, not a pair of zeros,
            // which would claim this endpoint wrote nothing at either tier.
            None,
        );
        // Persist it, same as the Claude path: the observer's counters are
        // in-memory and reset on restart.
        if let Some(class) = class {
            use headroom_core::request_outcome::OutcomeSink as _;
            let (reason, wasted) = class.as_record();
            ctx.sink.record_cache_outcome("routed", reason, wasted);
        }
    }

    /// Hand the turn's cache-token counts to the prefix-replay store, which
    /// needs them to judge how much of the prefix the provider actually held.
    ///
    /// Only on a clean completion, matching the Claude path's `MessageStop`
    /// gate: a turn that died mid-stream tells us nothing reliable about the
    /// cache, and recording it would corrupt next turn's replay decision.
    /// The Responses API reports cache *reads* only — there is no write
    /// counter to pass, unlike Anthropic's `cache_creation_input_tokens`.
    fn complete_replay(&self, usage: Option<&Value>) {
        let Some(ctx) = self.outcome.as_ref() else {
            return;
        };
        let Some(store) = ctx.replay_store.as_ref() else {
            return;
        };
        let cache_read = usage
            .and_then(|u| {
                u.get("input_tokens_details")
                    .or_else(|| u.get("prompt_tokens_details"))
            })
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        store.complete(&ctx.request_id, cache_read, 0);
    }

    /// Latch time-to-first-byte. Written once and never overwritten, mirroring
    /// `latch_ttfb` on the Claude path.
    fn latch_ttfb(&mut self) {
        if self.ttfb_ms == 0.0 {
            if let Some(ctx) = self.outcome.as_ref() {
                self.ttfb_ms = ctx.started_at.elapsed().as_secs_f64() * 1000.0;
            }
        }
    }

    /// Book the finished turn through the shared outcome funnel.
    ///
    /// `usage` is the provider's own block, in whichever shape the endpoint
    /// uses. Cache accounting follows the OpenAI convention the Claude path
    /// already encodes for these providers: `input_tokens` *includes* the
    /// cached prefix, so uncached is the difference. (Anthropic's own
    /// `input_tokens` already excludes it — getting this backwards would
    /// double-count the prefix.)
    fn emit_outcome(&mut self, usage: Option<&Value>, status_code: i64) {
        if self.outcome_emitted {
            return;
        }
        let Some(ctx) = self.outcome.as_ref() else {
            return;
        };
        self.outcome_emitted = true;
        book_routed_outcome(
            ctx,
            usage,
            self.total_output_tokens as i64,
            self.ttfb_ms,
            status_code,
        );
    }

    #[cfg(test)]
    fn process_line(&mut self, line: &str) -> Vec<String> {
        self.process_frame(None, line)
    }

    fn process_frame(&mut self, event_name: Option<&str>, data: &str) -> Vec<String> {
        let mut events = Vec::new();
        self.latch_ttfb();

        if data.trim().is_empty() || data.trim() == "[DONE]" {
            if data.trim() == "[DONE]" {
                // Last chance to book the turn: Chat Completions has no
                // terminal event, and a Responses stream can be cut off before
                // one arrives. No-op when a terminal event already booked it.
                let usage = self.last_usage.clone();
                self.emit_outcome(usage.as_ref(), 200);
            }
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
            if !usage.is_null() {
                self.last_usage = Some(usage.clone());
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

        // Quota can ride in the stream as well as the headers, and which one
        // carries it has changed before. Take it from wherever it shows up.
        if let Some(store) = self.codex_limits.as_ref() {
            if let Some(limits) = crate::codex_rate_limits::extract_rate_limits(&chunk) {
                store.record_rate_limits(&self.model, limits);
                self.codex_rate_limits_seen = true;
            }
        }

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
                let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
                self.complete_replay(usage.as_ref());
                self.complete_usage_observation(usage.as_ref());
                self.emit_outcome(usage.as_ref(), 200);
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
                // A completed response can still carry `incomplete_details`,
                // and truncation outranks a tool call: a `tool_use` stop on a
                // cut-off turn would have the client run a half-streamed call.
                let truncated = chunk
                    .get("response")
                    .and_then(|v| v.get("incomplete_details"))
                    .and_then(|v| v.get("reason"))
                    .and_then(|v| v.as_str())
                    == Some("max_output_tokens");
                let stop_reason = if truncated {
                    "max_tokens"
                } else if self.saw_tool_use {
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
                // Booked as a 500 so the outcome funnel routes it to
                // `record_failed` — a failed turn must not feed the save-rate.
                let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
                self.emit_outcome(usage.as_ref(), 500);
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
                // Outside the `if let`: a response that stopped short still
                // spent tokens, whether or not it said why.
                let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
                self.emit_outcome(usage.as_ref(), 200);
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
    codex_limits: crate::codex_rate_limits::CodexRateLimitStore,
    quota_seen_in_headers: bool,
    outcome: Option<RoutedOutcomeContext>,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::StreamExt;

    let mut translator = StreamTranslator::new(model)
        .with_codex_limits(codex_limits)
        .with_initial_rate_limits_seen(quota_seen_in_headers)
        .with_outcome(outcome);
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
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct EventCapture(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            use tracing::field::{Field, Visit};
            struct Visitor(String);
            impl Visit for Visitor {
                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!("{}={value:?} ", field.name()));
                }
                fn record_str(&mut self, field: &Field, value: &str) {
                    self.0.push_str(&format!("{}={value} ", field.name()));
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    /// Point the durable savings ledger at a throwaway file.
    ///
    /// `emit_request_outcome` appends to it whenever a turn saved tokens, and
    /// it resolves to `~/.headroom/savings_events.jsonl` by default — so
    /// without this the tests below write fake savings into the developer's
    /// real ledger, which `headroom savings` then reports. The temp dir is
    /// leaked deliberately: the path has to stay valid for the whole test
    /// binary, and the OS reclaims it.
    fn redirect_savings_ledger() {
        static LEDGER: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let path = LEDGER.get_or_init(|| {
            let dir = std::mem::ManuallyDrop::new(tempfile::tempdir().expect("tempdir"));
            dir.path().join("savings_events.jsonl")
        });
        std::env::set_var("HEADROOM_SAVINGS_EVENTS_PATH", path);
    }

    #[test]
    fn responses_stream_keeps_tool_calls() {
        let stream = concat!(
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",",
            "\"name\":\"headroom_retrieve\",\"arguments\":\"{}\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_1\",\"usage\":",
            "{\"input_tokens\":10,\"output_tokens\":5}}}\n",
            "\n",
        );

        let (turn, output_tokens) = responses_stream_to_turn(stream);

        assert_eq!(output_tokens, 5);
        assert_eq!(turn["id"], "resp_1");
        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 1, "tool call was dropped: {turn}");
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["name"], "headroom_retrieve");
    }

    #[test]
    fn responses_stream_keeps_text_alongside_a_tool_call() {
        let stream = concat!(
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"looking that up\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",",
            "\"name\":\"headroom_retrieve\",\"arguments\":\"{}\"}}\n",
            "\n",
        );

        let (turn, _) = responses_stream_to_turn(stream);

        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 2, "text was dropped: {turn}");
        // Text leads the call it introduces.
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "looking that up");
        assert_eq!(items[1]["type"], "function_call");
    }

    #[test]
    fn responses_stream_does_not_duplicate_text_already_sent_as_an_item() {
        let stream = concat!(
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hello\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"message\",\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}}\n",
            "\n",
        );

        let (turn, _) = responses_stream_to_turn(stream);

        let items = turn["output"].as_array().expect("output array");
        assert_eq!(items.len(), 1, "text carried twice: {turn}");
    }

    /// Minimal `AppState` for exercising the request-side stages.
    fn test_state(configure: impl FnOnce(&mut crate::config::Config)) -> AppState {
        let mut config =
            crate::config::Config::for_test(url::Url::parse("http://upstream:8080").unwrap());
        configure(&mut config);
        AppState {
            started_at: std::time::Instant::now(),
            config: std::sync::Arc::new(config),
            client: reqwest::Client::new(),
            bedrock_credentials: None,
            drift_state: crate::cache_stabilization::drift_detector::DriftState::new(8),
            tool_order_state: crate::cache_stabilization::tool_order::ToolOrderStore::default(),
            beta_sticky: crate::cache_stabilization::beta_sticky::BetaStickyState::new(8),
            replay_store: crate::cache_stabilization::prefix_replay::SessionReplayStore::new(8),
            working_dir_pins: crate::cache_stabilization::working_dir::WorkingDirPins::new(8),
            usage_observer: std::sync::Arc::new(
                crate::cache_stabilization::usage_observer::UsageObserver::new(),
            ),
            codex_rate_limits: crate::codex_rate_limits::CodexRateLimitStore::new(),
            ctx_observer: None,
            ctx_offload: None,
            ctx_inject: None,
            ccr_context_tracker: None,
            cost_tracker: std::sync::Arc::new(headroom_core::cost_tracker::CostTracker::new(
                None, "monthly",
            )),
            savings_tracker: std::sync::Arc::new(
                headroom_core::savings_tracker::SavingsTracker::new(None, false),
            ),
            request_logger: std::sync::Arc::new(crate::request_logger::RequestLogger::new(None)),
            vertex_token_source: std::sync::Arc::new(crate::vertex::StaticTokenSource::new(
                "test".to_string(),
            )),
            dynamic_upstream: crate::cc_switch_reconciler::new_dynamic_upstream(),
            ws_sessions: std::sync::Arc::new(std::sync::Mutex::new(
                crate::ws_session_registry::WebSocketSessionRegistry::new(),
            )),
            rate_limiter: None,
            semantic_cache: None,
            memory_handler: None,
            probe_recorder: None,
            compression_feedback: None,
            trusted_gateway_cidrs: vec![],
            background_compressor: None,
            compression_failure_action: crate::compression_failure::CompressionFailureAction {
                refuse: false,
                reason: "test".into(),
                frame_bytes: 0,
            },
            batch_context_store: std::sync::Arc::new(headroom_core::ccr::BatchContextStore::new(
                std::time::Duration::from_secs(86_400),
                10_000,
            )),
        }
    }

    fn conversation(tail: &str) -> Value {
        json!({
            "model": "claude-codex-5.6",
            "messages": [
                {"role": "user", "content": "first turn"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": tail}
            ]
        })
    }

    /// Compression is off unless the operator turned it on — the routed path
    /// must not start rewriting bodies that the Claude path would forward
    /// untouched.
    #[test]
    fn routed_compression_is_off_by_default() {
        let state = test_state(|c| {
            c.compression = false;
            c.compression_mode = crate::config::CompressionMode::Off;
            c.prefix_replay = false;
        });
        let mut body = conversation("hello");
        let before = body.clone();
        let report =
            apply_compression_and_replay(&state, &mut body, &HeaderMap::new(), "req-1", "sess-1");
        assert_eq!(body, before, "body must forward byte-equal");
        assert_eq!(report.tokens_saved, 0);
        assert!(!report.replay_parked);
    }

    /// `x-headroom-bypass` wins over the config, same as on the Claude path.
    #[test]
    fn routed_compression_honours_the_bypass_header() {
        let state = test_state(|c| {
            c.compression = true;
            c.compression_mode = crate::config::CompressionMode::AllMessages;
            c.prefix_replay = false;
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-headroom-bypass", HeaderValue::from_static("true"));
        let mut body = conversation("hello");
        let before = body.clone();
        let report = apply_compression_and_replay(&state, &mut body, &headers, "req-2", "sess-2");
        assert_eq!(body, before);
        assert!(report.transforms_applied.is_empty());
    }

    /// A conversation whose client-side `cache_control` markers move each turn
    /// — exactly the churn the replay stage exists to absorb. The stage
    /// rewrites these, so the forwarded bytes differ from the input and the
    /// replay assertion below cannot pass vacuously.
    fn conversation_with_cache_markers(marker_on: usize) -> Value {
        let mut messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "first turn"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "second turn"}]}),
        ];
        messages[marker_on]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        json!({"model": "claude-codex-5.6", "messages": messages})
    }

    /// The point of the stage: turn two forwards the bytes turn one forwarded,
    /// so the provider's prompt-cache prefix does not move even though the
    /// client shuffled its `cache_control` breakpoint in between.
    #[test]
    fn prefix_replay_reuses_the_previously_forwarded_prefix() {
        let state = test_state(|c| {
            c.compression = false;
            c.compression_mode = crate::config::CompressionMode::Off;
            c.prefix_replay = true;
        });
        let headers = HeaderMap::new();

        let mut turn1 = conversation_with_cache_markers(0);
        let raw1 = turn1["messages"].as_array().unwrap().clone();
        let r1 = apply_compression_and_replay(&state, &mut turn1, &headers, "req-a", "sess-x");
        assert!(r1.replay_parked, "turn one must park for turn two");
        let forwarded1 = turn1["messages"].as_array().unwrap().clone();
        assert_ne!(
            forwarded1, raw1,
            "guard: the stage must rewrite something, else the assertion below proves nothing"
        );

        // Close the turn out the way a clean stream does, then extend the
        // conversation append-only with the marker moved, as a client does.
        state.replay_store.complete("req-a", 1_000, 0);
        let mut turn2 = conversation_with_cache_markers(2);
        turn2["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": [{"type": "text", "text": "third"}]}));
        apply_compression_and_replay(&state, &mut turn2, &headers, "req-b", "sess-x");

        let forwarded2 = turn2["messages"].as_array().unwrap();
        assert_eq!(
            forwarded2.len(),
            forwarded1.len() + 1,
            "only the new message should be appended"
        );
        // Compared with `cache_control` stripped, matching the contract: the
        // replayed prefix is byte-identical in *content*, while the single
        // ephemeral breakpoint is deliberately re-placed on the new last
        // message each turn. That re-placement is the mechanism keeping the
        // marker count bounded — Anthropic hard-errors above four.
        assert_eq!(
            strip_cache_control(&forwarded2[..forwarded1.len()]),
            strip_cache_control(&forwarded1),
            "the replayed prefix must match what turn one forwarded"
        );
        let markers = forwarded2
            .iter()
            .filter(|m| {
                m["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
            })
            .count();
        assert_eq!(markers, 1, "markers must not accumulate across turns");
    }

    fn strip_cache_control(messages: &[Value]) -> Vec<Value> {
        messages
            .iter()
            .map(|m| {
                let mut m = m.clone();
                if let Some(blocks) = m["content"].as_array_mut() {
                    for b in blocks {
                        if let Some(obj) = b.as_object_mut() {
                            obj.remove("cache_control");
                        }
                    }
                }
                m
            })
            .collect()
    }

    /// The append-only guard: when an earlier message actually changed, the
    /// stored prefix no longer describes this conversation and replaying it
    /// would forward content the client did not send.
    #[test]
    fn prefix_replay_declines_when_history_was_rewritten() {
        let state = test_state(|c| {
            c.compression = false;
            c.compression_mode = crate::config::CompressionMode::Off;
            c.prefix_replay = true;
        });
        let headers = HeaderMap::new();

        let mut turn1 = conversation_with_cache_markers(0);
        apply_compression_and_replay(&state, &mut turn1, &headers, "req-a", "sess-y");
        state.replay_store.complete("req-a", 1_000, 0);

        // Rewrite history rather than appending to it.
        let mut turn2 = conversation_with_cache_markers(0);
        turn2["messages"][0]["content"][0]["text"] = json!("a different first turn");
        let expected_tail = turn2["messages"][0].clone();
        apply_compression_and_replay(&state, &mut turn2, &headers, "req-b", "sess-y");

        assert_eq!(
            turn2["messages"][0]["content"][0]["text"], expected_tail["content"][0]["text"],
            "the client's own first message must survive, not turn one's"
        );
    }

    /// CCR's retrieve tool only extends an existing `tools` array — the Claude
    /// path does not create one here, and a routed request must not either.
    #[tokio::test]
    async fn ccr_tool_is_injected_only_when_the_request_carries_tools() {
        let state = test_state(|c| c.ccr_inject_tool = true);
        let headers = HeaderMap::new();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let mut with_tools = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        });
        apply_ctx_request_transforms(&state, &mut with_tools, &headers, &addr, "req-test").await;
        let names: Vec<&str> = with_tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"headroom_retrieve"), "got {names:?}");

        let mut without_tools = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "hi"}]
        });
        apply_ctx_request_transforms(&state, &mut without_tools, &headers, &addr, "req-test").await;
        assert!(
            without_tools.get("tools").is_none(),
            "a request with no tools array must not grow one"
        );
    }

    /// Injecting the same tool twice would send the model a duplicate
    /// definition and move the cached prefix every turn.
    #[tokio::test]
    async fn ccr_tool_injection_is_idempotent() {
        let state = test_state(|c| c.ccr_inject_tool = true);
        let headers = HeaderMap::new();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let mut body = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        });
        apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test").await;
        apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test").await;
        let count = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == "headroom_retrieve")
            .count();
        assert_eq!(count, 1);
    }

    /// `--ccr-inject-tool` defaults to *true*, so the retrieve tool is the one
    /// stage that lands without being asked for — on both paths. Everything
    /// else here stays dormant until its flag is set.
    #[tokio::test]
    async fn only_ccr_injects_under_default_config() {
        let state = test_state(|_| {});
        assert!(
            state.config.ccr_inject_tool,
            "guard: this test encodes the shipped default"
        );
        let headers = HeaderMap::new();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let mut body = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        });
        let report =
            apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test").await;
        assert_eq!(report.transforms_applied, vec!["ccr_tool".to_string()]);
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    }

    /// Ordering guarantee: compaction runs after injection, so an injected
    /// tool is compacted like any other rather than slipping in behind it.
    #[test]
    fn tool_schema_compaction_strips_injected_tool_noise() {
        let mut body = json!({
            "model": "claude-codex-5.6",
            "tools": [{
                "name": "headroom_retrieve",
                "input_schema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "title": "Retrieve",
                    "type": "object",
                    "properties": {"hash": {"type": "string"}}
                }
            }]
        });
        let (changed, saved) = apply_tool_schema_compaction(&mut body);
        assert!(changed);
        assert!(saved > 0, "stripping schema noise should save tokens");
        let schema = &body["tools"][0]["input_schema"];
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("title").is_none());
        assert_eq!(schema["properties"]["hash"]["type"], "string");
    }

    /// A late MCP handshake splicing tools into the middle of the array moves
    /// the cached prefix. Stabilization replays last turn's order and appends
    /// genuinely-new tools at the end.
    #[test]
    fn tool_order_is_stable_when_a_late_tool_appears() {
        let store = crate::cache_stabilization::tool_order::ToolOrderStore::default();
        let tools = |names: &[&str]| {
            json!({
                "model": "claude-codex-5.6",
                "tools": names
                    .iter()
                    .map(|n| json!({"name": n, "input_schema": {"type": "object"}}))
                    .collect::<Vec<_>>()
            })
        };
        let order_of = |v: &Value| -> Vec<String> {
            v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        };

        let mut turn1 = tools(&["Read", "Write"]);
        apply_bytes_stage(&mut turn1, |b| {
            crate::proxy::maybe_stabilize_tool_order(b, &store, "sess-order", "r1")
        });
        assert_eq!(order_of(&turn1), vec!["Read", "Write"]);

        // An MCP server registers and the client splices its tool in first.
        let mut turn2 = tools(&["mcp__late__tool", "Read", "Write"]);
        apply_bytes_stage(&mut turn2, |b| {
            crate::proxy::maybe_stabilize_tool_order(b, &store, "sess-order", "r2")
        });
        assert_eq!(
            order_of(&turn2),
            vec!["Read", "Write", "mcp__late__tool"],
            "the established prefix must keep its order, new tools go last"
        );
    }

    /// `prompt_cache_key` belongs to the OpenAI request shape, so it is
    /// injected after translation — and only for PAYG callers.
    #[test]
    fn prompt_cache_key_is_injected_only_for_payg() {
        use crate::cache_stabilization::openai_cache_key::OpenAiShape;
        let inject = |auth| {
            let mut body = json!({"model": "gpt-5.6-luna", "input": [], "store": false});
            apply_bytes_stage(&mut body, |b| {
                crate::proxy::maybe_inject_openai_prompt_cache_key(
                    b,
                    OpenAiShape::Responses,
                    auth,
                    "r1",
                    "/v1/responses",
                )
            });
            body
        };
        assert!(
            inject(headroom_core::auth_mode::AuthMode::Payg)
                .get("prompt_cache_key")
                .is_some(),
            "a PAYG caller should get a synthesised key"
        );
        assert!(
            inject(headroom_core::auth_mode::AuthMode::Subscription)
                .get("prompt_cache_key")
                .is_none(),
            "a subscription caller is fingerprinted upstream; injecting would work against them"
        );
    }

    /// The bytes adapter must leave the body alone when a stage hands back
    /// something unparseable, rather than dropping the request on the floor.
    #[test]
    fn bytes_stage_adapter_preserves_the_body_on_failure() {
        let mut body = json!({"model": "m", "messages": []});
        let before = body.clone();
        apply_bytes_stage(&mut body, |_| bytes::Bytes::from_static(b"not json"));
        assert_eq!(body, before);
    }

    /// Compression is wired to a live dispatcher, not just gated correctly:
    /// a body with a compressible block must come back smaller.
    #[test]
    fn routed_compression_actually_shrinks_a_compressible_body() {
        let state = test_state(|c| {
            c.compression = true;
            c.compression_mode = crate::config::CompressionMode::AllMessages;
            c.prefix_replay = false;
        });
        // Repeated whitespace-heavy log output: the shape the live-zone
        // strategies are built for.
        let noisy = "ERROR   module.rs:12    something failed\n".repeat(400);
        let mut body = json!({
            "model": "claude-codex-5.6",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": noisy},
                    {"type": "text", "text": "what went wrong?"}
                ]
            }]
        });
        let before = serde_json::to_string(&body).unwrap().len();
        let report =
            apply_compression_and_replay(&state, &mut body, &HeaderMap::new(), "req-c", "sess-c");
        let after = serde_json::to_string(&body).unwrap().len();
        assert!(
            report.tokens_saved > 0,
            "expected a real saving, got {} (bytes {before} -> {after})",
            report.tokens_saved
        );
        assert!(after < before, "body should shrink: {before} -> {after}");
        assert!(
            !report.transforms_applied.is_empty(),
            "the strategy that ran should be named in the outcome"
        );
    }

    /// Regression for observation item 15: a conversation-sized CTX saving
    /// used to survive into later routed turns even when the live-zone
    /// dispatcher did nothing. The booked value must be this turn's measured
    /// compression result, including zero, never the incoming CTX value.
    #[test]
    fn routed_booking_does_not_reemit_ctx_savings_without_compression() {
        let mut ctx_report = CtxTransformReport {
            transforms_applied: vec!["ctx_offload".to_string()],
            tokens_saved: 4_522,
            session_key: "sess-stale".to_string(),
        };
        let compression_report = CompressionReport::default();

        let separately_measured_ctx =
            merge_routed_compression_report(&mut ctx_report, compression_report);

        assert_eq!(separately_measured_ctx, 4_522);
        assert_eq!(
            ctx_report.tokens_saved, 0,
            "no routed compression means the outcome must book zero, not a stale CTX value"
        );
        assert_eq!(ctx_report.transforms_applied, vec!["ctx_offload"]);
    }

    /// A different session must not inherit another's prefix.
    #[test]
    fn prefix_replay_is_scoped_to_its_session() {
        let state = test_state(|c| {
            c.compression = false;
            c.compression_mode = crate::config::CompressionMode::Off;
            c.prefix_replay = true;
        });
        let headers = HeaderMap::new();
        let mut a = conversation("session a");
        apply_compression_and_replay(&state, &mut a, &headers, "req-a", "sess-a");
        state.replay_store.complete("req-a", 1_000, 0);

        let mut b = conversation("session b");
        let before = b.clone();
        apply_compression_and_replay(&state, &mut b, &headers, "req-b", "sess-b");
        assert_eq!(
            crate::cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(&b),
            crate::cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(&before),
            "a cold session may gain a marker but must not replay session a"
        );
        let after_messages = b["messages"].as_array().unwrap();
        // Wrapped to block form (every eligible string is), so compare content
        // rather than bytes: session a's prefix would show up as other text.
        assert_eq!(after_messages[0]["content"][0]["text"], "first turn");
        assert_eq!(after_messages[1]["content"][0]["text"], "reply");
        assert!(
            after_messages[..2]
                .iter()
                .all(|m| m["content"][0]["cache_control"].is_null()),
            "history must not carry the breakpoint — it belongs on the newest message"
        );
        assert_eq!(after_messages[2]["content"][0]["text"], "session b");
        assert_eq!(
            after_messages[2]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    /// Translator wired to real trackers, so a test can assert on what the
    /// outcome funnel recorded rather than on the events it emitted.
    fn translator_with_outcome(
        model: &str,
        tokens_saved: i64,
    ) -> (
        StreamTranslator,
        std::sync::Arc<crate::request_logger::RequestLogger>,
        std::sync::Arc<headroom_core::cost_tracker::CostTracker>,
    ) {
        redirect_savings_ledger();
        let cost_tracker = std::sync::Arc::new(headroom_core::cost_tracker::CostTracker::new(
            None, "monthly",
        ));
        let request_logger = std::sync::Arc::new(crate::request_logger::RequestLogger::new(None));
        let ctx = RoutedOutcomeContext {
            sink: std::sync::Arc::new(crate::proxy::ProxyOutcomeSink {
                cost_tracker: cost_tracker.clone(),
                savings_tracker: std::sync::Arc::new(
                    headroom_core::savings_tracker::SavingsTracker::new(None, false),
                ),
                request_logger: request_logger.clone(),
            }),
            request_id: "req-test".to_string(),
            replay_store: None,
            usage_observer: None,
            session_key: "sess-test".to_string(),
            model: model.to_string(),
            provider: "openai_responses".to_string(),
            client: None,
            project: None,
            tokens_saved,
            transforms_applied: vec!["ctx_offload".to_string()],
            num_messages: 3,
            started_at: std::time::Instant::now(),
            overhead_ms: 1.5,
            forwarded_tokens_estimate: 777,
            upstream_attempts: 1,
        };
        let t = StreamTranslator::new(model.to_string()).with_outcome(Some(ctx));
        (t, request_logger, cost_tracker)
    }

    #[test]
    fn stream_end_emits_one_joinable_missing_quota_event() {
        use tracing_subscriber::layer::SubscriberExt;

        let (translator, _logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
        let capture = EventCapture::default();
        let lines = capture.0.clone();
        let subscriber = tracing_subscriber::registry().with(capture);
        tracing::subscriber::with_default(subscriber, || {
            let mut translator =
                translator.with_codex_limits(crate::codex_rate_limits::CodexRateLimitStore::new());
            translator.finish_rate_limit_observation();
            translator.finish_rate_limit_observation();
        });

        let joined = lines.lock().unwrap().join("\n");
        let missing: Vec<_> = joined
            .lines()
            .filter(|line| line.contains("codex_rate_limits_missing"))
            .collect();
        assert_eq!(missing.len(), 1, "{joined}");
        assert!(missing[0].contains("request_id=req-test"), "{joined}");
    }

    #[test]
    fn observed_stream_quota_suppresses_the_missing_event() {
        use tracing_subscriber::layer::SubscriberExt;

        let (translator, _logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
        let capture = EventCapture::default();
        let lines = capture.0.clone();
        let subscriber = tracing_subscriber::registry().with(capture);
        tracing::subscriber::with_default(subscriber, || {
            let mut translator =
                translator.with_codex_limits(crate::codex_rate_limits::CodexRateLimitStore::new());
            translator.process_frame(
                Some("response.created"),
                &json!({"rate_limits": {"primary": {"used_percent": 4}}}).to_string(),
            );
            translator.finish_rate_limit_observation();
        });

        let joined = lines.lock().unwrap().join("\n");
        assert!(!joined.contains("codex_rate_limits_missing"), "{joined}");
    }

    /// The gap this closes: routed traffic used to reach no tracker at all, so
    /// codex spend was invisible in /stats and the dashboard.
    // Async because a saving > 0 reaches `record_savings_ledger`, which pushes
    // the flocked disk append onto a blocking thread.
    #[tokio::test]
    async fn completed_responses_stream_books_a_request_outcome() {
        let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 400);
        t.process_frame(
            Some("response.completed"),
            &json!({
                "response": {
                    "usage": {
                        "input_tokens": 10_000,
                        "output_tokens": 250,
                        "input_tokens_details": {"cached_tokens": 9_000}
                    }
                }
            })
            .to_string(),
        );

        let entries = logger.get_recent(10);
        assert_eq!(entries.len(), 1, "the turn should be booked exactly once");
        let e = &entries[0];
        assert_eq!(e.model, "gpt-5.6-luna");
        assert_eq!(e.provider, "openai_responses");
        assert_eq!(e.output_tokens, 250);
        // Forwarded size is what upstream counted; the original is that plus
        // what the transforms removed.
        assert_eq!(e.input_tokens_optimized, 10_000);
        assert_eq!(e.input_tokens_original, 10_400);
        assert_eq!(e.tokens_saved, 400);
        assert!(e.cache_hit, "9k of 10k input tokens were served from cache");
        assert_eq!(e.transforms_applied, vec!["ctx_offload".to_string()]);
    }

    /// A stream carrying both a terminal event and a trailing `[DONE]`, plus
    /// the drop at the end, must still book exactly one turn.
    #[test]
    fn a_turn_is_booked_only_once() {
        let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
        t.process_frame(
            Some("response.completed"),
            &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 1}}}).to_string(),
        );
        t.process_frame(None, "[DONE]");
        drop(t);
        assert_eq!(logger.get_recent(10).len(), 1);
    }

    /// A turn cut off before any terminal event still spent tokens, and the
    /// Claude path books those too.
    #[test]
    fn dropped_stream_still_books_the_turn() {
        let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
        t.process_frame(
            Some("response.output_text.delta"),
            &json!({"delta": "partial"}).to_string(),
        );
        assert_eq!(logger.get_recent(10).len(), 0, "not booked mid-stream");
        drop(t);
        assert_eq!(
            logger.get_recent(10).len(),
            1,
            "dropping the translator books the interrupted turn"
        );
    }

    /// `response.failed` routes to `record_failed`, which deliberately skips
    /// the success funnel — a failed turn must not inflate the save rate.
    #[test]
    fn failed_response_is_not_logged_as_a_served_request() {
        let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 100);
        t.process_frame(
            Some("response.failed"),
            &json!({"response": {"error": {"message": "boom"}}}).to_string(),
        );
        assert_eq!(logger.get_recent(10).len(), 0);
    }

    /// Chat Completions delivers usage on its own chunk rather than a terminal
    /// event, so the numbers have to survive until the stream ends.
    #[test]
    fn chat_completions_usage_is_booked_at_stream_end() {
        let (mut t, logger, _cost) = translator_with_outcome("qwen-local", 0);
        t.process_frame(
            None,
            &json!({
                "choices": [{"delta": {"content": "hi"}}],
                "usage": {"prompt_tokens": 700, "completion_tokens": 20,
                          "prompt_tokens_details": {"cached_tokens": 500}}
            })
            .to_string(),
        );
        t.process_frame(None, "[DONE]");

        let entries = logger.get_recent(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input_tokens_optimized, 700);
        assert_eq!(entries[0].output_tokens, 20);
    }

    /// Unit tests elsewhere in this file build translators with no outcome
    /// context; that must stay a no-op rather than panicking on drop.
    #[test]
    fn translator_without_outcome_context_books_nothing() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        t.process_frame(
            Some("response.completed"),
            &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 1}}}).to_string(),
        );
    }

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
        // Text survives and keeps its order. This image has no media_type so
        // it cannot be carried, and leaves a marker where it stood.
        assert_eq!(
            output["input"][1]["output"],
            "file1\n[unsupported content block omitted: image]\nfile2"
        );
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

    /// A base64 image the backend accepts becomes a real `input_image`, and the
    /// surrounding text keeps its position around it.
    #[test]
    fn tool_result_images_become_input_image_in_place() {
        let out = tool_result_output_value(
            Some(&json!([
                {"type": "text", "text": "before"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="
                }},
                {"type": "text", "text": "after"}
            ])),
            false,
        );
        assert_eq!(
            out,
            json!([
                {"type": "input_text", "text": "before"},
                {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="},
                {"type": "input_text", "text": "after"}
            ])
        );
    }

    /// Whitespace and missing padding are legal on the way in but not in a data
    /// URL, so the payload is normalized rather than forwarded as-is.
    #[test]
    fn tool_result_image_payload_is_normalized() {
        let out = tool_result_output_value(
            Some(&json!([{"type": "image", "source": {
                "type": "base64", "media_type": "image/png", "data": "iVBOR w0KGgo"
            }}])),
            false,
        );
        assert_eq!(out[0]["image_url"], "data:image/png;base64,iVBORw0KGgo=");
    }

    /// What cannot be carried leaves a marker where it stood, so the model is
    /// not told the tool returned less than it did.
    #[test]
    fn uncarriable_tool_result_blocks_leave_placeholders() {
        let out = tool_result_output_value(
            Some(&json!([
                {"type": "text", "text": "before"},
                {"type": "image", "source": {"type": "url", "url": "https://example.invalid/a.png"}},
                {"type": "image", "source": {"type": "base64", "media_type": "text/plain", "data": "aGk="}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "not base64!"}},
                {"type": "tool_reference", "tool_name": "TaskCreate"},
                {"type": "text", "text": "after"}
            ])),
            false,
        );
        // No image survived, so the output stays a plain string.
        assert_eq!(
            out,
            json!(
                "before\n[unsupported content block omitted: image]\n[unsupported content block omitted: image]\n[unsupported content block omitted: image]\n[unsupported content block omitted: tool_reference]\nafter"
            )
        );
    }

    #[test]
    fn tool_result_error_marker_survives_both_shapes() {
        assert_eq!(
            tool_result_output_value(Some(&json!([{"type": "text", "text": "boom"}])), true),
            json!("Error: boom")
        );
        let with_image = tool_result_output_value(
            Some(&json!([
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="
                }}
            ])),
            true,
        );
        assert_eq!(
            with_image[0],
            json!({"type": "input_text", "text": "Error:"})
        );
        assert_eq!(with_image[1]["type"], "input_image");
    }

    /// The string shape must be byte-identical to before for the common case,
    /// or every cached prefix moves.
    #[test]
    fn text_only_tool_results_keep_the_plain_string_shape() {
        assert_eq!(
            tool_result_output_value(Some(&json!("plain")), false),
            json!("plain")
        );
        assert_eq!(
            tool_result_output_value(
                Some(&json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])),
                false
            ),
            json!("a\nb")
        );
        assert_eq!(tool_result_output_value(None, false), json!(""));
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

    /// A turn cut off at the token ceiling must say so, even when it completed
    /// and even when it had started a tool call.
    #[test]
    fn truncated_completion_reports_max_tokens() {
        for (frames, expected) in [
            (
                vec![(
                    "response.completed",
                    r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"output_tokens":9}}}"#,
                )],
                "max_tokens",
            ),
            (
                vec![
                    (
                        "response.output_item.added",
                        r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
                    ),
                    (
                        "response.completed",
                        r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"output_tokens":9}}}"#,
                    ),
                ],
                "max_tokens",
            ),
            (
                vec![
                    (
                        "response.output_item.added",
                        r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
                    ),
                    (
                        "response.completed",
                        r#"{"response":{"usage":{"output_tokens":9}}}"#,
                    ),
                ],
                "tool_use",
            ),
            (
                vec![(
                    "response.completed",
                    r#"{"response":{"usage":{"output_tokens":9}}}"#,
                )],
                "end_turn",
            ),
        ] {
            let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
            let sse = drive(&mut t, &frames);
            assert!(
                sse.contains(&format!(r#""stop_reason":"{expected}""#)),
                "expected {expected}, got: {sse}"
            );
        }
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
        // Present and false on every tool. Omitting it lets codex turn on
        // strict constrained decoding, which fills every optional parameter.
        for tool in tools {
            assert_eq!(
                tool["strict"],
                json!(false),
                "tool {} must pin strict=false",
                tool["name"]
            );
        }
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
        let cfg = crate::compression::ctx_offload::CtxOffloadConfig {
            min_bytes: 50_000,
            exclude_tools: Vec::new(),
            stale_margin: 0,
        };
        let out =
            crate::compression::ctx_offload::offload_anthropic_request(&mut parsed, &cfg, None);
        assert!(out.changed(), "expected a large tool_result to offload");
        let translated = anthropic_to_openai_responses_request(&parsed, false).unwrap();
        let serialized = serde_json::to_string(&translated).unwrap();
        assert!(!serialized.contains(&big), "raw payload must not survive");
        assert!(
            serialized.contains("headroom_retrieve"),
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

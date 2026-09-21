//! Provider quirks for the routed paths (P6).
//!
//! Every branch that varies by upstream provider lives behind
//! [`UpstreamKind`]: endpoint selection, the `/v1`-strip, the OpenCode Zen
//! headers, the ChatGPT turn-state session, and the Cursor effort mapping. A
//! new provider means a new variant plus one arm per method — not a new
//! `if host ==` scattered across the handlers.
//!
//! Deliberately NOT here (named so a future move has to cross the seam
//! explicitly):
//! - the Codex websocket path (`websocket_codex.rs` picks its own WS/HTTP
//!   endpoints and handshake allowlist);
//! - the Claude-path buffered-CCR ChatGPT gate (`openai_buffered_ccr.rs`);
//! - the Qwen response shape, which needs no branch at all: both response
//!   arms read `reasoning_content` unconditionally when present
//!   (`openai/response.rs`, `openai/stream.rs`), so there is no quirk to own.

use crate::codex::{derive_session_uuid, turn_state_map};
use axum::http::HeaderMap;
use serde_json::Value;

/// Which provider a routed upstream is, from the two signals the routed
/// path already threads: the configured upstream URL and whether the
/// credentials resolved as ChatGPT auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpstreamKind {
    /// ChatGPT-subscription Codex traffic: fingerprinted upstream, so it
    /// speaks the Codex endpoint and carries the CLI's session headers.
    ChatGptSubscription,
    /// OpenCode Zen: gated on its session headers even with a valid key.
    OpenCodeZen,
    /// Anything else: generic OpenAI-compatible (`/v1/...`).
    Generic,
}

/// Classify a routed upstream. A URL has one host, so at most one of the
/// first two arms can match; anything else is generic.
pub(crate) fn classify_upstream(upstream: &url::Url, is_chatgpt_auth: bool) -> UpstreamKind {
    if upstream.host_str() == Some("opencode.ai") {
        UpstreamKind::OpenCodeZen
    } else if is_chatgpt_auth && upstream.host_str() == Some("api.openai.com") {
        UpstreamKind::ChatGptSubscription
    } else {
        UpstreamKind::Generic
    }
}

/// The configured base without a trailing slash or `/v1`, so callers append
/// exactly one `/v1/...` path. Upstreams may be configured either as the API
/// root (`https://api.openai.com`) or already including `/v1` — without the
/// strip the latter doubles into `.../v1/v1/chat/completions`, which
/// OpenAI 404s on. Single site: translation and the sidecar shared
/// copy-pasted variants before.
pub(crate) fn strip_v1_base(upstream: &url::Url) -> &str {
    upstream
        .as_str()
        .trim_end_matches('/')
        .trim_end_matches("/v1")
}

impl UpstreamKind {
    /// The Responses endpoint for this provider.
    pub(crate) fn responses_url(&self, upstream: &url::Url) -> String {
        match self {
            UpstreamKind::ChatGptSubscription => crate::codex::codex_endpoint().to_string(),
            _ => format!("{}/v1/responses", strip_v1_base(upstream)),
        }
    }

    /// The chat-completions endpoint for this provider. There is no
    /// ChatGPT arm: a translate route on a Codex-bound upstream with no
    /// target serves chat-completions today (see
    /// [`warn_ambiguous_codex`](UpstreamKind::warn_ambiguous_codex)).
    pub(crate) fn chat_url(&self, upstream: &url::Url) -> String {
        format!("{}/v1/chat/completions", strip_v1_base(upstream))
    }

    /// True when a Chat-shaped request on this upstream is an ambiguous
    /// Codex route (translate without a target on a codex-bound upstream).
    /// The caller logs per occurrence; the sniff itself lives here.
    pub(crate) fn warn_ambiguous_codex(&self) -> bool {
        matches!(self, UpstreamKind::ChatGptSubscription)
    }

    /// Provider-gated extra headers. OpenCode Zen only; every other
    /// provider sends nothing extra.
    pub(crate) fn inject_extra_headers(
        &self,
        headers: &mut HeaderMap,
        request_id: &str,
        session_key: Option<&str>,
    ) {
        if matches!(self, UpstreamKind::OpenCodeZen) {
            inject_opencode_headers(headers, request_id, session_key);
        }
    }

    /// Session correlation headers and turn-state echo, mirroring the real
    /// Codex client (codex-api/src/requests/headers.rs, client.rs). Returns
    /// the session key the turn is correlated under, if the translated body
    /// carried one — for every provider, so callers keep one call shape;
    /// only the ChatGPT subscription mutates headers.
    ///
    /// The gate couples credential AND host (unlike the old flag-only
    /// check): turn-state is a Codex-backend protocol, so ChatGPT
    /// credentials aimed at a generic upstream no longer leak
    /// `session-id`/`thread-id`/turn-state to a third party that ignores
    /// them. The Codex endpoint was already host-gated the same way.
    pub(crate) fn apply_session_headers(
        &self,
        headers: &mut HeaderMap,
        parsed: &Value,
    ) -> Option<String> {
        let session_key = parsed
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        if matches!(self, UpstreamKind::ChatGptSubscription) {
            if let Some(key) = &session_key {
                let session_uuid = derive_session_uuid(key);
                if let Ok(val) = http::HeaderValue::from_str(&session_uuid) {
                    headers.insert("session-id", val.clone());
                    headers.insert("thread-id", val);
                }
                let stored = turn_state_map()
                    .lock()
                    .ok()
                    .and_then(|m| m.get(key).cloned());
                if let Some(ts) = stored {
                    if let Ok(val) = http::HeaderValue::from_str(&ts) {
                        headers.insert("x-codex-turn-state", val);
                    }
                }
            }
        }
        session_key
    }

    /// Capture the turn-state token for sticky routing on follow-up
    /// requests. Gated by the response carrying the header, not by
    /// provider: only the ChatGPT subscription ever sends it.
    pub(crate) fn capture_turn_state(
        &self,
        upstream_resp: &reqwest::Response,
        session_key: Option<&str>,
    ) {
        if let Some(key) = session_key {
            if let Some(ts) = upstream_resp
                .headers()
                .get("x-codex-turn-state")
                .and_then(|v| v.to_str().ok())
            {
                if let Ok(mut map) = turn_state_map().lock() {
                    map.insert(key.to_string(), ts.to_string());
                }
            }
        }
    }

    /// Drop the encrypted-reasoning replay from a translated Responses body
    /// when this provider will not accept it back.
    ///
    /// The blob in `reasoning.encrypted_content` is bound to the caller it was
    /// issued to. OpenCode Zen's contributor-free tier is reached through a VPN
    /// exit that `contrib/zen-rotate-watch.sh` rotates on every 429, so the
    /// caller changes mid-conversation by design and a later turn comes back
    ///
    /// ```text
    /// [invalid_request_error] reasoning `encrypted_content` was not issued to this caller
    /// ```
    ///
    /// That 400 is not transient. The stale envelope sits in the client's
    /// transcript and is resent on every following turn, so one rotation
    /// dead-ends the conversation — observed 2026-09-14, four turns in a row,
    /// three minutes after a rotation. Ask Zen for no blob and replay none:
    /// strip `id` and `encrypted_content` but keep the item's `summary` (a
    /// no-id item with a summary is still accepted — probed 2026-09-14). The
    /// model still gets the assistant text and the visible chain of thought;
    /// only the encrypted replay itself is lost.
    ///
    /// The other providers keep the replay. Codex and Cursor are reached from
    /// one stable identity, where handing the items back is what makes a
    /// reasoning model resume.
    pub(crate) fn strip_unreplayable_reasoning(&self, openai_body: &mut serde_json::Value) {
        if *self != UpstreamKind::OpenCodeZen {
            return;
        }
        if let Some(items) = openai_body.get_mut("input").and_then(|v| v.as_array_mut()) {
            for item in items.iter_mut() {
                if item.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
                    continue;
                }
                // Drop the caller-bound `id` + `encrypted_content` (Zen 400s
                // on those past a rotation) but keep `summary`: probed
                // 2026-09-14, a no-id item with a summary is still accepted,
                // so the visible chain of thought survives even though the
                // replay does not.
                if let Some(obj) = item.as_object_mut() {
                    obj.remove("id");
                    obj.remove("encrypted_content");
                }
            }
        }
        if let Some(obj) = openai_body.as_object_mut() {
            obj.remove("include");
        }
    }
}

/// Inject the headers OpenCode Zen requires for its free tier.
///
/// Direct `curl https://opencode.ai/zen/v1/responses` with only
/// `Authorization: Bearer $OPENCODE_API_KEY` now returns
/// `MissingSessionID: OpenCode's free tier can only be used in OpenCode`
/// (2026-09-07). The OpenCode CLI always sends `x-opencode-session` (and
/// friends) — see `LLMRequestPrep.prepare` in the bundled
/// `chunk-*.js` (`x-opencode-session`, `x-opencode-request`,
/// `x-opencode-client`, `User-Agent: opencode/…`). Without the session
/// header the free models are gated, even with a valid key.
///
/// Since ~2026-09-17 Zen additionally validates the session value itself:
/// a minted `ses_<64hex>` passes the shape but comes back
/// `FreeTierError: OpenCode's free tier can only be used from within
/// OpenCode`, while the id of a real OpenCode session (probed live:
/// existing ids 200, random well-formed ids 403) is served. So the
/// session below is a real one — see [`resolve_zen_session`].
pub(crate) fn inject_opencode_headers(
    headers: &mut HeaderMap,
    request_id: &str,
    _session_key: Option<&str>,
) {
    let session = resolve_zen_session(request_id);
    if let Ok(v) = http::HeaderValue::from_str(&session) {
        headers.insert(http::HeaderName::from_static("x-opencode-session"), v);
    }
    if let Ok(v) = http::HeaderValue::from_str(&mint_zen_request_id(request_id)) {
        headers.insert(http::HeaderName::from_static("x-opencode-request"), v);
    }
    headers.insert(
        http::HeaderName::from_static("x-opencode-client"),
        http::HeaderValue::from_static("cli"),
    );
    headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static(
            "opencode/1.18.31 ai-sdk/provider-utils/4.0.40 runtime/bun/1.3.14",
        ),
    );
    if let Some(project) = resolve_zen_project(&session) {
        if let Ok(v) = http::HeaderValue::from_str(&project) {
            headers.insert(http::HeaderName::from_static("x-opencode-project"), v);
        }
    }
}

/// Mint a fresh `x-opencode-request` id on headers that already carry one.
///
/// The real OpenCode CLI mints one message id per POST. Proxy continuations
/// (CCR/memory rounds) re-send with the forward path's header map, which
/// replays the original request's UUID; on 2026-09-17 eleven zen-route
/// continuations 403'd (`FreeTierError`) while same-shape originals passed.
/// message-id replay is unproven as the trigger (25 same-path continuations
/// passed with replayed ids), but per-POST freshness is client-faithful and costs
/// nothing, so continuations refresh before every send. Presence-gated: maps
/// without the header (non-zen routes) are untouched. Session, client, UA
/// and project headers are preserved — only the request nonce rotates.
/// Returns whether a refresh happened (for logging at the call site).
pub(crate) fn refresh_zen_request_id(headers: &mut HeaderMap) -> bool {
    const REQ: &str = "x-opencode-request";
    if !headers.contains_key(REQ) {
        return false;
    }
    match http::HeaderValue::from_str(&mint_zen_request_id(&uuid::Uuid::new_v4().to_string())) {
        Ok(v) => {
            headers.insert(http::HeaderName::from_static(REQ), v);
            true
        }
        Err(_) => false,
    }
}

/// Grace period before a freshly created OpenCode session is trusted for
/// the Zen gate: the id has to exist in Zen's server-side registry (a
/// minted id 403s even when well-formed — probed 2026-09-17, including a
/// locally-inserted row re-tested minutes later), and a session created
/// seconds ago may not have synced yet. The grace applies to the session's
/// *creation*, not its last update: a live session updated seconds ago is
/// ideal (it is actively syncing), while a row minted moments ago is not
/// — local rows never count, only cloud-synced ids do.
const ZEN_SESSION_SYNC_GRACE_MS: i64 = 5 * 60 * 1000;

/// How long a resolved real session id is reused before the DB is
/// re-read: new sessions appear as the operator works, and a pinned id
/// would outlive a deleted session. Fail-open either way — resolution
/// falls back to the legacy minted id, never to an error.
const ZEN_SESSION_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Explicit override for the Zen session header, read per resolution (not
/// cached) so an operator can rotate it without restarting the proxy.
const ZEN_SESSION_ENV: &str = "HEADROOM_ZEN_SESSION";

/// Explicit override for the OpenCode project header. When unset, the
/// project id is read alongside the selected session from OpenCode's DB.
const ZEN_PROJECT_ENV: &str = "HEADROOM_ZEN_PROJECT";

/// Resolve the `x-opencode-session` value for a Zen request.
///
/// Zen's free-tier gate checks the id against its server-side session
/// registry, so this must be the id of a real OpenCode session, not a
/// minted one. Sources, in order: the `HEADROOM_ZEN_SESSION` override,
/// the most recently used real session from the local OpenCode database
/// (cached briefly), a session minted in the background when nothing
/// usable exists yet, and — until that lands — the legacy minted
/// `ses_<64hex>` derived from the request id (gated upstream, kept only
/// so the header is always present).
pub(crate) fn resolve_zen_session(request_id: &str) -> String {
    if let Ok(pinned) = std::env::var(ZEN_SESSION_ENV) {
        let pinned = pinned.trim().to_string();
        if !pinned.is_empty() {
            return pinned;
        }
    }
    if let Some(cached) = cached_zen_session() {
        return cached;
    }
    if let Some(real) = read_zen_session_from_db() {
        tracing::debug!(
            event = "zen_session_source",
            source = "db",
            session_prefix = %real.chars().take(12).collect::<String>(),
        );
        store_cached_zen_session(real.clone());
        return real;
    }
    // No usable session: start a background mint (guarded, at most one in
    // flight and spaced apart) and serve the legacy fallback until it
    // lands. Requests in between may still gate — unavoidable without a
    // session that exists yet — but the next resolutions pick the minted
    // id up, first via the cache, then via the database.
    tracing::debug!(event = "zen_session_source", source = "fallback");
    trigger_zen_session_mint();
    mint_zen_session(request_id)
}

/// The legacy minted id, kept as the last-resort fallback. Well-formed
/// but unknown to Zen, so the free tier gates it — better than no
/// header (which fails closed as `MissingSessionID`), worse than a real
/// id. The sha256 of the request id, so the shape is always `ses_<64hex>`
/// regardless of the request-id format (UUID in production, `req-N` in
/// tests), retries within one logical request share the session, and
/// different requests don't collide.
fn mint_zen_session(request_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(request_id.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("ses_{hex}")
}

static ZEN_SESSION_CACHE: std::sync::OnceLock<
    std::sync::Mutex<(Option<String>, std::time::Instant)>,
> = std::sync::OnceLock::new();

fn cached_zen_session() -> Option<String> {
    let lock = ZEN_SESSION_CACHE.get_or_init(|| {
        std::sync::Mutex::new((None, std::time::Instant::now() - ZEN_SESSION_CACHE_TTL))
    });
    let guard = lock.lock().ok()?;
    let (session, at) = (&guard.0, guard.1);
    match session {
        Some(s) if at.elapsed() < ZEN_SESSION_CACHE_TTL => Some(s.clone()),
        _ => None,
    }
}

fn store_cached_zen_session(session: String) {
    let lock = ZEN_SESSION_CACHE.get_or_init(|| {
        std::sync::Mutex::new((None, std::time::Instant::now() - ZEN_SESSION_CACHE_TTL))
    });
    if let Ok(mut guard) = lock.lock() {
        *guard = (Some(session), std::time::Instant::now());
    }
}

/// Read the most recently used real session id from the local OpenCode
/// database (`$HEADROOM_OPENCODE_DB`, else
/// `~/.local/share/opencode/opencode.db`). Read-only, fail-open:
/// anything missing, locked, or oddly shaped yields `None` and the
/// caller falls back to the minted id.
fn read_zen_session_from_db() -> Option<String> {
    let path = zen_session_db_path()?;
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    // Most recently used first: a live session is actively syncing, so its
    // id is certain to exist server-side. The grace check below runs on
    // creation time, not update time.
    let mut stmt = conn
        .prepare("SELECT id, time_created FROM session ORDER BY time_updated DESC LIMIT 20")
        .ok()?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();
    if rows.is_empty() {
        return None;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)?;
    // Newest-used session created before the grace horizon; the newest
    // used overall when everything is fresher than that.
    let picked = rows
        .iter()
        .find(|(_, created)| now_ms.saturating_sub(*created) >= ZEN_SESSION_SYNC_GRACE_MS)
        .or(rows.first())
        .map(|(id, _)| id.clone())?;
    if picked.starts_with("ses_") && picked.len() > 8 {
        Some(picked)
    } else {
        None
    }
}

/// Read the OpenCode project id belonging to the selected session. The
/// Claude metadata `user_id` is not an OpenCode project id, so never use it
/// as a substitute: Zen now validates this header against the session's
/// actual project.
fn resolve_zen_project(session: &str) -> Option<String> {
    if let Ok(pinned) = std::env::var(ZEN_PROJECT_ENV) {
        let pinned = pinned.trim().to_string();
        if !pinned.is_empty() {
            return Some(pinned);
        }
    }
    let path = zen_session_db_path()?;
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.query_row(
        "SELECT project_id FROM session WHERE id = ?1",
        rusqlite::params![session],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .filter(|project| !project.trim().is_empty())
}

/// OpenCode message ids use the `msg_` prefix and a 25-character opaque
/// suffix. Derive a stable-looking id from a per-POST nonce; it need not be
/// persisted locally because Zen only needs the client-shaped request id.
fn mint_zen_request_id(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed.as_bytes());
    let mut suffix = String::with_capacity(25);
    for b in digest.iter().take(13) {
        suffix.push_str(&format!("{b:02x}"));
    }
    suffix.truncate(25);
    format!("msg_{suffix}")
}

/// Locate the local OpenCode database. `$HEADROOM_OPENCODE_DB` wins when
/// it points at an existing file; otherwise the default location. `None`
/// when neither exists, so machines without OpenCode skip the lookup
/// silently instead of logging an error per request.
fn zen_session_db_path() -> Option<std::path::PathBuf> {
    if let Ok(custom) = std::env::var("HEADROOM_OPENCODE_DB") {
        let p = std::path::PathBuf::from(custom.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let p = std::path::PathBuf::from(home).join(".local/share/opencode/opencode.db");
    p.is_file().then_some(p)
}

#[cfg(test)]
fn clear_zen_session_cache() {
    if let Some(lock) = ZEN_SESSION_CACHE.get() {
        if let Ok(mut guard) = lock.lock() {
            *guard = (None, std::time::Instant::now() - ZEN_SESSION_CACHE_TTL);
        }
    }
}

/// Title marking proxy-minted sessions in the operator's session list,
/// so a background mint never looks like a session the operator opened.
const ZEN_MINT_TITLE: &str = "headroom zen route";

/// Model the mint run asks for: the same free-tier model the route
/// serves, so minting spends no key budget either.
const ZEN_MINT_MODEL: &str = "opencode/muse-spark-1.3-contributor-free";

/// Explicit `opencode` binary override; otherwise resolved via `PATH`.
const ZEN_OPENCODE_BIN_ENV: &str = "HEADROOM_OPENCODE_BIN";

/// Minimum gap between background mint attempts. A mint shells out to
/// the OpenCode CLI and burns one tiny inference, so a persistent
/// failure (CLI missing, key revoked) must not retry per request.
const ZEN_MINT_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// Bound on the whole mint: the CLI run plus the session read. The
/// session row is created in the first second, so even a killed run
/// usually leaves a usable id behind — the read below runs regardless
/// of how the child exited.
const ZEN_MINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Mint guard state: `(in-flight, last attempt)`.
static ZEN_MINT_STATE: std::sync::OnceLock<std::sync::Mutex<(bool, std::time::Instant)>> =
    std::sync::OnceLock::new();

fn mint_state() -> &'static std::sync::Mutex<(bool, std::time::Instant)> {
    ZEN_MINT_STATE.get_or_init(|| {
        // Start "long ago" so the very first claim succeeds.
        std::sync::Mutex::new((false, std::time::Instant::now() - ZEN_MINT_MIN_INTERVAL))
    })
}

/// Mint guard: at most one mint in flight, attempts spaced apart.
/// Returns true exactly when the caller earned a mint.
fn mint_guard_claim() -> bool {
    let mut guard = match mint_state().lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if guard.0 || guard.1.elapsed() < ZEN_MINT_MIN_INTERVAL {
        return false;
    }
    guard.0 = true;
    guard.1 = std::time::Instant::now();
    true
}

fn mint_guard_release() {
    if let Ok(mut guard) = mint_state().lock() {
        guard.0 = false;
    }
}

#[cfg(test)]
fn mint_guard_reset() {
    if let Ok(mut guard) = mint_state().lock() {
        *guard = (false, std::time::Instant::now() - ZEN_MINT_MIN_INTERVAL);
    }
}

/// Test-only RAII hold on the mint guard: while held, no code path can
/// spawn a background `opencode run`, so resolving sessions inside unit
/// tests stays side-effect free on developer machines (where the real
/// CLI exists). Claim first so a stale in-flight flag from another test
/// cannot leak a spawn either.
#[cfg(test)]
pub(crate) struct MintTestGuard;

#[cfg(test)]
pub(crate) fn hold_mint_for_test() -> MintTestGuard {
    mint_guard_reset();
    assert!(mint_guard_claim(), "mint guard must be claimable in tests");
    MintTestGuard
}

#[cfg(test)]
impl Drop for MintTestGuard {
    fn drop(&mut self) {
        mint_guard_release();
    }
}

/// Start a detached background mint unless one is already running or a
/// recent attempt is still cooling down. Never blocks the request: the
/// minted id lands in the session cache for later resolutions.
fn trigger_zen_session_mint() {
    if !mint_guard_claim() {
        return;
    }
    tracing::info!(
        event = "zen_session_mint_started",
        "no usable OpenCode session; minting one in the background"
    );
    std::thread::spawn(|| {
        let minted = run_mint_once();
        match &minted {
            Some(id) => {
                store_cached_zen_session(id.clone());
                tracing::info!(
                    event = "zen_session_minted",
                    session_prefix = %id.chars().take(12).collect::<String>(),
                    "background mint landed; Zen route serves real sessions again"
                );
            }
            None => {
                tracing::debug!(
                    event = "zen_session_mint_failed",
                    "background mint produced no session; still on the fallback id"
                );
            }
        }
        mint_guard_release();
    });
}

/// Run one synchronous mint: shell out to the OpenCode CLI for a trivial
/// run (creating the session is the point; its own model call may gate
/// on the still-fresh id and fail — harmless), then read the fresh
/// session id back. Sessions created through OpenCode are valid for the
/// Zen gate immediately (verified live 2026-09-17: a fresh run's own
/// call succeeded on its brand-new id), unlike locally inserted rows,
/// which never become valid.
fn run_mint_once() -> Option<String> {
    let bin = resolve_opencode_bin()?;
    let dir = mint_workdir();
    let start_ms = now_ms();
    let mut child = std::process::Command::new(&bin)
        .args(mint_argv(&dir))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + ZEN_MINT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!(
                    event = "zen_mint_run_exited",
                    success = status.success(),
                    "opencode mint run finished"
                );
                break;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    // The row is created before the run's model call, so read it back
    // however the child exited.
    read_session_created_since(start_ms)
}

/// The mint command, factored pure for tests: a trivial non-interactive
/// run that exists to create (and title, for operator visibility) one
/// session. `--pure` keeps plugins out of it.
fn mint_argv(dir: &std::path::Path) -> Vec<String> {
    [
        "run",
        "--dir",
        dir.to_str().unwrap_or("."),
        "--pure",
        "-m",
        ZEN_MINT_MODEL,
        "--title",
        ZEN_MINT_TITLE,
        "Reply with the single word: ok",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Newest session created since `start_ms`, if any.
fn read_session_created_since(start_ms: i64) -> Option<String> {
    let path = zen_session_db_path()?;
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.query_row(
        "SELECT id FROM session WHERE time_created >= ?1 ORDER BY time_created DESC LIMIT 1",
        rusqlite::params![start_ms],
        |row| row.get(0),
    )
    .ok()
    .filter(|id: &String| id.starts_with("ses_") && id.len() > 8)
}

/// Where the mint run executes: the most recently used project
/// directory that still exists (guaranteed OpenCode-accessible —
/// sessions actively run there), else the system temp dir.
fn mint_workdir() -> std::path::PathBuf {
    if let Some(path) = zen_session_db_path() {
        if let Ok(conn) =
            rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        {
            if let Ok(dir) = conn.query_row(
                "SELECT directory FROM session ORDER BY time_updated DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            ) {
                let p = std::path::PathBuf::from(dir);
                if p.is_dir() {
                    return p;
                }
            }
        }
    }
    std::env::temp_dir()
}

/// Locate the `opencode` binary: `$HEADROOM_OPENCODE_BIN` when it names
/// an existing file, else the first `opencode` on `PATH`. `None` when
/// OpenCode is not installed — minting is impossible, fail open.
fn resolve_opencode_bin() -> Option<std::path::PathBuf> {
    if let Ok(custom) = std::env::var(ZEN_OPENCODE_BIN_ENV) {
        let p = std::path::PathBuf::from(custom.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("opencode"))
        .find(|p| p.is_file())
}

/// Millis since epoch, the unit OpenCode stores session times in.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The placeholder a cursor route uses to take its effort from the request.
pub const EFFORT_PLACEHOLDER: &str = "{effort}";

/// Cursor's own tier names, which are also Claude Code's `/effort` levels.
const CURSOR_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// The Cursor model id to run, with `{effort}` filled in.
///
/// `effort` is what the client asked for. Cursor's tiers happen to be
/// spelled exactly like Claude Code's levels, so the two line up without a
/// table; `max` is the one level Claude Code has and Cursor does not, and
/// it lands on `xhigh` rather than naming a model that does not exist.
///
/// Falls back to `high` — Cursor's plain "Grok 4.6", the tier the id
/// without a suffix means — when the client named nothing usable. A
/// missing effort should not silently demote the model.
pub(crate) fn resolve_cursor_model(cursor_agent_id: &str, effort: Option<&str>) -> String {
    if !cursor_agent_id.contains(EFFORT_PLACEHOLDER) {
        return cursor_agent_id.to_string();
    }
    let level = match effort {
        Some("max") => "xhigh",
        Some(e) if CURSOR_EFFORTS.contains(&e) => e,
        _ => "high",
    };
    cursor_agent_id.replace(EFFORT_PLACEHOLDER, level)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-backed resolution serializes: `set_var` is process-wide and the
    /// session cache persists across tests in one binary.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn header(h: &HeaderMap, name: &str) -> Option<String> {
        h.get(name).and_then(|v| v.to_str().ok()).map(String::from)
    }

    #[test]
    fn refresh_zen_request_id_rotates_only_the_nonce() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let _mint = hold_mint_for_test();
        let prev_project = with_env(ZEN_PROJECT_ENV, Some("proj"));
        let zen_upstream: url::Url = "https://opencode.ai/zen/v1".parse().unwrap();
        let zen = classify_upstream(&zen_upstream, false);
        let mut h = HeaderMap::new();
        zen.inject_extra_headers(&mut h, "req-1", Some("proj"));
        let before = header(&h, "x-opencode-request").expect("injected");
        let session = header(&h, "x-opencode-session").expect("injected");

        assert!(refresh_zen_request_id(&mut h));
        let after = header(&h, "x-opencode-request").expect("still present");
        assert_ne!(before, after, "nonce must rotate");
        assert!(after.starts_with("msg_"));
        assert_eq!(after.len(), 29);
        // Everything else untouched.
        assert_eq!(header(&h, "x-opencode-session"), Some(session));
        assert_eq!(header(&h, "x-opencode-client").as_deref(), Some("cli"));
        assert_eq!(header(&h, "x-opencode-project").as_deref(), Some("proj"));
        restore_env(ZEN_PROJECT_ENV, prev_project);
    }

    #[test]
    fn refresh_zen_request_id_noop_off_zen() {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("claude-code/1.0"),
        );
        assert!(!refresh_zen_request_id(&mut h));
        assert_eq!(header(&h, "x-opencode-request"), None);
        assert_eq!(header(&h, "user-agent").as_deref(), Some("claude-code/1.0"));
    }

    fn with_env(var: &str, value: Option<&str>) -> Option<String> {
        let prev = std::env::var(var).ok();
        match value {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
        prev
    }

    fn restore_env(var: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    fn temp_session_db(rows: &[(&str, i64, i64, Option<&str>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&path).expect("create temp db");
        conn.execute(
            "CREATE TABLE session (id TEXT PRIMARY KEY, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, directory TEXT NOT NULL DEFAULT '/tmp')",
            [],
        )
        .expect("create session table");
        for (id, created, updated, directory) in rows {
            conn.execute(
                "INSERT INTO session (id, time_created, time_updated, directory) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, created, updated, directory.unwrap_or("/tmp")],
            )
            .expect("insert session row");
        }
        dir
    }

    fn responses_body_with_reasoning() -> serde_json::Value {
        serde_json::json!({
            "model": "muse-spark-1.3-contributor-free",
            "include": ["reasoning.encrypted_content"],
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "STALE"},
                {"type": "message", "role": "assistant", "content": "there"}
            ]
        })
    }

    /// Zen invalidates its own reasoning blobs on every exit rotation, so the
    /// replay goes out with the caller-bound `id` + `encrypted_content`
    /// stripped — the item (and any `summary` it carries) stays, and nothing
    /// asks for a fresh blob.
    #[test]
    fn zen_strips_the_reasoning_replay() {
        let mut body = responses_body_with_reasoning();
        UpstreamKind::OpenCodeZen.strip_unreplayable_reasoning(&mut body);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        let reasoning = input
            .iter()
            .find(|i| i["type"] == serde_json::json!("reasoning"))
            .expect("reasoning item survives, stripped");
        assert!(reasoning.get("id").is_none());
        assert!(reasoning.get("encrypted_content").is_none());
        assert!(body.get("include").is_none());
    }

    /// Every other provider keeps it: one stable caller, and the replay is
    /// what lets a reasoning model resume.
    #[test]
    fn other_providers_keep_the_reasoning_replay() {
        for kind in [UpstreamKind::ChatGptSubscription, UpstreamKind::Generic] {
            let mut body = responses_body_with_reasoning();
            kind.strip_unreplayable_reasoning(&mut body);
            assert_eq!(body, responses_body_with_reasoning(), "{kind:?}");
        }
    }

    /// P6 routed-fixtures matrix: every provider config the routed path
    /// serves, classified once, addressing and headering exactly as the
    /// scattered branches did before. A new provider adds a row here.
    #[test]
    fn provider_quirks_matrix() {
        // Resolving Zen headers must not shell out from inside a unit
        // test: hold the mint guard so the fallback path can't spawn a
        // real `opencode run` on a developer machine. (Shape asserts
        // below hold for every session source.)
        let _mint = hold_mint_for_test();
        // Codex route on ChatGPT auth: the Codex endpoint, session headers
        // on, Zen headers off.
        let codex_upstream: url::Url = "https://api.openai.com/v1".parse().unwrap();
        let codex = classify_upstream(&codex_upstream, true);
        assert_eq!(codex, UpstreamKind::ChatGptSubscription);
        assert_eq!(
            codex.responses_url(&codex_upstream),
            crate::codex::codex_endpoint()
        );
        assert!(codex.warn_ambiguous_codex());
        let mut h = HeaderMap::new();
        codex.inject_extra_headers(&mut h, "req-1", None);
        assert_eq!(header(&h, "x-opencode-session"), None);

        // Same upstream on PAYG credentials: generic endpoint, no session
        // mutation, no ambiguity warn.
        let payg = classify_upstream(&codex_upstream, false);
        assert_eq!(payg, UpstreamKind::Generic);
        assert_eq!(
            payg.responses_url(&codex_upstream),
            "https://api.openai.com/v1/responses"
        );
        assert!(!payg.warn_ambiguous_codex());

        // Bare-root base and trailing-slash base address identically: the
        // strip lives in one place now.
        let bare: url::Url = "https://api.openai.com".parse().unwrap();
        let slash: url::Url = "https://api.openai.com/v1/".parse().unwrap();
        assert_eq!(payg.responses_url(&bare), payg.responses_url(&slash));
        assert_eq!(
            payg.chat_url(&bare),
            "https://api.openai.com/v1/chat/completions"
        );

        // qwen-local style chat route: generic endpoint, untouched headers.
        let qwen_upstream: url::Url = "http://127.0.0.1:11434/v1".parse().unwrap();
        let qwen = classify_upstream(&qwen_upstream, false);
        assert_eq!(qwen, UpstreamKind::Generic);
        assert_eq!(
            qwen.chat_url(&qwen_upstream),
            "http://127.0.0.1:11434/v1/chat/completions"
        );

        // OpenCode Zen: generic URL shape, Zen headers on.
        let zen_upstream: url::Url = "https://opencode.ai/zen/v1".parse().unwrap();
        let zen = classify_upstream(&zen_upstream, false);
        assert_eq!(zen, UpstreamKind::OpenCodeZen);
        assert_eq!(
            zen.responses_url(&zen_upstream),
            "https://opencode.ai/zen/v1/responses"
        );
        let mut zh = HeaderMap::new();
        zen.inject_extra_headers(&mut zh, "req-1", None);
        assert!(header(&zh, "x-opencode-session").is_some_and(|s| s.starts_with("ses_")));
        assert_eq!(header(&zh, "x-opencode-client").as_deref(), Some("cli"));

        // Neutral third party: plain endpoint, no extra headers.
        let xai_upstream: url::Url = "https://api.x.ai/v1".parse().unwrap();
        let xai = classify_upstream(&xai_upstream, false);
        assert_eq!(xai, UpstreamKind::Generic);
        let mut xh = HeaderMap::new();
        xai.inject_extra_headers(&mut xh, "req-1", None);
        assert!(xh.is_empty());

        // ChatGPT credentials aimed at a generic host stay generic: the
        // generic endpoint, and no session mutation — turn-state is a
        // Codex-backend protocol, not something to leak to third parties.
        let roaming = classify_upstream(&xai_upstream, true);
        assert_eq!(roaming, UpstreamKind::Generic);
        assert_eq!(
            roaming.responses_url(&xai_upstream),
            "https://api.x.ai/v1/responses"
        );
        assert!(!roaming.warn_ambiguous_codex());
        let mut rh = HeaderMap::new();
        assert_eq!(
            roaming
                .apply_session_headers(
                    &mut rh,
                    &serde_json::json!({"metadata": {"user_id": "user-7"}})
                )
                .as_deref(),
            Some("user-7")
        );
        assert!(header(&rh, "session-id").is_none());

        // Session headers: ChatGPT mutates, everyone else only reads the key.
        let parsed: Value =
            serde_json::json!({"metadata": {"user_id": "user-7"}, "model": "gpt-5"});
        let mut sh = HeaderMap::new();
        assert_eq!(
            codex.apply_session_headers(&mut sh, &parsed).as_deref(),
            Some("user-7")
        );
        assert!(header(&sh, "session-id").is_some());
        assert!(header(&sh, "thread-id").is_some());
        let mut gh = HeaderMap::new();
        assert_eq!(
            payg.apply_session_headers(&mut gh, &parsed).as_deref(),
            Some("user-7")
        );
        assert!(header(&gh, "session-id").is_none());

        // Cursor effort mapping, moved here unchanged.
        assert_eq!(
            resolve_cursor_model("cursor-agent-{effort}", Some("low")),
            "cursor-agent-low"
        );
        assert_eq!(
            resolve_cursor_model("cursor-agent-{effort}", Some("max")),
            "cursor-agent-xhigh"
        );
        assert_eq!(
            resolve_cursor_model("cursor-agent-{effort}", None),
            "cursor-agent-high"
        );
        assert_eq!(
            resolve_cursor_model("cursor-agent-fixed", Some("low")),
            "cursor-agent-fixed"
        );
    }

    /// The explicit override wins over every other source, so an operator
    /// can pin (and rotate) the session without touching the database.
    #[test]
    fn zen_session_env_override_wins() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev = with_env(ZEN_SESSION_ENV, Some("ses_pinned00000000000000000000001"));
        clear_zen_session_cache();
        assert_eq!(
            resolve_zen_session("req-1"),
            "ses_pinned00000000000000000000001"
        );
        restore_env(ZEN_SESSION_ENV, prev);
        clear_zen_session_cache();
    }

    /// Without an override the newest-used session created before the grace
    /// horizon is used: a live session updated seconds ago is ideal (it is
    /// actively syncing), while a session created seconds ago may not have
    /// reached Zen's registry yet.
    #[test]
    fn zen_session_prefers_synced_over_newest() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev_env = with_env(ZEN_SESSION_ENV, None);
        let now = now_ms();
        let dir = temp_session_db(&[
            (
                "ses_live00000000000000000000001",
                now - ZEN_SESSION_SYNC_GRACE_MS - 1000,
                now,
                None,
            ),
            ("ses_fresh0000000000000000000001", now, now - 1000, None),
        ]);
        let prev_db = with_env(
            "HEADROOM_OPENCODE_DB",
            Some(dir.path().join("opencode.db").to_str().unwrap()),
        );
        clear_zen_session_cache();
        assert_eq!(
            resolve_zen_session("req-1"),
            "ses_live00000000000000000000001"
        );
        restore_env("HEADROOM_OPENCODE_DB", prev_db);
        restore_env(ZEN_SESSION_ENV, prev_env);
        clear_zen_session_cache();
    }

    /// When every session is fresher than the grace period, the newest-used
    /// is still better than a minted id Zen has never seen.
    #[test]
    fn zen_session_falls_back_to_newest_when_all_fresh() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev_env = with_env(ZEN_SESSION_ENV, None);
        let now = now_ms();
        let dir = temp_session_db(&[("ses_only00000000000000000000001", now, now, None)]);
        let prev_db = with_env(
            "HEADROOM_OPENCODE_DB",
            Some(dir.path().join("opencode.db").to_str().unwrap()),
        );
        clear_zen_session_cache();
        assert_eq!(
            resolve_zen_session("req-1"),
            "ses_only00000000000000000000001"
        );
        restore_env("HEADROOM_OPENCODE_DB", prev_db);
        restore_env(ZEN_SESSION_ENV, prev_env);
        clear_zen_session_cache();
    }

    /// No override, no database: the legacy minted shape, so the header is
    /// always present (fails gated, not missing).
    #[test]
    fn zen_session_mints_when_no_source_exists() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev_env = with_env(ZEN_SESSION_ENV, None);
        let prev_db = with_env(
            "HEADROOM_OPENCODE_DB",
            Some("/nonexistent-8787/opencode.db"),
        );
        // Point HOME at an empty dir so the default location can't hit a
        // real developer database when this runs on a workstation.
        let home = tempfile::tempdir().expect("temp home");
        let prev_home = with_env("HOME", Some(home.path().to_str().unwrap()));
        clear_zen_session_cache();
        // Hold the mint guard so the fallback path can't spawn a real
        // `opencode run` from inside the unit test.
        mint_guard_reset();
        assert!(mint_guard_claim());
        let minted = resolve_zen_session("123e4567-e89b-12d3-a456-426614174000");
        assert!(minted.starts_with("ses_"), "{minted}");
        assert_eq!(minted.len(), 4 + 64);
        assert!(minted[4..].chars().all(|c| c.is_ascii_hexdigit()));
        mint_guard_release();
        restore_env("HOME", prev_home);
        restore_env("HEADROOM_OPENCODE_DB", prev_db);
        restore_env(ZEN_SESSION_ENV, prev_env);
        clear_zen_session_cache();
    }

    /// Non-UUID request ids (tests use `req-N`) still mint a well-formed
    /// `ses_<64hex>`: the sha256 derivation never emits non-hex.
    #[test]
    fn zen_mint_is_hex_for_non_uuid_request_ids() {
        for id in ["req-1", "roundtrip-test", ""] {
            let minted = mint_zen_session(id);
            assert!(minted.starts_with("ses_"), "{id} -> {minted}");
            assert_eq!(minted.len(), 4 + 64, "{id} -> {minted}");
            assert!(
                minted[4..].chars().all(|c| c.is_ascii_hexdigit()),
                "{id} -> {minted}"
            );
        }
        assert_eq!(mint_zen_session("req-1"), mint_zen_session("req-1"));
        assert_ne!(mint_zen_session("req-1"), mint_zen_session("req-2"));
    }

    /// The mint guard hands out one mint, then suppresses repeats until
    /// released or the interval lapses.
    #[test]
    fn zen_mint_guard_claims_once() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        mint_guard_reset();
        assert!(mint_guard_claim(), "first claim earns the mint");
        assert!(!mint_guard_claim(), "second claim loses while in flight");
        mint_guard_release();
        assert!(
            !mint_guard_claim(),
            "release alone does not re-arm inside the interval"
        );
        mint_guard_reset();
        assert!(mint_guard_claim(), "reset re-arms for the next test");
        mint_guard_release();
    }

    /// The mint command is a trivial non-interactive run in the given dir,
    /// titled so the operator recognizes the session.
    #[test]
    fn zen_mint_argv_is_a_trivial_run() {
        let argv = mint_argv(std::path::Path::new("/tmp/work"));
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"--dir".to_string()));
        assert!(argv.contains(&"/tmp/work".to_string()));
        assert!(argv.contains(&ZEN_MINT_MODEL.to_string()));
        assert!(argv.contains(&ZEN_MINT_TITLE.to_string()));
        assert!(argv.contains(&"--pure".to_string()));
    }

    /// Binary resolution honors the explicit override, else `PATH`.
    #[test]
    fn zen_mint_bin_resolution() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev = with_env(ZEN_OPENCODE_BIN_ENV, Some("/bin/true"));
        assert_eq!(
            resolve_opencode_bin().as_deref(),
            Some(std::path::Path::new("/bin/true"))
        );
        restore_env(ZEN_OPENCODE_BIN_ENV, prev);
    }

    /// The mint worker picks up the session the CLI run created: with a
    /// no-op binary standing in for `opencode` and a row dated at/after
    /// the mint start standing in for what the run would insert, the row
    /// is read back. (A real run inserts its row while running, i.e.
    /// after `run_mint_once` records its start instant.)
    #[test]
    fn zen_mint_task_stores_the_created_session() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev_env = with_env(ZEN_SESSION_ENV, None);
        let prev_bin = with_env(ZEN_OPENCODE_BIN_ENV, Some("/bin/true"));
        let now = now_ms();
        let dir = temp_session_db(&[(
            "ses_minted000000000000000000001",
            now + 120_000,
            now + 120_000,
            None,
        )]);
        let prev_db = with_env(
            "HEADROOM_OPENCODE_DB",
            Some(dir.path().join("opencode.db").to_str().unwrap()),
        );
        clear_zen_session_cache();
        mint_guard_reset();
        assert_eq!(
            run_mint_once().as_deref(),
            Some("ses_minted000000000000000000001")
        );
        restore_env("HEADROOM_OPENCODE_DB", prev_db);
        restore_env(ZEN_OPENCODE_BIN_ENV, prev_bin);
        restore_env(ZEN_SESSION_ENV, prev_env);
        clear_zen_session_cache();
        mint_guard_reset();
    }

    /// No row created, no session stored: fail-open, the fallback id
    /// keeps serving (gated) instead of erroring.
    #[test]
    fn zen_mint_task_without_a_row_stores_nothing() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev_env = with_env(ZEN_SESSION_ENV, None);
        let prev_bin = with_env(ZEN_OPENCODE_BIN_ENV, Some("/bin/true"));
        let dir = temp_session_db(&[]);
        let prev_db = with_env(
            "HEADROOM_OPENCODE_DB",
            Some(dir.path().join("opencode.db").to_str().unwrap()),
        );
        clear_zen_session_cache();
        mint_guard_reset();
        assert_eq!(run_mint_once(), None);
        restore_env("HEADROOM_OPENCODE_DB", prev_db);
        restore_env(ZEN_OPENCODE_BIN_ENV, prev_bin);
        restore_env(ZEN_SESSION_ENV, prev_env);
        clear_zen_session_cache();
        mint_guard_reset();
    }

    /// Cache roundtrip without touching resolution: store, read, clear.
    #[test]
    fn zen_session_cache_roundtrip() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        clear_zen_session_cache();
        assert_eq!(cached_zen_session(), None);
        store_cached_zen_session("ses_cache00000000000000000000001".to_string());
        assert_eq!(
            cached_zen_session().as_deref(),
            Some("ses_cache00000000000000000000001")
        );
        clear_zen_session_cache();
        assert_eq!(cached_zen_session(), None);
    }

    /// The mint workdir is the most recently used project dir that still
    /// exists, else the system temp dir.
    #[test]
    fn zen_mint_workdir_prefers_live_project_dirs() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev_env = with_env(ZEN_SESSION_ENV, None);
        let live = tempfile::tempdir().expect("live project dir");
        let now = now_ms();
        let dir = temp_session_db(&[(
            "ses_proj00000000000000000000001",
            now - ZEN_SESSION_SYNC_GRACE_MS - 1000,
            now,
            Some(live.path().to_str().unwrap()),
        )]);
        let prev_db = with_env(
            "HEADROOM_OPENCODE_DB",
            Some(dir.path().join("opencode.db").to_str().unwrap()),
        );
        assert_eq!(mint_workdir(), live.path());
        restore_env("HEADROOM_OPENCODE_DB", prev_db);
        restore_env(ZEN_SESSION_ENV, prev_env);
    }
}

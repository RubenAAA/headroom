//! Talking to the Codex backend as the Codex CLI does.
//!
//! Split out of `handlers::local_model` on 2026-08-26. None of this is about a
//! local model: it is OAuth refresh, an installation identity, and the header
//! set the codex backend gates traffic on. It lived there because that is where
//! the first `codex-*` route was written, not because it belonged.
//!
//! The values are mirrored from the CLI's own source (`codex-rs/login/src/auth`)
//! rather than invented. The backend buckets by originator and user-agent, so a
//! header that drifts from the CLI's is one that gets treated differently.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use axum::http::HeaderMap;
use serde_json::{json, Value};

/// Values mirrored from the Codex CLI source (codex-rs/login/src/auth):
/// the codex backend gates and buckets traffic by originator/user-agent,
/// and token refresh uses the CLI's public OAuth client id.
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Resolve a file that lives alongside auth.json in the codex home dir.
pub(crate) fn codex_home_sibling(auth_file: Option<&str>, name: &str) -> Option<std::path::PathBuf> {
    Some(std::path::Path::new(auth_file?).parent()?.join(name))
}

/// The installed Codex CLI version, read from version.json next to the auth
/// file, so our user-agent tracks whatever CLI release the user actually has.
pub(crate) fn codex_cli_version(auth_file: Option<&str>) -> String {
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
pub(crate) fn codex_installation_id(auth_file: Option<&str>) -> Option<String> {
    let content =
        std::fs::read_to_string(codex_home_sibling(auth_file, "installation_id")?).ok()?;
    let id = content.trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// User-agent matching the Codex CLI's format:
/// `{originator}/{version} ({os} {version}; {arch}) {terminal}`.
pub(crate) fn codex_user_agent(auth_file: Option<&str>) -> String {
    format!(
        "{CODEX_ORIGINATOR}/{} (Ubuntu 24.04; {}) WindowsTerminal",
        codex_cli_version(auth_file),
        std::env::consts::ARCH,
    )
}

/// W3C trace context header with random trace/span ids, as sent per-request
/// by the Codex CLI's instrumented HTTP client.
pub(crate) fn generate_traceparent() -> String {
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

pub(crate) fn turn_state_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    CODEX_TURN_STATE.get_or_init(Default::default)
}

// Encrypted reasoning items used to be cached here, keyed by session and
// anchored to the call_id that followed them. That cache is gone: the items now
// ride back to us inside the `thinking` block signature we hand the client.
// See `super::reasoning_signature` for why.

/// Derive a stable UUID-shaped session id from Claude Code's metadata.user_id
/// so `session-id`/`thread-id` headers stay constant within a session.
pub(crate) fn derive_session_uuid(user_id: &str) -> String {
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
pub(crate) async fn refresh_codex_token(client: &reqwest::Client, auth_file: &str) -> Option<String> {
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
pub(crate) fn read_codex_access_token(path: &str) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&data).ok()?;
    parsed
        .get("tokens")?
        .get("access_token")?
        .as_str()
        .map(String::from)
}

pub(crate) fn decode_openai_bearer_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let payload = URL_SAFE_NO_PAD.decode(payload.as_bytes()).ok()?;
    serde_json::from_slice(&payload).ok()
}

pub(crate) fn resolve_codex_routing_headers(
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

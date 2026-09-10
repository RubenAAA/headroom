//! Upstream credentials for routed model routes.
//!
//! A route either names its own credential (an environment variable read
//! once per request), declares itself anonymous (`none`), or inherits the
//! Codex headers the process was started with.

use crate::codex::{derive_session_uuid, resolve_codex_routing_headers, turn_state_map};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

/// Pick the upstream credential for a matched route, returning the headers and
/// whether they carry ChatGPT auth.
///
/// Codex headers are built from process-wide config — the auth file, the
/// `codex_cli_rs` originator, the ChatGPT bearer token — so they are only
/// right for a route actually bound for Codex. A route naming its own
/// credential says it is not, and gets that credential instead. A route naming
/// none keeps the old behavior, which is what makes this opt-in.
pub(crate) fn upstream_auth_headers(
    auth_env: Option<&str>,
    client_headers: &HeaderMap,
    codex_auth_file: Option<&str>,
) -> Result<(HeaderMap, bool), Response> {
    match auth_env {
        Some(var) => Ok((route_auth_headers(var)?, false)),
        None => Ok(resolve_codex_routing_headers(
            client_headers,
            codex_auth_file,
        )),
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
pub(crate) fn inject_opencode_headers(
    headers: &mut HeaderMap,
    request_id: &str,
    session_key: Option<&str>,
) {
    // `ses_` + 64 hex, like OpenCode's `ses_[0-9a-f]{64}`. Derive from the
    // request_id UUID so retries within the same logical request share the
    // same session, but different requests don't collide.
    let raw = request_id.replace('-', "");
    let mut hex = String::with_capacity(64);
    while hex.len() < 64 {
        hex.push_str(&raw);
    }
    hex.truncate(64);
    let session = format!("ses_{hex}");
    if let Ok(v) = http::HeaderValue::from_str(&session) {
        headers.insert(http::HeaderName::from_static("x-opencode-session"), v);
    }
    if let Ok(v) = http::HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()) {
        headers.insert(http::HeaderName::from_static("x-opencode-request"), v);
    }
    headers.insert(
        http::HeaderName::from_static("x-opencode-client"),
        http::HeaderValue::from_static("opencode"),
    );
    headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("opencode/1.18.29"),
    );
    if let Some(sk) = session_key {
        // Best-effort project correlation; not required for the gate, but
        // mirrors what OpenCode sends (`x-opencode-project`).
        if let Ok(v) = http::HeaderValue::from_str(sk) {
            headers.insert(http::HeaderName::from_static("x-opencode-project"), v);
        }
    }
}

/// Build upstream headers for a route that carries its own credential.
///
/// `var` is the name of an environment variable, not a token — see
/// [`crate::config::ModelRoute::auth_env`]. It is read here, once per request,
/// so an operator can rotate the token by restarting the shell that exports
/// it without touching the route table.
///
/// A missing or empty variable is an error rather than a header left off. The
/// silent version sends an unauthenticated request and gets back an upstream
/// 401, which reads like a bad token rather than a missing one.
pub(crate) fn route_auth_headers(var: &str) -> Result<HeaderMap, Response> {
    // `none` is not a variable: it declares the route carries no credential
    // at all. The upstream gets only Content-Type — no Authorization, and
    // none of the Codex identity headers the default path would add. For a
    // public anonymous upstream (e.g. OpenCode Zen's free tier, verified
    // 2026-09-06 to serve with no Authorization header at cost 0). Do not
    // name a real environment variable `none`; it would never be read.
    if var == "none" {
        let mut upstream_headers = HeaderMap::new();
        upstream_headers.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().expect("valid header"),
        );
        return Ok(upstream_headers);
    }

    let deny = |detail: String| -> Response {
        tracing::error!(
            event = "model_route_auth_env_unusable",
            env_var = %var,
            "route credential unusable"
        );
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(detail))
            .expect("static response")
    };

    let token = match std::env::var(var) {
        Ok(t) if !t.trim().is_empty() => t,
        _ => {
            return Err(deny(format!(
                "model route names ${var} for its credential, but that \
                 environment variable is unset or empty"
            )))
        }
    };

    let mut upstream_headers = HeaderMap::new();
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        "application/json".parse().expect("valid header"),
    );
    // A token with a newline or a stray control character would otherwise be
    // rejected deep inside the client with a message naming no variable.
    let value = http::HeaderValue::from_str(&format!("Bearer {}", token.trim()))
        .map_err(|_| deny(format!("${var} is not usable as an Authorization header")))?;
    upstream_headers.insert(http::header::AUTHORIZATION, value);
    Ok(upstream_headers)
}

/// Session correlation headers and turn-state echo, mirroring the real
/// Codex client (codex-api/src/requests/headers.rs, client.rs). Returns the
/// session key the turn is correlated under, if the translated body carried
/// one.
pub(crate) fn apply_codex_session_headers(
    headers: &mut HeaderMap,
    parsed: &Value,
    is_chatgpt_auth: bool,
) -> Option<String> {
    let session_key = parsed
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    if is_chatgpt_auth {
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

/// Capture the turn-state token for sticky routing on follow-up requests.
pub(crate) fn capture_turn_state(upstream_resp: &reqwest::Response, session_key: Option<&str>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use serde_json::json;

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
    /// Serializes the cases below: `set_var` is process-wide.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Writes a Codex auth file, so every case below is a choice the route made
    /// rather than the absence of anything to choose.
    fn codex_auth() -> (tempfile::TempDir, String) {
        let payload = json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-do-not-leak"}
        });
        let token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            json!({"tokens": {"access_token": token}}).to_string(),
        )
        .unwrap();
        (dir, token)
    }

    fn header(h: &HeaderMap, name: &str) -> Option<String> {
        h.get(name).and_then(|v| v.to_str().ok()).map(String::from)
    }

    /// The bug this exists to stop: with `--codex-auth-file` set, an xAI route
    /// used to be handed the ChatGPT bearer token and the Codex originator and
    /// send both to `api.x.ai`.
    #[test]
    fn a_route_credential_replaces_codex_auth_entirely() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, _codex_token) = codex_auth();
        let auth_path = dir.path().join("auth.json");
        std::env::set_var("HEADROOM_TEST_ROUTE_KEY", "xai-route-token");

        let (h, is_chatgpt) = upstream_auth_headers(
            Some("HEADROOM_TEST_ROUTE_KEY"),
            &HeaderMap::new(),
            auth_path.to_str(),
        )
        .expect("the variable is set");

        assert!(!is_chatgpt, "a route credential is not ChatGPT auth");
        assert_eq!(
            header(&h, "authorization").as_deref(),
            Some("Bearer xai-route-token")
        );
        assert_eq!(header(&h, "originator"), None);
        assert_eq!(header(&h, "ChatGPT-Account-ID"), None);
        assert_eq!(header(&h, "user-agent"), None);
        std::env::remove_var("HEADROOM_TEST_ROUTE_KEY");
    }

    /// The other half: a route naming no credential still gets Codex headers,
    /// or the test above would pass on a function that never routes anywhere.
    #[test]
    fn a_route_without_a_credential_still_gets_codex_headers() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, codex_token) = codex_auth();
        let auth_path = dir.path().join("auth.json");

        let (h, is_chatgpt) = upstream_auth_headers(None, &HeaderMap::new(), auth_path.to_str())
            .expect("codex headers never fail");

        assert!(is_chatgpt);
        assert_eq!(
            header(&h, "authorization"),
            Some(format!("Bearer {codex_token}"))
        );
        assert_eq!(
            header(&h, "ChatGPT-Account-ID").as_deref(),
            Some("acct-do-not-leak")
        );
        assert!(header(&h, "originator").is_some());
    }

    /// An unset variable means a typo in the flags file. Sending the request
    /// without the header turns that into an upstream 401, which reads like a
    /// revoked token and sends the operator looking in the wrong place.
    #[test]
    fn an_unset_variable_is_reported_rather_than_dropped() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("HEADROOM_TEST_MISSING_KEY");

        let err = upstream_auth_headers(Some("HEADROOM_TEST_MISSING_KEY"), &HeaderMap::new(), None)
            .expect_err("unset variable");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A variable exported as empty is the same mistake wearing a different
    /// hat, and `Bearer ` is a header no upstream wants.
    #[test]
    fn an_empty_variable_is_reported_too() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("HEADROOM_TEST_EMPTY_KEY", "   ");
        let err = upstream_auth_headers(Some("HEADROOM_TEST_EMPTY_KEY"), &HeaderMap::new(), None)
            .expect_err("empty variable");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        std::env::remove_var("HEADROOM_TEST_EMPTY_KEY");
    }

    /// A token pasted into a shell often keeps its trailing newline, which
    /// `HeaderValue` refuses. Trim it rather than fail on it.
    #[test]
    fn a_trailing_newline_is_trimmed_off_the_token() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("HEADROOM_TEST_NEWLINE_KEY", "xai-token\n");
        let (h, _) =
            upstream_auth_headers(Some("HEADROOM_TEST_NEWLINE_KEY"), &HeaderMap::new(), None)
                .expect("trimmed");
        assert_eq!(
            header(&h, "authorization").as_deref(),
            Some("Bearer xai-token")
        );
        std::env::remove_var("HEADROOM_TEST_NEWLINE_KEY");
    }

    /// `:auth=none` declares a public anonymous upstream: no Authorization
    /// even when a Codex auth file is configured, and none of the Codex
    /// identity headers either. This is what the OpenCode Zen free tier
    /// route uses.
    #[test]
    fn none_sends_no_credential_despite_a_codex_auth_file() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (dir, _codex_token) = codex_auth();
        let auth_path = dir.path().join("auth.json");

        // Seed the caller headers a broken impl could leak: the anonymous
        // arm must drop them, not merge them.
        let mut caller = HeaderMap::new();
        caller.insert(
            http::header::AUTHORIZATION,
            "Bearer caller-key".parse().expect("valid header"),
        );
        caller.insert(
            "ChatGPT-Account-ID",
            "acct-caller".parse().expect("valid header"),
        );

        let (h, is_chatgpt) = upstream_auth_headers(Some("none"), &caller, auth_path.to_str())
            .expect("none is always usable");

        assert!(!is_chatgpt, "no credential is not ChatGPT auth");
        assert_eq!(header(&h, "authorization"), None);
        assert_eq!(header(&h, "ChatGPT-Account-ID"), None);
        assert_eq!(header(&h, "originator"), None);
        assert_eq!(header(&h, "user-agent"), None);
        assert_eq!(
            header(&h, "content-type").as_deref(),
            Some("application/json")
        );
    }
}

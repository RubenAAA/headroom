//! Upstream credentials for routed model routes.
//!
//! A route either names its own credential (an environment variable read
//! once per request), declares itself anonymous (`none`), or inherits the
//! Codex headers the process was started with.

use crate::codex::resolve_codex_routing_headers;
use crate::routed::quirks::classify_upstream;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

/// The single upstream-header constructor (C4). Credential selection plus
/// the OpenCode sniff, so the main path and the routed sidecar cannot drift
/// into building different header sets for the same route kind.
///
/// Credential semantics: Codex headers come from process-wide config and are
/// only right for a Codex-bound route; a route naming its own credential says
/// it is not, and gets that credential instead. A route naming none keeps the
/// old behavior, which is what makes this opt-in.
///
/// Turn-state is deliberately NOT part of this: `UpstreamKind` session
/// headers run main-path-only after translation (the sidecar is a stateless
/// one-shot with no session to correlate), and turn-state capture stays with
/// response handling. That seam is named here so a future merge has to cross
/// it explicitly.
#[allow(clippy::result_large_err)]
pub(crate) fn auth_headers(
    auth_env: Option<&str>,
    client_headers: &HeaderMap,
    codex_auth_file: Option<&str>,
    upstream: &url::Url,
    request_id: &str,
    session_key: Option<&str>,
) -> Result<(HeaderMap, bool), Response> {
    let (mut headers, is_chatgpt_auth) = match auth_env {
        Some(var) => (route_auth_headers(var)?, false),
        None => resolve_codex_routing_headers(client_headers, codex_auth_file),
    };
    // Provider-gated extras (P6): the OpenCode Zen sniff lives in
    // `routed::quirks`, not inline here.
    classify_upstream(upstream, is_chatgpt_auth).inject_extra_headers(
        &mut headers,
        request_id,
        session_key,
    );
    Ok((headers, is_chatgpt_auth))
}

/// Build upstream headers for a route that carries its own credential.
///
/// `var` is the name of an environment variable, not a token — see
/// [`crate::config::ProviderRoute::auth_env`]. It is read here, once per request,
/// so an operator can rotate the token by restarting the shell that exports
/// it without touching the route table.
///
/// A missing or empty variable is an error rather than a header left off. The
/// silent version sends an unauthenticated request and gets back an upstream
/// 401, which reads like a bad token rather than a missing one.
#[allow(clippy::result_large_err)]
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

    /// The unified constructor against a neutral upstream, so these snapshots
    /// pin exactly what the main path and the sidecar both send.
    fn auth_for_test(
        auth_env: Option<&str>,
        caller: &HeaderMap,
        codex_auth_file: Option<&str>,
    ) -> Result<(HeaderMap, bool), Response> {
        let upstream: url::Url = "https://api.x.ai/v1".parse().expect("valid url");
        super::auth_headers(
            auth_env,
            caller,
            codex_auth_file,
            &upstream,
            "req-test",
            None,
        )
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

        let (h, is_chatgpt) = auth_for_test(
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

        let (h, is_chatgpt) = auth_for_test(None, &HeaderMap::new(), auth_path.to_str())
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

        let err = auth_for_test(Some("HEADROOM_TEST_MISSING_KEY"), &HeaderMap::new(), None)
            .expect_err("unset variable");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A variable exported as empty is the same mistake wearing a different
    /// hat, and `Bearer ` is a header no upstream wants.
    #[test]
    fn an_empty_variable_is_reported_too() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("HEADROOM_TEST_EMPTY_KEY", "   ");
        let err = auth_for_test(Some("HEADROOM_TEST_EMPTY_KEY"), &HeaderMap::new(), None)
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
        let (h, _) = auth_for_test(Some("HEADROOM_TEST_NEWLINE_KEY"), &HeaderMap::new(), None)
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

        let (h, is_chatgpt) = auth_for_test(Some("none"), &caller, auth_path.to_str())
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

    /// C4 snapshot: an `opencode.ai` upstream gains the Zen session headers
    /// through the same constructor — previously an inline sniff duplicated
    /// at both call sites, now asserted in one place.
    #[test]
    fn opencode_upstream_gains_session_headers() {
        let upstream: url::Url = "https://opencode.ai/zen/v1".parse().expect("valid url");
        let (h, _) = super::auth_headers(None, &HeaderMap::new(), None, &upstream, "req-1", None)
            .expect("no credential needed");
        let session = header(&h, "x-opencode-session");
        assert!(
            session.is_some_and(|s| s.starts_with("ses_")),
            "zen gate needs the session header: {h:?}"
        );
        assert!(header(&h, "x-opencode-request").is_some());
        assert_eq!(header(&h, "x-opencode-client").as_deref(), Some("opencode"));

        // And a non-OpenCode upstream gains none of them.
        let plain: url::Url = "https://api.x.ai/v1".parse().expect("valid url");
        let (p, _) = super::auth_headers(None, &HeaderMap::new(), None, &plain, "req-1", None)
            .expect("no credential needed");
        assert_eq!(header(&p, "x-opencode-session"), None);
    }
}

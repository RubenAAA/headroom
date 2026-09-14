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
    /// the model still gets the assistant text, and only the cross-turn chain
    /// of thought is lost.
    ///
    /// The other providers keep the replay. Codex and Cursor are reached from
    /// one stable identity, where handing the items back is what makes a
    /// reasoning model resume.
    pub(crate) fn strip_unreplayable_reasoning(&self, openai_body: &mut serde_json::Value) {
        if *self != UpstreamKind::OpenCodeZen {
            return;
        }
        if let Some(items) = openai_body.get_mut("input").and_then(|v| v.as_array_mut()) {
            items.retain(|item| item.get("type").and_then(|t| t.as_str()) != Some("reasoning"));
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

    fn header(h: &HeaderMap, name: &str) -> Option<String> {
        h.get(name).and_then(|v| v.to_str().ok()).map(String::from)
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
    /// replay goes out stripped and nothing asks for a fresh blob.
    #[test]
    fn zen_strips_the_reasoning_replay() {
        let mut body = responses_body_with_reasoning();
        UpstreamKind::OpenCodeZen.strip_unreplayable_reasoning(&mut body);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert!(input
            .iter()
            .all(|i| i["type"] != serde_json::json!("reasoning")));
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
        assert_eq!(
            header(&zh, "x-opencode-client").as_deref(),
            Some("opencode")
        );

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
}

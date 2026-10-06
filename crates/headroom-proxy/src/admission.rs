//! Admission gate: request rate, token rate and spend budget.
//!
//! One middleware in front of every generation route — Anthropic Messages
//! (plain and Foundry), OpenAI Chat Completions and Responses, Gemini
//! `generateContent`, and Anthropic on Bedrock and Vertex — so no route can
//! skip a limit. Upstream enforced these
//! per handler and missed routes more than once (`138736c9` found TPM never
//! checked, `f734c573` found the budget checked on Anthropic only). Here
//! `/v1/messages` usually reaches the catch-all rather than a handler, which
//! is another reason the gate sits in front of routing.
//!
//! Everything is off unless configured: an RPM or TPM of 0 is unlimited, and
//! without `--budget-limit-usd` the budget always allows. Internal
//! re-dispatches call `forward_http` directly, so they are never charged
//! twice.
//!
//! The Codex WebSocket is a GET and passes through here; it checks the budget
//! itself, before the upgrade and on every `response.create` frame.

use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use crate::proxy::AppState;

/// Whether `path` makes a model generate (and so costs money and quota).
fn is_generation_path(path: &str) -> bool {
    crate::compression::is_compressible_path(path)
        || path == "/anthropic/v1/messages"
        || gemini_generate_model(path).is_some()
        || bedrock_model(path).is_some()
        || vertex_model(path).is_some()
}

/// The model named by a Bedrock `invoke` / `converse` path (and their
/// streaming forms), e.g. `/model/anthropic.claude-sonnet-4/invoke`.
fn bedrock_model(path: &str) -> Option<&str> {
    let (model, action) = path.strip_prefix("/model/")?.split_once('/')?;
    matches!(
        action,
        "invoke" | "invoke-with-response-stream" | "converse" | "converse-stream"
    )
    .then_some(model)
}

/// The model named by a Vertex Anthropic publisher path, e.g.
/// `/v1beta1/projects/p/locations/l/publishers/anthropic/models/claude-sonnet-4:rawPredict`.
fn vertex_model(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/v1beta1/projects/")?;
    let (_, model_action) = rest.split_once("/publishers/anthropic/models/")?;
    let (model, action) = model_action.rsplit_once(':')?;
    (!model.contains('/') && matches!(action, "rawPredict" | "streamRawPredict")).then_some(model)
}

/// The model named by a Gemini `generateContent` / `streamGenerateContent`
/// path, e.g. `/v1beta/models/gemini-2.5-pro:generateContent`.
fn gemini_generate_model(path: &str) -> Option<&str> {
    let (model, action) = path.strip_prefix("/v1beta/models/")?.split_once(':')?;
    matches!(action, "generateContent" | "streamGenerateContent").then_some(model)
}

/// The bucket a caller's requests count against (port of upstream
/// `headroom/proxy/rate_limit_identity.py`, commit `6fb7cadf`, VAPT 01-F3).
/// Callers the proxy cannot authenticate are keyed on the peer only, so
/// rotating the credential header cannot buy a fresh bucket. Callers that
/// presented the proxy token, or connected directly from loopback, are
/// additionally keyed on an HMAC of their provider credential, so one
/// local caller's burst does not spend another's budget. Bucket keys never
/// hold a recoverable secret; the key is random per process, like the
/// buckets themselves.
pub(crate) fn rate_limit_identity(
    authenticated: bool,
    peer: Option<&str>,
    headers: &HeaderMap,
    gateway_cidrs: &[crate::forwarded_headers::IpCidr],
) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    static IDENTITY_SECRET: OnceLock<[u8; 32]> = OnceLock::new();

    let client_ip = crate::forwarded_headers::resolve_client_ip(peer, headers, gateway_cidrs);
    let identity = peer_group(&client_ip);
    let credential = is_trusted_request(authenticated, peer, headers, gateway_cidrs)
        .then(|| rate_limit_credential(headers))
        .flatten();
    let Some((kind, value)) = credential else {
        return identity;
    };
    let secret = IDENTITY_SECRET.get_or_init(|| {
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key
    });
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(kind.as_bytes());
    mac.update(b":");
    mac.update(value.as_bytes());
    format!(
        "{identity}|cred:{}",
        &hex::encode(mac.finalize().into_bytes())[..32]
    )
}

/// The `peer:` half of a bucket key. IPv6 callers sharing a /64 share a
/// bucket (one LAN, one bucket); IPv4-mapped IPv6 collapses onto its IPv4
/// peer. Anything unparseable is carried verbatim, truncated, so a weird
/// value groups with itself rather than with everyone.
fn peer_group(ip: &str) -> String {
    if ip.is_empty() {
        return "peer:unknown".to_string();
    }
    let Ok(addr) = ip.parse::<std::net::IpAddr>() else {
        let head: String = ip.chars().take(64).collect();
        return format!("peer:{head}");
    };
    match addr {
        std::net::IpAddr::V4(v4) => format!("peer:{v4}"),
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return format!("peer:{mapped}");
            }
            let mut prefix = v6.octets();
            prefix[8..].fill(0);
            format!("peer:{}/64", std::net::Ipv6Addr::from(prefix))
        }
    }
}

/// The provider credential a trusted caller is additionally keyed on:
/// `Authorization`, then Anthropic's `x-api-key`, then Azure-style
/// `api-key`, then Gemini's `x-goog-api-key` (upstream `b9e8462a`: clients
/// sending only `api-key` all shared one bucket).
fn rate_limit_credential(headers: &HeaderMap) -> Option<(&'static str, &str)> {
    ["authorization", "x-api-key", "api-key", "x-goog-api-key"]
        .into_iter()
        .find_map(|name| {
            let value = headers.get(name)?.to_str().ok()?;
            (!value.is_empty()).then_some((name, value))
        })
}

/// Whether the caller is one the credential may enter the bucket key for:
/// it presented the proxy token, or it connected directly from loopback. A
/// request relayed by a trusted gateway is never direct, even when the
/// forwarded address is one the header policy cannot use: the gateway
/// vouches for that address, not for the caller's credential.
fn is_trusted_request(
    authenticated: bool,
    peer: Option<&str>,
    headers: &HeaderMap,
    gateway_cidrs: &[crate::forwarded_headers::IpCidr],
) -> bool {
    if authenticated {
        return true;
    }
    if crate::forwarded_headers::peer_is_trusted_gateway(peer, gateway_cidrs)
        && let Some(xff) = headers.get("x-forwarded-for")
        && xff.to_str().is_ok_and(|v| !v.is_empty())
    {
        return false;
    }
    // A missing peer is not loopback (upstream `ee731d7e` hardened
    // `is_loopback_host(None)` to false), and an unknown peer must not
    // authenticate here regardless.
    matches!(peer, Some(host) if crate::loopback_guard::is_loopback_host(Some(host)))
}

/// A 429 in Anthropic's error shape. OpenAI clients read the same
/// `error.message` and `error.type` fields.
fn too_many_requests(message: String, retry_after_seconds: Option<f64>) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": {"type": "rate_limit_error", "message": message},
    });
    let mut builder = Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", "application/json");
    if let Some(wait) = retry_after_seconds {
        builder = builder.header("retry-after", (wait.ceil() as u64).max(1).to_string());
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("static response")
}

/// `Some(429)` when the spend budget for the current period is used up.
///
/// Not booked as `rate_limited`, on purpose and as upstream does: a spent
/// budget is not a rate, and the `budget_exceeded` event below records it.
pub(crate) fn budget_refusal(state: &AppState) -> Option<Response> {
    let tracker = &state.cost_tracker;
    let (allowed, _) = tracker.check_budget();
    if allowed {
        return None;
    }
    let period = &state.config.budget_period;
    tracing::warn!(
        event = "budget_exceeded",
        period = %period,
        spent_usd = tracker.get_period_cost(),
        "refusing a generation request: spend budget used up"
    );
    Some(too_many_requests(
        format!("Budget exceeded for {period} period"),
        None,
    ))
}

/// The provider label a generation path's outcomes carry, so the ledger files
/// our own 429 beside the provider's (`rate_limited_by_provider`).
fn provider_label(path: &str) -> &'static str {
    use crate::compression::{CompressibleEndpoint, classify_compressible_path};
    match classify_compressible_path(path) {
        Some(CompressibleEndpoint::AnthropicMessages) => "anthropic",
        Some(CompressibleEndpoint::OpenAiChatCompletions) => "openai_chat",
        Some(CompressibleEndpoint::OpenAiResponses) => "openai_responses",
        _ if path == "/anthropic/v1/messages" => "anthropic",
        _ if bedrock_model(path).is_some() => "bedrock_anthropic",
        _ if vertex_model(path).is_some() => "vertex_anthropic",
        _ => "gemini",
    }
}

fn rate_limited(state: &AppState, path: &str, what: &str, wait_seconds: f64) -> Response {
    crate::observability::proxy_counters::record_rate_limited("headroom");
    state
        .savings_tracker
        .record_rate_limited(Some(provider_label(path)), "headroom");
    too_many_requests(
        format!("{what} rate limited. Retry after {wait_seconds:.1}s"),
        Some(wait_seconds),
    )
}

/// Input tokens a request asks for, estimated before anything is sent.
/// Anthropic bodies go through the `count_tokens` estimator, which weighs
/// images by size rather than by their base64 text; other shapes count the
/// whole body with the model's tokenizer. Bedrock and Vertex carry Anthropic
/// bodies and name the model in the path.
fn estimate_request_tokens(path: &str, body: &[u8]) -> u64 {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let anthropic_on_cloud = bedrock_model(path).or_else(|| vertex_model(path));
    let model = gemini_generate_model(path)
        .or(anthropic_on_cloud)
        .or_else(|| parsed.get("model").and_then(Value::as_str))
        .unwrap_or("");
    if (path.ends_with("/v1/messages") || anthropic_on_cloud.is_some()) && parsed.is_object() {
        crate::handlers::count_tokens::estimate_input_tokens(&parsed, model)
    } else {
        headroom_core::tokenizer::get_tokenizer(model).count_text(&String::from_utf8_lossy(body))
            as u64
    }
}

/// Middleware: refuse a generation request over the request rate, the spend
/// budget or the token rate, in that order (upstream's handler order).
pub(crate) async fn admission_gate(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if req.method() != Method::POST || !is_generation_path(req.uri().path()) {
        return next.run(req).await;
    }
    let limiter = state.rate_limiter.clone();
    // The auth gate runs before this middleware and marks token callers, so
    // the bucket key can tell "authenticated" from "loopback-exempt".
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    let authenticated = req
        .extensions()
        .get::<crate::proxy_auth::ProxyTokenAuthenticated>()
        .is_some();
    let key = limiter
        .as_ref()
        .map(|_| {
            rate_limit_identity(
                authenticated,
                peer.as_deref(),
                req.headers(),
                &state.trusted_gateway_cidrs,
            )
        })
        .unwrap_or_default();

    if let Some(limiter) = &limiter {
        let r = limiter.check_request(&key);
        if !r.allowed {
            return rate_limited(&state, req.uri().path(), "Request", r.wait_seconds);
        }
    }
    if let Some(refusal) = budget_refusal(&state) {
        return refusal;
    }
    let Some(limiter) = limiter.filter(|l| l.limits_tokens()) else {
        return next.run(req).await;
    };

    // Only a TPM limit needs the body, so only then is it buffered here.
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, state.config.max_body_bytes as usize).await {
        Ok(b) => b,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .body(Body::from(format!("request body: {e}")))
                .expect("static response");
        }
    };
    let tokens = estimate_request_tokens(parts.uri.path(), &bytes);
    let r = limiter.check_tokens(&key, u32::try_from(tokens).unwrap_or(u32::MAX));
    if !r.allowed {
        return rate_limited(&state, parts.uri.path(), "Token", r.wait_seconds);
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_paths() {
        for p in [
            "/v1/messages",
            "/anthropic/v1/messages",
            "/v1/chat/completions",
            "/v1/responses",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
            "/model/anthropic.claude-sonnet-4/invoke",
            "/model/anthropic.claude-sonnet-4/invoke-with-response-stream",
            "/model/anthropic.claude-sonnet-4/converse",
            "/model/anthropic.claude-sonnet-4/converse-stream",
            "/v1beta1/projects/p/locations/l/publishers/anthropic/models/claude-sonnet-4:rawPredict",
            "/v1beta1/projects/p/locations/l/publishers/anthropic/models/claude-sonnet-4:streamRawPredict",
        ] {
            assert!(is_generation_path(p), "{p}");
        }
        for p in [
            "/v1/messages/count_tokens",
            "/v1/models",
            "/v1beta/models/gemini-2.5-pro:countTokens",
            "/v1beta/models/gemini-2.5-pro:batchGenerateContent",
            "/model/anthropic.claude-sonnet-4/count-tokens",
            "/v1beta1/projects/p/locations/l/publishers/anthropic/models/claude-sonnet-4:countTokens",
        ] {
            assert!(!is_generation_path(p), "{p}");
        }
    }

    use crate::forwarded_headers::load_trusted_gateway_cidrs;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        map
    }

    fn gateway() -> Vec<crate::forwarded_headers::IpCidr> {
        load_trusted_gateway_cidrs("10.20.0.0/16, 127.0.0.0/8").unwrap()
    }

    /// Port of upstream's `test_untrusted_identity_ignores_credential`: one
    /// peer, several credential headers, no authentication — one bucket.
    #[test]
    fn untrusted_identity_ignores_credential() {
        let peer = Some("203.0.113.9");
        let a = rate_limit_identity(
            false,
            peer,
            &headers(&[("authorization", "Bearer k1")]),
            &gateway(),
        );
        let b = rate_limit_identity(false, peer, &headers(&[("x-api-key", "k2")]), &gateway());
        let c = rate_limit_identity(false, peer, &HeaderMap::new(), &gateway());
        assert_eq!(a, "peer:203.0.113.9");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    /// Port of `test_trusted_identity_is_per_credential_and_owned_by_the_peer`.
    #[test]
    fn trusted_identity_is_per_credential_and_owned_by_the_peer() {
        let h = |v: &str| headers(&[("authorization", &format!("Bearer {v}"))]);
        let a = rate_limit_identity(true, Some("203.0.113.9"), &h("k1"), &gateway());
        let b = rate_limit_identity(true, Some("203.0.113.9"), &h("k2"), &gateway());
        let c = rate_limit_identity(true, Some("198.51.100.7"), &h("k1"), &gateway());
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("peer:203.0.113.9|cred:"), "{a}");
    }

    /// Port of `test_identity_covers_the_whole_credential`: a shared bucket
    /// must not come from hashing a prefix.
    #[test]
    fn identity_covers_the_whole_credential() {
        let h = |v: &str| headers(&[("x-api-key", v)]);
        let a = rate_limit_identity(false, Some("127.0.0.1"), &h("sk-live-aaaa1"), &gateway());
        let b = rate_limit_identity(false, Some("127.0.0.1"), &h("sk-live-aaaa2"), &gateway());
        assert_ne!(a, b);
        assert!(!a.contains("sk-live"), "{a}");
    }

    /// Port of `test_direct_loopback_stays_per_credential_without_a_token`.
    #[test]
    fn direct_loopback_stays_per_credential_without_a_token() {
        let h = |v: &str| headers(&[("x-api-key", v)]);
        let a = rate_limit_identity(false, Some("127.0.0.1"), &h("k1"), &gateway());
        let b = rate_limit_identity(false, Some("127.0.0.1"), &HeaderMap::new(), &gateway());
        assert_ne!(a, b);
        assert!(a.starts_with("peer:127.0.0.1|cred:"), "{a}");
        assert_eq!(b, "peer:127.0.0.1");
    }

    /// Port of `test_gateway_peer_alone_does_not_authenticate`.
    #[test]
    fn gateway_peer_alone_does_not_authenticate() {
        let h = headers(&[("authorization", "Bearer k1")]);
        let anon = rate_limit_identity(false, Some("10.20.3.4"), &h, &gateway());
        assert_eq!(anon, "peer:10.20.3.4");
        let authed = rate_limit_identity(true, Some("10.20.3.4"), &h, &gateway());
        assert!(authed.starts_with("peer:10.20.3.4|cred:"), "{authed}");
    }

    /// Port of `test_loopback_gateway_keeps_forwarded_client_per_credential`.
    #[test]
    fn loopback_gateway_keeps_forwarded_client() {
        let xff = headers(&[("x-forwarded-for", "203.0.113.9")]);
        let relayed_anon = rate_limit_identity(false, Some("127.0.0.1"), &xff, &gateway());
        assert_eq!(relayed_anon, "peer:203.0.113.9");
        let relayed_authed = rate_limit_identity(
            true,
            Some("127.0.0.1"),
            &headers(&[
                ("x-forwarded-for", "203.0.113.9"),
                ("authorization", "Bearer k1"),
            ]),
            &gateway(),
        );
        assert!(
            relayed_authed.starts_with("peer:203.0.113.9|cred:"),
            "{relayed_authed}"
        );
        // No forwarded address: still a direct loopback caller, per credential.
        let direct = rate_limit_identity(
            false,
            Some("127.0.0.1"),
            &headers(&[("authorization", "Bearer k1")]),
            &gateway(),
        );
        assert!(direct.starts_with("peer:127.0.0.1|cred:"), "{direct}");
    }

    /// Port of `test_unusable_forwarded_address_still_counts_as_relayed`.
    #[test]
    fn unusable_forwarded_address_still_relayed() {
        let xff = headers(&[("x-forwarded-for", ", 203.0.113.9"), ("x-api-key", "k1")]);
        let ident = rate_limit_identity(false, Some("127.0.0.1"), &xff, &gateway());
        assert!(!ident.contains("|cred:"), "{ident}");
    }

    /// Port of `test_ipv6_grouping`: a shared /64 shares a bucket, and
    /// IPv4-mapped IPv6 collapses onto its IPv4 peer.
    #[test]
    fn ipv6_grouping() {
        let empty = HeaderMap::new();
        let a = rate_limit_identity(false, Some("2001:db8::1"), &empty, &gateway());
        let b = rate_limit_identity(false, Some("2001:db8::2"), &empty, &gateway());
        let c = rate_limit_identity(false, Some("2001:db8:1::1"), &empty, &gateway());
        let d = rate_limit_identity(false, Some("::ffff:203.0.113.9"), &empty, &gateway());
        let e = rate_limit_identity(false, Some("203.0.113.9"), &empty, &gateway());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(d, e);
    }

    /// An unknown peer is not loopback and groups with itself, not everyone.
    #[test]
    fn unknown_peer_is_not_trusted() {
        let h = headers(&[("x-api-key", "k1")]);
        let ident = rate_limit_identity(false, None, &h, &gateway());
        assert_eq!(ident, "peer:unknown");
    }
}

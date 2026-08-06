//! Header filtering and X-Forwarded-* injection.
//!
//! Hop-by-hop headers per RFC 7230 §6.1 must not be forwarded.

use headroom_core::auth_mode::AuthMode;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::net::IpAddr;

/// Return true when inbound headers request full Headroom passthrough.
///
/// Checks `x-headroom-bypass: true` or `x-headroom-mode: passthrough`
/// (case-insensitive value match). Port of Python
/// `helpers._headroom_bypass_enabled`. Non-UTF-8 header values are treated
/// as absent.
pub fn headroom_bypass_enabled(headers: &HeaderMap) -> bool {
    let val = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_ascii_lowercase())
    };
    let bypass = val("x-headroom-bypass").as_deref() == Some("true");
    let passthrough = val("x-headroom-mode").as_deref() == Some("passthrough");
    bypass || passthrough
}

/// Extract `x-headroom-*` tags from inbound headers into a map with the
/// `x-headroom-` prefix stripped (lowercased key). Port of Python
/// `helpers.extract_tags`. Pure function, no I/O. Non-UTF-8 header values are
/// skipped.
pub fn extract_tags(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, value) in headers.iter() {
        let key = name.as_str().to_ascii_lowercase();
        if let Some(stripped) = key.strip_prefix(INTERNAL_HEADER_PREFIX) {
            if let Ok(v) = value.to_str() {
                out.insert(stripped.to_string(), v.to_string());
            }
        }
    }
    out
}

/// Split a comma-separated beta-header value into trimmed, non-empty tokens.
/// Port of Python `helpers._split_beta_tokens`.
fn split_beta_tokens(value: Option<&str>) -> Vec<&str> {
    match value {
        None => Vec::new(),
        Some(v) => v
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect(),
    }
}

/// Deterministic merge for `anthropic-beta` / `OpenAI-Beta` tokens.
///
/// Port of Python `helpers._merge_beta_tokens`. Client tokens come first in
/// original order; `headroom_required` tokens append in order, skipping any
/// already present. Dedupe is case-insensitive but the FIRST occurrence's
/// casing wins. Returns `""` when both inputs are empty.
pub fn merge_beta_tokens(client_value: Option<&str>, headroom_required: &[&str]) -> String {
    let mut seen_lower = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for token in split_beta_tokens(client_value) {
        let lower = token.to_ascii_lowercase();
        if seen_lower.insert(lower) {
            out.push(token.to_string());
        }
    }
    for token in headroom_required {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if seen_lower.insert(lower) {
            out.push(token.to_string());
        }
    }
    out.join(",")
}

/// Merge client `anthropic-beta` value with Headroom-required tokens. Thin
/// wrapper over [`merge_beta_tokens`] mirroring `helpers.merge_anthropic_beta`.
pub fn merge_anthropic_beta(client_value: Option<&str>, headroom_required: &[&str]) -> String {
    merge_beta_tokens(client_value, headroom_required)
}

/// Hop-by-hop header names that must be stripped (RFC 7230 §6.1).
/// Note: `Upgrade` is hop-by-hop in general, but for WebSocket the upgrade is
/// handled by the websocket module (axum's WebSocketUpgrade extracts it before
/// we ever try to forward it as plain HTTP).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Some additional headers that are typically managed by the HTTP client and
/// must not be copied across (reqwest/hyper sets them itself).
const CLIENT_MANAGED: &[&str] = &["host", "content-length"];

/// Internal-header prefix dropped from upstream-bound requests when
/// `Config::strip_internal_headers == StripInternalHeaders::Enabled` (PR-A5,
/// fixes P5-49). Case-insensitive prefix match. The Rust path mirrors the
/// Python `_strip_internal_headers` helper.
///
/// Response-side `X-Headroom-*` injection (e.g. `x-headroom-tokens-saved`)
/// is intentionally untouched — that direction is the proxy describing its
/// own work to the client and never crosses an upstream boundary.
pub const INTERNAL_HEADER_PREFIX: &str = "x-headroom-";

/// Per-request upstream override header. When present (after trimming
/// whitespace and stripping a trailing `/`), the proxy forwards this
/// request to the given base URL instead of the configured upstream.
/// Mirrors the Python proxy's `x-headroom-base-url` handling on the
/// OpenAI-compatible, Anthropic Messages (`/v1/messages`), and generic
/// passthrough routes.
pub const UPSTREAM_OVERRIDE_HEADER: &str = "x-headroom-base-url";

/// Returns true if `name` is hop-by-hop and must be stripped.
pub fn is_hop_by_hop(name: &HeaderName) -> bool {
    let n = name.as_str();
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(n))
}

/// Headers we additionally drop before forwarding the request to the upstream
/// (Host is rebuilt by reqwest for the upstream URL; Content-Length recomputed).
pub fn is_request_drop(name: &HeaderName) -> bool {
    if is_hop_by_hop(name) {
        return true;
    }
    let n = name.as_str();
    CLIENT_MANAGED.iter().any(|h| h.eq_ignore_ascii_case(n))
}

/// Returns true when `name` matches the internal `x-headroom-*` prefix
/// (case-insensitive). Pure function, no regex.
pub fn is_internal_header(name: &HeaderName) -> bool {
    name.as_str()
        .to_ascii_lowercase()
        .starts_with(INTERNAL_HEADER_PREFIX)
}

/// Headers we drop on the response side. Same hop-by-hop set; we don't touch
/// content-length since the response body length is known and we want clients
/// to see it.
pub fn is_response_drop(name: &HeaderName) -> bool {
    is_hop_by_hop(name)
}

/// Headers listed inside Connection: must be stripped too. Returns the lower-cased names.
pub fn connection_listed_headers(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Append `addr` to an existing X-Forwarded-For header, or set it.
pub fn append_xff(headers: &mut HeaderMap, addr: IpAddr) {
    let xff = HeaderName::from_static("x-forwarded-for");
    let new_value = match headers.get(&xff) {
        Some(existing) => match existing.to_str() {
            Ok(s) => format!("{s}, {addr}"),
            Err(_) => addr.to_string(),
        },
        None => addr.to_string(),
    };
    if let Ok(v) = HeaderValue::from_str(&new_value) {
        headers.insert(xff, v);
    }
}

/// Set `name` to `value`, replacing any prior value.
pub fn set_single(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(name, v);
    }
}

/// Remove every header whose name starts with `INTERNAL_HEADER_PREFIX`
/// (case-insensitive). Mutates in place. Returns the number of header
/// entries removed for structured logging.
pub fn strip_internal_headers(headers: &mut HeaderMap) -> usize {
    let to_remove: Vec<HeaderName> = headers
        .keys()
        .filter(|n| is_internal_header(n))
        .cloned()
        .collect();
    let mut removed = 0usize;
    for name in to_remove {
        // `remove` returns the first value; multi-valued internal headers are
        // not expected in practice but we drain them all for safety.
        while headers.remove(&name).is_some() {
            removed += 1;
        }
    }
    removed
}

/// Build a fresh HeaderMap suitable for forwarding to the upstream:
///   - hop-by-hop and connection-listed headers stripped
///   - Host/Content-Length removed (rebuilt by client)
///   - X-Forwarded-For appended
///   - X-Forwarded-Proto, X-Forwarded-Host set
///   - X-Request-Id ensured
///   - When `strip_internal == true`, `x-headroom-*` headers stripped
///     (PR-A5, fixes P5-49). Operators can disable via
///     `HEADROOM_PROXY_STRIP_INTERNAL_HEADERS=disabled` for diagnostic
///     shadow tracing.
///
/// PR-F4 (P5-53): the `X-Forwarded-*` / `X-Request-Id` injections are
/// skipped for [`AuthMode::Subscription`] — synthetic proxy headers on
/// subscription CLI traffic are a programmatic-fingerprint signal that
/// risks account revocation. PAYG and OAuth keep them.
pub fn build_forward_request_headers(
    incoming: &HeaderMap,
    client_addr: IpAddr,
    forwarded_proto: &str,
    forwarded_host: Option<&str>,
    request_id: &str,
    strip_internal: bool,
    auth_mode: AuthMode,
) -> HeaderMap {
    let connection_listed = connection_listed_headers(incoming);
    let mut out = HeaderMap::new();
    for (name, value) in incoming.iter() {
        if is_request_drop(name) {
            continue;
        }
        if connection_listed.iter().any(|h| h == name.as_str()) {
            continue;
        }
        if strip_internal && is_internal_header(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if auth_mode != AuthMode::Subscription {
        append_xff(&mut out, client_addr);
        set_single(
            &mut out,
            HeaderName::from_static("x-forwarded-proto"),
            forwarded_proto,
        );
        if let Some(host) = forwarded_host {
            set_single(&mut out, HeaderName::from_static("x-forwarded-host"), host);
        }
        set_single(
            &mut out,
            HeaderName::from_static("x-request-id"),
            request_id,
        );
    }
    out
}

/// Filter the upstream response headers before passing to the client.
pub fn filter_response_headers(incoming: &HeaderMap) -> HeaderMap {
    let connection_listed = connection_listed_headers(incoming);
    let mut out = HeaderMap::new();
    for (name, value) in incoming.iter() {
        if is_response_drop(name) {
            continue;
        }
        if connection_listed.iter().any(|h| h == name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;

    #[test]
    fn hop_by_hop_detection() {
        assert!(is_hop_by_hop(&HeaderName::from_static("connection")));
        assert!(is_hop_by_hop(&HeaderName::from_static("transfer-encoding")));
        assert!(is_hop_by_hop(&HeaderName::from_static("upgrade")));
        assert!(!is_hop_by_hop(&HeaderName::from_static("authorization")));
    }

    #[test]
    fn xff_appends() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        append_xff(&mut h, "5.6.7.8".parse().unwrap());
        assert_eq!(h.get("x-forwarded-for").unwrap(), "1.2.3.4, 5.6.7.8");
    }

    #[test]
    fn connection_listed_strip() {
        let mut h = HeaderMap::new();
        h.insert("connection", HeaderValue::from_static("close, x-foo"));
        h.insert("x-foo", HeaderValue::from_static("bar"));
        h.insert("x-keep", HeaderValue::from_static("yes"));
        let listed = connection_listed_headers(&h);
        assert!(listed.contains(&"x-foo".to_string()));
        assert!(listed.contains(&"close".to_string()));
    }

    #[test]
    fn internal_header_detected_case_insensitive() {
        assert!(is_internal_header(&HeaderName::from_static(
            "x-headroom-bypass"
        )));
        // HeaderName normalizes to lowercase internally, so any cased input
        // ends up matching the lowercase prefix.
        assert!(is_internal_header(
            &HeaderName::from_bytes(b"X-Headroom-Mode").unwrap()
        ));
        assert!(is_internal_header(
            &HeaderName::from_bytes(b"X-HEADROOM-FOO").unwrap()
        ));
        assert!(!is_internal_header(&HeaderName::from_static(
            "x-request-id"
        )));
        assert!(!is_internal_header(&HeaderName::from_static(
            "authorization"
        )));
    }

    #[test]
    fn strip_internal_headers_removes_only_internal_prefix() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer x"));
        h.insert("x-headroom-bypass", HeaderValue::from_static("true"));
        h.insert("x-headroom-mode", HeaderValue::from_static("passthrough"));
        h.insert("x-request-id", HeaderValue::from_static("req-1"));
        let removed = strip_internal_headers(&mut h);
        assert_eq!(removed, 2);
        assert!(h.get("x-headroom-bypass").is_none());
        assert!(h.get("x-headroom-mode").is_none());
        assert!(h.get("authorization").is_some());
        assert!(h.get("x-request-id").is_some());
    }

    #[test]
    fn build_forward_strips_internal_when_enabled() {
        let mut incoming = HeaderMap::new();
        incoming.insert("authorization", HeaderValue::from_static("Bearer x"));
        incoming.insert("x-headroom-bypass", HeaderValue::from_static("true"));
        let out = build_forward_request_headers(
            &incoming,
            "127.0.0.1".parse().unwrap(),
            "http",
            Some("h"),
            "req-1",
            true,
            AuthMode::Payg,
        );
        assert!(out.get("authorization").is_some());
        assert!(out.get("x-headroom-bypass").is_none());
    }

    #[test]
    fn build_forward_keeps_internal_when_disabled() {
        let mut incoming = HeaderMap::new();
        incoming.insert("authorization", HeaderValue::from_static("Bearer x"));
        incoming.insert("x-headroom-bypass", HeaderValue::from_static("true"));
        let out = build_forward_request_headers(
            &incoming,
            "127.0.0.1".parse().unwrap(),
            "http",
            Some("h"),
            "req-1",
            false,
            AuthMode::Payg,
        );
        assert!(out.get("authorization").is_some());
        assert!(out.get("x-headroom-bypass").is_some());
    }

    // ── PR-F4 (P5-53): X-Forwarded-* conditional on auth mode ────

    fn forward_for_mode(auth_mode: AuthMode) -> HeaderMap {
        let mut incoming = HeaderMap::new();
        incoming.insert("authorization", HeaderValue::from_static("Bearer x"));
        build_forward_request_headers(
            &incoming,
            "127.0.0.1".parse().unwrap(),
            "http",
            Some("h"),
            "req-1",
            true,
            auth_mode,
        )
    }

    #[test]
    fn payg_adds_xfwd() {
        let out = forward_for_mode(AuthMode::Payg);
        assert!(out.get("x-forwarded-for").is_some());
        assert!(out.get("x-forwarded-proto").is_some());
        assert!(out.get("x-forwarded-host").is_some());
        assert!(out.get("x-request-id").is_some());
    }

    #[test]
    fn oauth_adds_xfwd() {
        let out = forward_for_mode(AuthMode::OAuth);
        assert!(out.get("x-forwarded-for").is_some());
        assert!(out.get("x-forwarded-proto").is_some());
        assert!(out.get("x-forwarded-host").is_some());
        assert!(out.get("x-request-id").is_some());
    }

    #[test]
    fn subscription_no_xfwd() {
        let out = forward_for_mode(AuthMode::Subscription);
        assert!(out.get("x-forwarded-for").is_none());
        assert!(out.get("x-forwarded-proto").is_none());
        assert!(out.get("x-forwarded-host").is_none());
        assert!(out.get("x-request-id").is_none());
        // Non-synthetic headers still forwarded.
        assert!(out.get("authorization").is_some());
    }

    #[test]
    fn subscription_preserves_client_sent_xff() {
        // A client-sent X-Forwarded-For is forwarded as-is (copy loop);
        // Subscription mode only skips APPENDING our own hop.
        let mut incoming = HeaderMap::new();
        incoming.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let out = build_forward_request_headers(
            &incoming,
            "5.6.7.8".parse().unwrap(),
            "http",
            None,
            "req-1",
            true,
            AuthMode::Subscription,
        );
        assert_eq!(out.get("x-forwarded-for").unwrap(), "1.2.3.4");
    }

    // ── headroom_bypass_enabled ──────────────────────────────────

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn test_headroom_bypass_helper_is_transport_neutral() {
        assert!(headroom_bypass_enabled(&hm(&[(
            "x-headroom-bypass",
            "true"
        )])));
        assert!(headroom_bypass_enabled(&hm(&[(
            "x-headroom-mode",
            "passthrough"
        )])));
        assert!(headroom_bypass_enabled(&hm(&[(
            "x-headroom-bypass",
            "TRUE"
        )])));
        assert!(!headroom_bypass_enabled(&hm(&[("x-headroom-bypass", "1")])));
        assert!(!headroom_bypass_enabled(&HeaderMap::new()));
    }

    // ── extract_tags ─────────────────────────────────────────────

    #[test]
    fn test_extract_tags_strips_prefix_and_lowercases_key() {
        let h = hm(&[
            ("x-headroom-client", "codex"),
            ("X-Headroom-Project", "acme"),
            ("authorization", "Bearer x"),
        ]);
        let tags = extract_tags(&h);
        assert_eq!(tags.get("client").map(String::as_str), Some("codex"));
        assert_eq!(tags.get("project").map(String::as_str), Some("acme"));
        assert!(!tags.contains_key("authorization"));
        assert_eq!(tags.len(), 2);
    }

    // ── merge_beta_tokens / merge_anthropic_beta ─────────────────

    #[test]
    fn test_merge_helper_empty_inputs_returns_empty_string() {
        assert_eq!(merge_beta_tokens(None, &[]), "");
        assert_eq!(merge_beta_tokens(Some(""), &[]), "");
    }

    #[test]
    fn test_merge_helper_only_client() {
        assert_eq!(merge_beta_tokens(Some("a,b"), &[]), "a,b");
    }

    #[test]
    fn test_merge_helper_only_headroom() {
        assert_eq!(merge_beta_tokens(None, &["a", "b"]), "a,b");
    }

    #[test]
    fn test_merge_helper_preserves_client_order_and_appends_headroom() {
        assert_eq!(merge_beta_tokens(Some("z,a"), &["a", "m"]), "z,a,m");
    }

    #[test]
    fn test_dedupe_case_insensitive_preserves_first_casing() {
        assert_eq!(merge_beta_tokens(Some("Foo"), &["foo", "bar"]), "Foo,bar");
    }

    #[test]
    fn test_merge_helper_skips_empty_tokens() {
        assert_eq!(merge_beta_tokens(Some("a,,b, ,c"), &["", " "]), "a,b,c");
    }

    #[test]
    fn test_merge_helper_no_double_inject_when_already_present() {
        assert_eq!(merge_anthropic_beta(Some("a,b"), &["b"]), "a,b");
    }
}

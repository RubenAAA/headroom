//! Claude Code auto mode (upstream `12c15796`).
//!
//! An auto-mode turn asks Anthropic to run a safety classifier over the
//! model's tool calls. The client says so with a top-level `safeguards` field
//! in the body or a `dangerous-tool-use-*` token in `anthropic-beta`, and the
//! capability is negotiated by the client's own `anthropic-beta` and
//! `anthropic-version`. So on such a turn those two headers go upstream
//! exactly as the client sent them: no session-sticky union
//! (`cache_stabilization::beta_sticky`), no context-management or memory beta,
//! and the turn is not recorded into the sticky tracker either, so a
//! `dangerous-tool-use-*` token never sticks onto a later turn that dropped
//! it.
//!
//! Only a yes/no is taken from the body. The `safeguards` value itself is
//! never copied anywhere.

use axum::http::{HeaderMap, HeaderName};
use serde_json::Value;

const CAPABILITY_HEADERS: [HeaderName; 2] = [
    HeaderName::from_static("anthropic-beta"),
    HeaderName::from_static("anthropic-version"),
];

/// Whether `anthropic-beta` carries a `dangerous-tool-use-*` token, in any
/// of its header lines. Case-insensitive, like upstream's regex.
fn has_dangerous_tool_use_beta(headers: &HeaderMap) -> bool {
    const PREFIX: &str = "dangerous-tool-use-";
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|t| {
            t.len() > PREFIX.len()
                && t.get(..PREFIX.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(PREFIX))
        })
}

/// Whether this Anthropic turn runs the auto-mode classifier.
pub(crate) fn is_auto_mode_turn(body: &Value, client_headers: &HeaderMap) -> bool {
    body.get("safeguards").is_some() || has_dangerous_tool_use_beta(client_headers)
}

/// The client's `anthropic-beta` and `anthropic-version` lines, kept aside
/// before any stage touches the outgoing headers.
pub(crate) fn capability_headers(client_headers: &HeaderMap) -> HeaderMap {
    let mut kept = HeaderMap::new();
    for name in CAPABILITY_HEADERS {
        for value in client_headers.get_all(&name) {
            kept.append(name.clone(), value.clone());
        }
    }
    kept
}

/// Put the client's capability headers back on `outgoing`, replacing
/// whatever the pipeline made of them. A header the client did not send is
/// removed.
pub(crate) fn restore_capability_headers(kept: &HeaderMap, outgoing: &mut HeaderMap) {
    for name in CAPABILITY_HEADERS {
        outgoing.remove(&name);
        for value in kept.get_all(&name) {
            outgoing.append(name.clone(), value.clone());
        }
    }
}

/// Up to `limit` bytes of `data` for a log line, or a fixed marker when
/// `data` carries classifier payload (`safeguards` / `safeguard_results`)
/// anywhere, so none of it reaches the log.
pub(crate) fn log_preview(data: &[u8], limit: usize) -> String {
    if data.windows(9).any(|w| w == b"safeguard") {
        return "<redacted: classifier payload>".to_string();
    }
    String::from_utf8_lossy(&data[..data.len().min(limit)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn detects_either_signal_and_nothing_else() {
        let plain = json!({"messages": []});
        assert!(is_auto_mode_turn(
            &json!({"safeguards": {"x": 1}, "messages": []}),
            &HeaderMap::new()
        ));
        assert!(is_auto_mode_turn(
            &plain,
            &headers(&[("anthropic-beta", "a-1, Dangerous-Tool-Use-2026-09-01")])
        ));
        // A second header line counts too.
        assert!(is_auto_mode_turn(
            &plain,
            &headers(&[
                ("anthropic-beta", "a-1"),
                ("anthropic-beta", "dangerous-tool-use-x")
            ])
        ));
        assert!(!is_auto_mode_turn(
            &plain,
            &headers(&[("anthropic-beta", "context-management-2025-06-27")])
        ));
        assert!(!is_auto_mode_turn(
            &plain,
            &headers(&[(
                "anthropic-beta",
                "dangerous-tool-use, x-dangerous-tool-use-1"
            )])
        ));
    }

    #[test]
    fn restore_puts_back_exactly_what_the_client_sent() {
        let client = headers(&[
            ("anthropic-beta", "b-1,dangerous-tool-use-1"),
            ("anthropic-beta", "b-2"),
            ("x-api-key", "k"),
        ]);
        let kept = capability_headers(&client);
        let mut outgoing = headers(&[
            (
                "anthropic-beta",
                "b-1,dangerous-tool-use-1,sticky-3,context-management-2025-06-27",
            ),
            ("anthropic-version", "2023-06-01"),
            ("x-api-key", "k"),
        ]);
        restore_capability_headers(&kept, &mut outgoing);
        let betas: Vec<_> = outgoing.get_all("anthropic-beta").iter().collect();
        assert_eq!(betas, ["b-1,dangerous-tool-use-1", "b-2"]);
        // The client sent no version, so none goes out.
        assert!(outgoing.get("anthropic-version").is_none());
        assert_eq!(outgoing["x-api-key"], "k");
    }

    #[test]
    fn log_preview_never_shows_classifier_payload() {
        let event = br#"{"type":"safeguard_results","result":{"decision":"allow"}}"#;
        assert_eq!(log_preview(event, 96), "<redacted: classifier payload>");
        assert_eq!(log_preview(b"{\"type\":\"ping\"}", 6), "{\"type");
    }
}

//! Pin the Claude Code billing header that sits in `system[0]`.
//!
//! Claude Code puts a line like
//!
//! ```text
//! x-anthropic-billing-header: cc_version=2.1.231.ea8; cc_entrypoint=cli;
//! ```
//!
//! in the FIRST block of the `system` array (subagents append
//! `cc_is_subagent=true;`). Despite the name it is not a header — it is cached
//! content. No `cache_control` marker ever lands on block 0; the client places
//! them on blocks 1 and 2. Anthropic matches a cached prefix from byte 0, so
//! whatever sits in block 0 is inside every cached prefix while carrying no
//! marker of its own, and any change to it invalidates the whole prefix.
//!
//! Measured over 1,105 captured requests on 2026-08-11..13:
//!
//! - `cc_entrypoint` was `cli` on every single one. There is nothing to
//!   normalise there today; we pin it so a future entrypoint cannot silently
//!   split the cache.
//! - The tail of `cc_version` (`.ea8`, `.511`, `.a51`) is a per-process id,
//!   constant within a session and different across them.
//! - The version itself churned three times in two and a half days — 2.1.226,
//!   2.1.227, 2.1.231 — because Claude Code updates itself. Worse, 2.1.226 and
//!   2.1.227 were live at the SAME time on 08-11: an update landed while
//!   sessions were running, so old processes kept sending the old string. Two
//!   concurrent sessions with identical tools could not share a prefix.
//!
//! So we latch the first version this process observes and replay it for the
//! rest of the process's life. The cache then survives a client auto-update and
//! resets only when the proxy restarts, which is a boundary we control.
//!
//! `cc_is_subagent` is deliberately passed through untouched. It is a claim
//! about what the caller is, not incidental churn, and rewriting it would
//! misreport mode to the provider. It also buys nothing: subagents run a
//! different model from their parent, so the two can never share a prefix
//! whatever this block says.

use std::sync::OnceLock;

/// The version latched on the first request of this process that carries one.
static PINNED_CC_VERSION: OnceLock<String> = OnceLock::new();

/// Marks the `system[0]` text block we rewrite. Anything else is left alone.
const HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// Rewrite the billing header in `system[0]`, if there is one.
///
/// Byte-equal passthrough — the same cache-safety invariant
/// `sanitize_anthropic_model_id_in_body` keeps — when:
///
/// - the body is not valid JSON
/// - `system` is absent, or is not an array (the string form carries no
///   billing header)
/// - `system[0]` has no string `text` field
/// - that text does not start with `x-anthropic-billing-header:`
/// - no version has been observed yet and none is latched
/// - the rewrite reproduces the original text
/// - re-serialization fails
pub fn pin_billing_header_in_body(body: bytes::Bytes) -> bytes::Bytes {
    let Ok(mut parsed) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };

    let Some(serde_json::Value::Array(system)) = parsed.get_mut("system") else {
        return body;
    };

    let Some(first) = system.first_mut() else {
        return body;
    };

    let Some(serde_json::Value::String(text)) = first.get_mut("text") else {
        return body;
    };

    let Some(pinned) = pin_header_text(text, &PINNED_CC_VERSION) else {
        return body;
    };

    if pinned == *text {
        return body;
    }

    *text = pinned;
    match serde_json::to_vec(&parsed) {
        Ok(buf) => bytes::Bytes::from(buf),
        Err(_) => body,
    }
}

/// Pure core: rebuild the header text with a pinned version and `cli`
/// entrypoint. Takes the cell so tests can latch in isolation instead of
/// racing the process-wide static.
///
/// Emits fields in a fixed order — version, entrypoint, then everything else as
/// it arrived — so two requests carrying the same fields in a different order
/// still produce identical bytes.
fn pin_header_text(text: &str, cell: &OnceLock<String>) -> Option<String> {
    let rest = text.strip_prefix(HEADER_PREFIX)?;

    let mut observed_version = None;
    let mut others = Vec::new();
    for field in rest.split(';') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        match field.split_once('=') {
            Some(("cc_version", value)) => observed_version = Some(value),
            // Dropped, then re-emitted as `cli` below.
            Some(("cc_entrypoint", _)) => {}
            _ => others.push(field),
        }
    }

    // Latch the first version we ever see. When a request arrives without one
    // and nothing is latched yet, there is no version to invent — leave the
    // body untouched rather than emit a header we made up.
    let pinned = match observed_version {
        Some(observed) => cell.get_or_init(|| observed.to_string()),
        None => cell.get()?,
    };

    let mut out = String::with_capacity(text.len() + pinned.len());
    out.push_str(HEADER_PREFIX);
    out.push_str(" cc_version=");
    out.push_str(pinned);
    out.push_str("; cc_entrypoint=cli;");
    for field in others {
        out.push(' ');
        out.push_str(field);
        out.push(';');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latched(version: &str) -> OnceLock<String> {
        let cell = OnceLock::new();
        cell.set(version.to_string()).unwrap();
        cell
    }

    #[test]
    fn first_version_seen_is_the_one_replayed() {
        let cell = OnceLock::new();
        let first = pin_header_text(
            "x-anthropic-billing-header: cc_version=2.1.226.be8; cc_entrypoint=cli;",
            &cell,
        );
        assert_eq!(
            first.as_deref(),
            Some("x-anthropic-billing-header: cc_version=2.1.226.be8; cc_entrypoint=cli;")
        );

        // A client auto-update mid-run must not move the cache.
        let after_update = pin_header_text(
            "x-anthropic-billing-header: cc_version=2.1.231.ea8; cc_entrypoint=cli;",
            &cell,
        );
        assert_eq!(after_update, first);
    }

    #[test]
    fn per_process_suffixes_collapse_to_one_string() {
        let cell = latched("2.1.231.ea8");
        let a = pin_header_text(
            "x-anthropic-billing-header: cc_version=2.1.231.511; cc_entrypoint=cli;",
            &cell,
        );
        let b = pin_header_text(
            "x-anthropic-billing-header: cc_version=2.1.231.a51; cc_entrypoint=cli;",
            &cell,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn subagent_flag_survives() {
        let cell = latched("2.1.231.ea8");
        let out = pin_header_text(
            "x-anthropic-billing-header: cc_version=2.1.231.511; cc_entrypoint=cli; cc_is_subagent=true;",
            &cell,
        );
        assert_eq!(
            out.as_deref(),
            Some(
                "x-anthropic-billing-header: cc_version=2.1.231.ea8; cc_entrypoint=cli; cc_is_subagent=true;"
            )
        );
    }

    #[test]
    fn entrypoint_is_forced_to_cli_and_added_when_missing() {
        let cell = latched("2.1.231.ea8");
        let rewritten = pin_header_text(
            "x-anthropic-billing-header: cc_version=2.1.231.ea8; cc_entrypoint=sdk;",
            &cell,
        );
        let added = pin_header_text("x-anthropic-billing-header: cc_version=2.1.231.ea8;", &cell);
        let expected =
            Some("x-anthropic-billing-header: cc_version=2.1.231.ea8; cc_entrypoint=cli;");
        assert_eq!(rewritten.as_deref(), expected);
        assert_eq!(added.as_deref(), expected);
    }

    #[test]
    fn version_is_supplied_when_the_client_omits_it() {
        let cell = latched("2.1.231.ea8");
        let out = pin_header_text(
            "x-anthropic-billing-header: cc_entrypoint=cli; cc_is_subagent=true;",
            &cell,
        );
        assert_eq!(
            out.as_deref(),
            Some(
                "x-anthropic-billing-header: cc_version=2.1.231.ea8; cc_entrypoint=cli; cc_is_subagent=true;"
            )
        );
    }

    #[test]
    fn nothing_latched_and_nothing_observed_leaves_the_text_alone() {
        let cell = OnceLock::new();
        assert_eq!(
            pin_header_text("x-anthropic-billing-header: cc_entrypoint=cli;", &cell),
            None
        );
    }

    #[test]
    fn non_header_text_is_ignored() {
        let cell = latched("2.1.231.ea8");
        assert_eq!(
            pin_header_text("You are Claude Code, Anthropic's official CLI.", &cell),
            None
        );
    }

    #[test]
    fn body_passthrough_is_byte_equal_when_nothing_changes() {
        let body = bytes::Bytes::from_static(
            br#"{"model":"claude-opus-5","system":[{"type":"text","text":"You are Claude Code."}],"messages":[]}"#,
        );
        let out = pin_billing_header_in_body(body.clone());
        assert_eq!(out, body);
    }

    #[test]
    fn body_without_system_is_untouched() {
        let body = bytes::Bytes::from_static(br#"{"model":"claude-opus-5","messages":[]}"#);
        let out = pin_billing_header_in_body(body.clone());
        assert_eq!(out, body);
    }

    #[test]
    fn body_rewrite_touches_only_the_first_system_block() {
        let body = bytes::Bytes::from_static(
            br#"{"model":"claude-opus-5","system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=9.9.9.zzz; cc_entrypoint=vscode;"},{"type":"text","text":"You are Claude Code."}],"messages":[]}"#,
        );
        let out = pin_billing_header_in_body(body);
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let first = parsed["system"][0]["text"].as_str().unwrap();
        assert!(
            first.contains("cc_entrypoint=cli;"),
            "entrypoint should be forced to cli, got {first}"
        );
        assert_eq!(parsed["system"][1]["text"], "You are Claude Code.");
    }
}

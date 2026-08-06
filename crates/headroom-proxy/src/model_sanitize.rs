//! Anthropic model-id sanitization.
//!
//! Terminal wrappers occasionally leak ANSI styling into the model id
//! that reaches Headroom — most commonly a dangling bold-reset suffix
//! such as `glm-5.2[1m]`. Anthropic-compatible upstreams reject the
//! decorated id, so we strip the styling artifacts before forwarding
//! `/v1/messages` upstream.
//!
//! Ports Python's `sanitize_anthropic_model_id` (headroom/providers/
//! anthropic.py). The Python version originally scoped the dangling-
//! suffix cleanup to `claude-` ids; commit e22d7453 generalized it to
//! all Anthropic-compatible model ids (e.g. `glm-5.2[1m]`), which this
//! port mirrors.

use bytes::Bytes;
use std::sync::OnceLock;

use regex_lite::Regex;

/// Full ANSI escape sequences (CSI + a few C1 forms).
fn ansi_escape_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").unwrap())
}

/// Trailing bare `[…m]` SGR-style fragments left after a terminal
/// dropped the leading `\x1b` — e.g. `[1m]`, `[0;1m]`, or repeats.
fn dangling_style_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:\[[0-9;]*m\])+$").unwrap())
}

/// Return an Anthropic model id without terminal styling artifacts.
pub fn sanitize_anthropic_model_id(model: &str) -> String {
    let cleaned = ansi_escape_re().replace_all(model, "");
    let cleaned = cleaned.trim();
    dangling_style_suffix_re()
        .replace_all(cleaned, "")
        .into_owned()
}

/// Sanitize `body["model"]` in a buffered Anthropic Messages request.
///
/// No-op (returns the original bytes) when the body is not a JSON
/// object, has no string `model`, or the model needs no cleanup. On any
/// serialize failure the original bytes are returned so the passthrough
/// invariant holds.
pub fn sanitize_anthropic_model_in_body(body: Bytes) -> Bytes {
    let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(obj) = parsed.as_object_mut() else {
        return body;
    };
    let Some(raw) = obj.get("model").and_then(|m| m.as_str()) else {
        return body;
    };
    let cleaned = sanitize_anthropic_model_id(raw);
    if cleaned == raw {
        return body;
    }
    obj.insert("model".to_string(), serde_json::Value::String(cleaned));
    match serde_json::to_vec(&parsed) {
        Ok(v) => Bytes::from(v),
        Err(_) => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_dangling_bold_suffix() {
        assert_eq!(sanitize_anthropic_model_id("glm-5.2[1m]"), "glm-5.2");
    }

    #[test]
    fn strips_repeated_and_multi_param_suffix() {
        assert_eq!(
            sanitize_anthropic_model_id("claude-fable-5[0;1m][1m]"),
            "claude-fable-5"
        );
    }

    #[test]
    fn strips_full_ansi_escape() {
        assert_eq!(
            sanitize_anthropic_model_id("\x1b[1mclaude-opus-4-8\x1b[0m"),
            "claude-opus-4-8"
        );
    }

    #[test]
    fn leaves_clean_id_untouched() {
        assert_eq!(sanitize_anthropic_model_id("glm-5.2"), "glm-5.2");
    }

    #[test]
    fn body_model_sanitized() {
        let body =
            Bytes::from(serde_json::to_vec(&json!({"model": "glm-5.2[1m]", "x": 1})).unwrap());
        let out = sanitize_anthropic_model_in_body(body);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], json!("glm-5.2"));
        assert_eq!(v["x"], json!(1));
    }

    #[test]
    fn clean_body_returned_verbatim() {
        let body = Bytes::from_static(b"{\"model\":\"glm-5.2\"}");
        assert_eq!(sanitize_anthropic_model_in_body(body.clone()), body);
    }

    #[test]
    fn non_object_body_untouched() {
        let body = Bytes::from_static(b"[1,2]");
        assert_eq!(sanitize_anthropic_model_in_body(body.clone()), body);
    }
}

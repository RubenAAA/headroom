//! Request-body helpers for byte-faithful forwarding and message splicing.
//!
//! Rust port of the body-handling helpers in `headroom/proxy/helpers.py`:
//!   * `serialize_body_canonical` (~L333)
//!   * `BodyMutationTracker` (~L347)
//!   * `prepare_outbound_body_bytes` (~L386)
//!   * `_read_request_body_bytes` inbound decompression (~L2746) → [`decode_body`]
//!   * `append_text_to_latest_user_chat_message` (~L474)
//!   * `append_text_to_latest_user_input_item` (~L535)
//!
//! `serde_json` is configured workspace-wide with `preserve_order` (IndexMap),
//! so canonical re-serialization keeps insertion order like Python's
//! `json.dumps`, and `to_vec` is already compact + non-ASCII-preserving.

use serde_json::Value;

/// Forwarder byte-selection mode. Mirror of Python's `PythonForwarderMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwarderMode {
    /// Default: unmutated → passthrough original bytes; mutated → canonical.
    ByteFaithful,
    /// Rollback-only operator opt-in: always re-encode with legacy httpx-style
    /// separators (`", "` / `": "`) and ASCII escaping.
    LegacyJsonKwarg,
}

/// Re-serialize a request body deterministically with cache-stable formatting.
///
/// Compact separators, UTF-8 preserved (no `\uXXXX` escapes), insertion order
/// preserved — matching well-behaved API clients. Port of
/// `helpers.serialize_body_canonical`.
pub fn serialize_body_canonical(body: &Value) -> Vec<u8> {
    // serde_json::to_vec is compact (no spaces) and non-ASCII-preserving.
    serde_json::to_vec(body).unwrap_or_default()
}

/// Records whether a request body was mutated and why.
///
/// Port of `helpers.BodyMutationTracker`. The forwarder reads `mutated()` to
/// choose between byte-faithful passthrough and canonical re-serialization.
/// Reasons are order-preserving and de-duplicated.
#[derive(Debug, Default)]
pub struct BodyMutationTracker {
    mutated: bool,
    reasons: Vec<&'static str>,
}

impl BodyMutationTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the body as mutated and record a stable snake_case reason.
    /// Panics on an empty reason (mirrors Python's `ValueError`).
    pub fn mark_mutated(&mut self, reason: &'static str) {
        assert!(
            !reason.is_empty(),
            "BodyMutationTracker::mark_mutated: reason must be non-empty"
        );
        self.mutated = true;
        if !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
    }

    pub fn mutated(&self) -> bool {
        self.mutated
    }

    pub fn reasons(&self) -> &[&'static str] {
        &self.reasons
    }
}

/// Pick the outbound body bytes for a forwarder call.
///
/// Returns `(outbound_bytes, source_label)` where `source_label` is one of
/// `"passthrough"`, `"canonical"`, or `"legacy"` — matching Python's
/// `prepare_outbound_body_bytes` return contract.
pub fn prepare_outbound_body_bytes(
    body: &Value,
    original_body_bytes: &[u8],
    body_mutated: bool,
    mode: ForwarderMode,
) -> (Vec<u8>, &'static str) {
    match mode {
        ForwarderMode::LegacyJsonKwarg => (serialize_legacy_json(body), "legacy"),
        ForwarderMode::ByteFaithful => {
            if body_mutated {
                (serialize_body_canonical(body), "canonical")
            } else {
                (original_body_bytes.to_vec(), "passthrough")
            }
        }
    }
}

/// Legacy httpx-style JSON: `separators=(", ", ": ")`, `ensure_ascii=True`.
/// Order-preserving recursive writer to match Python `json.dumps` byte output
/// for the rollback path.
fn serialize_legacy_json(v: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_legacy(v, &mut out);
    out.into_bytes()
}

fn write_legacy(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_ascii_json_string(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_legacy(e, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, val)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_ascii_json_string(k, out);
                out.push_str(": ");
                write_legacy(val, out);
            }
            out.push('}');
        }
    }
}

/// Write a JSON string literal with `ensure_ascii=True` semantics: non-ASCII
/// chars are escaped as `\uXXXX` (surrogate pairs for astral chars).
fn write_ascii_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if c.is_ascii() => out.push(c),
            c => {
                let cp = c as u32;
                if cp > 0xFFFF {
                    let v = cp - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Read and (if needed) decompress a request body, returning raw bytes.
///
/// Port of Python `helpers._read_request_body_bytes`. Supports
/// `gzip`/`deflate` (flate2), `zstd`/`zstandard`, and `br` (brotli).
/// `None`, empty, or `identity` returns the bytes unchanged. Any other
/// encoding or a decompression failure returns `Err`.
pub fn decode_body(bytes: &[u8], content_encoding: Option<&str>) -> Result<Vec<u8>, String> {
    let encoding = content_encoding.unwrap_or("").trim().to_ascii_lowercase();
    match encoding.as_str() {
        "" | "identity" => Ok(bytes.to_vec()),
        "zstd" | "zstandard" => zstd::stream::decode_all(bytes)
            .map_err(|e| format!("Failed to decompress zstd request body: {e}")),
        "gzip" => {
            use std::io::Read;
            let mut d = flate2::read::GzDecoder::new(bytes);
            let mut out = Vec::new();
            d.read_to_end(&mut out)
                .map(|_| out)
                .map_err(|e| format!("Failed to decompress gzip request body: {e}"))
        }
        "deflate" => {
            use std::io::Read;
            let mut d = flate2::read::ZlibDecoder::new(bytes);
            let mut out = Vec::new();
            d.read_to_end(&mut out)
                .map(|_| out)
                .map_err(|e| format!("Failed to decompress deflate request body: {e}"))
        }
        "br" => {
            use std::io::Read;
            let mut d = brotli::Decompressor::new(bytes, 4096);
            let mut out = Vec::new();
            d.read_to_end(&mut out)
                .map(|_| out)
                .map_err(|e| format!("Failed to decompress brotli request body: {e}"))
        }
        other => Err(format!("Unsupported Content-Encoding: {other}")),
    }
}

/// Number of chars in `text` (Python `len(str)` semantics for the
/// "bytes_appended" return of the splicing helpers).
fn appended_len(text: &str) -> usize {
    text.chars().count()
}

/// Append `context_text` to the first text block of the latest user chat
/// message (OpenAI Chat Completions shape). Mutates `messages` in place.
///
/// Returns the number of chars appended (0 when no eligible user message was
/// found — no mutation). Port of `helpers.append_text_to_latest_user_chat_message`.
pub fn append_text_to_latest_user_chat_message(
    messages: &mut Vec<Value>,
    context_text: &str,
) -> usize {
    splice_latest_user(messages, context_text, &["text", "input_text"])
}

/// OpenAI Responses `body["input"]` analog. Same semantics; the eligible text
/// block types are `input_text`/`text`. Port of
/// `helpers.append_text_to_latest_user_input_item`.
pub fn append_text_to_latest_user_input_item(items: &mut Vec<Value>, context_text: &str) -> usize {
    splice_latest_user(items, context_text, &["input_text", "text"])
}

/// Shared splice: find the latest `role == "user"` item and append to its
/// first eligible text block (string content, or first matching typed block).
fn splice_latest_user(items: &mut [Value], context_text: &str, text_types: &[&str]) -> usize {
    if items.is_empty() || context_text.is_empty() {
        return 0;
    }

    for idx in (0..items.len()).rev() {
        let item = &items[idx];
        if !item.is_object() {
            continue;
        }
        if item.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }

        let content = item.get("content").cloned();
        match content {
            Some(Value::String(s)) => {
                let joined = format!("{s}\n\n{context_text}");
                items[idx]["content"] = Value::String(joined);
                return appended_len(context_text);
            }
            Some(Value::Array(parts)) if !parts.is_empty() => {
                let mut new_parts = Vec::with_capacity(parts.len());
                let mut appended = false;
                for part in parts {
                    let is_text = !appended
                        && part.is_object()
                        && part
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|t| text_types.contains(&t));
                    if is_text {
                        let existing = part.get("text").and_then(Value::as_str).unwrap_or("");
                        let mut new_part = part.clone();
                        new_part["text"] = Value::String(format!("{existing}\n\n{context_text}"));
                        new_parts.push(new_part);
                        appended = true;
                    } else {
                        new_parts.push(part);
                    }
                }
                if appended {
                    items[idx]["content"] = Value::Array(new_parts);
                    return appended_len(context_text);
                }
                // User item but no eligible text block — stop, no mutation.
                return 0;
            }
            _ => return 0,
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── serialize_body_canonical ─────────────────────────────────

    #[test]
    fn test_canonical_is_compact_and_preserves_unicode() {
        let body = json!({"b": 1, "a": "café"});
        let bytes = serialize_body_canonical(&body);
        // preserve_order: keys stay b,a; compact: no spaces; UTF-8 preserved.
        assert_eq!(String::from_utf8(bytes).unwrap(), r#"{"b":1,"a":"café"}"#);
    }

    // ── BodyMutationTracker ──────────────────────────────────────

    #[test]
    fn test_tracker_starts_unmutated() {
        let t = BodyMutationTracker::new();
        assert!(!t.mutated());
        assert!(t.reasons().is_empty());
    }

    #[test]
    fn test_tracker_marks_and_dedupes_reasons() {
        let mut t = BodyMutationTracker::new();
        t.mark_mutated("memory_injection");
        t.mark_mutated("memory_injection");
        t.mark_mutated("compression_smart_crusher");
        assert!(t.mutated());
        assert_eq!(
            t.reasons(),
            &["memory_injection", "compression_smart_crusher"]
        );
    }

    #[test]
    #[should_panic]
    fn test_tracker_empty_reason_panics() {
        BodyMutationTracker::new().mark_mutated("");
    }

    // ── prepare_outbound_body_bytes ──────────────────────────────

    #[test]
    fn test_prepare_unmutated_is_passthrough() {
        let body = json!({"a": 1});
        let original = b"{\"a\":  1}"; // deliberately non-canonical spacing
        let (out, src) =
            prepare_outbound_body_bytes(&body, original, false, ForwarderMode::ByteFaithful);
        assert_eq!(src, "passthrough");
        assert_eq!(out, original);
    }

    #[test]
    fn test_prepare_mutated_is_canonical() {
        let body = json!({"a": 1});
        let (out, src) = prepare_outbound_body_bytes(&body, b"", true, ForwarderMode::ByteFaithful);
        assert_eq!(src, "canonical");
        assert_eq!(out, br#"{"a":1}"#);
    }

    #[test]
    fn test_prepare_legacy_mode_reencodes() {
        let body = json!({"a": 1, "b": "café"});
        let (out, src) =
            prepare_outbound_body_bytes(&body, b"", false, ForwarderMode::LegacyJsonKwarg);
        assert_eq!(src, "legacy");
        // legacy: spaces after separators, ensure_ascii escapes non-ASCII.
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"a\": 1, \"b\": \"caf\\u00e9\"}"
        );
    }

    #[test]
    fn test_legacy_escapes_astral_as_surrogate_pair() {
        let body = json!({"e": "😀"});
        let (out, _) =
            prepare_outbound_body_bytes(&body, b"", false, ForwarderMode::LegacyJsonKwarg);
        // astral char → UTF-16 surrogate pair escape (Python ensure_ascii).
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"e\": \"\\ud83d\\ude00\"}"
        );
    }

    // ── decode_body ──────────────────────────────────────────────

    #[test]
    fn test_decode_identity_and_none() {
        assert_eq!(decode_body(b"hello", None).unwrap(), b"hello");
        assert_eq!(decode_body(b"hello", Some("identity")).unwrap(), b"hello");
        assert_eq!(decode_body(b"hello", Some("")).unwrap(), b"hello");
    }

    #[test]
    fn test_decode_gzip_roundtrip() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"the quick brown fox").unwrap();
        let comp = enc.finish().unwrap();
        assert_eq!(
            decode_body(&comp, Some("gzip")).unwrap(),
            b"the quick brown fox"
        );
    }

    #[test]
    fn test_decode_deflate_roundtrip() {
        use flate2::{write::ZlibEncoder, Compression};
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"payload").unwrap();
        let comp = enc.finish().unwrap();
        assert_eq!(decode_body(&comp, Some("deflate")).unwrap(), b"payload");
    }

    #[test]
    fn test_decode_zstd_roundtrip() {
        let comp = zstd::stream::encode_all(&b"zstd payload"[..], 0).unwrap();
        assert_eq!(decode_body(&comp, Some("zstd")).unwrap(), b"zstd payload");
        assert_eq!(
            decode_body(&comp, Some("zstandard")).unwrap(),
            b"zstd payload"
        );
    }

    #[test]
    fn test_decode_brotli_roundtrip() {
        use std::io::Write;
        let mut comp = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut comp, 4096, 5, 22);
            w.write_all(b"brotli payload").unwrap();
        }
        assert_eq!(decode_body(&comp, Some("br")).unwrap(), b"brotli payload");
    }

    #[test]
    fn test_decode_unsupported_encoding_errors() {
        assert!(decode_body(b"x", Some("snappy")).is_err());
    }

    // ── message splicing ─────────────────────────────────────────

    #[test]
    fn test_append_chat_string_content() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
        ];
        let n = append_text_to_latest_user_chat_message(&mut msgs, "CTX");
        assert_eq!(n, 3);
        assert_eq!(msgs[1]["content"], json!("hi\n\nCTX"));
    }

    #[test]
    fn test_append_chat_latest_user_only() {
        let mut msgs = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": "a"}),
            json!({"role": "user", "content": "second"}),
        ];
        append_text_to_latest_user_chat_message(&mut msgs, "X");
        assert_eq!(msgs[0]["content"], json!("first"));
        assert_eq!(msgs[2]["content"], json!("second\n\nX"));
    }

    #[test]
    fn test_append_chat_list_content_first_text_block() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": "u"}},
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "second"}
            ]
        })];
        let n = append_text_to_latest_user_chat_message(&mut msgs, "CTX");
        assert_eq!(n, 3);
        assert_eq!(msgs[0]["content"][1]["text"], json!("hello\n\nCTX"));
        // only the first eligible text block is touched
        assert_eq!(msgs[0]["content"][2]["text"], json!("second"));
    }

    #[test]
    fn test_append_chat_no_eligible_block_is_noop() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [{"type": "image_url", "image_url": {"url": "u"}}]
        })];
        let before = msgs.clone();
        let n = append_text_to_latest_user_chat_message(&mut msgs, "CTX");
        assert_eq!(n, 0);
        assert_eq!(msgs, before);
    }

    #[test]
    fn test_append_empty_context_is_noop() {
        let mut msgs = vec![json!({"role": "user", "content": "hi"})];
        assert_eq!(append_text_to_latest_user_chat_message(&mut msgs, ""), 0);
        assert_eq!(msgs[0]["content"], json!("hi"));
    }

    #[test]
    fn test_append_input_item_responses_shape() {
        let mut items = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "q"}]
        })];
        let n = append_text_to_latest_user_input_item(&mut items, "CTX");
        assert_eq!(n, 3);
        assert_eq!(items[0]["content"][0]["text"], json!("q\n\nCTX"));
    }
}

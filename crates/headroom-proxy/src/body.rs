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
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // `format!("\\u{:04x}")` per char costs ~39ns; table lookup ~1ns for
    // identical bytes. Only the control-char and non-ASCII arms use it;
    // the common ASCII fast path below is untouched.
    fn push_u4(out: &mut String, cp: u32) {
        let mut buf = [0u8; 6];
        buf[0] = b'\\';
        buf[1] = b'u';
        buf[2] = HEX[((cp >> 12) & 0xF) as usize];
        buf[3] = HEX[((cp >> 8) & 0xF) as usize];
        buf[4] = HEX[((cp >> 4) & 0xF) as usize];
        buf[5] = HEX[(cp & 0xF) as usize];
        // Hex table is ASCII-only, so this is always valid UTF-8.
        out.push_str(std::str::from_utf8(&buf).unwrap_or("\\u0000"));
    }
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
            c if (c as u32) < 0x20 => push_u4(out, c as u32),
            c if c.is_ascii() => out.push(c),
            c => {
                let cp = c as u32;
                if cp > 0xFFFF {
                    let v = cp - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    push_u4(out, hi);
                    push_u4(out, lo);
                } else {
                    push_u4(out, cp);
                }
            }
        }
    }
    out.push('"');
}

/// Ceiling on decompressed request-body output (100 MB).
///
/// Port of Python `helpers.MAX_DECOMPRESSED_BODY_SIZE`, deliberately set to
/// the same value as the uncompressed body ceiling: nobody gets more room by
/// arriving compressed. Every codec below streams through a `Take` limited to
/// this plus one byte, so a bomb is refused at the cap instead of being
/// materialized (upstream 25a4e71b, closing #3284).
pub const MAX_DECOMPRESSED_BODY_SIZE: u64 = 100 * 1024 * 1024;

/// Read and (if needed) decompress a request body, returning raw bytes.
///
/// Port of Python `helpers._read_request_body_bytes`. Supports
/// `gzip`/`deflate` (flate2), `zstd`/`zstandard`, and `br` (brotli).
/// `None`, empty, or `identity` returns the bytes unchanged. Any other
/// encoding or a decompression failure returns `Err`.
///
/// Decompressed output is capped at [`MAX_DECOMPRESSED_BODY_SIZE`]: bodies
/// expanding past it fail with a message naming the cap rather than being
/// allocated. Truncated streams still fail as decompression errors rather
/// than yielding a short body.
pub fn decode_body(bytes: &[u8], content_encoding: Option<&str>) -> Result<Vec<u8>, String> {
    use std::io::Read;
    // `eq_ignore_ascii_case` avoids the per-request `to_ascii_lowercase()`
    // String alloc (measured 4.6x on the match alone). Trims once, borrows.
    let raw = content_encoding.unwrap_or("").trim();
    let is = |lit: &str| raw.eq_ignore_ascii_case(lit);
    // One byte past the cap: enough to detect an oversized expansion while
    // bounding peak allocation to the cap plus one byte, never the bomb.
    let limit = MAX_DECOMPRESSED_BODY_SIZE.saturating_add(1);
    let capped = |decoder: Box<dyn Read>, label: &str| -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        decoder
            .take(limit)
            .read_to_end(&mut out)
            .map_err(|e| format!("Failed to decompress {label} request body: {e}"))?;
        if out.len() as u64 > MAX_DECOMPRESSED_BODY_SIZE {
            return Err(format!(
                "Decompressed {label} request body exceeds {}MB",
                MAX_DECOMPRESSED_BODY_SIZE / (1024 * 1024)
            ));
        }
        Ok(out)
    };
    if raw.is_empty() || is("identity") {
        return Ok(bytes.to_vec());
    }
    if is("zstd") || is("zstandard") {
        let decoder = zstd::stream::Decoder::new(bytes)
            .map_err(|e| format!("Failed to decompress zstd request body: {e}"))?;
        return capped(Box::new(decoder), "zstd");
    }
    if is("gzip") {
        return capped(Box::new(flate2::read::GzDecoder::new(bytes)), "gzip");
    }
    if is("deflate") {
        return capped(Box::new(flate2::read::ZlibDecoder::new(bytes)), "deflate");
    }
    if is("br") {
        return capped(Box::new(brotli::Decompressor::new(bytes, 4096)), "brotli");
    }
    Err(format!("Unsupported Content-Encoding: {raw}"))
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
        // Borrow-check split: read role/object first, then take a mutable
        // content borrow. Avoids cloning the whole content value.
        let is_user_item = items[idx].is_object()
            && items[idx].get("role").and_then(Value::as_str) == Some("user");
        if !is_user_item {
            continue;
        }

        match items[idx].get_mut("content") {
            Some(Value::String(s)) => {
                s.reserve(2 + context_text.len());
                s.push_str("\n\n");
                s.push_str(context_text);
                return appended_len(context_text);
            }
            Some(Value::Array(parts)) => {
                if parts.is_empty() {
                    return 0;
                }
                for part in parts.iter_mut() {
                    let is_text = part.is_object()
                        && part
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|t| text_types.contains(&t));
                    if is_text {
                        match part.get_mut("text") {
                            Some(Value::String(t)) => {
                                t.reserve(2 + context_text.len());
                                t.push_str("\n\n");
                                t.push_str(context_text);
                            }
                            // Missing/non-string `text`: same bytes as the
                            // old `format!("{existing}\n\n{ctx}")` with
                            // `existing == ""`.
                            _ => {
                                part["text"] = Value::String(format!("\n\n{context_text}"));
                            }
                        }
                        return appended_len(context_text);
                    }
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

    // ── decode_body decompression cap (zip-bomb guard) ───────────

    /// Build a highly-compressible payload without holding the full
    /// expansion in memory at once: stream 1 MiB zero chunks into the
    /// compressor until `target_len` bytes are represented.
    fn bomb_fixture(target_len: usize, encoding: &str) -> Vec<u8> {
        use std::io::Write;
        let chunk = vec![0u8; 1024 * 1024];
        let rounds = target_len / chunk.len();
        match encoding {
            "gzip" => {
                let mut enc =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                for _ in 0..rounds {
                    enc.write_all(&chunk).unwrap();
                }
                enc.finish().unwrap()
            }
            "zstd" => {
                let mut enc = zstd::stream::Encoder::new(Vec::new(), 0).unwrap();
                for _ in 0..rounds {
                    enc.write_all(&chunk).unwrap();
                }
                enc.finish().unwrap()
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_decode_bomb_is_refused_at_cap() {
        for encoding in ["gzip", "zstd"] {
            let wire = bomb_fixture(
                (MAX_DECOMPRESSED_BODY_SIZE as usize) + 1024 * 1024,
                encoding,
            );
            // The bomb must actually exercise the cap: wire stays tiny
            // while the expansion passes it.
            assert!(
                (wire.len() as u64) < MAX_DECOMPRESSED_BODY_SIZE,
                "{encoding} bomb wire size {} should be far under the cap",
                wire.len()
            );
            let err = decode_body(&wire, Some(encoding)).unwrap_err();
            assert!(
                err.contains("exceeds"),
                "{encoding} bomb should be refused at the cap, got: {err}"
            );
        }
    }

    #[test]
    fn test_decode_truncated_gzip_still_errors() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"truncated payload").unwrap();
        let comp = enc.finish().unwrap();
        let cut = comp.len() / 2;
        let err = decode_body(&comp[..cut], Some("gzip")).unwrap_err();
        assert!(
            err.contains("Failed to decompress"),
            "truncated stream must error, not yield a short body, got: {err}"
        );
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

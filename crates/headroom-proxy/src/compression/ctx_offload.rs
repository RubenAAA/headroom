//! CTX-3 — tool_result offload transform.
//!
//! Replaces oversized `tool_result` blocks in an Anthropic `/v1/messages`
//! body with a deterministic **structural digest** and stashes the original
//! bytes in the CCR store for later retrieval. Applied in the request path
//! **before** the live-zone compressors ([`crate::compression::compress_anthropic_request`]).
//!
//! # Why this is cache-safe (invariants I1–I6, `docs/ctx-mode-in-headroom-plan.md`)
//!
//! - **I1 (pure function).** The replacement is `digest(bytes)`: the
//!   content-type detector + compressor stack ([`compress_block_for_offload`],
//!   all pure) plus a **fixed** footer `<<ctx:HASH>> (N bytes offloaded; …)`
//!   where `HASH = blake3(bytes)[:24]` ([`compute_key`]) and `N = bytes.len()`.
//!   No timestamps, counters, RNG, or session state. Same input bytes → same
//!   output bytes on every call, across process restarts.
//! - **I2 (stable re-application).** Because the digest is recomputable from
//!   the block's own bytes, a block offloaded in turn N and resent raw by the
//!   client in turn N+1 is replaced with the *identical* digest again — no
//!   store lookup on the request path. That is why this transform is exempt
//!   from the frozen-count floor and may rewrite blocks in **all** messages.
//! - **I3 (append-only).** Thresholds are static config; a block only ever
//!   converts raw→digest, never back.
//! - **I6 (tokenizer gate).** If the digest is not smaller in tokens than the
//!   original, the original is kept. This decision is itself a pure function
//!   of the bytes (deterministic tokenizer over deterministic strings).
//!
//! ## Determinism audit (where it could leak, and what we did)
//!
//! - **Object key order / number formatting.** The body round-trips through
//!   `serde_json` with the `preserve_order` + `arbitrary_precision` features
//!   (see workspace `Cargo.toml`): object keys keep input order and numbers
//!   re-serialize byte-for-byte. Re-serialization is therefore deterministic
//!   turn-to-turn (the acceptance harness in `tests/ctx_cache_stability.rs`
//!   asserts prefix-stability directly).
//! - **HashMap iteration.** None: the walk is an ordered traversal of the
//!   `messages`/`content` arrays; nothing here iterates a hash map.
//! - **Float formatting.** The footer contains only integers; body floats are
//!   handled by `arbitrary_precision` above.
//! - **Compressor nondeterminism.** [`compress_block_for_offload`] reuses the
//!   live-zone compressors, all pure. When no compressor rewrites the block
//!   (e.g. `PlainText` without the kompress model on disk), the digest falls
//!   back to a deterministic preview cut ([`preview`]) — the context-mode
//!   behaviour — so plaintext tool output still offloads. The one
//!   environment-scoped caveat: whether the kompress arm or the preview arm
//!   runs depends on the HF model being in the on-disk cache, which is stable
//!   per machine. Documented on [`compress_block_for_offload`].
//!
//! # Composition with `context_editing`
//!
//! `maybe_inject_context_management` (`context_editing.rs`) injects Anthropic
//! server-side `context_management` (`clear_tool_uses`) directives into the
//! top-level request object. It never touches message `content`, so it
//! composes with this transform: after offload, `clear_tool_uses` simply fires
//! on already-small digests, which is harmless. Both-on is the default posture.

use headroom_core::ccr::compute_key;
use headroom_core::tokenizer::get_tokenizer;
use headroom_core::transforms::{compress_block_for_offload, DEFAULT_MODEL};
use serde_json::Value;

/// Static per-request offload settings (I3: never changes mid-session).
#[derive(Debug, Clone, Copy)]
pub struct CtxOffloadConfig {
    /// A block's serialized content must exceed this many bytes to qualify.
    pub min_bytes: usize,
}

/// One offloaded block, handed to the background worker for storage. Never
/// touched on the request path beyond construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffloadRecord {
    /// `blake3(original)[:24]` — the retrieval key embedded in the digest.
    pub hash: String,
    /// The original block content bytes (what the client sent).
    pub original: String,
    /// Deterministic chunk title for the FTS index: the paired `tool_use`'s
    /// command/tool name. Empty when no pairing was found.
    pub title: String,
}

/// Result of running the transform over one request body.
#[derive(Debug, Default)]
pub struct OffloadOutcome {
    /// Number of blocks replaced with a digest.
    pub blocks_offloaded: usize,
    /// Originals to persist off the request path (may be empty).
    pub records: Vec<OffloadRecord>,
}

impl OffloadOutcome {
    /// Whether any block was rewritten (i.e. the body bytes changed).
    pub fn changed(&self) -> bool {
        self.blocks_offloaded > 0
    }
}

/// Fixed marker prefix. A block whose text already contains this is a digest
/// (idempotency fast path).
const MARKER_PREFIX: &str = "<<ctx:";

/// Preview budget for the no-compressor fallback, in bytes. Matches
/// context-mode's `FETCH_PREVIEW_LIMIT` (3072): enough to orient the model,
/// small enough that offload always shrinks a qualifying (>50KB) block.
const PREVIEW_BYTES: usize = 3072;

/// Longest prefix of `text` whose UTF-8 encoding is at most `max_bytes`,
/// cut on a char boundary, with a truncation notice. Pure function of
/// `text` (I1) — the context-mode preview cut, ported.
fn preview(text: &str, max_bytes: usize) -> String {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n…[truncated — retrieval pointer below]",
        &text[..end]
    )
}

/// Build the deterministic footer appended to a digest. Pure function of
/// `hash` and `orig_len` — no volatile fields (I1).
fn footer(hash: &str, orig_len: usize) -> String {
    format!(
        "\n{MARKER_PREFIX}{hash}>> ({orig_len} bytes offloaded; \
         retrieve: headroom ctx get {hash})"
    )
}

/// Walk every message's `tool_result` blocks and replace qualifying ones with
/// a digest. Mutates `parsed` in place; returns what changed + records to
/// persist. Pure function of `parsed` + `config` (I1/I2).
pub fn offload_anthropic_request(parsed: &mut Value, config: &CtxOffloadConfig) -> OffloadOutcome {
    let mut outcome = OffloadOutcome::default();

    // First pass (immutable borrow): map tool_use_id → command/tool title so
    // the FTS chunk title is deterministic. Built from a clone so the second
    // pass can take a mutable borrow without aliasing.
    let tool_titles = collect_tool_titles(parsed);

    let Some(messages) = parsed.get_mut("messages").and_then(Value::as_array_mut) else {
        return outcome;
    };

    for message in messages.iter_mut() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let title = tool_titles.get(&tool_use_id).cloned().unwrap_or_default();
            if let Some(record) = offload_tool_result(block, config, &title) {
                outcome.blocks_offloaded += 1;
                outcome.records.push(record);
            }
        }
    }

    outcome
}

/// Extract, digest, and replace one `tool_result` block's content. Returns the
/// record to persist, or `None` if the block did not qualify / did not shrink /
/// was already a digest.
fn offload_tool_result(
    block: &mut Value,
    config: &CtxOffloadConfig,
    title: &str,
) -> Option<OffloadRecord> {
    let content = block.get("content")?;
    let original = tool_result_text(content)?;

    // Idempotency fast path (I2): a block that already carries our marker is a
    // digest — pass through untouched. (Re-processing the raw block would yield
    // identical bytes anyway; this just avoids re-hashing.)
    if original.contains(MARKER_PREFIX) {
        return None;
    }

    // Qualify on serialized byte length (I3: static threshold).
    if original.len() <= config.min_bytes {
        return None;
    }

    let hash = compute_key(original.as_bytes());
    // Structural compressor when one applies; otherwise a plain preview cut,
    // mirroring context-mode's behaviour (charSafePrefix + pointer). The
    // original is always retrievable by hash, so the digest only needs to
    // orient the model, not preserve information. Both arms are pure
    // functions of the block bytes (I1/I2).
    let (strategy, compressed) = compress_block_for_offload(&original);
    let body = if strategy.is_some() {
        compressed
    } else {
        preview(&original, PREVIEW_BYTES)
    };
    let digest = format!("{body}{}", footer(&hash, original.len()));

    // Tokenizer gate (I6): keep the original unless the digest is strictly
    // smaller in tokens. Deterministic — pure function of the two strings.
    let tokenizer = get_tokenizer(DEFAULT_MODEL);
    if tokenizer.count_text(&digest) >= tokenizer.count_text(&original) {
        return None;
    }

    // Replace content, preserving the client's shape: string stays string,
    // array becomes a single text block. Both are deterministic.
    match block.get("content") {
        Some(Value::String(_)) => {
            block["content"] = Value::String(digest);
        }
        _ => {
            block["content"] = Value::Array(vec![serde_json::json!({
                "type": "text",
                "text": digest,
            })]);
        }
    }

    Some(OffloadRecord {
        hash,
        original,
        title: title.to_string(),
    })
}

/// Concatenated text of a `tool_result` `content` field. `content` is either a
/// string or an array of blocks; only `text` blocks contribute (matching how
/// the client renders tool output). `None` when there is no text at all.
fn tool_result_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let mut out = String::new();
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

/// Map every `tool_use` block's id → a deterministic title (the Bash command
/// string when present, else the tool name). Used as the FTS chunk title so it
/// is byte-derived from the same request.
fn collect_tool_titles(parsed: &Value) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(messages) = parsed.get("messages").and_then(Value::as_array) else {
        return map;
    };
    for message in messages {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
            let title = block
                .get("input")
                .and_then(|i| i.get("command"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| name.to_string());
            map.insert(id.to_string(), title);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(min: usize) -> CtxOffloadConfig {
        CtxOffloadConfig { min_bytes: min }
    }

    /// A tool_result body whose content is `body`, paired to a bash tool_use.
    fn req(body: &str) -> Value {
        json!({
            "model": "claude-3-5-sonnet-20241022",
            "messages": [
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"tu_1","name":"Bash",
                     "input":{"command":"cat big.log"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"tu_1","content": body}
                ]}
            ]
        })
    }

    fn first_tool_result_text(parsed: &Value) -> String {
        let content = &parsed["messages"][1]["content"][0]["content"];
        tool_result_text(content).unwrap_or_default()
    }

    #[test]
    fn offloads_oversized_block_with_marker() {
        // Repetitive log content compresses well and is comfortably > 200 B.
        let body = "ERROR: disk full\n".repeat(50);
        let mut parsed = req(&body);
        let out = offload_anthropic_request(&mut parsed, &cfg(200));
        assert_eq!(out.blocks_offloaded, 1);
        assert_eq!(out.records.len(), 1);
        let text = first_tool_result_text(&parsed);
        assert!(text.contains("<<ctx:"));
        assert!(text.contains("bytes offloaded"));
        // Title is the paired bash command (deterministic).
        assert_eq!(out.records[0].title, "cat big.log");
        assert_eq!(out.records[0].original, body);
        assert_eq!(out.records[0].hash.len(), 24);
    }

    #[test]
    fn plaintext_read_output_offloads_via_preview_fallback() {
        // Simulates a Claude Code `Read` result: line-number prefixes make the
        // content classify as PlainText, where no structural compressor
        // applies (kompress is off by default). The preview fallback must
        // still offload it — this was silently a no-op before.
        let mut body = String::new();
        for i in 1..=1900 {
            body.push_str(&format!(
                "{i}\t[[package]]\nname = \"crate-{i}\"\nchecksum = \"abc{i}\"\n"
            ));
        }
        assert!(body.len() > 50_000);
        let mut parsed = req(&body);
        let out = offload_anthropic_request(&mut parsed, &cfg(50_000));
        assert_eq!(out.blocks_offloaded, 1);
        let text = first_tool_result_text(&parsed);
        assert!(text.len() < 4096, "digest must be small, got {}", text.len());
        assert!(text.starts_with("1\t[[package]]"), "preview keeps the head");
        assert!(text.contains("truncated"));
        assert!(text.contains(&format!("retrieve: headroom ctx get {}", out.records[0].hash)));
        assert_eq!(out.records[0].original, body);
    }

    #[test]
    fn preview_cuts_on_char_boundary() {
        // A multi-byte char straddling the budget must not split.
        let body = format!("{}é{}", "x".repeat(3071), "y".repeat(60_000));
        let cut = preview(&body, PREVIEW_BYTES);
        assert!(cut.starts_with(&"x".repeat(3071)));
        assert!(!cut.contains('\u{FFFD}'));
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    #[test]
    fn below_threshold_is_untouched() {
        let body = "small output";
        let mut parsed = req(body);
        let out = offload_anthropic_request(&mut parsed, &cfg(50_000));
        assert_eq!(out.blocks_offloaded, 0);
        assert_eq!(first_tool_result_text(&parsed), body);
    }

    #[test]
    fn digest_is_deterministic_same_bytes_twice() {
        let body = "ERROR: disk full\n".repeat(50);
        let mut a = req(&body);
        let mut b = req(&body);
        offload_anthropic_request(&mut a, &cfg(200));
        offload_anthropic_request(&mut b, &cfg(200));
        assert_eq!(a, b, "same input bytes must produce identical output bytes");
    }

    #[test]
    fn already_offloaded_block_passes_through() {
        let body = "ERROR: disk full\n".repeat(50);
        let mut parsed = req(&body);
        offload_anthropic_request(&mut parsed, &cfg(200));
        let after_first = parsed.clone();
        // Re-running must be a no-op (idempotent): the digest already carries
        // the marker.
        let out = offload_anthropic_request(&mut parsed, &cfg(200));
        assert_eq!(out.blocks_offloaded, 0);
        assert_eq!(parsed, after_first);
    }

    #[test]
    fn marker_format_is_pinned() {
        let f = footer("abc123", 1234);
        assert_eq!(f, "\n<<ctx:abc123>> (1234 bytes offloaded; retrieve: headroom ctx get abc123)");
    }

    #[test]
    fn array_content_shape_preserved_as_text_block() {
        let body = "ERROR: disk full\n".repeat(50);
        let mut parsed = json!({
            "model": "claude-3-5-sonnet-20241022",
            "messages": [
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"x"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"tu_1",
                     "content":[{"type":"text","text": body}]}
                ]}
            ]
        });
        let out = offload_anthropic_request(&mut parsed, &cfg(200));
        assert_eq!(out.blocks_offloaded, 1);
        let content = &parsed["messages"][1]["content"][0]["content"];
        assert!(content.is_array());
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"].as_str().unwrap().contains("<<ctx:"));
    }
}

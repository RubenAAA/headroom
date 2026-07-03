//! Conversation identity (CTX-2a).
//!
//! A *client* is identified by `derive_session_key` (auth-header / IP). A
//! *conversation* is finer-grained: one Claude Code client runs many
//! conversations (main + subagents) concurrently. [`conversation_key`] is the
//! stable per-conversation fingerprint — SHA-256 over the session key plus the
//! two request fields constant across every turn of one conversation and
//! distinct across concurrent ones: `system` and the first message.
//!
//! This is the single implementation of the conversation key; the CTX-7
//! re-cache watchdog (`cache_stabilization::usage_observer`) delegates here so
//! the two features can never drift.
//!
//! [`classify`] turns the rolling **prefix-chain** (persisted by
//! `headroom_core::ctx::SessionsStore`) into a per-request classification:
//! new / continuation / compaction-or-resume / branch. It is a **pure
//! function** of the previous recorded turn and the current request body, so
//! it is unit-testable without a DB — the background worker does the DB lookup
//! and passes the previous turn in.

use headroom_core::ctx::PrefixTurn;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Stable per-conversation key: SHA-256 over the auth-derived session key plus
/// `system` and the first message. 16-hex-char prefix, opaque, safe to log.
///
/// Byte-identical to the original `usage_observer::conversation_key` (moved
/// here so there is one implementation).
pub fn conversation_key(parsed: &Value, session_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_key.as_bytes());
    if let Some(system) = parsed.get("system") {
        hasher.update(system.to_string().as_bytes());
    }
    if let Some(first) = parsed.get("messages").and_then(|m| m.get(0)) {
        hasher.update(first.to_string().as_bytes());
    }
    hex16(&hasher.finalize())
}

/// Number of messages in the request body (`messages.len()`), 0 if absent.
pub fn message_count(parsed: &Value) -> u64 {
    parsed
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.len() as u64)
        .unwrap_or(0)
}

/// Rolling hash of the first `prefix_len` messages — the stable prefix that
/// should carry over unchanged between consecutive turns of one conversation.
/// SHA-256 over each message's canonical JSON, length-prefixed so two adjacent
/// messages can't be confused with one concatenated message.
pub fn prefix_hash(parsed: &Value, prefix_len: u64) -> String {
    let mut hasher = Sha256::new();
    if let Some(msgs) = parsed.get("messages").and_then(Value::as_array) {
        for msg in msgs.iter().take(prefix_len as usize) {
            let s = msg.to_string();
            hasher.update((s.len() as u64).to_le_bytes());
            hasher.update(s.as_bytes());
        }
    }
    hex16(&hasher.finalize())
}

/// Text of the first `role == "user"` message. `content` may be a string or an
/// array of blocks; text blocks are concatenated.
pub fn first_user_message_text(parsed: &Value) -> Option<String> {
    let msgs = parsed.get("messages").and_then(Value::as_array)?;
    let msg = msgs
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))?;
    Some(message_text(msg))
}

/// Extract the concatenated text of a single message's `content`.
fn message_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
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
            out
        }
        _ => String::new(),
    }
}

/// Markers that betray a compaction/resume preamble as the first user message.
/// Claude Code injects a summary block on `/compact` and on resume; these are
/// the stable lead-ins. Case-insensitive substring match.
const COMPACTION_MARKERS: &[&str] = &[
    "this session is being continued from a previous conversation",
    "your task is to create a detailed summary of the conversation",
    "the conversation is summarized below",
    "<summary>",
];

/// Whether `text` looks like a compaction/resume summary preamble.
pub fn has_compaction_marker(text: &str) -> bool {
    let lower = text.to_lowercase();
    COMPACTION_MARKERS.iter().any(|m| lower.contains(m))
}

/// Classification of one request relative to the conversation's known chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// No prior turn recorded for this `conv_id`.
    New,
    /// Extends the known chain: the recorded prefix still matches.
    Continuation,
    /// A compaction/resume: shorter/reset prefix, or a summary preamble.
    CompactionOrResume,
    /// Diverged from the known chain at a shared turn (a branch).
    Branch,
}

/// The index of the first *new* message to extract from, given a
/// classification and the previous turn. Continuations extract only the tail
/// (messages the model hadn't seen yet); every other class re-scans from 0.
pub fn extract_from_index(class: Classification, prev: Option<&PrefixTurn>) -> usize {
    match class {
        Classification::Continuation => prev.map(|p| p.turn_n as usize).unwrap_or(0),
        _ => 0,
    }
}

/// Classify the current request against the previous recorded turn.
///
/// NOTE (CTX-2b): [`conversation_key`] keys on the first message, so a
/// compaction that *replaces* history with a summary produces a **new**
/// `conv_id` and lands as [`Classification::New`] rather than
/// `CompactionOrResume` — proper resume-linking needs a `session_key → conv`
/// map, deferred to CTX-2b. Within a single `conv_id`, this still detects
/// resume/branch correctly.
pub fn classify(prev: Option<&PrefixTurn>, parsed: &Value) -> Classification {
    let turn_n = message_count(parsed);
    let has_marker = first_user_message_text(parsed)
        .map(|t| has_compaction_marker(&t))
        .unwrap_or(false);

    match prev {
        None => Classification::New,
        Some(p) => {
            if turn_n >= p.turn_n {
                // The current request's first `p.turn_n` messages should equal
                // the whole message list recorded at that turn if this is a
                // clean continuation.
                if prefix_hash(parsed, p.turn_n) == p.prefix_hash {
                    Classification::Continuation
                } else if has_marker {
                    Classification::CompactionOrResume
                } else {
                    Classification::Branch
                }
            } else {
                // Shorter prefix than last seen — a reset. A summary preamble
                // makes it a resume; otherwise treat the divergence as a branch.
                if has_marker {
                    Classification::CompactionOrResume
                } else {
                    Classification::Branch
                }
            }
        }
    }
}

/// First 16 hex chars (8 bytes) of a SHA-256 digest.
fn hex16(digest: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(16);
    for b in &digest[..8] {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(system: &str, messages: Value) -> Value {
        json!({ "system": system, "messages": messages })
    }

    #[test]
    fn conversation_key_stable_and_distinct() {
        let a = req("sys", json!([{"role":"user","content":"hello"}]));
        // Same system + first message → same key regardless of later messages.
        let a2 = req(
            "sys",
            json!([{"role":"user","content":"hello"},{"role":"assistant","content":"hi"}]),
        );
        assert_eq!(conversation_key(&a, "sk"), conversation_key(&a2, "sk"));
        // Different first message → different key.
        let b = req("sys", json!([{"role":"user","content":"different"}]));
        assert_ne!(conversation_key(&a, "sk"), conversation_key(&b, "sk"));
        // Different session key → different key.
        assert_ne!(conversation_key(&a, "sk"), conversation_key(&a, "other"));
        assert_eq!(conversation_key(&a, "sk").len(), 16);
    }

    #[test]
    fn classify_new_when_no_prev() {
        let r = req("s", json!([{"role":"user","content":"hi"}]));
        assert_eq!(classify(None, &r), Classification::New);
    }

    #[test]
    fn classify_continuation_extends_chain() {
        // Turn 1: two messages.
        let t1 = req(
            "s",
            json!([{"role":"user","content":"hi"},{"role":"assistant","content":"yo"}]),
        );
        let prev = PrefixTurn {
            turn_n: 2,
            prefix_hash: prefix_hash(&t1, 2),
        };
        // Turn 2: same first two messages + a new user turn.
        let t2 = req(
            "s",
            json!([
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"yo"},
                {"role":"user","content":"next"}
            ]),
        );
        assert_eq!(classify(Some(&prev), &t2), Classification::Continuation);
        assert_eq!(extract_from_index(Classification::Continuation, Some(&prev)), 2);
    }

    #[test]
    fn classify_branch_when_prefix_diverges() {
        let t1 = req(
            "s",
            json!([{"role":"user","content":"hi"},{"role":"assistant","content":"yo"}]),
        );
        let prev = PrefixTurn {
            turn_n: 2,
            prefix_hash: prefix_hash(&t1, 2),
        };
        // Turn 2 rewrote the assistant reply at index 1 → prefix hash differs.
        let branched = req(
            "s",
            json!([
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"DIFFERENT"},
                {"role":"user","content":"next"}
            ]),
        );
        assert_eq!(classify(Some(&prev), &branched), Classification::Branch);
    }

    #[test]
    fn classify_resume_on_marker_and_shorter_prefix() {
        let prev = PrefixTurn {
            turn_n: 8,
            prefix_hash: "whatever".to_string(),
        };
        // Shorter message list whose first user message is a compaction summary.
        let resumed = req(
            "s",
            json!([{
                "role":"user",
                "content":"This session is being continued from a previous conversation. Summary: ..."
            }]),
        );
        assert_eq!(
            classify(Some(&prev), &resumed),
            Classification::CompactionOrResume
        );
    }

    #[test]
    fn first_user_text_handles_string_and_blocks() {
        let s = req("s", json!([{"role":"user","content":"plain"}]));
        assert_eq!(first_user_message_text(&s).as_deref(), Some("plain"));
        let blocks = req(
            "s",
            json!([{"role":"user","content":[
                {"type":"text","text":"a"},
                {"type":"image"},
                {"type":"text","text":"b"}
            ]}]),
        );
        assert_eq!(first_user_message_text(&blocks).as_deref(), Some("a\nb"));
    }
}

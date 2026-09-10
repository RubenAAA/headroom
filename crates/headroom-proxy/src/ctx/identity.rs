//! Conversation identity (CTX-2a).
//!
//! A *client* is identified by `derive_session_key` (auth-header / IP). A
//! *conversation* is finer-grained: one Claude Code client runs many
//! conversations (main + subagents) concurrently. [`conversation_key`] is the
//! stable per-conversation fingerprint — SHA-256 over the session key plus the
//! two request fields meant to hold still across every turn of one conversation
//! and to differ across concurrent ones: `system` and the first message. Parts
//! of `system` do not hold still, and [`conversation_key`] says which and why.
//!
//! The CTX-7 re-cache watchdog (`cache_stabilization::usage_observer`) keeps its
//! own key, deliberately excluding `system` so it can still recognise a
//! conversation whose system prompt was mutated. The two are not interchangeable.
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

/// Stable per-conversation key: SHA-256 over the auth-derived session key, the
/// text of `system`, and the first message. 16-hex-char prefix, opaque, safe to
/// log.
///
/// # Why the system block is hashed as text, minus two lines
///
/// This key decides whether the recall injection is recomputed. The injected
/// text lands in `messages[0]`, at the very front of the cached prefix, so a new
/// key mid-conversation rewrites byte 0 and re-creates the whole conversation.
/// Measured over 3,796 ledger-joined turns of captured traffic on 2026-08-17,
/// the key changed mid-conversation on 30 turns and those turns billed 1,639,073
/// tokens of cache creation — around 150,000 each, against a 2,000 baseline for
/// turns whose prefix broke at the append point instead.
///
/// Everything that churned was one of three things:
///
/// - `cc_version`, the client's own version string (24 changed lines)
/// - the live working directory, which moves on a `cd` (4 changed lines)
/// - `cache_control` markers moving between blocks, which is not text at all
///   (dropped from the first message too — see the note at the hash site)
///
/// Hashing the text only and dropping those two lines takes the 30 changes to 1
/// and the cost to 34,270 tokens, while leaving 121 distinct keys over 120
/// distinct conversations — so no two conversations are merged into one recall
/// decision. Dropping `system` from the key entirely scores the same, and is
/// rejected: the block still separates conversations that differ only by project
/// instructions, and nothing here needs it to be blind to that.
///
/// The proxy already holds both churning lines steady on the wire (the
/// `cc_version` pin and `--hold-working-directory`), but this key is derived
/// from the body as it arrives, so neither pin protects it.
///
/// Changing the derivation orphans rows stored under the old key. That costs one
/// re-decided injection per live conversation, which is the same worst case as a
/// restart and is what `ctx::inject` already documents itself as tolerating.
pub fn conversation_key(parsed: &Value, session_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_key.as_bytes());
    match parsed.get("system") {
        Some(Value::String(text)) => hash_stable_lines(&mut hasher, text),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    hash_stable_lines(&mut hasher, text);
                }
            }
        }
        _ => {}
    }
    if let Some(first) = parsed.get("messages").and_then(|m| m.get(0)) {
        // Third churning input, and the one the list above named but the code
        // did not act on: the client's cache breakpoint sits on the last
        // message, so on turn 1 it sits on `messages[0]` and on turn 2 it has
        // moved off. Hashing the raw first message therefore split a third of
        // all conversations in two at their second turn — 75 keys over the
        // 2,648 requests of the `headroom-capture-netvalue` capture, against 48
        // once the marker is dropped. Each split orphaned the events captured
        // so far under a dead key and re-extracted the history under the new
        // one, which the `(session_id, type, data_hash)` dedup cannot catch
        // because the session id is exactly what changed.
        let mut buf = String::new();
        write_without_cache_control(first, &mut buf);
        hasher.update(buf.as_bytes());
    }
    hex16(&hasher.finalize())
}

/// The two lines of the system preamble that change while the conversation does
/// not. See [`conversation_key`] for what they cost.
fn line_churns(line: &str) -> bool {
    // Matched without the leading `w` so it covers both the client's "Working
    // directory:" and the "working directory:" of the environment preamble.
    line.contains("cc_version") || line.contains("orking directory:")
}

/// Feed one system text into the hasher a line at a time, skipping the churning
/// ones. Lines are length-prefixed so two of them cannot be confused with one
/// joined line.
fn hash_stable_lines(hasher: &mut Sha256, text: &str) {
    for line in text.lines() {
        if line_churns(line) {
            continue;
        }
        hasher.update((line.len() as u64).to_le_bytes());
        hasher.update(line.as_bytes());
    }
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
///
/// # Why `cache_control` is stripped first
///
/// The client moves its cache breakpoint to the new tail on every turn, which
/// means the message that carried `cache_control` at turn N no longer carries
/// it at turn N+1 — inside the very prefix this hash is supposed to find
/// unchanged. Hashing the raw JSON therefore made [`classify`] answer
/// [`Classification::Branch`] on every single turn, `extract_from_index`
/// return 0, and the extractor re-run over the whole conversation. Measured
/// over the 134-turn conversation in the `headroom-capture-netvalue` capture:
///
/// | prefix hashed over | continuations | branches |
/// |---|---:|---:|
/// | raw message JSON | 0 | 133 |
/// | `cache_control` stripped | 130 | 3 |
///
/// The cost landed in the sessions DB, where one project's file reached 9.7 GB
/// holding 3,907,865 event rows of which 47,557 were distinct.
///
/// A breakpoint is a caching hint, never conversation content, so removing it
/// cannot hide a real edit to the prefix.
pub fn prefix_hash(parsed: &Value, prefix_len: u64) -> String {
    let mut hasher = Sha256::new();
    if let Some(msgs) = parsed.get("messages").and_then(Value::as_array) {
        let mut buf = String::new();
        for msg in msgs.iter().take(prefix_len as usize) {
            buf.clear();
            write_without_cache_control(msg, &mut buf);
            hasher.update((buf.len() as u64).to_le_bytes());
            hasher.update(buf.as_bytes());
        }
    }
    hex16(&hasher.finalize())
}

/// Append `v` to `out` as JSON, dropping every `cache_control` key at any
/// depth. Writes straight into the buffer rather than cloning and mutating the
/// message, which on a long conversation is the difference between one small
/// string and a copy of the whole history per turn.
fn write_without_cache_control(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            out.push('{');
            let mut first = true;
            for (k, val) in map {
                if k == "cache_control" {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                // Serialize the borrowed key directly: the old
                // `Value::String(k.clone()).to_string()` cloned the key
                // plus a wrapper `Value` per object key for identical
                // output bytes.
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write_without_cache_control(val, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_without_cache_control(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
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

    /// A system block as the client sends it: an array of text blocks, the last
    /// one carrying a cache marker.
    fn blocks(preamble: &str, marked: bool) -> Value {
        let mut tail = json!({"type": "text", "text": "project instructions"});
        if marked {
            tail["cache_control"] = json!({"type": "ephemeral"});
        }
        json!([{"type": "text", "text": preamble}, tail])
    }

    fn with_system(system: Value) -> Value {
        json!({ "system": system, "messages": [{"role":"user","content":"hello"}] })
    }

    #[test]
    fn a_new_client_version_keeps_the_conversation() {
        let before = with_system(blocks("cc_version: 2.1.7\nplatform: linux", false));
        let after = with_system(blocks("cc_version: 2.1.8\nplatform: linux", false));
        assert_eq!(
            conversation_key(&before, "sk"),
            conversation_key(&after, "sk")
        );
    }

    #[test]
    fn a_changed_working_directory_keeps_the_conversation() {
        let before = with_system(blocks("Working directory: /home/a\nplatform: linux", false));
        let after = with_system(blocks("Working directory: /home/b\nplatform: linux", false));
        assert_eq!(
            conversation_key(&before, "sk"),
            conversation_key(&after, "sk")
        );
    }

    #[test]
    fn a_moved_cache_marker_keeps_the_conversation() {
        let before = with_system(blocks("platform: linux", false));
        let after = with_system(blocks("platform: linux", true));
        assert_eq!(
            conversation_key(&before, "sk"),
            conversation_key(&after, "sk")
        );
    }

    #[test]
    fn different_project_instructions_are_different_conversations() {
        let a = with_system(blocks("platform: linux", false));
        let mut b = with_system(blocks("platform: linux", false));
        b["system"][1]["text"] = json!("other instructions");
        assert_ne!(conversation_key(&a, "sk"), conversation_key(&b, "sk"));
    }

    /// The churn filter must not be so broad that two system prompts differing
    /// only outside those lines collapse together.
    #[test]
    fn lines_cannot_be_confused_with_one_joined_line() {
        let a = with_system(json!("alpha\nbeta"));
        let b = with_system(json!("alphabeta"));
        assert_ne!(conversation_key(&a, "sk"), conversation_key(&b, "sk"));
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
        assert_eq!(
            extract_from_index(Classification::Continuation, Some(&prev)),
            2
        );
    }

    /// Turn 1 puts the cache breakpoint on `messages[0]`, because it is also
    /// the last message. Turn 2 moves it off. If that counted, the conversation
    /// would get a second identity at its second turn and lose everything
    /// captured under the first.
    #[test]
    fn conversation_key_survives_the_breakpoint_leaving_the_first_message() {
        let turn1 = req(
            "sys",
            json!([{"role":"user","content":[
                {"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}
            ]}]),
        );
        let turn2 = req(
            "sys",
            json!([
                {"role":"user","content":[{"type":"text","text":"hello"}]},
                {"role":"assistant","content":[
                    {"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}
                ]}
            ]),
        );
        assert_eq!(
            conversation_key(&turn1, "sk"),
            conversation_key(&turn2, "sk")
        );
    }

    /// The client moves its cache breakpoint to the new tail every turn, so
    /// the message that carried `cache_control` at turn N has lost it by turn
    /// N+1. Before the hash learned to ignore it, this made every turn of every
    /// conversation a branch, `extract_from_index` returned 0, and the whole
    /// history was re-extracted and re-inserted once per request.
    #[test]
    fn moving_the_cache_breakpoint_is_still_a_continuation() {
        let t1 = req(
            "s",
            json!([
                {"role":"user","content":"hi"},
                {"role":"assistant","content":[
                    {"type":"text","text":"yo","cache_control":{"type":"ephemeral"}}
                ]}
            ]),
        );
        let prev = PrefixTurn {
            turn_n: 2,
            prefix_hash: prefix_hash(&t1, 2),
        };
        // Turn 2: identical first two messages except the breakpoint has moved
        // off the assistant turn and onto the new tail.
        let t2 = req(
            "s",
            json!([
                {"role":"user","content":"hi"},
                {"role":"assistant","content":[{"type":"text","text":"yo"}]},
                {"role":"user","content":[
                    {"type":"text","text":"next","cache_control":{"type":"ephemeral"}}
                ]}
            ]),
        );
        assert_eq!(classify(Some(&prev), &t2), Classification::Continuation);
        assert_eq!(
            extract_from_index(Classification::Continuation, Some(&prev)),
            2,
            "capture resumes after the messages already recorded"
        );
    }

    /// Ignoring `cache_control` must not extend to ignoring content. A real
    /// edit inside the prefix still has to read as a branch.
    #[test]
    fn stripping_cache_control_does_not_hide_a_real_prefix_edit() {
        let a = req(
            "s",
            json!([{"role":"user","content":[
                {"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}
            ]}]),
        );
        let b = req(
            "s",
            json!([{"role":"user","content":[{"type":"text","text":"hello"}]}]),
        );
        assert_ne!(prefix_hash(&a, 1), prefix_hash(&b, 1));
    }

    /// `cache_control` is dropped wherever it sits, not only on content
    /// blocks — Claude Code also marks whole messages.
    #[test]
    fn cache_control_is_dropped_at_every_depth() {
        let bare = req("s", json!([{"role":"user","content":"hi"}]));
        let marked = req(
            "s",
            json!([{"role":"user","content":"hi","cache_control":{"type":"ephemeral"}}]),
        );
        assert_eq!(prefix_hash(&bare, 1), prefix_hash(&marked, 1));
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

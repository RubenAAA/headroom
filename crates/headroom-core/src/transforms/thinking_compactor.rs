//! Convert prior-turn extended-thinking blocks into Kompressed `text` blocks.
//!
//! Faithful Rust port of `headroom/transforms/thinking_compactor.py`.
//!
//! On Claude 4.6+ models, prior-turn thinking is re-sent as input and **billed**
//! (verified live: opus-4-6 +995 tok/block, sonnet-4-6 +688; pre-4.6 models
//! strip it server-side, so this transform is a no-op there — gate on model
//! generation at the call site with [`bills_prior_thinking`]). Two findings
//! dictate the mechanism:
//!
//! 1. **Editing a thinking block in place is futile** — Anthropic pins the
//!    original via the block `signature` and re-expands it server-side,
//!    ignoring whatever text you send (verified: 835==835). The only ways to
//!    actually shrink thinking are to *drop* the block or *convert it to a
//!    plain `text` block* (no signature → the shorter text is billed as-is;
//!    verified 716<835). This transform does the latter, running each block's
//!    text through Kompress.
//!
//! 2. **Cache safety comes from determinism, not from "only touch the delta".**
//!    The client re-sends the *original* thinking every turn, but the prompt
//!    cache holds the *compacted* form we forwarded last turn. So we map
//!    original→compacted deterministically (memoized by content hash) every
//!    turn — the forwarded prefix is then byte-stable and the cache still hits.
//!    The last `keep_last_turns` assistant turns keep their thinking intact (the
//!    active reasoning the model needs); a turn aging out of that window is the
//!    only byte change, a bounded recent-region re-write.
//!
//! Flag-gated at the call site (`HEADROOM_THINKING_COMPACT`). Fail-open: any
//! Kompress error leaves the original block untouched. `keep_last_turns` is the
//! quality knob. Nothing here panics.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::OnceLock;

use lru::LruCache;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::kompress::Kompress;

/// Number of most-recent assistant turns whose reasoning is left intact.
pub const DEFAULT_KEEP_LAST_TURNS: usize = 1;
/// Reasoning below this word count is not worth a Kompress call.
pub const DEFAULT_MIN_WORDS: usize = 40;

/// Prefix on the emitted text block so the compaction is legible to the model
/// (and greppable in logs). Kept short; the token cost is negligible vs the
/// block.
pub const MARKER: &str = "[prior reasoning, compressed]";

/// Bound on the original-thinking→compacted memo. Mirrors Python's `_CACHE_CAP`.
const CACHE_CAP: usize = 8192;

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

// original-thinking-hash -> compacted text. Deterministic memo so the same
// thinking always yields the same bytes (cache stability), even if Kompress is
// nondeterministic. Bounded LRU; ONNX Kompress is deterministic so
// eviction+recompute is byte-identical anyway.
//
// Divergence from Python: the key is SHA-256 rather than SHA-1 (this crate has
// no SHA-1 dependency). The key is a purely internal memo handle — it never
// reaches the wire — so output stays byte-identical to Python's.
fn memo() -> &'static Mutex<LruCache<String, String>> {
    static MEMO: OnceLock<Mutex<LruCache<String, String>>> = OnceLock::new();
    MEMO.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(CACHE_CAP).expect("non-zero cap"),
        ))
    })
}

// ─── Types ───────────────────────────────────────────────────────────────

/// The compressor the compactor runs reasoning text through.
///
/// Python duck-types on `compress(text, allow_download=False) -> KompressResult`
/// and catches every exception; the Rust equivalent is this trait returning
/// `None` for "no result" (the fail-open path). Implemented for [`Kompress`];
/// tests and the remote compressor supply their own impls.
pub trait ThinkingCompressor {
    /// Compact `text`, or `None` when the compressor could not produce one.
    /// Never panics — the caller treats `None` as "leave the block untouched".
    fn compress_thinking(&self, text: &str) -> Option<String>;
}

impl ThinkingCompressor for Kompress {
    fn compress_thinking(&self, text: &str) -> Option<String> {
        // Mirrors Python's `allow_download=False` call: the model is loaded
        // cache-only off the request path, so this never blocks on a download.
        let compressed = Kompress::compress(self, text).compressed;
        if compressed.is_empty() {
            None
        } else {
            Some(compressed)
        }
    }
}

/// Sidecar counters returned alongside the rewritten messages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingCompactStats {
    /// Assistant turns that had at least one block rewritten.
    pub turns_compacted: usize,
    /// Individual thinking blocks / reasoning spans rewritten or dropped.
    pub blocks: usize,
    /// Word count of the reasoning before rewriting.
    pub words_before: usize,
    /// Word count after rewriting (0 for dropped spans).
    pub words_after: usize,
}

// ─── Model gate ──────────────────────────────────────────────────────────

/// True if `model` re-bills prior-turn thinking as input (so compaction pays).
///
/// Claude 4.6+ (and the 5 family) keep prior-turn thinking in context and bill
/// it; pre-4.6 (sonnet-4-5, haiku-4-5, 3.x) strip it server-side. Verified live:
/// opus-4-6/sonnet-4-6 bill, sonnet-4-5/haiku-4-5 strip. **Conservative** —
/// returns `false` unless the version is confidently >= 4.6, because compacting
/// on a stripping model would turn free (stripped) thinking into billed text.
/// (Opus 4.5 reportedly bills too, but is excluded here pending verification —
/// costs only missed savings.)
pub fn bills_prior_thinking(model: &str) -> bool {
    let lowered = model.to_lowercase();
    let mut nums: Vec<u32> = Vec::new();
    for part in lowered.split('-') {
        if !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()) {
            // Saturate rather than overflow on an absurd date-like segment; the
            // comparison below only cares about small version numbers.
            nums.push(part.parse::<u32>().unwrap_or(u32::MAX));
        } else if !nums.is_empty() {
            // Version digits are contiguous; stop at the family/date boundary.
            break;
        }
    }
    let Some(&major) = nums.first() else {
        return false;
    };
    let minor = nums.get(1).copied().unwrap_or(0);
    major >= 5 || (major, minor) >= (4, 6)
}

// ─── Memoized compaction ─────────────────────────────────────────────────

/// Deterministically compact `text`; `None` if the compressor is absent, failed,
/// or returned nothing.
fn memo_compact(text: &str, kompress: Option<&dyn ThinkingCompressor>) -> Option<String> {
    let kompress = kompress?;
    let key = hex::encode(Sha256::digest(text.as_bytes()));
    // `get` is the LRU touch, matching Python's `move_to_end` on a hit.
    if let Ok(mut cache) = memo().lock() {
        if let Some(hit) = cache.get(&key) {
            return Some(hit.clone());
        }
    }
    // Fail OPEN — never break the proxy on a bad compressor.
    let compacted = match kompress.compress_thinking(text) {
        Some(c) if !c.is_empty() => c,
        _ => {
            tracing::warn!("thinking compaction produced nothing; leaving block untouched");
            return None;
        }
    };
    if let Ok(mut cache) = memo().lock() {
        // `put` inserts at MRU and evicts the LRU entry past the cap.
        cache.put(key, compacted.clone());
    }
    Some(compacted)
}

fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

fn is_thinking_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("thinking")
}

/// Indices of the assistant turns whose reasoning must be left intact — the
/// last `keep_last_turns` of them (none when `keep_last_turns == 0`).
fn keep_indices(messages: &[Value], keep_last_turns: usize) -> Vec<usize> {
    if keep_last_turns == 0 {
        return Vec::new();
    }
    let asst: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map(|(i, _)| i)
        .collect();
    let start = asst.len().saturating_sub(keep_last_turns);
    asst[start..].to_vec()
}

// ─── Anthropic thinking blocks ───────────────────────────────────────────

/// Replace `thinking` blocks with Kompressed `text` blocks.
///
/// Every assistant turn except the last `keep_last_turns` (which keep their
/// thinking verbatim) has each of its `thinking` blocks converted to a `text`
/// block holding the Kompressed summary. Deterministic per content, so the
/// forwarded prefix stays cache-stable across turns. The input slice is never
/// mutated — untouched messages are cloned through.
///
/// * `messages` — provider-native Anthropic messages.
/// * `kompress` — the compressor; `None` makes the whole pass a no-op (mirrors
///   Python's `kompress=None` guard).
/// * `keep_last_turns` — most-recent assistant turns left intact (the active
///   reasoning). `0` compacts everything.
/// * `min_words` — thinking blocks below this word count are left as-is.
pub fn compact_thinking_to_text(
    messages: &[Value],
    kompress: Option<&dyn ThinkingCompressor>,
    keep_last_turns: usize,
    min_words: usize,
) -> (Vec<Value>, ThinkingCompactStats) {
    let mut stats = ThinkingCompactStats::default();
    if kompress.is_none() {
        return (messages.to_vec(), stats);
    }
    let keep = keep_indices(messages, keep_last_turns);

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for (i, m) in messages.iter().enumerate() {
        let content = m.get("content");
        let is_assistant = m.get("role").and_then(Value::as_str) == Some("assistant");
        let blocks = content.and_then(Value::as_array);
        let has_thinking = blocks.is_some_and(|bs| bs.iter().any(is_thinking_block));
        if !is_assistant || keep.contains(&i) || blocks.is_none() || !has_thinking {
            out.push(m.clone());
            continue;
        }
        let blocks = blocks.expect("checked above");

        let mut new_content: Vec<Value> = Vec::with_capacity(blocks.len());
        let mut turn_compacted = false;
        for block in blocks {
            if !is_thinking_block(block) {
                new_content.push(block.clone());
                continue;
            }
            let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
            let words = word_count(text);
            if words < min_words {
                new_content.push(block.clone());
                continue;
            }
            // Skip if compaction failed or didn't actually shrink the block.
            let Some(compacted) = memo_compact(text, kompress).filter(|c| word_count(c) < words)
            else {
                new_content.push(block.clone());
                continue;
            };
            let mut text_block = json!({
                "type": "text",
                "text": format!("{MARKER} {compacted}"),
            });
            // Preserve a cache breakpoint if one happened to sit on the thinking
            // block (rare — breakpoints usually sit on the last block of a
            // message), so we never silently drop a `cache_control` marker.
            if let Some(cc) = block.get("cache_control") {
                text_block["cache_control"] = cc.clone();
            }
            new_content.push(text_block);
            turn_compacted = true;
            stats.blocks += 1;
            stats.words_before += words;
            stats.words_after += word_count(&compacted);
        }

        if turn_compacted {
            stats.turns_compacted += 1;
            let mut nm = m.clone();
            nm["content"] = Value::Array(new_content);
            out.push(nm);
        } else {
            out.push(m.clone());
        }
    }

    (out, stats)
}

// ─── OpenAI-chat plain-text reasoning ────────────────────────────────────

/// Compact each `<think>…</think>` span (GLM / DeepSeek-R1 inline reasoning).
///
/// `drop == false` (warm) Kompresses the inner text; `drop == true` (cold hook)
/// removes the whole span. Returns `(new_content, blocks, words_before,
/// words_after)`. String-scan — the tags are a fixed literal delimiter, not a
/// heuristic. Leaves unmatched/short spans untouched.
fn compact_think_spans(
    content: &str,
    kompress: Option<&dyn ThinkingCompressor>,
    min_words: usize,
    drop: bool,
) -> (String, usize, usize, usize) {
    let (mut blocks, mut wb, mut wa) = (0usize, 0usize, 0usize);
    let mut parts = String::with_capacity(content.len());
    let mut pos = 0usize;
    loop {
        let Some(start) = content[pos..].find(OPEN_TAG).map(|o| pos + o) else {
            parts.push_str(&content[pos..]);
            break;
        };
        let inner_start = start + OPEN_TAG.len();
        let Some(end) = content[inner_start..]
            .find(CLOSE_TAG)
            .map(|o| inner_start + o)
        else {
            // Unterminated — leave the remainder as-is.
            parts.push_str(&content[pos..]);
            break;
        };
        parts.push_str(&content[pos..start]);
        let inner = &content[inner_start..end];
        let words = word_count(inner);
        pos = end + CLOSE_TAG.len();
        if words >= min_words && drop {
            // Cold hook: drop the span entirely (append nothing).
            blocks += 1;
            wb += words;
            continue;
        }
        let mut new_inner = inner.to_string();
        if words >= min_words {
            if let Some(comp) = memo_compact(inner, kompress).filter(|c| word_count(c) < words) {
                wa += word_count(&comp);
                new_inner = comp;
                blocks += 1;
                wb += words;
            }
        }
        parts.push_str(OPEN_TAG);
        parts.push_str(&new_inner);
        parts.push_str(CLOSE_TAG);
    }
    (parts, blocks, wb, wa)
}

/// Compact plain-text reasoning in OpenAI-chat messages (Kimi / GLM /
/// DeepSeek-R1).
///
/// Unlike Anthropic thinking / OpenAI reasoning (encrypted handles), these
/// models resend reasoning as PLAIN TEXT billed as input — so we can actually
/// shrink it (verified: Kimi K2.7 resends `reasoning_content` at +1,558 input
/// tok/block). Two shapes, both handled:
///
/// * **Kimi:** the assistant message's `reasoning_content` field.
/// * **GLM / DeepSeek-R1:** inline `<think>…</think>` in string content.
///
/// `drop == false` (warm): Kompress the reasoning (deterministic →
/// cache-stable). `drop == true` (cold-prefix hook): remove the reasoning
/// outright — the full block, not just ~15% — safe because the cold turn
/// re-caches from scratch. Shape-driven — no model gate; no-ops when no
/// plain-text reasoning is present (OpenAI's encrypted models, Kimi k2.6). Keeps
/// the last `keep_last_turns` assistant turns intact.
pub fn compact_reasoning_openai_chat(
    messages: &[Value],
    kompress: Option<&dyn ThinkingCompressor>,
    keep_last_turns: usize,
    min_words: usize,
    drop: bool,
) -> (Vec<Value>, ThinkingCompactStats) {
    let mut stats = ThinkingCompactStats::default();
    if kompress.is_none() && !drop {
        // `drop` needs no compressor.
        return (messages.to_vec(), stats);
    }
    let keep = keep_indices(messages, keep_last_turns);

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for (i, m) in messages.iter().enumerate() {
        if m.get("role").and_then(Value::as_str) != Some("assistant") || keep.contains(&i) {
            out.push(m.clone());
            continue;
        }
        let mut changed = false;
        let mut nm = m.clone();

        // (1) Kimi: `reasoning_content` field (plain text, no signature → editable)
        if let Some(rc) = m.get("reasoning_content").and_then(Value::as_str) {
            let rc_words = word_count(rc);
            if rc_words >= min_words {
                if drop {
                    // Cold hook: drop the whole reasoning block.
                    nm["reasoning_content"] = Value::String(String::new());
                    changed = true;
                    stats.blocks += 1;
                    stats.words_before += rc_words;
                } else if let Some(comp) =
                    memo_compact(rc, kompress).filter(|c| word_count(c) < rc_words)
                {
                    stats.words_after += word_count(&comp);
                    nm["reasoning_content"] = Value::String(comp);
                    changed = true;
                    stats.blocks += 1;
                    stats.words_before += rc_words;
                }
            }
        }

        // (2) GLM / DeepSeek-R1: inline `<think>…</think>` in string content
        if let Some(c) = m.get("content").and_then(Value::as_str) {
            if c.contains(OPEN_TAG) {
                let (new_c, b, wb, wa) = compact_think_spans(c, kompress, min_words, drop);
                if b > 0 {
                    nm["content"] = Value::String(new_c);
                    changed = true;
                    stats.blocks += b;
                    stats.words_before += wb;
                    stats.words_after += wa;
                }
            }
        }

        if changed {
            stats.turns_compacted += 1;
            out.push(nm);
        } else {
            out.push(m.clone());
        }
    }

    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Every expected value below was produced by running the Python reference
    // (`headroom/transforms/thinking_compactor.py`) on the same input with the
    // same fake compressor.

    /// Python's `_FakeKompress`: always returns `"short summary"` and counts
    /// calls (so the memo can be observed).
    struct FakeKompress {
        calls: AtomicUsize,
    }

    impl FakeKompress {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ThinkingCompressor for FakeKompress {
        fn compress_thinking(&self, _text: &str) -> Option<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some("short summary".to_string())
        }
    }

    /// A compressor that always fails — exercises the fail-open path.
    struct DeadKompress;
    impl ThinkingCompressor for DeadKompress {
        fn compress_thinking(&self, _text: &str) -> Option<String> {
            None
        }
    }

    /// `n` copies of `word`, space-joined. The memo is process-global, so each
    /// test uses a distinct filler word to stay independent of test ordering.
    fn long(word: &str, n: usize) -> String {
        vec![word; n].join(" ")
    }

    fn anthropic_msgs(thinking: &str) -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": thinking, "signature": "sig1"},
                    {"type": "tool_use", "id": "t1", "name": "calc", "input": {}},
                ],
            }),
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}],
            }),
            json!({
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": thinking, "signature": "sig2"}],
            }),
        ]
    }

    #[test]
    fn converts_thinking_and_keeps_last_turn() {
        let k = FakeKompress::new();
        let text = long("reasoning", 60);
        let msgs = anthropic_msgs(&text);
        let (out, stats) = compact_thinking_to_text(&msgs, Some(&k), 1, DEFAULT_MIN_WORDS);

        assert_eq!(
            out[1]["content"][0],
            json!({"type": "text", "text": "[prior reasoning, compressed] short summary"})
        );
        // tool_use must be preserved verbatim.
        assert_eq!(out[1]["content"][1]["type"], json!("tool_use"));
        // Last assistant turn keeps its thinking.
        assert_eq!(out[3]["content"][0]["type"], json!("thinking"));
        assert_eq!(
            stats,
            ThinkingCompactStats {
                turns_compacted: 1,
                blocks: 1,
                words_before: 60,
                words_after: 2,
            }
        );
        // Input must not be mutated.
        assert_eq!(msgs[1]["content"][0]["type"], json!("thinking"));
    }

    #[test]
    fn identical_thinking_hits_the_memo() {
        let k = FakeKompress::new();
        let text = long("memoized", 60);
        let msgs = anthropic_msgs(&text);
        let (out1, _) = compact_thinking_to_text(&msgs, Some(&k), 1, DEFAULT_MIN_WORDS);
        let calls_before = k.calls();
        let (out2, _) = compact_thinking_to_text(&msgs, Some(&k), 1, DEFAULT_MIN_WORDS);
        // Byte-identical across runs, and no second compressor call.
        assert_eq!(out1[1]["content"][0], out2[1]["content"][0]);
        assert_eq!(k.calls(), calls_before);
    }

    #[test]
    fn keep_last_turns_zero_compacts_everything() {
        let k = FakeKompress::new();
        let text = long("everything", 60);
        let msgs = anthropic_msgs(&text);
        let (out, stats) = compact_thinking_to_text(&msgs, Some(&k), 0, DEFAULT_MIN_WORDS);
        assert_eq!(out[3]["content"][0]["type"], json!("text"));
        assert_eq!(
            stats,
            ThinkingCompactStats {
                turns_compacted: 2,
                blocks: 2,
                words_before: 120,
                words_after: 4,
            }
        );
    }

    #[test]
    fn short_thinking_and_no_compressor_are_no_ops() {
        let k = FakeKompress::new();
        // 10 words < min_words=40 → untouched, no compressor call.
        let msgs = anthropic_msgs(&long("brief", 10));
        let (out, stats) = compact_thinking_to_text(&msgs, Some(&k), 0, DEFAULT_MIN_WORDS);
        assert_eq!(out, msgs);
        assert_eq!(stats, ThinkingCompactStats::default());
        assert_eq!(k.calls(), 0);

        // No compressor at all → messages pass through unchanged.
        let msgs = anthropic_msgs(&long("nocomp", 60));
        let (out, stats) = compact_thinking_to_text(&msgs, None, 0, DEFAULT_MIN_WORDS);
        assert_eq!(out, msgs);
        assert_eq!(stats, ThinkingCompactStats::default());
    }

    #[test]
    fn fail_open_leaves_the_block_untouched() {
        let msgs = anthropic_msgs(&long("failopen", 60));
        let (out, stats) =
            compact_thinking_to_text(&msgs, Some(&DeadKompress), 0, DEFAULT_MIN_WORDS);
        assert_eq!(out, msgs);
        assert_eq!(stats, ThinkingCompactStats::default());
    }

    #[test]
    fn cache_control_is_carried_to_the_text_block() {
        let k = FakeKompress::new();
        let text = long("cached", 60);
        let msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": [{
                    "type": "thinking",
                    "thinking": text,
                    "signature": "s",
                    "cache_control": {"type": "ephemeral"},
                }],
            }),
            json!({"role": "user", "content": "next"}),
        ];
        let (out, _) = compact_thinking_to_text(&msgs, Some(&k), 0, DEFAULT_MIN_WORDS);
        assert_eq!(
            out[1]["content"][0],
            json!({
                "type": "text",
                "text": "[prior reasoning, compressed] short summary",
                "cache_control": {"type": "ephemeral"},
            })
        );
    }

    #[test]
    fn model_gate_matches_python() {
        for m in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-opus-4-8",
            "claude-sonnet-5",
        ] {
            assert!(bills_prior_thinking(m), "{m} must bill");
        }
        for m in [
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
            "claude-3-5-sonnet-20241022",
            "claude-opus-4-1-20250805",
            "gpt-4o",
            "some-model",
            "",
        ] {
            assert!(!bills_prior_thinking(m), "{m} must not bill");
        }
        // Version digits are contiguous: the trailing date is not a version.
        assert!(bills_prior_thinking("CLAUDE-OPUS-4-6-20260101"));
    }

    fn openai_msgs(reasoning: &str) -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "q"}),
            // Kimi: reasoning_content on an OLD assistant turn -> compacted.
            json!({"role": "assistant", "content": "kimi answer", "reasoning_content": reasoning}),
            json!({"role": "user", "content": "q2"}),
            // GLM: inline <think> on an OLD assistant turn -> inner compacted.
            json!({"role": "assistant", "content": format!("<think>{reasoning}</think> glm answer")}),
            json!({"role": "user", "content": "q3"}),
            // OpenAI encrypted case: no plain-text reasoning -> must no-op.
            json!({"role": "assistant", "content": "plain answer, no reasoning"}),
        ]
    }

    #[test]
    fn openai_chat_warm_compacts_both_shapes() {
        let k = FakeKompress::new();
        let msgs = openai_msgs(&long("openai", 60));
        let (out, stats) =
            compact_reasoning_openai_chat(&msgs, Some(&k), 1, DEFAULT_MIN_WORDS, false);
        assert_eq!(out[1]["reasoning_content"], json!("short summary"));
        assert_eq!(
            out[3]["content"],
            json!("<think>short summary</think> glm answer")
        );
        assert_eq!(out[5]["content"], json!("plain answer, no reasoning"));
        assert_eq!(
            stats,
            ThinkingCompactStats {
                turns_compacted: 2,
                blocks: 2,
                words_before: 120,
                words_after: 4,
            }
        );
    }

    #[test]
    fn openai_chat_keeps_the_last_turns_reasoning() {
        let k = FakeKompress::new();
        let text = long("lastturn", 60);
        let msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "a", "reasoning_content": text}),
        ];
        let (out, stats) =
            compact_reasoning_openai_chat(&msgs, Some(&k), 1, DEFAULT_MIN_WORDS, false);
        assert_eq!(out[1]["reasoning_content"], json!(text));
        assert_eq!(stats, ThinkingCompactStats::default());
    }

    #[test]
    fn openai_chat_drop_needs_no_compressor() {
        let msgs = openai_msgs(&long("dropped", 60));
        let (out, stats) = compact_reasoning_openai_chat(&msgs, None, 1, DEFAULT_MIN_WORDS, true);
        assert_eq!(out[1]["reasoning_content"], json!(""));
        // The whole <think> span is removed, leaving the answer (and its
        // leading space) behind.
        assert_eq!(out[3]["content"], json!(" glm answer"));
        assert_eq!(out[5]["content"], json!("plain answer, no reasoning"));
        assert_eq!(
            stats,
            ThinkingCompactStats {
                turns_compacted: 2,
                blocks: 2,
                words_before: 120,
                words_after: 0,
            }
        );
    }

    #[test]
    fn think_span_edges_match_python() {
        let k = FakeKompress::new();
        let text = long("spans", 60);

        // Unterminated span: the remainder is left exactly as-is.
        let msgs = vec![
            json!({"role": "assistant", "content": format!("a <think>{text} tail")}),
            json!({"role": "user", "content": "u"}),
        ];
        let (out, stats) =
            compact_reasoning_openai_chat(&msgs, Some(&k), 0, DEFAULT_MIN_WORDS, false);
        assert_eq!(out[0]["content"], msgs[0]["content"]);
        assert_eq!(stats, ThinkingCompactStats::default());

        // Two spans, one long and one short: only the long one is compacted.
        let msgs = vec![json!({
            "role": "assistant",
            "content": format!("x<think>{text}</think>y<think>tiny bit</think>z"),
        })];
        let (out, stats) =
            compact_reasoning_openai_chat(&msgs, Some(&k), 0, DEFAULT_MIN_WORDS, false);
        assert_eq!(
            out[0]["content"],
            json!("x<think>short summary</think>y<think>tiny bit</think>z")
        );
        assert_eq!(
            stats,
            ThinkingCompactStats {
                turns_compacted: 1,
                blocks: 1,
                words_before: 60,
                words_after: 2,
            }
        );

        // Same two spans under drop: the long one vanishes, the short one stays.
        let (out, stats) = compact_reasoning_openai_chat(&msgs, None, 0, DEFAULT_MIN_WORDS, true);
        assert_eq!(out[0]["content"], json!("xy<think>tiny bit</think>z"));
        assert_eq!(
            stats,
            ThinkingCompactStats {
                turns_compacted: 1,
                blocks: 1,
                words_before: 60,
                words_after: 0,
            }
        );
    }
}

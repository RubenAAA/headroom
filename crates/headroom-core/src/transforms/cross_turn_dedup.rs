//! Cross-turn (whole-conversation) verbatim de-duplication.
//!
//! Faithful Rust port of `headroom/transforms/cross_turn_dedup.py`.
//!
//! Bash coding agents re-display the same file bytes many times across turns
//! (`cat foo.py` -> `sed -n 75,100p foo.py` -> `git diff` -> `cat foo.py`
//! again). Every per-block compressor is blind to this: the redundancy is
//! *across* blocks. This transform replaces a contiguous span in a later tool
//! output that already appeared verbatim in an earlier tool output with a
//! compact in-context pointer to the original.
//!
//! Two hard invariants, both required for production use:
//!
//! 1. CACHE-SAFETY via *prefix-monotonicity*. Blocks are processed in order and
//!    a block is only ever matched against content from *strictly earlier*
//!    blocks. Therefore the rewritten output of blocks `0..k` is byte-identical
//!    whether or not block `k+1` exists — appending a turn never mutates an
//!    earlier turn, so the upstream prompt-cache prefix stays byte-stable.
//!    References are ABSOLUTE (an earlier block's ordinal), never relative, so a
//!    frozen pointer's text never changes. [`is_prefix_monotonic`] asserts this.
//!
//! 2. ACCURACY via *no information leaves the window*. Only spans that are
//!    present VERBATIM in an earlier block's already-emitted output are
//!    back-referenced (the "verbatim corpus"), and the earliest occurrence is
//!    never rewritten (keep-earliest), so the original the pointer names is
//!    always physically in context. Only large, non-trivial contiguous spans
//!    are folded.
//!
//! Deterministic, never panics (returns input unchanged on any inconsistency).

use std::collections::HashMap;

use serde_json::Value;

/// A run must be at least this many lines AND this many chars to be worth a
/// pointer. Small dups are left alone (fragmenting context is not worth it) —
/// and a larger floor keeps the pointer comfortably shorter than the span it
/// replaces, so a fold is always a net byte win.
// Lowered with the compact pointer below: at ~100 chars a pointer made
// sub-7-line folds net-negative, so the abundant short (2-4 line) re-read
// repeats were left uncompressed. A ~35-char pointer plus MIN_LINES=3 lets
// those pay off (~4.3% -> ~6% lossless on Opus).
pub const DEFAULT_MIN_LINES: usize = 3;
pub const DEFAULT_MIN_CHARS: usize = 40;
/// Cap anchor candidates examined per line so a hot line (e.g. `    return`)
/// can't blow up matching. Deterministic: candidates are kept in first-seen order.
pub const MAX_ANCHOR_CANDIDATES: usize = 16;

/// One tool-output block.
///
/// `turn` is a STABLE absolute ordinal used in the pointer text (must not change
/// as the conversation grows). `protected` marks blocks that must not be
/// rewritten (e.g. carry a `cache_control` breakpoint) — they are still indexed
/// as reference targets.
#[derive(Debug, Clone)]
pub struct DedupBlock {
    pub text: String,
    pub turn: usize,
    pub protected: bool,
}

impl DedupBlock {
    pub fn new(text: impl Into<String>, turn: usize) -> Self {
        Self {
            text: text.into(),
            turn,
            protected: false,
        }
    }

    pub fn protected(text: impl Into<String>, turn: usize) -> Self {
        Self {
            text: text.into(),
            turn,
            protected: true,
        }
    }
}

/// Sidecar stats returned alongside the rewritten blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DedupStats {
    pub spans_folded: usize,
    pub lines_removed: usize,
    pub chars_removed: usize,
    pub blocks: usize,
    /// Set when the pass aborted defensively and returned the input unchanged.
    pub error: bool,
}

/// A line too common/short to safely anchor a match on its own.
fn is_trivial(line: &str) -> bool {
    let s = line.trim();
    if s.len() < 4 {
        return true;
    }
    matches!(
        s,
        "return"
            | "pass"
            | "else:"
            | "try:"
            | "except:"
            | "finally:"
            | "break"
            | "continue"
            | "});"
            | "})"
            | "],"
            | "),"
            | "\"\"\""
            | "'''"
            | "..."
    )
}

/// A one-line, obviously-a-reference marker naming the in-context original.
///
/// Includes a first-line anchor so the model can locate the block it already
/// saw. Marker-free of any `hash=` retrieval token: recovery is in-context (the
/// original is physically present earlier in the same request).
/// `delta` is the uniform line-number offset of THIS span relative to the
/// referenced original (0 for a byte-identical fold). When non-zero the span is
/// the same content re-read after an edit shifted its line numbers; stating the
/// offset means the named original plus `delta` reconstructs this span's exact
/// numbered bytes, so recovery stays in-window.
fn pointer(span: &[&str], ref_turn: usize, delta: i64) -> String {
    let mut anchor: String = span
        .iter()
        .find(|ln| !ln.trim().is_empty())
        .map(|ln| num_and_key(ln).2.trim())
        .unwrap_or("")
        .to_string();
    // Char-count truncation (span anchors are ASCII source in practice; use
    // chars to stay panic-safe on multibyte input).
    if anchor.chars().count() > 20 {
        let truncated: String = anchor.chars().take(17).collect();
        anchor = format!("{truncated}...");
    }
    let anchor = py_repr(&anchor);
    if delta != 0 {
        format!(
            "[↑{}L same as msg {ref_turn} {delta:+}L: {anchor}]",
            span.len()
        )
    } else {
        format!("[↑{}L same as msg {ref_turn}: {anchor}]", span.len())
    }
}

/// Python's `repr()` for a `str`, which Rust's `{:?}` does not reproduce:
/// CPython prefers single quotes, switching to double only when the string
/// contains a single quote and no double quote. The pointer text is compared
/// byte-for-byte against the Python reference, so the quote choice matters.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Split a leading unpadded line number.
///
/// Returns `(number, match_key, content)`:
/// * `number` — the leading line number, or `None` when absent.
/// * `match_key` — the line with the leading number stripped (separator kept),
///   so two occurrences of the same content at DIFFERENT line numbers share a
///   key (`92:foo` and `94:foo` both key on `:foo`). Non-numbered lines key on
///   themselves, so exact matching is unchanged for them.
/// * `content` — the text after the separator, for the triviality test.
///
/// This is what lets cross-turn dedup survive an edit that renumbers a file: a
/// re-read whose line numbers all shifted by a constant still folds, and the
/// shift is carried in the pointer so the original bytes stay recoverable.
///
/// Mirrors Python's `^([1-9]\d*)(:|\t)(.*)$` — a leading zero does not count as
/// a line number.
fn num_and_key(line: &str) -> (Option<i64>, &str, &str) {
    let bytes = line.as_bytes();
    if bytes
        .first()
        .is_some_and(|b| b.is_ascii_digit() && *b != b'0')
    {
        let mut i = 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i < bytes.len() && (bytes[i] == b':' || bytes[i] == b'\t') {
            // `i` indexes ASCII digits/separator, so these slices are on
            // char boundaries.
            return (line[..i].parse::<i64>().ok(), &line[i..], &line[i + 1..]);
        }
    }
    (None, line, line)
}

/// Record each non-trivial line's `(block_pos, line_idx)` as a future anchor.
///
/// Keeps first-seen order and caps the candidate list per line. Only VERBATIM
/// (surviving) lines should be passed here — never the lines of a span that was
/// replaced by a pointer (those are `None`).
fn index_lines(
    lines: &[Option<String>],
    block_pos: usize,
    anchor_index: &mut HashMap<String, Vec<(usize, usize)>>,
) {
    for (li, ln) in lines.iter().enumerate() {
        let Some(ln) = ln else { continue };
        // Indexed by the line-number-stripped `match_key` so a later renumbered
        // re-read of the same content finds this anchor. Triviality is judged
        // on the content, not the numbered line.
        let (_, key, content) = num_and_key(ln);
        if is_trivial(content) {
            continue;
        }
        let bucket = anchor_index.entry(key.to_string()).or_default();
        if bucket.len() < MAX_ANCHOR_CANDIDATES {
            bucket.push((block_pos, li));
        }
    }
}

/// Longest contiguous run in `cur` starting at `start` whose content appears in
/// a single earlier block — allowing a UNIFORM line-number shift.
///
/// Returns `(length, block_pos, ref_line_idx, delta)` or `None`. `delta` is the
/// constant line-number offset of the run against the referenced original (0 for
/// a byte-identical fold). `corpus[block_pos]` holds that block's VERBATIM lines
/// (`None` where a span was already folded, which breaks contiguity).
///
/// A run extends while each pair shares a `match_key` (content modulo the
/// leading line number) AND every numbered pair has the SAME numeric offset — so
/// an edit shifting a file's numbers by a constant still folds, while a
/// non-uniform change (an edit *inside* the span) ends the run at the
/// divergence. Non-numbered lines must match exactly, since they carry no
/// offset to check.
fn longest_match(
    cur: &[&str],
    start: usize,
    anchor_index: &HashMap<String, Vec<(usize, usize)>>,
    corpus: &[Vec<Option<String>>],
) -> Option<(usize, usize, usize, i64)> {
    let (_, anchor_key, _) = num_and_key(cur[start]);
    let candidates = anchor_index.get(anchor_key)?;
    let mut best_len = 0usize;
    let mut best_bp = usize::MAX;
    let mut best_li = usize::MAX;
    let mut best_delta = 0i64;
    for &(bp, li) in candidates {
        let block_lines = &corpus[bp];
        let mut k = 0usize;
        let mut delta: Option<i64> = None;
        while start + k < cur.len() && li + k < block_lines.len() {
            // End of a folded block in the corpus — the run ends here.
            let Some(cb) = block_lines[li + k].as_deref() else {
                break;
            };
            let ca = cur[start + k];
            let (na, ka, _) = num_and_key(ca);
            let (nb, kb, _) = num_and_key(cb);
            if ka != kb {
                break; // different content -> run ends
            }
            match (na, nb) {
                (Some(na), Some(nb)) => {
                    let d = na - nb;
                    match delta {
                        None => delta = Some(d),
                        // Non-uniform shift (an edit inside the span).
                        Some(prev) if prev != d => break,
                        Some(_) => {}
                    }
                }
                // A non-numbered line must match exactly.
                _ if ca != cb => break,
                _ => {}
            }
            k += 1;
        }
        // Deterministic tie-break: longer wins; on ties keep the earliest
        // (smallest block_pos, then line) already held in best_*.
        if k > best_len {
            best_len = k;
            best_bp = bp;
            best_li = li;
            best_delta = delta.unwrap_or(0);
        }
    }
    if best_len == 0 {
        return None;
    }
    Some((best_len, best_bp, best_li, best_delta))
}

/// Rewrite later verbatim spans to in-context pointers. Prefix-monotonic
/// (cache-safe) and information-preserving (accuracy-safe). Returns
/// `(new_blocks, stats)`. Never panics.
pub fn dedup_blocks(blocks: &[DedupBlock]) -> (Vec<DedupBlock>, DedupStats) {
    dedup_blocks_with(blocks, DEFAULT_MIN_LINES, DEFAULT_MIN_CHARS)
}

/// [`dedup_blocks`] with explicit thresholds.
pub fn dedup_blocks_with(
    blocks: &[DedupBlock],
    min_lines: usize,
    min_chars: usize,
) -> (Vec<DedupBlock>, DedupStats) {
    let mut stats = DedupStats {
        blocks: blocks.len(),
        ..Default::default()
    };

    // corpus[i] = verbatim lines of block i's OUTPUT (None where folded).
    let mut corpus: Vec<Vec<Option<String>>> = Vec::with_capacity(blocks.len());
    let mut anchor_index: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut out_blocks: Vec<DedupBlock> = Vec::with_capacity(blocks.len());

    for blk in blocks {
        let lines: Vec<&str> = blk.text.split('\n').collect();

        if blk.protected {
            // Never rewrite; still a valid verbatim reference target.
            let verbatim: Vec<Option<String>> =
                lines.iter().map(|l| Some((*l).to_string())).collect();
            index_lines(&verbatim, corpus.len(), &mut anchor_index);
            corpus.push(verbatim);
            out_blocks.push(blk.clone());
            continue;
        }

        let mut out: Vec<String> = Vec::new();
        let mut verbatim: Vec<Option<String>> = Vec::new();
        let mut i = 0usize;
        let n = lines.len();
        while i < n {
            let m = longest_match(&lines, i, &anchor_index, &corpus);
            if let Some((mlen, mbp, _mli, mdelta)) = m {
                if mlen >= min_lines {
                    let span = &lines[i..i + mlen];
                    let span_text = span.join("\n");
                    // Chars, not bytes — Python's `len()` gates on characters,
                    // so a multibyte span would otherwise clear the threshold
                    // in Rust while Python left it alone.
                    if span_text.chars().count() >= min_chars {
                        let ref_turn = blocks[mbp].turn;
                        let ptr = pointer(span, ref_turn, mdelta);
                        // Python's `len()` counts CHARACTERS. The pointer now
                        // contains `↑` (3 bytes, 1 char), so byte lengths would
                        // under-report the saving by 2 per fold and drift from
                        // the Python-reported stats.
                        stats.chars_removed += span_text
                            .chars()
                            .count()
                            .saturating_sub(ptr.chars().count());
                        out.push(ptr);
                        // Folded span is NOT verbatim in this block's output:
                        // mark None so it can't seed a later contiguous match,
                        // and don't index it (keep-earliest).
                        verbatim.extend(std::iter::repeat(None).take(mlen));
                        stats.spans_folded += 1;
                        stats.lines_removed += mlen;
                        i += mlen;
                        continue;
                    }
                }
            }
            out.push(lines[i].to_string());
            verbatim.push(Some(lines[i].to_string()));
            i += 1;
        }

        // Index only the surviving verbatim lines of THIS block (first-seen).
        // None entries (folded spans) are kept in place so positions stay
        // aligned with `corpus`; index_lines skips them.
        index_lines(&verbatim, corpus.len(), &mut anchor_index);
        corpus.push(verbatim);
        out_blocks.push(DedupBlock {
            text: out.join("\n"),
            turn: blk.turn,
            protected: false,
        });
    }

    (out_blocks, stats)
}

/// CACHE-SAFETY invariant: for every k, `dedup(blocks[:k])` equals
/// `dedup(full)` truncated to its first k blocks. i.e. appending a later turn
/// never changes an earlier turn's rewritten bytes, so the prompt-cache prefix
/// stays stable.
pub fn is_prefix_monotonic(blocks: &[DedupBlock]) -> bool {
    is_prefix_monotonic_with(blocks, DEFAULT_MIN_LINES, DEFAULT_MIN_CHARS)
}

pub fn is_prefix_monotonic_with(blocks: &[DedupBlock], min_lines: usize, min_chars: usize) -> bool {
    let (full, _) = dedup_blocks_with(blocks, min_lines, min_chars);
    let full_text: Vec<&str> = full.iter().map(|b| b.text.as_str()).collect();
    for k in 1..=blocks.len() {
        let (partial, _) = dedup_blocks_with(&blocks[..k], min_lines, min_chars);
        let partial_text: Vec<&str> = partial.iter().map(|b| b.text.as_str()).collect();
        if partial_text != full_text[..k] {
            return false;
        }
    }
    true
}

/// Whole-conversation cross-turn de-dup over an Anthropic/OpenAI
/// `messages` array, mutating it in place
/// (cache-safe, information-lossless). Port of
/// `ContentRouter._cross_turn_dedup_messages`.
///
/// Runs AFTER per-block compression, over the final message forms: a span
/// in a later tool output that appeared verbatim in an earlier tool output
/// is replaced by an in-context pointer to the original.
///
/// Eligible blocks (matching Python):
/// - Anthropic shape: `messages[i].content[j]` objects with
///   `type == "tool_result"` and a string `content` field.
/// - OpenAI shape: `messages[i]` with `role == "tool"` and a string
///   `content` field.
///
/// A block is protected (reference target only, never rewritten) when its
/// message index is below `frozen_message_count` or when the block/message
/// carries a `cache_control` key. `turn` in the pointer text is the
/// message index — a stable absolute ordinal in an append-only
/// conversation.
///
/// Mutates `messages` in place only when at least one span folds; returns
/// the stats either way.
pub fn dedup_messages(messages: &mut [Value], frozen_message_count: usize) -> DedupStats {
    // (message index, Some(block index) for content-list blocks or None
    // for string-content tool messages), parallel to `dblocks`.
    let mut locs: Vec<(usize, Option<usize>)> = Vec::new();
    let mut dblocks: Vec<DedupBlock> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        let frozen = i < frozen_message_count;
        match msg.get("content") {
            Some(Value::Array(content)) => {
                for (bidx, block) in content.iter().enumerate() {
                    let Some(obj) = block.as_object() else {
                        continue;
                    };
                    if obj.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(text) = obj.get("content").and_then(Value::as_str) else {
                        continue;
                    };
                    if text.is_empty() {
                        continue;
                    }
                    let protected = frozen || obj.contains_key("cache_control");
                    locs.push((i, Some(bidx)));
                    dblocks.push(DedupBlock {
                        text: text.to_string(),
                        turn: i,
                        protected,
                    });
                }
            }
            Some(Value::String(content)) => {
                if msg.get("role").and_then(Value::as_str) != Some("tool") {
                    continue;
                }
                if content.is_empty() {
                    continue;
                }
                let protected = frozen
                    || msg
                        .as_object()
                        .is_some_and(|o| o.contains_key("cache_control"));
                locs.push((i, None));
                dblocks.push(DedupBlock {
                    text: content.clone(),
                    turn: i,
                    protected,
                });
            }
            _ => {}
        }
    }

    if dblocks.len() < 2 {
        return DedupStats {
            blocks: dblocks.len(),
            ..Default::default()
        };
    }

    let (deduped, stats) = dedup_blocks(&dblocks);
    if stats.spans_folded == 0 {
        return stats;
    }

    for (((mi, blk_idx), od), nd) in locs.iter().zip(&dblocks).zip(&deduped) {
        if od.protected || nd.text == od.text {
            continue;
        }
        let new_text = Value::String(nd.text.clone());
        match blk_idx {
            Some(bidx) => {
                if let Some(slot) = messages[*mi]
                    .get_mut("content")
                    .and_then(Value::as_array_mut)
                    .and_then(|c| c.get_mut(*bidx))
                    .and_then(Value::as_object_mut)
                {
                    slot.insert("content".to_string(), new_text);
                }
            }
            None => {
                if let Some(obj) = messages[*mi].as_object_mut() {
                    obj.insert("content".to_string(), new_text);
                }
            }
        }
    }

    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    fn blk(text: &str, turn: usize) -> DedupBlock {
        DedupBlock::new(text, turn)
    }

    /// A realistic, non-trivial multi-line source span.
    fn code(prefix: &str, n: usize) -> String {
        (0..n)
            .map(|i| {
                format!(
                    "{prefix}    result_{i} = compute_overdraft(business_id={i}, amount={})",
                    i * 100
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ptr_re() -> Regex {
        // `[↑<N>L same as msg <T>[ <±D>L]: <repr(anchor)>]`
        Regex::new(r"^\[↑(\d+)L same as msg (\d+)(?: ([+-]\d+)L)?: (.*)\]$").unwrap()
    }

    /// Re-apply a uniform line-number offset to a span, mirroring what the
    /// pointer's `delta` tells a reader to do.
    fn shift_span(span: &[&str], delta: i64) -> Vec<String> {
        span.iter()
            .map(|ln| match num_and_key(ln) {
                (Some(n), key, _) => format!("{}{}", n + delta, key),
                _ => (*ln).to_string(),
            })
            .collect()
    }

    /// Replace each pointer with the referenced turn's original lines and assert
    /// it reproduces the original block — proves references are faithful &
    /// in-context.
    fn reconstruct(orig_blocks: &[DedupBlock], out_blocks: &[DedupBlock]) {
        let re = ptr_re();
        let by_turn: HashMap<usize, Vec<&str>> = orig_blocks
            .iter()
            .map(|b| (b.turn, b.text.split('\n').collect()))
            .collect();
        for (orig, out) in orig_blocks.iter().zip(out_blocks.iter()) {
            if orig.protected {
                assert_eq!(out.text, orig.text);
                continue;
            }
            let mut rebuilt: Vec<String> = Vec::new();
            for line in out.text.split('\n') {
                if let Some(m) = re.captures(line) {
                    let span_len: usize = m[1].parse().unwrap();
                    let t: usize = m[2].parse().unwrap();
                    let delta: i64 = m.get(3).map_or(0, |d| d.as_str().parse().unwrap());
                    // Strip the repr quotes and any truncation ellipsis.
                    let anchor_repr = &m[4];
                    let anchor = anchor_repr[1..anchor_repr.len() - 1].trim_end_matches("...");
                    assert!(t < orig.turn, "reference must point to an EARLIER turn");

                    // The compact pointer carries no line range — recovery is
                    // in-context, so locate the span by its anchor the way a
                    // reader would, then re-apply the stated offset.
                    let src = &by_turn[&t];
                    let start = (0..src.len().saturating_sub(span_len - 1))
                        .find(|&i| {
                            src[i..i + span_len]
                                .iter()
                                .find(|l| !l.trim().is_empty())
                                .is_some_and(|l| num_and_key(l).2.trim().starts_with(anchor))
                        })
                        .unwrap_or_else(|| panic!("anchor {anchor:?} not found in turn {t}"));
                    rebuilt.extend(shift_span(&src[start..start + span_len], delta));
                    continue;
                }
                rebuilt.push(line.to_string());
            }
            assert_eq!(
                rebuilt.join("\n"),
                orig.text,
                "turn {} not faithfully reconstructable",
                orig.turn
            );
        }
    }

    #[test]
    fn verbatim_reread_is_folded_keep_earliest() {
        let span = code("", 8);
        let blocks = vec![
            blk(&format!("cat merge.py\n{span}\ntail"), 1),
            blk(&format!("sed run\n{span}\nmore"), 5),
        ];
        let (out, stats) = dedup_blocks(&blocks);
        assert_eq!(out[0].text, blocks[0].text); // earliest untouched
        assert!(out[1].text.contains("same as msg ")); // later occurrence folded
        assert_eq!(stats.spans_folded, 1);
        reconstruct(&blocks, &out);
    }

    #[test]
    fn cache_safety_prefix_monotonic() {
        let span = code("x", 10);
        let blocks = vec![
            blk(&format!("intro line one\nintro line two\n{span}"), 1),
            blk("unrelated diff output\n@@ -1 +1 @@\n-a\n+b", 2),
            blk(&format!("here again:\n{span}"), 3),
            blk(&format!("and once more\n{span}\ntrailer"), 4),
        ];
        assert!(is_prefix_monotonic(&blocks));
    }

    #[test]
    fn below_min_lines_not_folded() {
        // 2 lines is below the (now 3-line) floor. At exactly 3 it folds —
        // both behaviours verified against Python.
        let span = code("", 2);
        let blocks = vec![blk(&span, 1), blk(&span, 2)];
        let (out, stats) = dedup_blocks(&blocks);
        assert_eq!(stats.spans_folded, 0);
        assert_eq!(out[1].text, blocks[1].text);
    }

    #[test]
    fn trivial_repeated_lines_not_folded() {
        let junk = vec!["}"; 20].join("\n"); // trivial lines only
        let blocks = vec![blk(&junk, 1), blk(&junk, 2)];
        let (_out, stats) = dedup_blocks(&blocks);
        assert_eq!(stats.spans_folded, 0);
    }

    #[test]
    fn deterministic() {
        let span = code("z", 9);
        let blocks = vec![
            blk(&span, 1),
            blk(&format!("mid\n{span}"), 2),
            blk(&span, 3),
        ];
        let (a, _) = dedup_blocks(&blocks);
        let (b, _) = dedup_blocks(&blocks);
        let at: Vec<&str> = a.iter().map(|x| x.text.as_str()).collect();
        let bt: Vec<&str> = b.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(at, bt);
    }

    #[test]
    fn protected_block_not_rewritten_but_is_reference_target() {
        let span = code("", 8);
        let blocks = vec![
            DedupBlock::protected(span.clone(), 1), // cache_control block — never rewritten
            blk(&format!("later:\n{span}"), 2),     // should still fold against the protected one
        ];
        let (out, _stats) = dedup_blocks(&blocks);
        assert_eq!(out[0].text, blocks[0].text);
        assert!(out[1].text.contains("same as msg "));
        reconstruct(&blocks, &out);
    }

    #[test]
    fn info_preserving_reconstruction_multiref() {
        let s1 = code("a", 7);
        let s2 = code("b", 8);
        let blocks = vec![
            blk(&format!("h1\n{s1}"), 1),
            blk(&format!("h2\n{s2}"), 2),
            blk(&format!("mix\n{s1}\n---\n{s2}"), 3), // two folds in one block
        ];
        let (out, stats) = dedup_blocks(&blocks);
        assert_eq!(stats.spans_folded, 2);
        reconstruct(&blocks, &out);
    }

    // ─── Line-number-tolerant folding (upstream addition) ────────────────
    //
    // Expected values were produced by running Python's `cross_turn_dedup`
    // on these exact inputs.

    #[test]
    fn at_the_new_floor_three_lines_folds() {
        // The point of lowering MIN_LINES to 3: short re-read repeats now pay
        // off. Byte-for-byte what Python emits for this span.
        let span = code("", 3);
        let blocks = vec![blk(&span, 1), blk(&span, 2)];
        let (out, stats) = dedup_blocks(&blocks);
        assert_eq!(stats.spans_folded, 1);
        assert_eq!(out[1].text, "[↑3L same as msg 1: 'result_0 = comput...']");
    }

    #[test]
    fn num_and_key_matches_python() {
        assert_eq!(num_and_key("92:foo"), (Some(92), ":foo", "foo"));
        assert_eq!(num_and_key("hello"), (None, "hello", "hello"));
        // A leading zero is not a line number (Python's `[1-9]\d*`).
        assert_eq!(num_and_key("092:foo"), (None, "092:foo", "092:foo"));
        // Tab separator is equally valid.
        assert_eq!(num_and_key("7\tbar"), (Some(7), "\tbar", "bar"));
        // A number with no separator is just text.
        assert_eq!(num_and_key("42"), (None, "42", "42"));
    }

    #[test]
    fn pointer_matches_python_bytes() {
        let span = ["10:def foo():", "11:    return 1"];
        assert_eq!(pointer(&span, 1, 0), "[↑2L same as msg 1: 'def foo():']");
        let shifted = ["15:def foo():", "16:    return 1"];
        assert_eq!(
            pointer(&shifted, 1, 5),
            "[↑2L same as msg 1 +5L: 'def foo():']"
        );
        // Negative offsets carry their sign too.
        assert_eq!(
            pointer(&shifted, 1, -3),
            "[↑2L same as msg 1 -3L: 'def foo():']"
        );
    }

    #[test]
    fn py_repr_prefers_single_quotes_like_python() {
        assert_eq!(py_repr("plain"), "'plain'");
        // Contains a single quote and no double quote -> CPython switches.
        assert_eq!(py_repr("it's"), "\"it's\"");
        // Both kinds present -> stays single-quoted, escaping the single.
        assert_eq!(py_repr("it's \"q\""), r#"'it\'s "q"'"#);
    }

    #[test]
    fn renumbered_reread_still_folds_and_states_the_offset() {
        // The whole point of the delta: an edit above shifted every line
        // number by +5, and the content is otherwise identical. Byte-for-byte
        // what Python produces for these blocks.
        let orig = "10:def foo():\n11:    return 1\n12:\n13:class Bar:\n14:    pass\n";
        let shifted = "15:def foo():\n16:    return 1\n17:\n18:class Bar:\n19:    pass\n";
        let blocks = vec![
            DedupBlock {
                text: orig.to_string(),
                turn: 1,
                protected: false,
            },
            DedupBlock {
                text: shifted.to_string(),
                turn: 2,
                protected: false,
            },
        ];
        let (out, stats) = dedup_blocks(&blocks);
        assert_eq!(out[1].text, "[↑6L same as msg 1 +5L: 'def foo():']");
        assert_eq!(stats.spans_folded, 1);
        assert_eq!(stats.lines_removed, 6);
        assert_eq!(stats.chars_removed, 23);
    }

    #[test]
    fn a_non_uniform_shift_ends_the_run() {
        // An edit INSIDE the span breaks the constant offset, so the run must
        // stop at the divergence rather than fold mismatched content.
        let a = "10:alpha line here\n11:beta line here\n12:gamma line here\n13:delta line here\n";
        // Same first two lines at +5, then the numbering jumps: not uniform.
        let b = "15:alpha line here\n16:beta line here\n99:gamma line here\n100:delta line here\n";
        let blocks = vec![
            DedupBlock {
                text: a.to_string(),
                turn: 1,
                protected: false,
            },
            DedupBlock {
                text: b.to_string(),
                turn: 2,
                protected: false,
            },
        ];
        let (out, _) = dedup_blocks(&blocks);
        // Whatever folds, the diverging lines must survive verbatim.
        assert!(
            out[1].text.contains("99:gamma line here") || out[1].text == b,
            "non-uniform tail must not be folded away: {}",
            out[1].text
        );
    }

    fn tool_result_msg(text: &str, tid: &str) -> Value {
        serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tid, "content": text}],
        })
    }

    /// One block has nothing earlier to point at, so the pass must leave it
    /// alone. Guards the entry point against a fold that references a turn
    /// that isn't there.
    #[test]
    fn dedup_messages_single_block_noop() {
        let span = code("", 12);
        let mut messages = vec![tool_result_msg(&span, "t1")];
        let stats = dedup_messages(&mut messages, 0);
        assert_eq!(stats.spans_folded, 0);
        assert_eq!(messages[0]["content"][0]["content"].as_str().unwrap(), span);
    }

    /// The same span read twice folds on the later read, and the pointer names
    /// the message index it came from.
    #[test]
    fn dedup_messages_folds_reread_tool_result() {
        let span = code("", 12);
        let mut messages = vec![
            serde_json::json!({"role": "user", "content": "fix the overdraft bug"}),
            serde_json::json!({"role": "assistant", "content": "cat merge.py"}),
            tool_result_msg(&format!("$ cat merge.py\n{span}\n# end"), "t1"),
            serde_json::json!({"role": "assistant", "content": "sed -n range"}),
            tool_result_msg(&format!("$ sed -n 1,20p merge.py\n{span}\n# more"), "t2"),
        ];
        let stats = dedup_messages(&mut messages, 0);
        assert_eq!(stats.spans_folded, 1);

        let earlier = messages[2]["content"][0]["content"].as_str().unwrap();
        let later = messages[4]["content"][0]["content"].as_str().unwrap();
        assert!(!earlier.contains("same as msg "), "earliest read is kept");
        assert!(earlier.contains(&span));
        assert!(later.contains("same as msg "), "re-read must fold: {later}");
        // The pointer's turn is the referenced message's index.
        assert!(later.contains("same as msg 2"), "wrong reference: {later}");
    }

    /// Everything inside the frozen prefix is off limits: rewriting it would
    /// bust the prompt cache the prefix exists to protect.
    #[test]
    fn dedup_messages_frozen_prefix_protected() {
        let span = code("", 12);
        let dup = format!("head\n{span}\ntail");
        let mut messages = vec![tool_result_msg(&dup, "t1"), tool_result_msg(&dup, "t2")];
        let stats = dedup_messages(&mut messages, 2);
        assert_eq!(stats.spans_folded, 0);
        assert_eq!(messages[0]["content"][0]["content"].as_str().unwrap(), dup);
        assert_eq!(messages[1]["content"][0]["content"].as_str().unwrap(), dup);
    }

    /// A `cache_control` block may be pointed *at* but never rewritten — same
    /// reasoning as the frozen prefix, marked per-block instead.
    ///
    /// The marked block is deliberately a *later* occurrence. As the earliest
    /// one it would survive anyway, because the first copy of a span is always
    /// kept, and the test would pass with `cache_control` handling deleted.
    #[test]
    fn dedup_messages_cache_control_block_is_target_only() {
        let span = code("", 12);
        let cached = format!("cached:\n{span}");
        let mut messages = vec![
            tool_result_msg(&format!("first:\n{span}"), "t1"),
            serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t2",
                    "content": cached,
                    "cache_control": {"type": "ephemeral"},
                }],
            }),
            tool_result_msg(&format!("later:\n{span}"), "t3"),
        ];
        let stats = dedup_messages(&mut messages, 0);

        assert_eq!(
            messages[1]["content"][0]["content"].as_str().unwrap(),
            cached,
            "a cache_control block must survive byte-for-byte even as a re-read"
        );
        // The unprotected re-read still folds, so protection is scoped to the
        // marked block rather than switching the whole pass off.
        assert_eq!(stats.spans_folded, 1);
        assert!(messages[2]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("same as msg "));
    }

    /// OpenAI-shaped `role: "tool"` messages carry their text as a plain string
    /// rather than a content list, and must fold the same way.
    #[test]
    fn dedup_messages_openai_tool_role_string_content() {
        let span = code("", 12);
        let mut messages = vec![
            serde_json::json!({"role": "tool", "tool_call_id": "t1", "content": format!("a\n{span}")}),
            serde_json::json!({"role": "tool", "tool_call_id": "t2", "content": format!("b\n{span}")}),
        ];
        let stats = dedup_messages(&mut messages, 0);
        assert_eq!(stats.spans_folded, 1);
        assert!(!messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("same as msg "));
        assert!(messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("same as msg "));
    }

    /// Appending a turn must not change a single byte of what came before, or
    /// every request re-writes the cached prefix it was supposed to reuse.
    #[test]
    fn dedup_messages_router_prefix_stability() {
        let span = code("", 12);
        let m1 = vec![
            serde_json::json!({"role": "user", "content": "fix the overdraft bug"}),
            serde_json::json!({"role": "assistant", "content": "cat merge.py"}),
            tool_result_msg(&format!("$ cat merge.py\n{span}\n# end"), "t1"),
        ];
        let mut m2 = m1.clone();
        m2.push(serde_json::json!({"role": "assistant", "content": "sed -n range"}));
        m2.push(tool_result_msg(
            &format!("$ sed -n 1,20p merge.py\n{span}\n# more"),
            "t2",
        ));

        let mut out1 = m1;
        dedup_messages(&mut out1, 0);
        let mut out2 = m2;
        dedup_messages(&mut out2, 0);

        assert_eq!(
            out2[..3],
            out1[..],
            "prompt-cache prefix must stay byte-stable"
        );
    }
}

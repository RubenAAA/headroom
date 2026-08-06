//! Prompt-conditioned relevance split for KEEP/DROP compression decisions.
//!
//! Segments tool output into coherent records, scores each against the request's
//! information need (user prompt + the triggering tool call's args) using the
//! existing `RelevanceScorer` (BM25 / bge-small embeddings / hybrid), and
//! partitions the content into ordered KEEP/DROP runs.
//!
//! The split is mode-agnostic: this module decides *what* is worth keeping
//! verbatim vs. what is a low-value tail; the caller applies the disposition.
//! Nothing here emits markers or calls a compressor.
//!
//! Segmentation is boundary-aware, not line-based: blank lines delimit records,
//! indented continuation lines stay attached to their parent, and dense
//! blank-free streams are packed into small fixed windows. The partition is
//! lossless — `segment(content).join("") == content` — so KEEP runs
//! reconstruct the original bytes exactly.

use crate::relevance::RelevanceScorer;

/// Compose the information-need query for relevance scoring.
///
/// The user's prompt is the high-level intent; the triggering tool call's args
/// (a grep pattern, a read path, a search query) are the precise, per-output
/// ask and usually the sharpest signal. Both are included so the lexical (BM25)
/// half locks onto exact tokens while the semantic half tracks the intent.
pub fn build_relevance_query(user_query: &str, tool_name: &str, tool_args: &str) -> String {
    let mut parts = Vec::new();
    let q = user_query.trim();
    if !q.is_empty() {
        parts.push(q.to_string());
    }
    let call_parts: Vec<&str> = [tool_name.trim(), tool_args.trim()]
        .iter()
        .copied()
        .filter(|p| !p.is_empty())
        .collect();
    let call = call_parts.join(" ");
    if !call.is_empty() {
        parts.push(call);
    }
    parts.join("\n")
}

/// Partition content into coherent records.
///
/// Lossless partition: the concatenation of all segments equals the original.
/// Blank lines delimit records; oversized or dense blank-free blocks are packed
/// into windows of at most `window` lines / `max_chars` chars, with indented
/// continuation lines held to their window so multi-line units aren't cut.
pub fn segment(content: &str, window: usize, max_chars: usize) -> Vec<String> {
    // Split like Python's splitlines(keepends=True) — preserve line endings
    let lines: Vec<&str> = splitlines_keepends(content);
    if lines.len() <= 1 {
        return if content.is_empty() {
            vec![]
        } else {
            vec![content.to_string()]
        };
    }

    // Pass 1: blank-line-delimited blocks
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for &line in &lines {
        cur.push(line);
        if line.trim().is_empty() {
            blocks.push(cur);
            cur = Vec::new();
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }

    // Pass 2: pack/window each block
    let mut segments = Vec::new();
    for block in &blocks {
        let block_len = block.len();
        let block_chars: usize = block.iter().map(|l| l.len()).sum();
        if block_len <= window && block_chars <= max_chars {
            segments.push(block.concat().to_string());
            continue;
        }
        let mut i = 0;
        let n = block_len;
        while i < n {
            let mut j = std::cmp::min(i + window, n);
            while j < n && (block[j].starts_with(' ') || block[j].starts_with('\t')) {
                j += 1;
            }
            let chunk: String = block[i..j].concat();
            segments.push(chunk);
            i = j;
        }
    }

    segments
}

/// Split string into lines keeping the line endings (like Python's str.splitlines(keepends=True)).
fn splitlines_keepends(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'\r' {
            let end = if i + 1 < len && bytes[i + 1] == b'\n' {
                i + 2
            } else {
                i + 1
            };
            lines.push(&s[start..end]);
            start = end;
            i = end;
        } else if bytes[i] == b'\n' {
            lines.push(&s[start..i + 1]);
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    if start < len {
        lines.push(&s[start..]);
    }
    lines
}

/// Otsu's method: the cut between two classes that maximizes between-class
/// variance. Parameter-free — the candidate cuts are the data's own values.
/// Returns the midpoint of the winning adjacent pair.
fn otsu_threshold(values: &[f64]) -> f64 {
    let mut xs: Vec<f64> = values.to_vec();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    let total: f64 = xs.iter().sum();

    let mut w0 = 0.0_f64;
    let mut sum0 = 0.0_f64;
    let mut best_t = xs[0];
    let mut best_var = -1.0_f64;

    for i in 0..n - 1 {
        w0 += 1.0;
        sum0 += xs[i];
        let w1 = n as f64 - w0;
        let m0 = sum0 / w0;
        let m1 = (total - sum0) / w1;
        let between = w0 * w1 * (m0 - m1).powi(2);
        if between > best_var {
            best_var = between;
            best_t = (xs[i] + xs[i + 1]) / 2.0;
        }
    }
    best_t
}

/// Data-driven KEEP/DROP cut for one output's relevance scores.
///
/// The operative cut is the natural relevant/irrelevant break (Otsu) for this
/// output and query — so it moves with the score distribution instead of a
/// fixed constant. It's floored by `floor` so records below the absolute
/// minimum relevance are never kept verbatim.
pub fn adaptive_threshold(values: &[f64], floor: f64) -> f64 {
    // Check if all values are the same (within floating-point precision)
    let unique_count = values
        .iter()
        .map(|v| (v * 1e9).round() as i64)
        .collect::<std::collections::HashSet<_>>()
        .len();
    if unique_count < 2 {
        return floor;
    }
    f64::max(otsu_threshold(values), floor)
}

/// Split content into ordered `(keep, text)` runs by relevance to query.
///
/// A record is KEEP when its relevance score clears the cut. With `adaptive`
/// (default), the cut is the natural relevant/irrelevant break (Otsu), floored
/// by `threshold`. Returns a single KEEP run (no split) when the query is
/// empty, the content is a single record, or it segments into more than
/// `max_records` records.
pub fn plan_relevance_split(
    content: &str,
    query: &str,
    scorer: &dyn RelevanceScorer,
    threshold: f64,
    adaptive: bool,
    window: usize,
    max_chars: usize,
    max_records: Option<usize>,
) -> Vec<(bool, String)> {
    if query.trim().is_empty() {
        return vec![(true, content.to_string())];
    }
    let segs = segment(content, window, max_chars);
    if segs.len() < 2 {
        return vec![(true, content.to_string())];
    }
    if let Some(max) = max_records {
        if segs.len() > max {
            return vec![(true, content.to_string())];
        }
    }

    let seg_refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    let scores = scorer.score_batch(&seg_refs, query);
    let cut = if adaptive {
        let score_vals: Vec<f64> = scores.iter().map(|s| s.score).collect();
        adaptive_threshold(&score_vals, threshold)
    } else {
        threshold
    };

    let mut runs: Vec<(bool, String)> = Vec::new();
    for (seg, sc) in segs.iter().zip(scores.iter()) {
        let keep = sc.score >= cut;
        if let Some(last) = runs.last_mut() {
            if last.0 == keep {
                last.1.push_str(seg);
                continue;
            }
        }
        runs.push((keep, seg.clone()));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relevance::RelevanceScore;

    /// Deterministic fake scorer — score = fraction of query terms present.
    struct KeywordScorer;

    impl RelevanceScorer for KeywordScorer {
        fn score(&self, item: &str, context: &str) -> RelevanceScore {
            let terms: Vec<&str> = context.split_whitespace().collect();
            if terms.is_empty() {
                return RelevanceScore::empty("no query terms");
            }
            let hits = terms
                .iter()
                .filter(|t| item.to_lowercase().contains(&t.to_lowercase()))
                .count();
            RelevanceScore::new(hits as f64 / terms.len() as f64, "keyword", vec![])
        }
    }

    // ── segment ──────────────────────────────────────────────────────────

    #[test]
    fn segment_partition_is_lossless() {
        let text = "a\nb\n\n  cont\nc\n";
        let segs = segment(text, 8, 1200);
        let joined: String = segs.concat();
        assert_eq!(joined, text);
    }

    #[test]
    fn segment_windows_dense_stream_losslessly() {
        let text: String = (0..20).map(|i| format!("line{}\n", i)).collect();
        let segs = segment(&text, 5, 1200);
        let joined: String = segs.concat();
        assert_eq!(joined, text);
        assert!(segs.len() > 1); // dense blank-free stream got windowed
    }

    #[test]
    fn segment_keeps_indented_continuation_attached() {
        let text = "ERROR boom\n  File a.py line 1\n  File b.py line 2\nnext record\n";
        let segs = segment(text, 1, 1200);
        let joined: String = segs.concat();
        assert_eq!(joined, text);
        for s in &segs {
            assert!(!s.starts_with(' ') && !s.starts_with('\t')); // every segment starts at a head line
        }
    }

    // ── build_relevance_query ────────────────────────────────────────────

    #[test]
    fn build_query_composes_prompt_and_tool_args() {
        let q = build_relevance_query("I need entities", "Bash", "grep -rn 'class .*Entity' src/");
        assert!(q.contains("entities"));
        assert!(q.contains("grep"));
        assert!(q.contains("Entity"));
    }

    #[test]
    fn build_query_handles_missing_pieces() {
        assert_eq!(build_relevance_query("", "", ""), "");
        assert_eq!(
            build_relevance_query("just a prompt", "", ""),
            "just a prompt"
        );
    }

    // ── adaptive_threshold ───────────────────────────────────────────────

    #[test]
    fn adaptive_threshold_splits_at_the_natural_gap() {
        let t = adaptive_threshold(&[0.92, 0.88, 0.12, 0.05], 0.25);
        assert!(0.12 < t && t < 0.88);
    }

    #[test]
    fn adaptive_threshold_is_floored() {
        assert_eq!(adaptive_threshold(&[0.30, 0.28, 0.05, 0.03], 0.25), 0.25);
    }

    #[test]
    fn adaptive_threshold_all_equal_uses_floor() {
        assert_eq!(adaptive_threshold(&[0.4, 0.4, 0.4], 0.25), 0.25);
    }

    #[test]
    fn adaptive_threshold_moves_with_distribution() {
        let high = adaptive_threshold(&[0.95, 0.9, 0.6, 0.55], 0.1);
        let low = adaptive_threshold(&[0.4, 0.35, 0.08, 0.05], 0.1);
        assert!(high > low);
    }

    // ── plan_relevance_split ─────────────────────────────────────────────

    #[test]
    fn split_keeps_relevant_drops_irrelevant() {
        let content = "the oauth token refresh failed here\n\nunrelated debug noise about widgets\n\nanother oauth token line\n";
        let runs = plan_relevance_split(
            content,
            "oauth token",
            &KeywordScorer,
            0.5,
            true,
            8,
            1200,
            None,
        );
        let kept: String = runs
            .iter()
            .filter(|(k, _)| *k)
            .map(|(_, t)| t.as_str())
            .collect();
        let dropped: String = runs
            .iter()
            .filter(|(k, _)| !*k)
            .map(|(_, t)| t.as_str())
            .collect();
        assert!(kept.contains("oauth token"));
        assert!(dropped.contains("widgets"));
        // partition stays lossless
        let all: String = runs.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(all, content);
    }

    #[test]
    fn empty_query_yields_no_split() {
        let result = plan_relevance_split("x\ny\n", "", &KeywordScorer, 0.5, true, 8, 1200, None);
        assert_eq!(result, vec![(true, "x\ny\n".to_string())]);
    }

    #[test]
    fn single_record_yields_no_split() {
        let result =
            plan_relevance_split("solo", "anything", &KeywordScorer, 0.5, true, 8, 1200, None);
        assert_eq!(result, vec![(true, "solo".to_string())]);
    }

    #[test]
    fn split_respects_max_records_cap() {
        let content: String = (0..10).map(|i| format!("rec {} widget\n\n", i)).collect();
        let result = plan_relevance_split(
            &content,
            "widget",
            &KeywordScorer,
            0.5,
            true,
            8,
            1200,
            Some(3),
        );
        assert_eq!(result, vec![(true, content)]); // over the cap → no split
    }
}

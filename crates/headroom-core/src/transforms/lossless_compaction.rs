//! Format-native, reversible lossless compaction for no-CCR proxy mode.
//!
//! Every helper here is pure and keeps its output *looking like its own
//! type* — grep stays grep, logs stay logs, diffs stay diffs. No retrieval
//! marker (`<<ccr:…>>` / `Retrieve …`) is ever emitted, so the proxy needs
//! no MCP retrieve round-trip to stay recoverable.
//!
//! The reversible transforms ship with exact inverses and are self-checked at
//! runtime by [`compact_lossless`]: if a round-trip does not reproduce the
//! original (modulo intentionally-dropped non-semantic bits such as ANSI color)
//! or the result is not actually smaller, the original content is returned
//! unchanged. Nothing here panics.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

// ─── Regex patterns (mirrors Python `_ANSI_RE`, `_RUN_MARKER_RE`, etc.) ──

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").expect("ANSI_RE is a valid regex"))
}

/// Minimum block length worth folding, and the guard rails that keep the
/// quadratic-ish scan bounded. Mirror Python's `_FOLD_*` constants.
pub const FOLD_MIN_BLOCK: usize = 3;
const FOLD_MAX_BLOCK: usize = 64;
const FOLD_MAX_CANDIDATES: usize = 8;
const FOLD_MAX_LINES: usize = 20_000;

fn block_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\.\.\. \(repeats (\d+) lines from (\d+) lines back\)$")
            .expect("BLOCK_MARKER_RE is a valid regex")
    })
}

fn run_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\.\.\. \(repeated (\d+) times\)$").expect("RUN_MARKER_RE is a valid regex")
    })
}

fn grep_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?P<path>[^\n:]+):(?P<line>\d+):(?P<content>.*)$")
            .expect("GREP_ROW_RE is a valid regex")
    })
}

fn heading_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?P<line>\d+):(?P<content>.*)$").expect("HEADING_ROW_RE is a valid regex")
    })
}

fn dir_data_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?P<base>[^/\n:]+):(?P<line>\d+):(?P<content>.*)$")
            .expect("DIR_DATA_RE is a valid regex")
    })
}

/// A whole-line file path: optional `./`/`../` root, >=1 directory segment,
/// then a basename. No whitespace or ':' (so grep `path:line:content` rows —
/// handled by [`search_heading`] — are excluded). Directory-only lines (trailing
/// '/') don't match (empty basename), which keeps the fold unambiguous.
fn path_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?P<dir>(?:\.{0,2}/)?(?:[^/\s:]+/)+)(?P<base>[^/\s:]+)$")
            .expect("PATH_ROW_RE is a valid regex")
    })
}

fn diff_index_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^index [0-9a-fA-F]+\.\.[0-9a-fA-F]+( [0-7]+)?$")
            .expect("DIFF_INDEX_RE is a valid regex")
    })
}

// ─── Split/join helpers ──────────────────────────────────────────────────

/// Split into lines, remembering whether a trailing newline was present.
/// Returns (lines, had_trailing_newline).
fn split_keep_trailing(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (vec![], false);
    }
    let had_trailing = text.ends_with('\n');
    let body = if had_trailing {
        &text[..text.len() - 1]
    } else {
        text
    };
    (body.split('\n').collect(), had_trailing)
}

/// Join lines, optionally re-adding a trailing newline.
fn join(lines: &[&str], had_trailing: bool) -> String {
    let mut out = lines.join("\n");
    if had_trailing {
        out.push('\n');
    }
    out
}

// ─── Public API ──────────────────────────────────────────────────────────

/// Remove ANSI CSI/SGR (color) escape sequences. Color is non-semantic.
pub fn strip_ansi(text: &str) -> String {
    ansi_re().replace_all(text, "").to_string()
}

/// Collapse runs of >=2 identical consecutive lines (syslog convention).
///
/// A run of N (N>=2) identical lines becomes the line once followed by
/// `... (repeated N times)`. Exact inverse: [`expand_runs`].
pub fn collapse_runs(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let n = lines.len();
    while i < n {
        let mut j = i;
        while j + 1 < n && lines[j + 1] == lines[i] {
            j += 1;
        }
        let run_len = j - i + 1;
        if run_len >= 2 {
            out.push(lines[i].to_string());
            out.push(format!("... (repeated {} times)", run_len));
        } else {
            out.push(lines[i].to_string());
        }
        i = j + 1;
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Collapse multi-line blocks that repeat earlier content into back-refs.
///
/// The block-level generalization of [`collapse_runs`]: a run of K consecutive
/// lines (K >= [`FOLD_MIN_BLOCK`]) that exactly reproduces K lines seen D lines
/// earlier becomes `... (repeats K lines from D lines back)`. The repeats need
/// not be adjacent, which is what config payloads actually look like — k8s
/// container stanzas repeat with only the `name:` line differing, so their
/// identical tails fold even though no two whole stanzas are consecutive.
///
/// Coordinates are in ORIGINAL lines, and the fold is only taken when the block
/// does not overlap its anchor (K <= D), so on expansion the referenced region
/// is always already reconstructed. Exact inverse: [`unfold_repeated_blocks`].
pub fn fold_repeated_blocks(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    let n = lines.len();
    if n < FOLD_MIN_BLOCK * 2 || n > FOLD_MAX_LINES {
        return text.to_string();
    }
    // Recent original positions per distinct line, bounded so a pathological
    // input (one line repeated a million times) cannot blow up the scan.
    let mut positions: HashMap<&str, Vec<usize>> = HashMap::new();

    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < n {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if let Some(bucket) = positions.get(lines[i]) {
            for &q in bucket.iter().rev() {
                // `i - q` caps the match so the block can never overlap its
                // own anchor; that is what makes expansion order-independent.
                let max_len = FOLD_MAX_BLOCK.min(n - i).min(i - q);
                let mut length = 0usize;
                while length < max_len && lines[q + length] == lines[i + length] {
                    length += 1;
                }
                if length > best_len {
                    best_len = length;
                    best_dist = i - q;
                }
            }
        }
        if best_len >= FOLD_MIN_BLOCK {
            let marker = format!("... (repeats {best_len} lines from {best_dist} lines back)");
            // Only fold when the marker is actually shorter than the block it
            // replaces (+1 per line for the newline it carries).
            let block_chars: usize = (0..best_len).map(|k| lines[i + k].len() + 1).sum();
            if block_chars > marker.len() + 1 {
                out.push(marker);
                for k in 0..best_len {
                    remember_position(&mut positions, lines[i + k], i + k);
                }
                i += best_len;
                continue;
            }
        }
        remember_position(&mut positions, lines[i], i);
        out.push(lines[i].to_string());
        i += 1;
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Fold grep `path:line:content` rows by DIRECTORY.
///
/// Consecutive rows whose path shares a parent directory collapse to that
/// directory once (a header ending in `/`), then `base:line:content` rows
/// beneath it. Complements [`search_heading`] (which factors a repeated *file*):
/// this factors a repeated *directory* across distinct files — the common
/// `grep -rn` case where each file has a single match, so file-heading saves
/// nothing but the shared directory repeats on every row. Rows whose path has
/// no `/` pass through untouched. Exactly reversed by [`search_dir_unheading`];
/// [`compact_lossless`] verifies the round-trip.
pub fn search_dir_heading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current_dir: Option<String> = None;
    for line in &lines {
        let caps = grep_row_re().captures(line);
        let matched = caps.as_ref().filter(|c| c["path"].contains('/'));
        match matched {
            Some(c) => {
                let path = &c["path"];
                let cut = path.rfind('/').expect("path contains '/'") + 1;
                let (dir_part, base) = path.split_at(cut);
                if current_dir.as_deref() != Some(dir_part) {
                    out.push(dir_part.to_string());
                    current_dir = Some(dir_part.to_string());
                }
                out.push(format!("{}:{}:{}", base, &c["line"], &c["content"]));
            }
            None => {
                out.push((*line).to_string());
                current_dir = None;
            }
        }
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Exact inverse of [`search_dir_heading`].
///
/// A *header* is a line ending in `/` immediately followed by a
/// `base:line:content` data row; it is consumed and re-prefixed onto each
/// following data row until a non-data line appears.
pub fn search_dir_unheading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current_dir: Option<&str> = None;
    let n = lines.len();
    let mut i = 0;
    while i < n {
        let line = lines[i];
        let is_data = dir_data_re().is_match(line);
        if let Some(dir) = current_dir {
            if is_data {
                out.push(format!("{dir}{line}"));
                i += 1;
                continue;
            }
        }
        if line.ends_with('/') && i + 1 < n && dir_data_re().is_match(lines[i + 1]) {
            current_dir = Some(line);
            i += 1;
            continue;
        }
        current_dir = None;
        out.push(line.to_string());
        i += 1;
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Fold a *pure* file-path listing (`find` / `ls -1` / `rg -l` output) into
/// ripgrep-heading form: each parent directory printed once on its own line
/// (ending in `/`), then the bare basenames beneath it.
///
/// Reversibility is not assumed here — [`compact_lossless`] verifies the exact
/// round-trip via [`path_unheading`] and discards the fold on any mismatch
/// (e.g. a stray no-slash line mistaken for a basename), so mixed content is
/// always safe. Requires >=2 path rows or there is nothing to group.
pub fn path_heading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.iter().filter(|l| path_row_re().is_match(l)).count() < 2 {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in &lines {
        match path_row_re().captures(line) {
            Some(c) => {
                let dir = c["dir"].to_string();
                if current.as_deref() != Some(dir.as_str()) {
                    out.push(dir.clone());
                    current = Some(dir);
                }
                out.push(c["base"].to_string());
            }
            None => {
                // Blank line inside/around the listing.
                out.push((*line).to_string());
                current = None;
            }
        }
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Exact inverse of [`path_heading`].
///
/// A *header* is a line ending in `/` immediately followed by a basename row
/// (a non-empty line with no `/`); it is consumed and re-prefixed onto each
/// following basename row until a blank line or another header.
pub fn path_unheading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let is_base = |l: &str| !l.is_empty() && !l.contains('/');
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<&str> = None;
    let n = lines.len();
    let mut i = 0;
    while i < n {
        let line = lines[i];
        if let Some(dir) = current {
            if is_base(line) {
                out.push(format!("{dir}{line}"));
                i += 1;
                continue;
            }
        }
        if line.ends_with('/') && i + 1 < n && is_base(lines[i + 1]) {
            current = Some(line);
            i += 1;
            continue;
        }
        current = None;
        out.push(line.to_string());
        i += 1;
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Track recent original positions of `line`, bounded per distinct line so a
/// pathological input cannot make the candidate scan blow up.
fn remember_position<'a>(
    positions: &mut HashMap<&'a str, Vec<usize>>,
    line: &'a str,
    index: usize,
) {
    let bucket = positions.entry(line).or_default();
    bucket.push(index);
    if bucket.len() > FOLD_MAX_CANDIDATES {
        bucket.remove(0);
    }
}

/// Exact inverse of [`fold_repeated_blocks`].
pub fn unfold_repeated_blocks(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    for line in &lines {
        if let Some(caps) = block_marker_re().captures(line) {
            let length: usize = caps[1].parse().unwrap_or(0);
            let dist: usize = caps[2].parse().unwrap_or(0);
            // `length <= dist` is the same non-overlap invariant the folder
            // enforced; a marker violating it is treated as literal text
            // rather than trusted, so a hand-written line cannot corrupt output.
            if dist <= out.len() && length <= dist {
                let start = out.len() - dist;
                let slice: Vec<String> = out[start..start + length].to_vec();
                out.extend(slice);
                continue;
            }
        }
        out.push((*line).to_string());
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Exact inverse of [`collapse_runs`].
pub fn expand_runs(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let n = lines.len();
    while i < n {
        let line = lines[i];
        if i + 1 < n {
            if let Some(caps) = run_marker_re().captures(lines[i + 1]) {
                if let Some(count_str) = caps.get(1) {
                    if let Ok(count) = count_str.as_str().parse::<usize>() {
                        for _ in 0..count {
                            out.push(line.to_string());
                        }
                        i += 2;
                        continue;
                    }
                }
            }
        }
        out.push(line.to_string());
        i += 1;
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// True if any run-collapse marker line is present.
pub fn is_run_collapsed(text: &str) -> bool {
    for line in text.split('\n') {
        if run_marker_re().is_match(line) {
            return true;
        }
    }
    false
}

/// Convert grep `path:line:content` rows into ripgrep --heading form.
///
/// Consecutive rows sharing a path collapse to the path once on its own line
/// (a *header* line), then `line:content` rows beneath it. Lines that don't
/// match the `path:line:content` shape are passed through untouched.
pub fn search_heading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current_path: Option<&str> = None;
    for line in &lines {
        if let Some(caps) = grep_row_re().captures(line) {
            let path = caps.name("path").unwrap().as_str();
            let line_num = caps.name("line").unwrap().as_str();
            let content = caps.name("content").unwrap().as_str();
            if current_path != Some(path) {
                out.push(path.to_string());
                current_path = Some(path);
            }
            out.push(format!("{}:{}", line_num, content));
        } else {
            out.push(line.to_string());
            current_path = None;
        }
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Exact inverse of [`search_heading`].
pub fn search_unheading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current_path: Option<&str> = None;
    let n = lines.len();
    let mut i = 0;
    while i < n {
        let line = lines[i];
        let is_data = heading_row_re().is_match(line);
        if let Some(path) = current_path {
            if is_data {
                if let Some(caps) = heading_row_re().captures(line) {
                    let line_num = caps.name("line").unwrap().as_str();
                    let content = caps.name("content").unwrap().as_str();
                    out.push(format!("{}:{}:{}", path, line_num, content));
                }
                i += 1;
                continue;
            }
        }
        // Not a data row under an active header. Decide if THIS line is a new
        // header: it must not be a data row itself and must be followed by a
        // data row.
        if !is_data && i + 1 < n && heading_row_re().is_match(lines[i + 1]) {
            current_path = Some(line);
            i += 1;
            continue;
        }
        // Plain passthrough line
        current_path = None;
        out.push(line.to_string());
        i += 1;
    }
    let out_refs: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
    join(&out_refs, had_trailing)
}

/// Drop `index <sha>..<sha>` lines from a unified diff (still applies).
pub fn diff_strip_index(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let out: Vec<&str> = lines
        .iter()
        .filter(|line| !diff_index_re().is_match(line))
        .copied()
        .collect();
    join(&out, had_trailing)
}

/// Dispatch format-native lossless compaction by `kind`.
///
/// `kind` in {"log", "search", "diff", "text"}. For reversible kinds the
/// round-trip is verified internally (modulo the intentionally-dropped
/// non-semantic bits, e.g. ANSI color for logs); if verification fails or the
/// result is not smaller, the original content is returned unchanged. Never
/// panics; unknown kinds pass through.
pub fn compact_lossless(content: &str, kind: &str) -> String {
    if content.is_empty() {
        return content.to_string();
    }

    let result = match kind {
        "log" => {
            // ANSI is non-semantic and dropped one-way; run-collapse must be
            // exactly reversible against the de-ANSI'd baseline.
            let baseline = strip_ansi(content);
            let candidate = collapse_runs(&baseline);
            if expand_runs(&candidate) != baseline {
                return content.to_string();
            }
            if candidate.len() < content.len() {
                candidate
            } else {
                return content.to_string();
            }
        }
        "search" => {
            // Two independent folds; keep the smaller that round-trips exactly.
            // `search_heading` factors a repeated FILE (many matches in one
            // file); `search_dir_heading` factors a repeated DIRECTORY (one
            // match each across many files in a dir — the `grep -rn` case the
            // file fold misses entirely).
            let mut best = content.to_string();
            let attempts: [(String, fn(&str) -> String); 2] = [
                (search_heading(content), search_unheading),
                (search_dir_heading(content), search_dir_unheading),
            ];
            for (candidate, inverse) in attempts {
                if inverse(&candidate) == content && candidate.len() < best.len() {
                    best = candidate;
                }
            }
            best
        }
        "paths" => {
            // Pure path listings (find / ls -1 / rg -l): fold repeated parents.
            let candidate = path_heading(content);
            if path_unheading(&candidate) != content {
                return content.to_string();
            }
            if candidate.len() < content.len() {
                candidate
            } else {
                return content.to_string();
            }
        }
        "config" => {
            // Structured config (YAML/TOML/INI): single-line runs first, then
            // repeated multi-line stanzas. The inverse applies in reverse order.
            let candidate = fold_repeated_blocks(&collapse_runs(content));
            if expand_runs(&unfold_repeated_blocks(&candidate)) != content {
                return content.to_string();
            }
            if candidate.len() < content.len() {
                candidate
            } else {
                return content.to_string();
            }
        }
        "diff" => {
            // Purely subtractive of non-semantic bookkeeping lines; the
            // remaining hunks still apply. No exact inverse needed.
            let candidate = diff_strip_index(content);
            if candidate.len() < content.len() {
                candidate
            } else {
                return content.to_string();
            }
        }
        "text" => {
            // Collapse blank-line runs; reversible against itself.
            let candidate = collapse_runs(content);
            if expand_runs(&candidate) != content {
                return content.to_string();
            }
            if candidate.len() < content.len() {
                candidate
            } else {
                return content.to_string();
            }
        }
        _ => return content.to_string(),
    };

    result
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- strip_ansi ---

    #[test]
    fn strip_ansi_removes_only_escapes() {
        let colored = "\x1b[31mERROR\x1b[0m: boom \x1b[1mbold\x1b[0m end";
        assert_eq!(strip_ansi(colored), "ERROR: boom bold end");
        let plain = "no escapes here : 1:2:3";
        assert_eq!(strip_ansi(plain), plain);
    }

    // --- collapse_runs / expand_runs ---

    #[test]
    fn collapse_expand_runs_byte_roundtrip() {
        let log = "starting worker\n\
                    connection refused\n\
                    connection refused\n\
                    connection refused\n\
                    connection refused\n\
                    connection refused\n\
                    retrying\n\
                    retrying\n\
                    done\n";
        let collapsed = collapse_runs(log);
        assert!(is_run_collapsed(&collapsed));
        assert!(collapsed.len() < log.len());
        assert_eq!(expand_runs(&collapsed), log);
    }

    #[test]
    fn collapse_runs_no_trailing_newline_roundtrip() {
        let log = "a\na\na\nb";
        assert_eq!(expand_runs(&collapse_runs(log)), log);
    }

    #[test]
    fn collapse_runs_singletons_untouched() {
        let log = "one\ntwo\nthree\n";
        assert_eq!(collapse_runs(log), log);
        assert!(!is_run_collapsed(log));
    }

    #[test]
    fn collapse_runs_empty() {
        assert_eq!(collapse_runs(""), "");
        assert_eq!(expand_runs(""), "");
    }

    // --- search_heading / search_unheading ---

    #[test]
    fn search_heading_unheading_roundtrip() {
        let grep = "src/app.py:10:def main():\n\
                     src/app.py:11:    run()\n\
                     src/app.py:42:    return 0\n\
                     src/util.py:3:import os\n\
                     src/util.py:9:import sys\n";
        let headed = search_heading(grep);
        // heading form: each path appears once as its own header line
        assert_eq!(headed.matches("src/app.py").count(), 1);
        assert_eq!(headed.matches("src/util.py").count(), 1);
        assert_eq!(search_unheading(&headed), grep);
    }

    #[test]
    fn search_heading_smaller_for_repeated_paths() {
        let grep: String = (1..30)
            .map(|i| format!("a/very/long/path/module.py:{}:line{}", i, i))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let headed = search_heading(&grep);
        assert!(headed.len() < grep.len());
        assert_eq!(search_unheading(&headed), grep);
    }

    #[test]
    fn search_heading_leaves_non_matching_lines() {
        let text = "just some prose\nnot a grep row at all\n";
        assert_eq!(search_heading(text), text);
        assert_eq!(search_unheading(text), text);
    }

    #[test]
    fn search_heading_mixed_content_roundtrip() {
        let grep = "banner line\nsrc/a.py:1:x\nsrc/a.py:2:y\nmiddle prose\nsrc/b.py:5:z\n";
        let headed = search_heading(grep);
        assert_eq!(search_unheading(&headed), grep);
    }

    // --- diff_strip_index ---

    #[test]
    fn diff_strip_index_keeps_plus_minus() {
        let diff = "diff --git a/f.py b/f.py\n\
                     index 0123abc..def4567 100644\n\
                     --- a/f.py\n\
                     +++ b/f.py\n\
                     @@ -1,3 +1,3 @@\n\
                     context\n\
                     -old line\n\
                     +new line\n";
        let stripped = diff_strip_index(diff);
        assert!(!stripped.contains("index 0123abc..def4567"));
        assert!(stripped.contains("-old line"));
        assert!(stripped.contains("+new line"));
        assert!(stripped.contains("@@ -1,3 +1,3 @@"));
        assert!(stripped.contains("--- a/f.py"));
        assert!(stripped.len() < diff.len());
    }

    // --- compact_lossless ---

    #[test]
    fn compact_lossless_log_roundtrips_modulo_ansi() {
        let log = format!("\x1b[31mfail\x1b[0m\n{}", "fail\n".repeat(5));
        let out = compact_lossless(&log, "log");
        // recoverable modulo ANSI: expand back == de-ANSI'd original
        assert_eq!(expand_runs(&out), strip_ansi(&log));
        assert!(out.len() < log.len());
    }

    #[test]
    fn compact_lossless_returns_original_when_not_smaller() {
        let log = "line one\nline two\nline three\n";
        assert_eq!(compact_lossless(log, "log"), log);
    }

    #[test]
    fn compact_lossless_search() {
        let grep: String = (1..20)
            .map(|i| format!("pkg/mod.py:{}:code{}", i, i))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let out = compact_lossless(&grep, "search");
        assert!(out.len() < grep.len());
        assert_eq!(search_unheading(&out), grep);
    }

    #[test]
    fn compact_lossless_unknown_kind_passthrough() {
        assert_eq!(compact_lossless("whatever", "mystery"), "whatever");
    }

    #[test]
    fn compact_lossless_never_panics_on_empty() {
        for kind in &["log", "search", "paths", "diff", "text", "config"] {
            assert_eq!(compact_lossless("", kind), "");
        }
    }

    // ─── Block folding (upstream addition) ───────────────────────────────

    #[test]
    fn fold_repeated_blocks_round_trips() {
        // Two k8s-ish stanzas differing only in the `name:` line: the identical
        // tails fold even though the whole stanzas are never adjacent.
        let text = "  - name: alpha\n    image: repo/service:1.2.3\n    ports:\n    - containerPort: 8080\n    resources:\n      limits:\n        memory: 512Mi\n  - name: beta\n    image: repo/service:1.2.3\n    ports:\n    - containerPort: 8080\n    resources:\n      limits:\n        memory: 512Mi\n";
        let folded = fold_repeated_blocks(text);
        assert!(
            folded.len() < text.len(),
            "should actually shrink: {folded}"
        );
        assert!(
            folded.contains("... (repeats "),
            "marker expected: {folded}"
        );
        assert_eq!(unfold_repeated_blocks(&folded), text, "must round-trip");
    }

    #[test]
    fn fold_repeated_blocks_leaves_short_input_alone() {
        let text = "a\nb\nc\n";
        assert_eq!(fold_repeated_blocks(text), text);
    }

    #[test]
    fn unfold_rejects_a_marker_that_would_overlap_its_anchor() {
        // `length > dist` violates the non-overlap invariant the folder
        // enforces, so a hand-written marker is kept as literal text rather
        // than expanded into something the folder could never have produced.
        let hostile = "x\n... (repeats 9 lines from 1 lines back)\n";
        assert_eq!(unfold_repeated_blocks(hostile), hostile);
    }

    #[test]
    fn config_kind_round_trips_through_both_stages() {
        let text = "key: 1\nkey: 1\nkey: 1\n  - name: a\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n  - name: b\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n";
        let out = compact_lossless(text, "config");
        // Whatever it returns must be recoverable: either untouched, or foldable
        // back to the exact original through both inverses in reverse order.
        if out != text {
            assert!(out.len() < text.len());
            assert_eq!(expand_runs(&unfold_repeated_blocks(&out)), text);
        }
    }

    // ─── Directory-level grep folding (upstream addition) ────────────────

    #[test]
    fn search_dir_heading_folds_the_grep_rn_case() {
        // One match per file across a shared directory — the case the FILE
        // fold saves nothing on, because no path repeats.
        let text = "src/alpha.rs:12:use std::io;\nsrc/beta.rs:34:use std::io;\nsrc/gamma.rs:56:use std::io;\n";
        let folded = search_dir_heading(text);
        assert!(folded.len() < text.len(), "should shrink: {folded}");
        assert_eq!(search_dir_unheading(&folded), text, "must round-trip");
        // And the dispatcher should pick it over the file fold.
        let out = compact_lossless(text, "search");
        assert!(out.len() < text.len());
        assert_eq!(search_dir_unheading(&out), text);
    }

    #[test]
    fn search_keeps_whichever_fold_is_smaller() {
        // Many matches in ONE file: the file fold should win here.
        let text = "src/alpha.rs:1:a\nsrc/alpha.rs:2:b\nsrc/alpha.rs:3:c\nsrc/alpha.rs:4:d\n";
        let out = compact_lossless(text, "search");
        assert!(out.len() < text.len());
        // Recoverable by one of the two inverses.
        assert!(
            search_unheading(&out) == text || search_dir_unheading(&out) == text,
            "must round-trip through one of the folds: {out}"
        );
    }

    #[test]
    fn search_rows_without_a_slash_pass_through() {
        let text = "README:1:hello\nREADME:2:world\n";
        assert_eq!(search_dir_unheading(&search_dir_heading(text)), text);
    }

    // ─── Path listing folding ────────────────────────────────────────────

    #[test]
    fn path_heading_round_trips() {
        let text = "src/handlers/alpha.rs\nsrc/handlers/beta.rs\nsrc/handlers/gamma.rs\n";
        let folded = path_heading(text);
        assert!(folded.len() < text.len(), "should shrink: {folded}");
        assert_eq!(path_unheading(&folded), text, "must round-trip");
        let out = compact_lossless(text, "paths");
        assert!(out.len() < text.len());
        assert_eq!(path_unheading(&out), text);
    }

    #[test]
    fn path_heading_needs_at_least_two_rows() {
        let text = "src/only.rs\n";
        assert_eq!(path_heading(text), text);
    }

    // Byte-parity against the Python reference. Expected values were produced by
    // running `headroom.transforms.lossless_compaction` on these exact inputs —
    // round-tripping alone would not catch a fold that is merely *a* valid
    // encoding rather than *the* one Python emits.
    #[test]
    fn matches_python_reference_bytes() {
        let cfg = "  - name: alpha\n    image: repo/service:1.2.3\n    ports:\n    - containerPort: 8080\n    resources:\n      limits:\n        memory: 512Mi\n  - name: beta\n    image: repo/service:1.2.3\n    ports:\n    - containerPort: 8080\n    resources:\n      limits:\n        memory: 512Mi\n";
        assert_eq!(
            fold_repeated_blocks(cfg),
            "  - name: alpha\n    image: repo/service:1.2.3\n    ports:\n    - containerPort: 8080\n    resources:\n      limits:\n        memory: 512Mi\n  - name: beta\n... (repeats 6 lines from 7 lines back)\n"
        );

        let grep = "src/alpha.rs:12:use std::io;\nsrc/beta.rs:34:use std::io;\nsrc/gamma.rs:56:use std::io;\n";
        assert_eq!(
            search_dir_heading(grep),
            "src/\nalpha.rs:12:use std::io;\nbeta.rs:34:use std::io;\ngamma.rs:56:use std::io;\n"
        );

        let paths = "src/handlers/alpha.rs\nsrc/handlers/beta.rs\nsrc/handlers/gamma.rs\n";
        assert_eq!(
            path_heading(paths),
            "src/handlers/\nalpha.rs\nbeta.rs\ngamma.rs\n"
        );

        let conf = "key: 1\nkey: 1\nkey: 1\n  - name: a\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n  - name: b\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n";
        assert_eq!(
            compact_lossless(conf, "config"),
            "key: 1\n... (repeated 3 times)\n  - name: a\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n  - name: b\n... (repeats 3 lines from 4 lines back)\n"
        );
    }

    #[test]
    fn paths_kind_bails_out_on_mixed_content() {
        // A bare no-slash line adjacent to a fold could be mistaken for a
        // basename; the round-trip check must catch it and return the original.
        let text = "src/a.rs\nsrc/b.rs\nstray\nsrc/c.rs\n";
        let out = compact_lossless(text, "paths");
        assert_eq!(
            path_unheading(&out),
            text,
            "whatever is returned must recover exactly"
        );
    }
}

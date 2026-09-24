//! Sparse-overhead audit (§5 of the harness doc).
//!
//! Offline-only. Reads Anthropic captures and measures, inside `tool_result`
//! blocks only, the token share of three repeated overheads the harness doc
//! names: line-number prefixes, absolute-path spans, and ANSI escape codes.
//! Then simulates the doc's concrete proposal — numbering every 10th line
//! instead of every line — by retokenizing with sparse numbers and reporting
//! the delta. Ranking input for `harness-sparse-line-numbers.md`, not a
//! transform itself.
//!
//! Line-number rule mirrors `transforms/cross_turn_dedup.rs`: a leading
//! ASCII digit run followed by a separator. ANSI stripping is an explicit
//! byte scan (no regex, per build-constraint policy).
//!
//! Usage:
//!   cargo run -p headroom-proxy --bin sparse_overhead_audit -- <capture_dir>

use std::collections::{BTreeMap, HashMap};

use headroom_core::tokenizer::{get_tokenizer, Tokenizer};
use serde_json::Value;

/// Separator set after a leading digit run for the run to count as a line
/// number prefix (whitespace, colon, pipe, bracket, paren). Dash is
/// deliberately excluded: date fragments like `2026-09-24` would otherwise
/// false-positive.
fn split_number_prefix(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i > 7 {
        return None;
    }
    if i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b':' | b'|' | b']' | b')') {
        // Avoid matching a bare year/date fragment mid-prose: require the
        // line to start with the digits (it does — i starts at 0) and the
        // rest to be non-empty.
        let (pre, rest) = line.split_at(i);
        if !rest.trim().is_empty() {
            return Some((pre, rest));
        }
    }
    None
}

/// Strip ANSI CSI sequences (`ESC [ params final`) via explicit scan.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Whitespace-separated span that looks like a filesystem path.
fn is_path_span(span: &str) -> bool {
    span.len() >= 5
        && span.contains('/')
        && (span.starts_with('/')
            || span.starts_with("~/")
            || span.starts_with("./")
            || span.starts_with("mcp__"))
}

/// First three components of an absolute path, for repetition counting.
fn path_root(span: &str) -> Option<String> {
    let span =
        span.trim_matches(|c| matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';' | ':'));
    if !span.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = span.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "/{}",
        parts.iter().take(3).cloned().collect::<Vec<_>>().join("/")
    ))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Category {
    File,
    Search,
    Command,
    Edit,
    Web,
    Other,
    Unknown,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::File => "file",
            Category::Search => "search",
            Category::Command => "command",
            Category::Edit => "edit_write",
            Category::Web => "web",
            Category::Other => "other",
            Category::Unknown => "unknown_parent",
        }
    }
}

fn category_of(tool: &str) -> Category {
    let t = tool.to_lowercase();
    let base = t.trim_start_matches('_');
    if base.starts_with("mcp__") {
        return Category::Other;
    }
    match base {
        "read" | "view" | "read_file" => Category::File,
        "grep" | "glob" => Category::Search,
        "bash"
        | "bash_background"
        | "bash_background_output"
        | "bash_background_wait"
        | "bash_background_kill" => Category::Command,
        "edit" | "multiedit" | "write" | "apply_patch" => Category::Edit,
        "webfetch" | "websearch" => Category::Web,
        _ => Category::Other,
    }
}

/// Collect text from a tool_result block (string or array content).
fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut out = String::new();
            for b in arr {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
            out
        }
        _ => String::new(),
    }
}

#[derive(Default)]
struct Accum {
    blocks: usize,
    lines: usize,
    numbered_lines: usize,
    tokens: usize,
    number_prefix_tokens: usize,
    path_span_tokens: usize,
    ansi_bytes: usize,
    sparse_tokens: usize,
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: sparse_overhead_audit <capture_dir>");
        std::process::exit(2);
    });

    let mut by_cat: BTreeMap<Category, Accum> = BTreeMap::new();
    let mut path_roots: HashMap<String, usize> = HashMap::new();
    let mut turns = 0usize;
    let mut result_blocks = 0usize;

    let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| {
        eprintln!("read capture dir {dir}: {e}");
        std::process::exit(1);
    });
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("out")
        {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let env: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if env.get("endpoint").and_then(|e| e.as_str()) != Some("anthropic") {
            continue;
        }
        let body = env.get("body").cloned().unwrap_or(Value::Null);
        if body.is_null() {
            continue;
        }
        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown");
        let tok = get_tokenizer(model);
        let tok: &dyn Tokenizer = tok.as_ref();
        turns += 1;

        // tool_use id → name across the turn's messages.
        let empty = vec![];
        let msgs = body
            .get("messages")
            .and_then(|m| m.as_array())
            .unwrap_or(&empty);
        let mut id_map: HashMap<String, String> = HashMap::new();
        for msg in msgs {
            for b in msg
                .get("content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    if let (Some(id), Some(name)) = (
                        b.get("id").and_then(|i| i.as_str()),
                        b.get("name").and_then(|n| n.as_str()),
                    ) {
                        id_map.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }

        for msg in msgs {
            for b in msg
                .get("content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                    continue;
                }
                let parent = b
                    .get("tool_use_id")
                    .and_then(|i| i.as_str())
                    .and_then(|id| id_map.get(id))
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let cat = if parent.is_empty() {
                    Category::Unknown
                } else {
                    category_of(parent)
                };
                let text = result_text(&b);
                if text.is_empty() {
                    continue;
                }
                result_blocks += 1;
                let acc = by_cat.entry(cat).or_default();
                acc.blocks += 1;

                let full_tokens = tok.count_text(&text);
                acc.tokens += full_tokens;

                // ANSI share: bytes + retokenized saving.
                let stripped_ansi = strip_ansi(&text);
                if stripped_ansi.len() != text.len() {
                    acc.ansi_bytes += text.len() - stripped_ansi.len();
                }

                // Per-line number prefixes + sparse simulation.
                let mut prefix_joined = String::new();
                let mut sparse_text = String::new();
                for (idx, line) in text.lines().enumerate() {
                    acc.lines += 1;
                    if let Some((pre, _rest)) = split_number_prefix(line) {
                        acc.numbered_lines += 1;
                        prefix_joined.push_str(pre);
                        prefix_joined.push('\n');
                        // Keep the number on every 10th line only.
                        if (idx + 1) % 10 == 0 {
                            sparse_text.push_str(line);
                        } else {
                            sparse_text.push_str(line[pre.len()..].trim_start());
                        }
                    } else {
                        sparse_text.push_str(line);
                    }
                    sparse_text.push('\n');
                }
                if !prefix_joined.is_empty() {
                    acc.number_prefix_tokens += tok.count_text(&prefix_joined);
                }
                // Sparse saving measured against the ANSI-stripped text so
                // the two levers don't double-count each other.
                let sparse_base = tok.count_text(&stripped_ansi);
                let sparse_new = tok.count_text(&strip_ansi(&sparse_text));
                acc.sparse_tokens += sparse_base.saturating_sub(sparse_new);

                // Path-span share.
                let mut path_joined = String::new();
                for span in text.split_whitespace() {
                    if is_path_span(span) {
                        path_joined.push_str(span);
                        path_joined.push(' ');
                        if let Some(root) = path_root(span) {
                            *path_roots.entry(root).or_insert(0) += 1;
                        }
                    }
                }
                if !path_joined.is_empty() {
                    acc.path_span_tokens += tok.count_text(&path_joined);
                }
            }
        }
    }

    println!("Scanned {turns} turns, {result_blocks} tool_result blocks from {dir}\n");
    println!(
        "{:<12} {:>8} {:>12} {:>10} {:>10} {:>10} {:>12}",
        "category", "blocks", "tokens", "num_pre%", "path%", "sparse%", "ansi_bytes"
    );
    println!("{}", "-".repeat(92));
    let mut order: Vec<_> = by_cat.iter().collect();
    order.sort_by_key(|a| std::cmp::Reverse(a.1.tokens));
    for (cat, a) in order {
        let num_pct = pct(a.number_prefix_tokens, a.tokens);
        let path_pct = pct(a.path_span_tokens, a.tokens);
        let sparse_pct = pct(a.sparse_tokens, a.tokens);
        println!(
            "{:<12} {:>8} {:>12} {:>9.1}% {:>9.1}% {:>9.1}% {:>12}",
            cat.label(),
            a.blocks,
            a.tokens,
            num_pct,
            path_pct,
            sparse_pct,
            a.ansi_bytes
        );
    }
    let mut roots: Vec<_> = path_roots.iter().collect();
    roots.sort_by(|a, b| b.1.cmp(a.1));
    println!("\n== top repeated path roots ==");
    for (root, n) in roots.iter().take(10) {
        println!("  {n:>8}  {root}");
    }
    println!("\nNotes: number rule = leading digit run (<=7 digits) + separator, mirroring");
    println!("cross_turn_dedup. Sparse = keep number on every 10th line, measured against");
    println!("ANSI-stripped text. Shares are tokenizer ratios on wire text, not billed cost;");
    println!("reads are free on subscription — the creation weight is what would ship.");
}

fn pct(a: usize, b: usize) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64 * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_prefix_splits() {
        assert!(split_number_prefix("92:foo").is_some());
        assert!(split_number_prefix("12\tpackage main").is_some());
        assert!(split_number_prefix("2026-09-24 not a number").is_none());
        assert!(split_number_prefix("no digits here").is_none());
        assert!(split_number_prefix("12345678 too long").is_none());
    }

    #[test]
    fn ansi_strips_csi() {
        assert_eq!(strip_ansi("\x1b[32mok\x1b[0m"), "ok");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn path_spans_detected() {
        assert!(is_path_span("/home/ruben/headroom/x.rs"));
        assert!(!is_path_span("hello"));
        assert!(!is_path_span("a/b"));
        assert_eq!(
            path_root("/home/ruben/headroom/crates/x.rs").as_deref(),
            Some("/home/ruben/headroom")
        );
    }

    #[test]
    fn categories_route() {
        assert_eq!(category_of("Read"), Category::File);
        assert_eq!(category_of("Grep"), Category::Search);
        assert_eq!(category_of("Bash"), Category::Command);
        assert_eq!(category_of("mcp__s__f"), Category::Other);
    }
}

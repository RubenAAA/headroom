//! Content type detection for multi-format compression.
//!
//! Direct port of `headroom/transforms/content_detector.py`. This module
//! detects the type of tool output content so the upstream
//! `ContentRouter` can dispatch it to the right compressor:
//!
//! - **JsonArray**: Structured JSON data → `SmartCrusher`
//! - **SourceCode**: Python, JavaScript, Go, Rust, etc. → `CodeAwareCompressor`
//! - **SearchResults**: grep / ripgrep output (`file:line:content`)
//! - **BuildOutput**: Compiler / test / lint logs
//! - **GitDiff**: Unified diff format → `DiffCompressor`
//! - **Html**: Web pages (needs extraction, not compression)
//! - **PlainText**: Generic fallback
//!
//! Detection is **regex-based** — no ML, no model loading, no I/O.
//! Magika integration lives one level up in `ContentRouter`, not here.
//!
//! # Parity with Python
//!
//! Regex patterns, dispatch order, confidence formulas, and line-count
//! caps are byte-equal with the Python source. Recorded fixtures in
//! `tests/parity/fixtures/content_detector/` lock the output across
//! the bridge.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

/// Content types recognized by the detector. String tags match Python's
/// `ContentType` enum values 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    JsonArray,
    SourceCode,
    SearchResults,
    BuildOutput,
    GitDiff,
    Html,
    Tabular,
    /// YAML / TOML / INI configuration content.
    StructuredConfig,
    PlainText,
}

impl ContentType {
    /// Stable string tag — matches Python's `ContentType.<NAME>.value`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ContentType::JsonArray => "json_array",
            ContentType::SourceCode => "source_code",
            ContentType::SearchResults => "search",
            ContentType::BuildOutput => "build",
            ContentType::GitDiff => "diff",
            ContentType::Html => "html",
            ContentType::Tabular => "tabular",
            ContentType::StructuredConfig => "structured_config",
            ContentType::PlainText => "text",
        }
    }
}

/// Result of `detect_content_type`. `metadata` is per-type free-form key/
/// value data — same shape as Python's `dict[str, Any]`. We use
/// `serde_json::Map` so PyO3 can convert it to a Python dict on the
/// boundary without losing type fidelity.
#[derive(Debug, Clone)]
pub struct DetectionResult {
    pub content_type: ContentType,
    pub confidence: f64,
    pub metadata: Map<String, Value>,
}

impl DetectionResult {
    fn new(content_type: ContentType, confidence: f64, metadata: Map<String, Value>) -> Self {
        Self {
            content_type,
            confidence,
            metadata,
        }
    }

    fn plain_text(confidence: f64) -> Self {
        Self::new(ContentType::PlainText, confidence, Map::new())
    }
}

// ─── Regex patterns (compiled once, shared) ───────────────────────────

/// `file:line:` (grep -n style) — first column on a non-blank line.
static SEARCH_RESULT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^\s:]+:\d+:").unwrap());

/// Diff-header detection. Recognizes:
/// - `git diff` (`diff --git`, `--- a/`)
/// - merge-commit headers (`diff --combined`, `diff --cc`)
/// - regular hunk headers (`@@ -A,B +C,D @@`)
/// - combined-diff hunk headers (`@@@ ... @@@`)
///
/// Mirrors Python's bug-fix from 2026-04-25 that widened the grammar
/// to handle merge-commit diffs from `git log -p`.
static DIFF_HEADER_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(diff --git|diff --combined |diff --cc |--- a/|@@\s+-\d+,\d+\s+\+\d+,\d+\s+@@|@@@+\s+-\d+(?:,\d+)?\s+(?:-\d+(?:,\d+)?\s+)+\+\d+(?:,\d+)?\s+@@@+)",
    )
    .unwrap()
});

/// Lines starting with `+` or `-` followed by a non-`+`/`-` char (i.e.
/// real change lines, not header lines like `+++ b/file`).
static DIFF_CHANGE_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[+-][^+-]").unwrap());

// ─── Code patterns by language ─────────────────────────────────────────

struct CodePatterns {
    name: &'static str,
    patterns: Vec<Regex>,
}

static CODE_PATTERNS: LazyLock<Vec<CodePatterns>> = LazyLock::new(|| {
    vec![
        CodePatterns {
            name: "python",
            patterns: vec![
                Regex::new(r"^\s*(def|class|import|from|async def)\s+\w+").unwrap(),
                Regex::new(r"^\s*@\w+").unwrap(),
                Regex::new(r#"^\s*""""#).unwrap(),
                Regex::new(r"^\s*if __name__\s*==").unwrap(),
            ],
        },
        CodePatterns {
            name: "javascript",
            patterns: vec![
                Regex::new(r"^\s*(function|const|let|var|class|import|export)\s+").unwrap(),
                Regex::new(r"^\s*(async\s+function|=>\s*\{)").unwrap(),
                Regex::new(r"^\s*module\.exports").unwrap(),
            ],
        },
        CodePatterns {
            name: "typescript",
            patterns: vec![
                Regex::new(r"^\s*(interface|type|enum|namespace)\s+\w+").unwrap(),
                // Python uses `pattern.match(line)` which is start-anchored,
                // so this pattern only ever fires on lines literally starting
                // with `:`. We anchor with `^` to keep parity (the `regex`
                // crate's `is_match` is unanchored by default).
                Regex::new(r"^:\s*(string|number|boolean|any|void)\b").unwrap(),
            ],
        },
        CodePatterns {
            name: "go",
            patterns: vec![
                Regex::new(r"^\s*(func|type|package|import)\s+").unwrap(),
                Regex::new(r"^\s*func\s+\([^)]+\)\s+\w+").unwrap(),
            ],
        },
        CodePatterns {
            name: "rust",
            patterns: vec![
                Regex::new(r"^\s*(fn|struct|enum|impl|mod|use|pub)\s+").unwrap(),
                Regex::new(r"^\s*#\[").unwrap(),
            ],
        },
        CodePatterns {
            name: "java",
            patterns: vec![
                Regex::new(r"^\s*(public|private|protected)\s+(class|interface|enum)").unwrap(),
                Regex::new(r"^\s*@\w+").unwrap(),
                Regex::new(r"^\s*package\s+[\w.]+;").unwrap(),
            ],
        },
        CodePatterns {
            name: "php",
            patterns: vec![
                // Python `.match()` is start-anchored, so these two only ever
                // fire on a line that literally begins with the token — same
                // `^` treatment as the typescript pattern above.
                Regex::new(r"^<\?php\b").unwrap(),
                Regex::new(r"^\s*namespace\s+[\w\\]+\s*;").unwrap(),
                Regex::new(r"^\s*use\s+[\w\\]+(\s+as\s+\w+)?\s*;").unwrap(),
                Regex::new(
                    r"^\s*(public|private|protected|static|abstract|final)?\s*function\s+\w+\s*\(",
                )
                .unwrap(),
                Regex::new(r"^\$this->").unwrap(),
            ],
        },
    ]
});

// ─── Log / build output patterns ───────────────────────────────────────
//
// Order matters: indices 0–1 (`ERROR` and `WARN` family) are treated as
// "error" matches by `try_detect_log`, contributing extra to confidence.
// Same ordering as Python.

static LOG_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)\b(ERROR|FAIL|FAILED|FATAL|CRITICAL)\b").unwrap(),
        Regex::new(r"(?i)\b(WARN|WARNING)\b").unwrap(),
        Regex::new(r"(?i)\b(INFO|DEBUG|TRACE)\b").unwrap(),
        Regex::new(r"^\s*\d{4}-\d{2}-\d{2}").unwrap(),
        Regex::new(r"^\s*\[\d{2}:\d{2}:\d{2}\]").unwrap(),
        Regex::new(r"^={3,}|^-{3,}").unwrap(),
        Regex::new(r"^\s*PASSED|^\s*FAILED|^\s*SKIPPED").unwrap(),
        Regex::new(r"^npm ERR!|^yarn error|^cargo error").unwrap(),
        Regex::new(r"Traceback \(most recent call last\)").unwrap(),
        Regex::new(r"^\s*at\s+[\w.$]+\(").unwrap(),
    ]
});

// ─── HTML patterns ─────────────────────────────────────────────────────

static HTML_DOCTYPE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*<!doctype\s+html").unwrap());
static HTML_TAG_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<html[\s>]").unwrap());
static HTML_HEAD_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<head[\s>]").unwrap());
static HTML_BODY_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<body[\s>]").unwrap());
static HTML_STRUCTURAL_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)<(div|span|script|style|link|meta|nav|header|footer|aside|article|section|main)[\s>]",
    )
    .unwrap()
});

// ─── Public entry point ────────────────────────────────────────────────

/// Detect the type of `content` for routing. Mirrors Python's
/// `detect_content_type`.
///
/// Dispatch order (matches Python verbatim):
/// 1. Empty / whitespace-only → `PlainText` confidence 0.0
/// 2. JSON array (highest priority for `SmartCrusher`)
/// 3. Git diff (≥ 0.7 confidence required)
/// 4. HTML (≥ 0.7 confidence required)
/// 5. Search results (≥ 0.6 confidence required)
/// 6. Build / log output (≥ 0.5 confidence required)
/// 7. Source code (≥ 0.5 confidence required)
/// 8. Fallback to `PlainText` confidence 0.5
pub fn detect_content_type(content: &str) -> DetectionResult {
    if content.is_empty() || content.trim().is_empty() {
        return DetectionResult::plain_text(0.0);
    }

    if let Some(r) = try_detect_json(content) {
        return r;
    }
    if let Some(r) = try_detect_diff(content) {
        if r.confidence >= 0.7 {
            return r;
        }
    }
    if let Some(r) = try_detect_html(content) {
        if r.confidence >= 0.7 {
            return r;
        }
    }
    if let Some(r) = try_detect_search(content) {
        if r.confidence >= 0.6 {
            return r;
        }
    }
    if let Some(r) = try_detect_log(content) {
        if r.confidence >= 0.5 {
            return r;
        }
    }
    // Tabular detection runs after search/log so those claim content first.
    if let Some(r) = try_detect_tabular(content) {
        if r.confidence >= 0.6 {
            return r;
        }
    }
    // Config detection runs after tabular and before code: a `key: value`
    // config would otherwise read as source code.
    if let Some(r) = try_detect_structured_config(content) {
        if r.confidence >= 0.6 {
            return r;
        }
    }
    if let Some(r) = try_detect_code(content) {
        if r.confidence >= 0.5 {
            return r;
        }
    }
    DetectionResult::plain_text(0.5)
}

// ─── Structured config (YAML / TOML / INI) ───────────────────────────────

static CONFIG_SECTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*\[\[?[\w.\-"' ]+\]\]?\s*$"#).expect("valid"));
static TOML_ASSIGN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*(?:[\w.\-]+|"[^"]+"|'[^']+')\s*=\s*\S"#).expect("valid"));
static INI_ASSIGN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[\w.\-@ ]+?\s*[=:]\s*").expect("valid"));
static YAML_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:-\s+)?(?:[\w.\-/]+|"[^"]+"|'[^']+')\s*:(?:\s|$)"#).expect("valid")
});
static YAML_LIST_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*-\s+\S").expect("valid"));
static YAML_DOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^---\s*$|^\.\.\.\s*$").expect("valid"));
static CONFIG_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[#;]").expect("valid"));

/// True if `content` parses as TOML.
fn try_parse_toml(content: &str) -> bool {
    content.parse::<toml::Table>().is_ok()
}

/// Permissive INI acceptance check, standing in for Python's
/// `configparser.ConfigParser(interpolation=None, strict=False)`.
///
/// DIVERGENCE: Python confirms with the stdlib parser; Rust has no equivalent,
/// so this reimplements the acceptance rule — at least one `[section]` header,
/// and every non-comment line after the first section is either a section
/// header, a `key = value` / `key: value` assignment, or an indented
/// continuation. `strict=False` means duplicates are fine, so they are not
/// checked. Content that reaches here has already passed the section +
/// assignment-share gate, so this is a rejection filter for malformed input
/// rather than a general parser; exotic INI that configparser accepts and this
/// rejects would fall through to the YAML heuristic or plain text, never to a
/// wrong claim.
fn parses_as_ini(content: &str) -> bool {
    let mut seen_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || CONFIG_COMMENT_RE.is_match(line) {
            continue;
        }
        if CONFIG_SECTION_RE.is_match(line) {
            seen_section = true;
            continue;
        }
        if !seen_section {
            // A value before any section header is a configparser error
            // (MissingSectionHeaderError), unless it is a continuation.
            if !line.starts_with(char::is_whitespace) {
                return false;
            }
            continue;
        }
        if INI_ASSIGN_RE.is_match(line) || line.starts_with(char::is_whitespace) {
            continue;
        }
        return false;
    }
    seen_section
}

/// Disambiguate `[section]`-shaped config: TOML first, then INI.
///
/// Both flavors share the section-header line shape; only a real parse tells
/// them apart. Returns `Some("toml")`, `Some("ini")`, or `None` when neither
/// accepts the content — in which case it is not claimed as config at all.
fn parse_config_flavor(content: &str) -> Option<&'static str> {
    if content.len() > 1_000_000 {
        return None;
    }
    if try_parse_toml(content) {
        return Some("toml");
    }
    if parses_as_ini(content) {
        return Some("ini");
    }
    None
}

/// Detect structured config content (YAML, TOML, INI).
///
/// TOML/INI claims are parser-confirmed so they carry high confidence. YAML has
/// no parse step here, so its claim is heuristic: key/list/document-marker line
/// share plus a structure signal, guarded against prose and markdown
/// front-matter.
pub fn try_detect_structured_config(content: &str) -> Option<DetectionResult> {
    let head = content.trim_start().chars().next()?;
    if head == '{' || head == '<' {
        // JSON objects and markup are never config; JSON arrays and real
        // TOML/INI `[section]` headers disambiguate below.
        return None;
    }

    let lines: Vec<&str> = content.split('\n').take(200).collect();
    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if non_empty.len() < 3 {
        return None;
    }
    // Comment lines are neutral: excluded from the line-share ratio so
    // comment-heavy configs and #-heading markdown skew it neither way.
    let body: Vec<&str> = non_empty
        .into_iter()
        .filter(|l| !CONFIG_COMMENT_RE.is_match(l))
        .collect();
    if body.len() < 3 {
        return None;
    }
    let body_len = body.len() as f64;

    // TOML / INI: require a section header plus an assignment-dominant body,
    // then let a real parse confirm and disambiguate.
    let sections = body
        .iter()
        .filter(|l| CONFIG_SECTION_RE.is_match(l))
        .count();
    if sections >= 1 {
        let assigns = body
            .iter()
            .filter(|l| TOML_ASSIGN_RE.is_match(l) || INI_ASSIGN_RE.is_match(l))
            .count();
        if assigns >= 2 && (sections + assigns) as f64 / body_len >= 0.6 {
            if let Some(flavor) = parse_config_flavor(content) {
                let share = (sections + assigns) as f64 / body_len;
                let mut metadata = serde_json::Map::new();
                metadata.insert("flavor".into(), serde_json::json!(flavor));
                metadata.insert("sections".into(), serde_json::json!(sections));
                metadata.insert("assignments".into(), serde_json::json!(assigns));
                return Some(DetectionResult {
                    content_type: ContentType::StructuredConfig,
                    confidence: (0.7 + share * 0.25).min(0.95),
                    metadata,
                });
            }
        }
    }

    // Markdown front-matter guard: a `---` fence closed within 60 lines and
    // followed by non-YAML content is a markdown document, not standalone YAML.
    if lines.first().map(|l| l.trim()) == Some("---") {
        for idx in 1..lines.len().min(60) {
            let t = lines[idx].trim();
            if t == "---" || t == "..." {
                let tail: Vec<&str> = lines[idx + 1..]
                    .iter()
                    .copied()
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                let tail_yaml = tail
                    .iter()
                    .filter(|l| YAML_KEY_RE.is_match(l) || YAML_LIST_RE.is_match(l))
                    .count();
                if !tail.is_empty() && (tail_yaml as f64 / tail.len() as f64) < 0.3 {
                    return None;
                }
                break;
            }
        }
    }

    // YAML heuristic.
    let yaml_keys = body.iter().filter(|l| YAML_KEY_RE.is_match(l)).count();
    let yaml_lists = body
        .iter()
        .filter(|l| YAML_LIST_RE.is_match(l) && !YAML_KEY_RE.is_match(l))
        .count();
    let doc_marks = body
        .iter()
        .filter(|l| YAML_DOC_RE.is_match(l.trim()))
        .count();
    if yaml_keys < 3 {
        return None;
    }
    let share = (yaml_keys + yaml_lists + doc_marks) as f64 / body_len;
    if share < 0.6 {
        return None;
    }
    // Prose guards: config lines are short field-ish tuples; prose reads like
    // sentences.
    let enders = body
        .iter()
        .filter(|l| {
            let t = l.trim_end();
            t.ends_with('.') || t.ends_with('!') || t.ends_with('?')
        })
        .count();
    if enders as f64 / body_len >= 0.5 {
        return None;
    }
    let avg_words = body
        .iter()
        .map(|l| l.split_whitespace().count())
        .sum::<usize>() as f64
        / body_len;
    if avg_words > 8.0 {
        return None;
    }
    // Structure signal: nested indentation, a document marker, or a real list.
    let indents: HashSet<usize> = body
        .iter()
        .filter(|l| YAML_KEY_RE.is_match(l) || YAML_LIST_RE.is_match(l))
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .collect();
    if indents.len() < 2 && doc_marks == 0 && yaml_lists < 3 {
        return None;
    }

    let mut metadata = serde_json::Map::new();
    metadata.insert("flavor".into(), serde_json::json!("yaml"));
    metadata.insert("keys".into(), serde_json::json!(yaml_keys));
    metadata.insert("list_items".into(), serde_json::json!(yaml_lists));
    Some(DetectionResult {
        content_type: ContentType::StructuredConfig,
        confidence: (0.55 + share * 0.35).min(0.9),
        metadata,
    })
}

/// Quick check: is `content` a JSON array of dictionaries (the format
/// `SmartCrusher` natively handles)? Convenience wrapper around
/// `detect_content_type`.
pub fn is_json_array_of_dicts(content: &str) -> bool {
    let result = detect_content_type(content);
    if result.content_type != ContentType::JsonArray {
        return false;
    }
    result
        .metadata
        .get("is_dict_array")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// ─── Per-type detection helpers ────────────────────────────────────────

/// Decode a run of whitespace-separated top-level JSON values.
///
/// Web search tools (SerpAPI, Tavily, custom backends) commonly emit
/// back-to-back JSON objects separated only by whitespace rather than a real
/// array: `{"title": ...} {"title": ...} {"title": ...}`. Returns the list of
/// decoded values, or None if the text isn't a clean run of JSON values
/// separated only by whitespace.
fn decode_concatenated_json(content: &str) -> Option<Vec<Value>> {
    let mut items: Vec<Value> = Vec::new();
    let stream = serde_json::Deserializer::from_str(content).into_iter::<Value>();
    for value in stream {
        match value {
            Ok(v) => items.push(v),
            Err(_) => return None,
        }
    }
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

/// Convert whitespace-separated JSON objects into a canonical JSON array.
///
/// SmartCrusher only compresses JSON arrays, so this rewrites the
/// space-separated web_search shape (`{...} {...} {...}`) into
/// `[{...}, {...}, {...}]`. Returns None unless the content is two or more
/// whitespace-separated JSON objects.
pub fn normalize_concatenated_json(content: &str) -> Option<String> {
    let stripped = content.trim();
    if !stripped.starts_with('{') {
        return None;
    }
    let items = decode_concatenated_json(stripped)?;
    if items.len() >= 2 && items.iter().all(|v| v.is_object()) {
        return serde_json::to_string(&items).ok();
    }
    None
}

fn try_detect_json(content: &str) -> Option<DetectionResult> {
    let trimmed = content.trim();

    if trimmed.starts_with('[') {
        let parsed: Value = serde_json::from_str(trimmed).ok()?;
        let arr = parsed.as_array()?;
        let item_count = arr.len();
        let is_dict_array = !arr.is_empty() && arr.iter().all(|v| v.is_object());
        let confidence = if is_dict_array { 1.0 } else { 0.8 };
        return Some(DetectionResult::new(
            ContentType::JsonArray,
            confidence,
            json!({
                "item_count": item_count,
                "is_dict_array": is_dict_array,
            })
            .as_object()
            .cloned()
            .unwrap(),
        ));
    }

    // Space-separated JSON objects (typical web_search output) aren't a valid
    // array, so they'd fall through to PLAIN_TEXT and skip SmartCrusher at 0%
    // compression. SmartCrusher normalizes this shape to a real array before
    // crushing (#1741).
    if trimmed.starts_with('{') {
        let items = decode_concatenated_json(trimmed)?;
        if items.len() >= 2 && items.iter().all(|v| v.is_object()) {
            return Some(DetectionResult::new(
                ContentType::JsonArray,
                1.0,
                json!({
                    "item_count": items.len(),
                    "is_dict_array": true,
                    "concatenated": true,
                })
                .as_object()
                .cloned()
                .unwrap(),
            ));
        }
    }

    None
}

fn try_detect_diff(content: &str) -> Option<DetectionResult> {
    // Window: 500 lines (extended from 50 in Python's 2026-04-25 fix).
    let mut header_matches: u32 = 0;
    let mut change_matches: u32 = 0;
    for line in content.split('\n').take(500) {
        if DIFF_HEADER_PATTERN.is_match(line) {
            header_matches += 1;
        }
        if DIFF_CHANGE_PATTERN.is_match(line) {
            change_matches += 1;
        }
    }
    if header_matches == 0 {
        return None;
    }
    // Same formula as Python: 0.5 + 0.2 * headers + 0.05 * changes, capped at 1.0
    let confidence =
        (0.5 + (header_matches as f64) * 0.2 + (change_matches as f64) * 0.05).min(1.0);
    Some(DetectionResult::new(
        ContentType::GitDiff,
        confidence,
        json!({
            "header_matches": header_matches,
            "change_lines": change_matches,
        })
        .as_object()
        .cloned()
        .unwrap(),
    ))
}

fn try_detect_html(content: &str) -> Option<DetectionResult> {
    // Sample first 3000 chars (byte-indexed; matches Python's str slice
    // for ASCII inputs which is the common HTML case).
    let sample: &str = if content.len() > 3000 {
        // Find the last char-boundary <= 3000 so we don't slice mid-codepoint.
        let mut cutoff = 3000;
        while !content.is_char_boundary(cutoff) {
            cutoff -= 1;
        }
        &content[..cutoff]
    } else {
        content
    };

    let has_doctype = HTML_DOCTYPE_PATTERN.is_match(sample);
    let has_html_tag = HTML_TAG_PATTERN.is_match(sample);
    let has_head = HTML_HEAD_PATTERN.is_match(sample);
    let has_body = HTML_BODY_PATTERN.is_match(sample);
    let structural_matches = HTML_STRUCTURAL_TAGS.find_iter(sample).count() as u32;

    if !has_doctype && !has_html_tag && structural_matches < 3 {
        return None;
    }

    let mut confidence = 0.0_f64;
    if has_doctype {
        confidence += 0.5;
    }
    if has_html_tag {
        confidence += 0.3;
    }
    if has_head {
        confidence += 0.1;
    }
    if has_body {
        confidence += 0.1;
    }
    confidence += (structural_matches as f64 * 0.03).min(0.3);
    confidence = confidence.min(1.0);

    if confidence < 0.5 {
        return None;
    }
    Some(DetectionResult::new(
        ContentType::Html,
        confidence,
        json!({
            "has_doctype": has_doctype,
            "has_html_tag": has_html_tag,
            "structural_tags": structural_matches,
        })
        .as_object()
        .cloned()
        .unwrap(),
    ))
}

pub fn try_detect_search(content: &str) -> Option<DetectionResult> {
    let lines: Vec<&str> = content.split('\n').take(100).collect();
    if lines.is_empty() {
        return None;
    }
    let mut matching_lines: u32 = 0;
    for line in &lines {
        if !line.trim().is_empty() && SEARCH_RESULT_PATTERN.is_match(line) {
            matching_lines += 1;
        }
    }
    if matching_lines == 0 {
        return None;
    }
    let non_empty_lines = lines.iter().filter(|l| !l.trim().is_empty()).count() as u32;
    if non_empty_lines == 0 {
        return None;
    }
    let ratio = matching_lines as f64 / non_empty_lines as f64;
    if ratio < 0.3 {
        return None;
    }
    let confidence = (0.4 + ratio * 0.6).min(1.0);
    Some(DetectionResult::new(
        ContentType::SearchResults,
        confidence,
        json!({
            "matching_lines": matching_lines,
            "total_lines": non_empty_lines,
        })
        .as_object()
        .cloned()
        .unwrap(),
    ))
}

pub fn try_detect_log(content: &str) -> Option<DetectionResult> {
    let lines: Vec<&str> = content.split('\n').take(200).collect();
    if lines.is_empty() {
        return None;
    }
    let mut pattern_matches: u32 = 0;
    let mut error_matches: u32 = 0;
    for line in &lines {
        for (i, pattern) in LOG_PATTERNS.iter().enumerate() {
            if pattern.is_match(line) {
                pattern_matches += 1;
                if i < 2 {
                    error_matches += 1;
                }
                break; // one pattern per line is enough
            }
        }
    }
    if pattern_matches == 0 {
        return None;
    }
    let non_empty_lines = lines.iter().filter(|l| !l.trim().is_empty()).count() as u32;
    if non_empty_lines == 0 {
        return None;
    }
    let ratio = pattern_matches as f64 / non_empty_lines as f64;
    if ratio < 0.1 {
        return None;
    }
    let confidence = (0.3 + ratio * 0.5 + (error_matches as f64) * 0.05).min(1.0);
    Some(DetectionResult::new(
        ContentType::BuildOutput,
        confidence,
        json!({
            "pattern_matches": pattern_matches,
            "error_matches": error_matches,
            "total_lines": non_empty_lines,
        })
        .as_object()
        .cloned()
        .unwrap(),
    ))
}

// ─── Tabular detection ──────────────────────────────────────────────────

fn md_cell_count(row: &str) -> usize {
    row.trim()
        .trim_matches('|')
        .split('|')
        .filter(|c| !c.trim().is_empty())
        .count()
}

fn is_md_separator(line: &str) -> bool {
    let cells: Vec<&str> = line
        .trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect();
    if cells.len() < 2 {
        return false;
    }
    let re = md_sep_cell_re_static();
    cells.iter().all(|c| re.is_match(c))
}

fn md_sep_cell_re_static() -> &'static Regex {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^:?-{2,}:?$").unwrap());
    &RE
}

fn try_detect_markdown_table(lines: &[&str]) -> Option<DetectionResult> {
    for i in 0..lines.len().saturating_sub(1) {
        let header = lines[i];
        let sep = lines[i + 1];
        if header.contains('|') && is_md_separator(sep) {
            let cols = md_cell_count(header);
            if cols >= 2 {
                let mut meta = serde_json::Map::new();
                meta.insert("format".to_string(), Value::String("markdown".to_string()));
                meta.insert("columns".to_string(), json!(cols));
                return Some(DetectionResult {
                    content_type: ContentType::Tabular,
                    confidence: 0.95,
                    metadata: meta,
                });
            }
        }
    }
    None
}

fn looks_like_prose(sample: &[&str], delim: &str) -> bool {
    let enders = sample
        .iter()
        .filter(|r| {
            let trimmed = r.trim_end();
            trimmed.ends_with('.') || trimmed.ends_with('!') || trimmed.ends_with('?')
        })
        .count();
    if sample.len() > 0 && enders as f64 / sample.len() as f64 >= 0.5 {
        return true;
    }
    let cells: Vec<&str> = sample
        .iter()
        .flat_map(|r| r.split(delim))
        .map(|c| c.trim())
        .collect();
    if cells.is_empty() {
        return false;
    }
    let avg_words: f64 = cells
        .iter()
        .map(|c| c.split_whitespace().count())
        .sum::<usize>() as f64
        / cells.len() as f64;
    avg_words > 3.0
}

fn try_detect_delimited(lines: &[&str]) -> Option<DetectionResult> {
    let sample: Vec<&str> = lines.iter().take(20).copied().collect();
    if sample.len() < 3 {
        return None;
    }

    let delimiters: &[(&str, f64)] = &[(",", 0.85), ("\t", 0.7), (";", 0.85), ("|", 0.85)];
    let mut best: Option<DetectionResult> = None;

    for &(delim, min_consistency) in delimiters {
        let counts: Vec<usize> = sample
            .iter()
            .map(|row| row.matches(delim).count())
            .collect();
        if counts[0] == 0 {
            continue;
        }

        // Find most common count
        let mut freq_map: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for &c in &counts {
            *freq_map.entry(c).or_insert(0) += 1;
        }
        let (common_count, freq) = freq_map.iter().max_by_key(|(_, &f)| f)?;

        if *common_count == 0 {
            continue;
        }
        let consistency = *freq as f64 / sample.len() as f64;
        let ncols = *common_count + 1;
        if ncols < 2 || consistency < min_consistency {
            continue;
        }
        if looks_like_prose(&sample, delim) {
            continue;
        }
        let confidence = (0.5 + consistency * 0.3 + (ncols.min(5) as f64) * 0.03).min(0.95);
        if best.as_ref().map_or(true, |b| confidence > b.confidence) {
            let mut meta = serde_json::Map::new();
            meta.insert("format".to_string(), Value::String("csv".to_string()));
            meta.insert("delimiter".to_string(), Value::String(delim.to_string()));
            meta.insert("columns".to_string(), json!(ncols));
            best = Some(DetectionResult {
                content_type: ContentType::Tabular,
                confidence,
                metadata: meta,
            });
        }
    }
    best
}

fn try_detect_tabular(content: &str) -> Option<DetectionResult> {
    let lines: Vec<&str> = content
        .lines()
        .filter(|ln| !ln.trim().is_empty())
        .take(50)
        .collect();
    if lines.len() < 3 {
        return None;
    }
    if let Some(r) = try_detect_markdown_table(&lines) {
        return Some(r);
    }
    try_detect_delimited(&lines)
}

fn try_detect_code(content: &str) -> Option<DetectionResult> {
    let lines: Vec<&str> = content.split('\n').take(100).collect();
    if lines.is_empty() {
        return None;
    }
    // Track scores in **first-match insertion order** to mirror Python's
    // dict semantics. Python:
    //
    //   language_scores: dict[str, int] = {}
    //   ...
    //   best_lang = max(language_scores, key=lambda k: language_scores[k])
    //
    // - Languages are inserted into the dict the first time they match a
    //   line, so the dict's iteration order is the order languages first
    //   showed up — NOT registration order.
    // - `max(...)` returns the FIRST element with the maximum value when
    //   multiple keys tie, per the language spec.
    //
    // We replicate both with a Vec and a manual `find(score == max)` for
    // the first-on-tie tie-break (Rust's `max_by` returns LAST on ties).
    let mut language_scores: Vec<(&'static str, u32)> = Vec::new();

    for line in &lines {
        for cp in CODE_PATTERNS.iter() {
            for pattern in &cp.patterns {
                if pattern.is_match(line) {
                    if let Some(entry) = language_scores.iter_mut().find(|(n, _)| *n == cp.name) {
                        entry.1 += 1;
                    } else {
                        language_scores.push((cp.name, 1));
                    }
                    break;
                }
            }
        }
    }

    if language_scores.is_empty() {
        return None;
    }
    let max_score = language_scores.iter().map(|x| x.1).max().unwrap_or(0);
    let (best_lang, best_score) = *language_scores
        .iter()
        .find(|x| x.1 == max_score)
        .expect("language_scores non-empty");
    if best_score < 3 {
        return None;
    }
    let non_empty_lines = lines.iter().filter(|l| !l.trim().is_empty()).count() as u32;
    let ratio = best_score as f64 / non_empty_lines.max(1) as f64;
    let confidence = (0.4 + ratio * 0.4 + (best_score as f64) * 0.02).min(1.0);
    Some(DetectionResult::new(
        ContentType::SourceCode,
        confidence,
        json!({
            "language": best_lang,
            "pattern_matches": best_score,
        })
        .as_object()
        .cloned()
        .unwrap(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_plain_text_zero_confidence() {
        let r = detect_content_type("");
        assert_eq!(r.content_type, ContentType::PlainText);
        assert_eq!(r.confidence, 0.0);
    }

    #[test]
    fn whitespace_only_returns_plain_text_zero_confidence() {
        let r = detect_content_type("   \n\t  ");
        assert_eq!(r.content_type, ContentType::PlainText);
        assert_eq!(r.confidence, 0.0);
    }

    #[test]
    fn json_array_of_dicts_high_confidence() {
        let r = detect_content_type(r#"[{"id": 1}, {"id": 2}]"#);
        assert_eq!(r.content_type, ContentType::JsonArray);
        assert_eq!(r.confidence, 1.0);
        assert_eq!(
            r.metadata.get("is_dict_array").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(r.metadata.get("item_count").unwrap().as_u64(), Some(2));
    }

    #[test]
    fn json_array_of_scalars_lower_confidence() {
        let r = detect_content_type(r#"[1, 2, 3]"#);
        assert_eq!(r.content_type, ContentType::JsonArray);
        assert_eq!(r.confidence, 0.8);
        assert_eq!(
            r.metadata.get("is_dict_array").unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn empty_json_array_not_dict_array() {
        let r = detect_content_type("[]");
        assert_eq!(r.content_type, ContentType::JsonArray);
        assert_eq!(r.confidence, 0.8);
        assert_eq!(
            r.metadata.get("is_dict_array").unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn json_object_falls_through_to_text() {
        // Detector only handles arrays.
        let r = detect_content_type(r#"{"id": 1}"#);
        assert_eq!(r.content_type, ContentType::PlainText);
    }

    #[test]
    fn search_results_detected() {
        let content =
            "src/main.py:42:def process():\nsrc/util.py:13:    return None\nlib/x.py:7:class X:";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SearchResults);
        assert!(r.confidence >= 0.6);
    }

    #[test]
    fn git_diff_detected() {
        let content = "\
diff --git a/foo.py b/foo.py
--- a/foo.py
+++ b/foo.py
@@ -1,3 +1,4 @@
 def hello():
-    print('hi')
+    print('hello')
+    print('world')
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::GitDiff);
        assert!(r.confidence >= 0.7);
    }

    #[test]
    fn html_doctype_detected() {
        let content = "\
<!DOCTYPE html>
<html>
<head><title>X</title></head>
<body><div>hi</div></body>
</html>";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::Html);
        assert!(r.confidence >= 0.7);
    }

    #[test]
    fn build_output_detected() {
        let content = "\
[INFO] Starting build
[INFO] Compiling 42 sources
[ERROR] Compilation failed
[WARN] Deprecated API
FAILED test_one
PASSED test_two
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::BuildOutput);
        assert!(r.confidence >= 0.5);
    }

    #[test]
    fn python_code_detected() {
        let content = "\
import os
from typing import Any

def process(data):
    return data

class Service:
    def __init__(self):
        pass

    @property
    def x(self):
        return 1

if __name__ == '__main__':
    process({})
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("python"));
    }

    #[test]
    fn php_code_detected() {
        // Without a `php` entry here, raw PHP fell through to plain text and
        // never reached the code-aware route.
        let content = "\
<?php

namespace Acme\\Widgets;

use Acme\\Support\\Logger;

class WidgetService
{
    private $logger;

    public function process(int $input): int
    {
        return $input + 1;
    }

    public function describe(): string
    {
        return 'widget';
    }
}
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("php"));
        // Python reports 0.6333… on this exact input.
        assert!(
            (r.confidence - 0.633_333_333_333_333_3).abs() < 1e-9,
            "got {}",
            r.confidence
        );
    }

    #[test]
    fn rust_code_detected() {
        let content = "\
use std::sync::Arc;

#[derive(Debug)]
pub struct Foo {
    bar: u32,
}

pub fn baz() -> u32 {
    42
}

impl Foo {
    pub fn new() -> Self {
        Self { bar: 0 }
    }
}
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("rust"));
    }

    #[test]
    fn go_code_detected() {
        let content = "\
package main

import \"fmt\"

func main() {
    fmt.Println(\"hello\")
}

type Service struct{}

func (s *Service) Do() {}

func helper() {}
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("go"));
    }

    #[test]
    fn fallback_to_plain_text() {
        let content = "Just some random text without any special structure.";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::PlainText);
        assert_eq!(r.confidence, 0.5);
    }

    #[test]
    fn is_json_array_of_dicts_true_path() {
        assert!(is_json_array_of_dicts(r#"[{"a": 1}, {"a": 2}]"#));
    }

    #[test]
    fn is_json_array_of_dicts_scalars_returns_false() {
        assert!(!is_json_array_of_dicts(r#"[1, 2, 3]"#));
    }

    #[test]
    fn is_json_array_of_dicts_object_returns_false() {
        assert!(!is_json_array_of_dicts(r#"{"a": 1}"#));
    }

    #[test]
    fn is_json_array_of_dicts_empty_returns_false() {
        // Empty array is JsonArray but not is_dict_array.
        assert!(!is_json_array_of_dicts("[]"));
    }

    #[test]
    fn diff_low_confidence_does_not_short_circuit() {
        // Single header with no change lines yields 0.7 — borderline.
        // Should still register as diff (>= 0.7 threshold).
        let content = "diff --git a/x b/x\n";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::GitDiff);
    }

    #[test]
    fn html_below_threshold_falls_through() {
        // Just one structural tag — not enough.
        let r = detect_content_type("<div>hello</div>");
        assert_ne!(r.content_type, ContentType::Html);
    }

    #[test]
    fn content_type_string_tags_match_python() {
        assert_eq!(ContentType::JsonArray.as_str(), "json_array");
        assert_eq!(ContentType::SourceCode.as_str(), "source_code");
        assert_eq!(ContentType::SearchResults.as_str(), "search");
        assert_eq!(ContentType::BuildOutput.as_str(), "build");
        assert_eq!(ContentType::GitDiff.as_str(), "diff");
        assert_eq!(ContentType::Html.as_str(), "html");
        assert_eq!(ContentType::PlainText.as_str(), "text");
    }

    // --- Space-separated JSON objects (parity: 5194bdc5) ---

    #[test]
    fn space_separated_json_objects_detected_as_array() {
        let content = r#"{"title": "Result 1", "url": "a"} {"title": "Result 2", "url": "b"} {"title": "Result 3", "url": "c"}"#;
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::JsonArray);
        assert_eq!(r.confidence, 1.0);
        assert_eq!(r.metadata.get("concatenated"), Some(&json!(true)));
        assert_eq!(r.metadata.get("item_count"), Some(&json!(3)));
    }

    #[test]
    fn newline_separated_json_objects_detected() {
        let content = "{\"a\": 1}\n{\"b\": 2}";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::JsonArray);
        assert_eq!(r.metadata.get("concatenated"), Some(&json!(true)));
    }

    #[test]
    fn single_json_object_not_treated_as_array() {
        // A single object isn't a >=2 run — must not be JSON_ARRAY.
        let r = detect_content_type(r#"{"only": "one"}"#);
        assert_ne!(r.content_type, ContentType::JsonArray);
    }

    #[test]
    fn normalize_concatenated_json_builds_array() {
        let content = r#"{"a": 1} {"b": 2}"#;
        let out = normalize_concatenated_json(content).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn normalize_concatenated_json_rejects_non_object_run() {
        assert!(normalize_concatenated_json("[1, 2, 3]").is_none());
        assert!(normalize_concatenated_json(r#"{"only": 1}"#).is_none());
        assert!(normalize_concatenated_json("not json").is_none());
    }

    // ─── Structured config detection (upstream addition) ─────────────────
    //
    // Expected values were produced by running Python's `detect_content_type`
    // on these exact inputs. Detection heuristics are where a port drifts
    // silently, so the type, the confidence, and the metadata are all pinned.

    const TOML: &str = "[package]\nname = \"headroom\"\nversion = \"1.0\"\nedition = \"2021\"\n\n[dependencies]\nserde = \"1\"\nregex = \"1\"\n";
    const INI: &str =
        "[server]\nhost = localhost\nport = 8080\ntimeout = 30\n\n[client]\nretries = 3\n";
    const YAML: &str =
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  labels:\n    app: web\nspec:\n  replicas: 3\n";
    const YAML_LIST: &str =
        "servers:\n  - name: a\n  - name: b\n  - name: c\nport: 80\ndebug: true\n";

    #[test]
    fn detects_toml_matching_python() {
        let r = detect_content_type(TOML);
        assert_eq!(r.content_type, ContentType::StructuredConfig);
        assert!((r.confidence - 0.95).abs() < 1e-9, "conf {}", r.confidence);
        assert_eq!(r.metadata["flavor"], json!("toml"));
        assert_eq!(r.metadata["sections"], json!(2));
        assert_eq!(r.metadata["assignments"], json!(5));
    }

    #[test]
    fn detects_ini_matching_python() {
        let r = detect_content_type(INI);
        assert_eq!(r.content_type, ContentType::StructuredConfig);
        assert!((r.confidence - 0.95).abs() < 1e-9, "conf {}", r.confidence);
        assert_eq!(r.metadata["flavor"], json!("ini"));
        assert_eq!(r.metadata["sections"], json!(2));
        assert_eq!(r.metadata["assignments"], json!(4));
    }

    #[test]
    fn detects_yaml_matching_python() {
        for (src, keys) in [(YAML, 8), (YAML_LIST, 6)] {
            let r = detect_content_type(src);
            assert_eq!(r.content_type, ContentType::StructuredConfig);
            assert!((r.confidence - 0.9).abs() < 1e-9, "conf {}", r.confidence);
            assert_eq!(r.metadata["flavor"], json!("yaml"));
            assert_eq!(r.metadata["keys"], json!(keys));
            assert_eq!(r.metadata["list_items"], json!(0));
        }
    }

    #[test]
    fn prose_and_code_are_not_claimed_as_config() {
        // The prose guards (sentence-enders, average word count) exist so a
        // paragraph never gets routed to the config fold.
        let prose = "This is a sentence about things. Here is another one. And a third sentence follows.\nIt continues on. More prose here.\n";
        assert_eq!(
            detect_content_type(prose).content_type,
            ContentType::PlainText
        );
        let code = "def foo():\n    return 1\n\nclass Bar:\n    pass\n";
        assert_eq!(
            detect_content_type(code).content_type,
            ContentType::PlainText
        );
    }

    #[test]
    fn markdown_front_matter_is_not_yaml() {
        // A `---` fence closed early and followed by prose is a markdown
        // document, not a standalone YAML config.
        let md = "---\ntitle: Post\nauthor: me\n---\n\nThis is the body of the post with real prose in it.\nMore paragraphs follow here naturally.\nAnd yet more text that is clearly not config.\n";
        assert_eq!(detect_content_type(md).content_type, ContentType::PlainText);
    }

    #[test]
    fn json_objects_are_never_config() {
        // Scoped to what config detection guarantees: a `{`-headed body is
        // rejected before any config heuristic runs.
        //
        // NOTE: Python classifies this as `json_array` with `is_object: true`,
        // while Rust's `try_detect_json` returns PlainText. That divergence
        // pre-dates this change and is unrelated to config detection, so it is
        // deliberately not asserted here.
        assert!(try_detect_structured_config(r#"{"a": 1, "b": 2, "c": 3}"#).is_none());
        assert_ne!(
            detect_content_type(r#"{"a": 1, "b": 2, "c": 3}"#).content_type,
            ContentType::StructuredConfig
        );
    }

    #[test]
    fn config_needs_a_real_parse_to_be_claimed() {
        // Section-shaped but neither parser accepts it: must fall through
        // rather than be claimed with a fabricated flavor.
        assert!(!parses_as_ini("value = 1\n[section]\nk = v\n"));
        // A bare `v` is not a valid TOML value, so TOML rejects it and INI
        // claims it — same answer Python's tomllib/configparser pair gives.
        assert_eq!(parse_config_flavor("[a]\nk = v\n"), Some("ini"));
        // Quoting the value makes it valid TOML, and TOML is tried first.
        assert_eq!(parse_config_flavor("[a]\nk = \"v\"\n"), Some("toml"));
        assert!(parse_config_flavor(&"x".repeat(1_000_001)).is_none());
    }
}

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
//! `upstream-python/tests/parity/fixtures/content_detector/` lock the output across
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

/// `path-NN-content` shape of `grep -A`/`-B`/`-C` context lines (upstream
/// #3599). The path group is non-greedy so the earliest `-digits-` marker
/// wins, mirroring the search parser that anchors on the first line-number
/// marker in the line.
static GREP_CONTEXT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^\s:]+?)-(\d+)-").unwrap());

/// Same idea with the path/line separator left as `:`:
/// `path:NN-content`. Real GNU grep emits dashes in both positions, but the
/// reported repro builds context lines this way, so both shapes must route
/// identically.
static GREP_COLON_DASH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^\s:]+:(\d+)-").unwrap());

/// Shared path-shape guard for every grep-line shape.
///
/// Rules out markup tags and `key=value` log prefixes; a single helper so
/// the three shapes cannot drift apart.
fn prefix_looks_like_path(prefix: &str) -> bool {
    !prefix.contains('<') && !prefix.contains('>') && !prefix.contains('=')
}

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
                // Short variable declarations (`x := f()`) are the only
                // signal in headerless fragments — tool output rarely shows
                // the file top. `:=` at line start is vanishingly rare in
                // prose, and the >= 3-hit gate below absorbs strays.
                // Measured 2026-09-18 over captured bodies: fixes Go
                // fragments, zero moves on agreement blocks.
                Regex::new(r"^\s*\w[\w.]*\s*:=").unwrap(),
            ],
        },
        CodePatterns {
            // SQL had no entry at all: query blocks fell to text even with
            // SELECT/FROM/WHERE on consecutive lines. Uppercase-anchored so
            // prose ("from the docs…") never matches. Measured 2026-09-18:
            // fixes query blocks (including ones Magika also missed);
            // apparent "regressions" were all real SQL under a prose header.
            name: "sql",
            patterns: vec![
                Regex::new(r"^\s*(SELECT|WITH|INSERT\s+INTO|UPDATE|DELETE\s+FROM)\b").unwrap(),
                Regex::new(r"^\s*(FROM|WHERE|JOIN|GROUP\s+BY|ORDER\s+BY|HAVING|LIMIT)\b").unwrap(),
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

// ─── Line-number prefixes ──────────────────────────────────────────
//
// Tool output routinely prefixes code with line numbers (`1\tpackage …`
// from grep -n / reviewers, `1 │ …` from renderers). The tabular stage
// reads those as consistent TSV columns and the start-anchored code
// patterns never see the keywords. Measured 2026-09-18 over captured
// bodies (`upstream-python/bench/_detect_miss_probes.py`): tolerating the
// prefix in the CODE view only fixes line-numbered Go/Rust/Python while a
// whole-text strip moved prose — so the strip applies to code matching
// (and the tabular guard below), nowhere else.
static LINE_NUM_TAB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+\t").expect("valid"));
static LINE_NUM_BAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+\s*[│|]\s?").expect("valid"));

/// Detection-time view for CODE patterns: strip one `N<TAB>` / `N │`
/// line-number prefix. Returns the raw line when no prefix is present.
fn strip_line_number_prefix(line: &str) -> &str {
    for re in [&LINE_NUM_TAB_RE, &LINE_NUM_BAR_RE] {
        if let Some(m) = re.find(line) {
            debug_assert_eq!(m.start(), 0);
            return &line[m.end()..];
        }
    }
    line
}

// ─── Shell scripts ─────────────────────────────────────────────────
//
// Shell has no header keywords (`func`, `import`, …), so script bodies —
// especially after a tool-output header line like `Exit code 1` — fall to
// text. There is not even a shebang rule today. A bare `WORD=` pattern
// fires on analysis prose (`dob=both`, `name_sim=1.0`), so the rule is
// conjunctive: a shebang within the first 3 non-empty lines (tolerating
// the header) plus >= 2 shell-ish body lines. Measured 2026-09-18: fixes
// shebang scripts, zero moves on agreement blocks.
static SHELL_BODY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*[A-Za-z_]\w*=\S|^\s*(cd|export|exit|exec|source)\b|\$\(|^\s*(if|then|fi|for|while|do|done|case|esac|function)\b",
    )
    .expect("valid")
});

/// Conjunctive shell check; see [`SHELL_BODY_RE`]. Returns SourceCode
/// with language "shell", or None.
fn try_detect_shell(lines: &[&str]) -> Option<DetectionResult> {
    let nonempty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .take(100)
        .collect();
    // Shebang tolerates a tool-output header above it, but must be
    // unindented: `line.starts_with`, mirroring the measured rule.
    if !nonempty.iter().take(3).any(|l| l.starts_with("#!")) {
        return None;
    }
    let hits = nonempty
        .iter()
        .filter(|l| SHELL_BODY_RE.is_match(l))
        .count();
    if hits < 2 {
        return None;
    }
    Some(DetectionResult::new(
        ContentType::SourceCode,
        0.7,
        json!({
            "language": "shell",
            "pattern_matches": hits,
        })
        .as_object()
        .cloned()
        .unwrap(),
    ))
}

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
/// 7. Tabular (≥ 0.6 confidence required; runs after search/log so those claim content first)
/// 8. Structured config (≥ 0.6 confidence required; runs after tabular, before code)
/// 9. Source code (≥ 0.5 confidence required)
/// 10. Fallback to `PlainText` confidence 0.5
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

/// True when a line looks like `path:line:content` grep output.
///
/// The bare `^[^\s:]+:\d+:` shape also matches ISO-8601 timestamps
/// (`…T09:57:59…`) and the XML-ish wrappers harnesses prepend to user turns
/// (Copilot CLI's `<current_datetime>…` line), which misroutes prose to the
/// SearchCompressor — and that compressor keeps only matching lines, deleting
/// the rest. So the pre-colon segment must additionally look like a file path:
/// no angle brackets and no `=` (rules out markup tags and `key=value:12:` log
/// lines).
///
/// `grep -A`/`-B`/`-C` context lines (`path-NN-content` and the reported
/// `path:NN-content` shape) count too (upstream #3599): without this branch
/// those lines read as prose and code in them reaches the word-dropping
/// Kompress compressor. The colon branch runs first; context is tried after.
fn is_search_result_line(line: &str) -> bool {
    if SEARCH_RESULT_PATTERN.is_match(line) {
        let prefix = line.split(':').next().unwrap_or("");
        return prefix_looks_like_path(prefix);
    }
    if is_grep_colon_dash_line(line) {
        return true;
    }
    is_grep_context_line(line)
}

/// True when a line looks like `path:NN-content` grep context output.
///
/// Same idea as [`is_grep_context_line`] with the path/line separator left
/// as `:`. Real GNU grep emits dashes in both positions, but the reported
/// repro builds context lines this way, so both shapes must route
/// identically (upstream #3599). Split out so the section splitter in
/// `content_router` carves the same lines the detector claims.
pub(crate) fn is_grep_colon_dash_line(line: &str) -> bool {
    if !GREP_COLON_DASH_PATTERN.is_match(line) {
        return false;
    }
    let prefix = line.split(':').next().unwrap_or("");
    prefix_looks_like_path(prefix)
}

/// True when a line looks like `path-NN-content` grep context output.
///
/// GNU grep (and ripgrep / git grep) separate `-A`/`-B`/`-C` context lines
/// with `-` where match lines use `:`. The prefix must additionally look
/// like a file path: the same `</>=` exclusions as the colon branch, plus
/// it must contain `/` or `.` — which keeps dates (`2026-09-14`) and dashed
/// prose (`version-2-release`) out while accepting real paths, including
/// dashed names (`my-file.py`).
pub(crate) fn is_grep_context_line(line: &str) -> bool {
    let Some(caps) = GREP_CONTEXT_PATTERN.captures(line) else {
        return false;
    };
    let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    if !prefix_looks_like_path(prefix) {
        return false;
    }
    prefix.contains('/') || prefix.contains('.')
}

pub fn try_detect_search(content: &str) -> Option<DetectionResult> {
    let lines: Vec<&str> = content.split('\n').take(100).collect();
    if lines.is_empty() {
        return None;
    }
    let mut matching_lines: u32 = 0;
    for line in &lines {
        if !line.trim().is_empty() && is_search_result_line(line) {
            matching_lines += 1;
        }
    }
    // Absolute floor: a single coincidental `word:digits:` line (a timestamp,
    // a URL, a time literal inside prose) must not classify a whole payload as
    // search results — the SearchCompressor drops every non-matching line, so
    // a false positive is data loss. A genuine one-line grep result loses
    // nothing by staying uncompressed: all of its lines match, so the
    // compressor would have kept it verbatim anyway.
    if matching_lines < 2 {
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

/// True when every sampled row's first tab field is an integer and the
/// integers strictly increase down the rows: `grep -n` / reviewer line
/// numbers, not a data column. Accepted false negative: a genuine TSV
/// whose id column happens to run 1,2,3… — none appeared in 800+
/// agreement blocks over captured bodies, and search/log/config claim
/// their content before tabular runs.
fn first_col_sequential_ints(sample: &[&str]) -> bool {
    if sample.len() < 3 {
        return false;
    }
    let mut prev: Option<u64> = None;
    for row in sample {
        if !row.contains('\t') {
            return false;
        }
        let first = row.split('\t').next().map(str::trim).unwrap_or("");
        let n: u64 = match first.parse() {
            Ok(n) => n,
            Err(_) => return false,
        };
        if prev.is_some_and(|p| n <= p) {
            return false;
        }
        prev = Some(n);
    }
    true
}

fn looks_like_prose(sample: &[&str], delim: &str) -> bool {
    let enders = sample
        .iter()
        .filter(|r| {
            let trimmed = r.trim_end();
            trimmed.ends_with('.') || trimmed.ends_with('!') || trimmed.ends_with('?')
        })
        .count();
    if !sample.is_empty() && enders as f64 / sample.len() as f64 >= 0.5 {
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
        // `N<TAB>code` excerpts are column-consistent TSV to a counter but
        // line numbers to a reader; the code stage tolerates the prefix, so
        // yield the tab candidacy when the first column is a strictly
        // increasing integer run. Other delimiters are untouched:
        // comma/semicolon id columns are real data.
        if delim == "\t" && first_col_sequential_ints(&sample) {
            continue;
        }
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
        if best.as_ref().is_none_or(|b| confidence > b.confidence) {
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
    // Shell scripts have no header keywords for the pattern table below;
    // the conjunctive shebang rule runs first so they are not hostage to
    // it (config/INI-looking assignments still route to config — that
    // stage runs before code in the orchestrator).
    if let Some(r) = try_detect_shell(&lines) {
        return Some(r);
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

    // Match against the line-number-tolerant view: a `17\t` / `17 │`
    // prefix is tooling, not content. Tabular/search/log saw the raw lines
    // in their own earlier stages; only code matching looks through it.
    let viewed: Vec<&str> = lines.iter().map(|l| strip_line_number_prefix(l)).collect();
    for line in &viewed {
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
    let non_empty_lines = viewed.iter().filter(|l| !l.trim().is_empty()).count() as u32;
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

    /// Upstream #3599: `grep -A`/`-B`/`-C` context lines must classify as
    /// search results, not prose — code in them reached the word-dropping
    /// Kompress compressor. Both the real GNU shape (`path-NN-content`)
    /// and the reported `path:NN-content` shape route identically.
    #[test]
    fn grep_context_lines_detected() {
        // Context-heavy -C output: 2 match lines in 10 would read as
        // 20% < the 30% floor without the context branch, routing code
        // to the word-dropping Kompress compressor.
        let content = "src/main.py:42:def process():\n\
                       src/main.py-38-# helper\n\
                       src/main.py-39-# more context\n\
                       src/main.py-40-x = 1\n\
                       src/main.py-41-y = 2\n\
                       src/main.py-43-    result = compute()\n\
                       src/main.py-44-    return result\n\
                       src/main.py-45-\n\
                       src/main.py-46-# trailing\n\
                       src/util.py:13:    return None";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SearchResults);
    }

    #[test]
    fn grep_colon_dash_context_lines_detected() {
        // Same floor logic for the reported `path:NN-content` shape: a
        // lone match line in context would otherwise stay below it.
        let content = "src/main.py:42:def process():\n\
                       src/main.py:43-    result = compute()\n\
                       src/main.py:44-    return result\n\
                       src/main.py:45-    x = 1\n\
                       src/main.py:46-    y = 2\n\
                       src/main.py:47-    return x";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SearchResults);
    }

    #[test]
    fn grep_context_predicate_rejects_dates_and_dashed_prose() {
        // `2026-09-14` parses as path `2026`, line `09` — but the prefix
        // has neither `/` nor `.`, so it stays out. Same for dashed prose
        // and markup/log prefixes.
        assert!(!is_grep_context_line("2026-09-14 release notes"));
        assert!(!is_grep_context_line("version-2-release is out"));
        assert!(!is_grep_context_line("<li-2-highlighted text"));
        assert!(!is_grep_context_line("timeout=30-12-retried"));
        // Real paths route, including dashed names.
        assert!(is_grep_context_line(
            "src/main.py-43-    result = compute()"
        ));
        assert!(is_grep_context_line("my-file.py-7-class Worker:"));
    }

    /// Regression: wrap-copilot ate one-line interactive prompts (2026-08-23).
    /// Copilot CLI prepends `<current_datetime>…</current_datetime>` to every
    /// interactive turn. That single line matches the bare `file:line:`
    /// pattern, so a datetime plus a one-line prompt classified as search
    /// results (1 match / 2 lines = 50% ≥ 30%) and the SearchCompressor
    /// deleted the prompt — the model received only the timestamp.
    #[test]
    fn a_datetime_prefixed_user_message_is_not_search_output() {
        let incident = "<current_datetime>2026-08-23T09:57:59.792+02:00</current_datetime>\n\n\
                        Please update the PR desc and check .overlay/ for hints.";
        assert!(try_detect_search(incident).is_none());
        assert_ne!(
            detect_content_type(incident).content_type,
            ContentType::SearchResults
        );
    }

    #[test]
    fn one_coincidental_line_is_not_enough() {
        assert!(try_detect_search("src/foo.py:12:def foo():").is_none());
        assert!(try_detect_search(
            "Meeting at 09:30:00 tomorrow.\nBring the reports.\nDo not forget coffee."
        )
        .is_none());

        let two = "src/foo.py:12:def foo():\nsrc/bar.py:34:    foo()";
        let r = try_detect_search(two).expect("two genuine grep lines still classify");
        assert_eq!(r.content_type, ContentType::SearchResults);
    }

    /// Markup and `key=value` lines are not file paths, even carrying `:\d+:`.
    #[test]
    fn tag_like_and_key_value_prefixes_are_rejected() {
        assert!(try_detect_search(
            "<log time=\"10:00:00\">started</log>\n<log time=\"10:00:01\">stopped</log>"
        )
        .is_none());
        assert!(try_detect_search("timeout=30:12:retried\ntimeout=31:12:retried").is_none());
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
    fn go_assign_fragment_detected() {
        // Headerless fragments carry no `func`/`package` in view; `:=` is
        // the only signal. (Corpus case: tool output starting mid-function.)
        let content = "\
entryA := matching.LineupPlayerEntry{Provider: pA}
evEntryA := matching.EventsPlayerEntry{Provider: pA}
normA := NormalizePlayerName(a.FirstName, a.LastName)
for ib, b := range bs {
\tvar lineupFeat matching.LineupPairFeatures
\tif lineupIdx != nil {
\t\teventsFeat = sig.Events.PairFeatures(
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("go"));
    }

    #[test]
    fn sql_block_detected() {
        // SQL had no entry: query blocks fell to text.
        let content = "\
WITH team_members AS (
  SELECT m.group_id, m.provider FROM members m
  JOIN groups g ON g.id = m.group_id
  WHERE m.type = 'team'
)
SELECT group_id FROM team_members GROUP BY group_id
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("sql"));
    }

    #[test]
    fn shell_shebang_offset_detected() {
        // Shebang tolerates one tool-output header line above it.
        let content = "\
Exit code 1
#!/bin/bash
# read-only query helper
cd /home/ruben/meta
PW=$(grep -A5 'x' .env | head -1)
PGPASSWORD=\"$PW\" psql -h example.com
echo done
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("shell"));
    }

    #[test]
    fn shell_without_shebang_stays_text() {
        // The conjunctive rule needs the shebang: `WORD=` lines alone
        // (analysis prose is full of `dob=both`) must never fire it.
        let content = "\
# read-only query helper
cd /home/ruben/meta
PW=$(grep -A5 'x' .env | head -1)
PGPASSWORD=\"$PW\" psql -h example.com
echo done
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::PlainText);
    }

    #[test]
    fn prose_with_equals_stays_text() {
        // `dob=both`, `name_sim=1.0`: bare `WORD=` is prose, not shell.
        let content = "\
pairs with BYTE-IDENTICAL full names: 704
  dob=both     n= 689
  surname_sim >= 0.9: 867/969 = 89.5%
  CONFIRMED SAME PERSON (merged, both present and equal): 969
  CLAIM: one hundred percent of merges involve enetpulse
";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::PlainText);
    }

    #[test]
    fn line_numbered_go_detected() {
        // `N<TAB>` prefixes are tooling; the keywords underneath count.
        let content = "1\tpackage matching\n2\t\n3\timport (\n4\t\t\"strings\"\n5\t)\n6\t\n7\tfunc Compare(a string) string {\n8\t\treturn strings.ToUpper(a)\n9\t}\n";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("go"));
    }

    #[test]
    fn pipe_bar_numbered_code_detected() {
        let content = "1 │ package matching\n2 │\n3 │ import (\n4 │ \t\"strings\"\n5 │ )\n6 │\n7 │ func Compare(a string) string {\n8 │ \treturn a\n9 │ }\n";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::SourceCode);
        assert_eq!(r.metadata.get("language").unwrap().as_str(), Some("go"));
    }

    #[test]
    fn numbered_list_stays_text() {
        // The prefix strip must not conjure code from prose.
        let content = "1\tBuy milk\n2\tWalk the dog\n3\tCall mom about dinner\n4\tFinish the quarterly report draft\n";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::PlainText);
    }

    #[test]
    fn line_numbered_code_not_tabular() {
        // Sequential-int first column is line numbers, not data: the tab
        // candidacy yields so code (or text) wins over a TSV misroute.
        let content = "1\tfoo(\n2\tbar(\n3\tbaz(\n4\tqux(\n";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::PlainText);
    }

    #[test]
    fn non_sequential_int_first_col_stays_tabular() {
        // The guard only fires on *increasing* runs: a real id column
        // routes tabular as before.
        let content = "5\tapple\n3\tbanana\n9\tcherry\n7\tdate\n";
        let r = detect_content_type(content);
        assert_eq!(r.content_type, ContentType::Tabular);
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

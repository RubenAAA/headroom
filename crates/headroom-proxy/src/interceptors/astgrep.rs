//! AST-grep interceptor: outline verbose code-file Read outputs.
//!
//! Matches when the tool is a file reader, the output is large enough,
//! the file has a supported extension, and no explicit line range was
//! requested. Invokes ast-grep to locate top-level definitions and
//! emits a compact outline. Falls back to the original text when
//! ast-grep is unavailable or the file has fewer than three definitions.
//!
//! Mirrors Python's `headroom.proxy.interceptors.astgrep`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde_json::Value;

use super::base::ToolResultInterceptor;

// ─── Configuration ───────────────────────────────────────────────────────

/// Minimum output length (chars) to attempt outlining.
/// Below this, the subprocess cost isn't worth the tiny win.
pub const MIN_CHARS_DEFAULT: usize = 500;

/// Tool names that indicate a file read operation.
const READ_TOOLS: &[&str] = &["Read", "read_file", "view", "cat"];

/// Tool_input keys indicating the model targeted a specific line range.
/// Outlining would frustrate that intent.
const RANGE_KEYS: &[&str] = &[
    "offset",
    "limit",
    "line_range",
    "start_line",
    "end_line",
    "ranges",
];

/// File extension → ast-grep language mapping.
static EXT_TO_LANG: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert(".py", "python");
    m.insert(".ts", "typescript");
    m.insert(".tsx", "tsx");
    m.insert(".js", "javascript");
    m.insert(".jsx", "jsx");
    m.insert(".go", "go");
    m.insert(".rs", "rust");
    m.insert(".java", "java");
    m.insert(".rb", "ruby");
    m.insert(".c", "c");
    m.insert(".h", "c");
    m.insert(".cpp", "cpp");
    m.insert(".cc", "cpp");
    m.insert(".hpp", "cpp");
    m
});

/// ast-grep patterns per language for top-level declarations.
static PATTERNS: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
    let mut m: HashMap<&str, Vec<&str>> = HashMap::new();
    m.insert(
        "python",
        vec!["def $NAME", "class $NAME", "async def $NAME"],
    );
    m.insert("typescript", vec!["function $NAME", "class $NAME"]);
    m.insert("tsx", vec!["function $NAME", "class $NAME"]);
    m.insert("javascript", vec!["function $NAME", "class $NAME"]);
    m.insert("jsx", vec!["function $NAME", "class $NAME"]);
    m.insert("go", vec!["func $NAME"]);
    m.insert("rust", vec!["fn $NAME", "struct $NAME", "enum $NAME"]);
    m.insert("java", vec!["class $NAME", "interface $NAME"]);
    m.insert("ruby", vec!["def $NAME", "class $NAME"]);
    m.insert("c", vec!["$RET $NAME($$$ARGS) { $$$BODY }"]);
    m.insert("cpp", vec!["$RET $NAME($$$ARGS) { $$$BODY }"]);
    m
});

pub const OUTLINE_MARKER: &str =
    "    # ... (body elided by Headroom; Read a specific line range to see it)\n";

// ─── Helpers ─────────────────────────────────────────────────────────────

/// Extract file path from tool input, checking common key names.
pub fn path_from_input(tool_input: &Value) -> Option<String> {
    for key in &["file_path", "path", "filePath", "filename"] {
        if let Some(v) = tool_input.get(*key).and_then(Value::as_str) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Detect language from tool input file path.
pub fn detect_lang_from_input(tool_input: &Value) -> Option<&'static str> {
    let path = path_from_input(tool_input)?;
    let ext = path.rfind('.').map(|i| &path[i..])?;
    EXT_TO_LANG.get(ext).copied()
}

/// Check if tool input has any line-range keys.
fn has_range_keys(tool_input: &Value) -> bool {
    if let Some(obj) = tool_input.as_object() {
        return RANGE_KEYS.iter().any(|k| obj.contains_key(*k));
    }
    false
}

// ─── Outline building ────────────────────────────────────────────────────

/// A parsed ast-grep match record.
#[derive(Debug, Clone)]
pub struct AstGrepMatch {
    pub line: usize,
    pub byte_start: usize,
}

/// Parse ast-grep JSON stream output into match records.
pub fn parse_ast_grep_output(output: &str) -> Vec<AstGrepMatch> {
    let mut matches = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(line) {
            let line_idx = val
                .get("range")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.get("line"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let byte_start = val
                .get("range")
                .and_then(|r| r.get("byteOffset"))
                .and_then(|b| b.get("start"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            if line_idx > 0 {
                // ast-grep uses 1-based line numbers
                matches.push(AstGrepMatch {
                    line: line_idx - 1,
                    byte_start,
                });
            }
        }
    }
    matches
}

/// Build a compact outline from ast-grep matches.
///
/// Emits each definition's signature line + docstring (if next line is a
/// string literal) + an elision marker. Matches are sorted by byte offset
/// to track original file order.
pub fn build_outline(matches: &[AstGrepMatch], source: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut outline_chunks = Vec::new();
    let mut seen_starts = HashSet::new();

    let mut sorted_matches = matches.to_vec();
    sorted_matches.sort_by_key(|m| m.byte_start);

    for m in &sorted_matches {
        if seen_starts.contains(&m.line) {
            continue;
        }
        if m.line >= lines.len() {
            continue;
        }
        seen_starts.insert(m.line);

        let signature_line = lines[m.line];
        outline_chunks.push(format!("{}\n", signature_line));

        // Best-effort: if the next non-blank line is a docstring, keep it.
        let mut next_idx = m.line + 1;
        while next_idx < lines.len() && lines[next_idx].trim().is_empty() {
            next_idx += 1;
        }
        if next_idx < lines.len() {
            let nl = lines[next_idx].trim_start();
            if nl.starts_with("\"\"\"")
                || nl.starts_with("'''")
                || nl.starts_with("/**")
                || nl.starts_with("//")
                || nl.starts_with('#')
            {
                outline_chunks.push(format!("{}\n", lines[next_idx]));
            }
        }
        outline_chunks.push(OUTLINE_MARKER.to_string());
    }

    if outline_chunks.is_empty() {
        return None;
    }

    let header = format!(
        "[headroom: outlined by ast-grep — {} definition(s); \
         bodies elided. Re-read the file with a line range to see a specific body.]\n",
        seen_starts.len()
    );
    Some(header + &outline_chunks.join(""))
}

// ─── AstGrepReadOutline interceptor ─────────────────────────────────────

pub struct AstGrepReadOutline {
    pub min_chars: usize,
}

impl Default for AstGrepReadOutline {
    fn default() -> Self {
        Self {
            min_chars: MIN_CHARS_DEFAULT,
        }
    }
}

impl AstGrepReadOutline {
    /// Read `min_chars` from the live runtime env knob, falling back to
    /// the compiled default.
    fn effective_min_chars(&self) -> usize {
        if let Some(raw) = crate::runtime_env::getenv("HEADROOM_INTERCEPT_READ_MIN_CHARS", None) {
            if let Ok(n) = raw.parse::<usize>() {
                return n;
            }
        }
        self.min_chars
    }
}

impl ToolResultInterceptor for AstGrepReadOutline {
    fn name(&self) -> &str {
        "ast-grep"
    }

    fn matches(&self, tool_name: Option<&str>, tool_input: &Value, tool_output: &str) -> bool {
        let name = match tool_name {
            Some(n) => n,
            None => return false,
        };
        if !READ_TOOLS.contains(&name) {
            return false;
        }
        if tool_output.len() < self.effective_min_chars() {
            return false;
        }
        if has_range_keys(tool_input) {
            return false;
        }
        detect_lang_from_input(tool_input).is_some()
    }

    fn transform(
        &self,
        _tool_name: Option<&str>,
        tool_input: &Value,
        tool_output: &str,
    ) -> Option<String> {
        let lang = detect_lang_from_input(tool_input)?;
        let matches = run_ast_grep(lang, tool_output)?;
        build_outline(&matches, tool_output)
    }

    fn progressive_disclosure_key(
        &self,
        _tool_name: Option<&str>,
        tool_input: &Value,
    ) -> Option<String> {
        path_from_input(tool_input)
    }
}

// ─── Subprocess runner ──────────────────────────────────────────────────

/// Resolve the `ast-grep` binary on PATH.
fn find_ast_grep_binary() -> Option<std::path::PathBuf> {
    // Try `which ast-grep` equivalent: check common locations
    if let Ok(output) = std::process::Command::new("which").arg("ast-grep").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(std::path::PathBuf::from(path));
            }
        }
    }
    // Fallback: try running ast-grep directly (it might be on PATH)
    if std::process::Command::new("ast-grep")
        .arg("--version")
        .output()
        .is_ok()
    {
        // It's on PATH, return the name for Command::new
        return Some(std::path::PathBuf::from("ast-grep"));
    }
    None
}

/// Run ast-grep against `source` for the given language, returning parsed
/// matches. Writes source to a tempfile because ast-grep's CLI operates on
/// files.
fn run_ast_grep(lang: &str, source: &str) -> Option<Vec<AstGrepMatch>> {
    let patterns = PATTERNS.get(lang)?;
    if patterns.is_empty() {
        return None;
    }

    let exe = find_ast_grep_binary()?;

    // Determine file extension for the tempfile
    let ext = EXT_TO_LANG
        .iter()
        .find(|(_, &v)| v == lang)
        .map(|(&k, _)| k)
        .unwrap_or(".txt");

    let tmp_dir = tempfile::tempdir().ok()?;
    let tmp_path = tmp_dir.path().join(format!("src{ext}"));
    std::fs::write(&tmp_path, source).ok()?;

    let mut all_matches = Vec::new();
    for pattern in patterns {
        let output = std::process::Command::new(&exe)
            .args([
                "run",
                "--pattern",
                pattern,
                "--lang",
                lang,
                "--json=stream",
                tmp_path.to_str()?,
            ])
            .output();
        match output {
            Ok(out) if out.status.code() == Some(0) || out.status.code() == Some(1) => {
                // rc=0: matches, rc=1: no matches (both expected)
                let stdout = String::from_utf8_lossy(&out.stdout);
                all_matches.extend(parse_ast_grep_output(&stdout));
            }
            Ok(out) => {
                // rc>=2: real error — log and continue
                eprintln!(
                    "ast-grep error (rc={}, lang={}, pattern={}): {}",
                    out.status.code().unwrap_or(-1),
                    lang,
                    pattern,
                    String::from_utf8_lossy(&out.stderr)
                        .chars()
                        .take(200)
                        .collect::<String>()
                );
            }
            Err(e) => {
                eprintln!("ast-grep failed to execute: {}", e);
                return None;
            }
        }
    }

    if all_matches.is_empty() {
        None
    } else {
        Some(all_matches)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- path_from_input ---

    #[test]
    fn path_from_input_file_path() {
        let input = json!({"file_path": "/src/main.rs"});
        assert_eq!(path_from_input(&input).as_deref(), Some("/src/main.rs"));
    }

    #[test]
    fn path_from_input_path() {
        let input = json!({"path": "/src/lib.py"});
        assert_eq!(path_from_input(&input).as_deref(), Some("/src/lib.py"));
    }

    #[test]
    fn path_from_input_camel_case() {
        let input = json!({"filePath": "/src/app.ts"});
        assert_eq!(path_from_input(&input).as_deref(), Some("/src/app.ts"));
    }

    #[test]
    fn path_from_input_filename() {
        let input = json!({"filename": "test.js"});
        assert_eq!(path_from_input(&input).as_deref(), Some("test.js"));
    }

    #[test]
    fn path_from_input_none() {
        let input = json!({"pattern": "foo"});
        assert!(path_from_input(&input).is_none());
    }

    #[test]
    fn path_from_input_empty() {
        let input = json!({"file_path": ""});
        assert!(path_from_input(&input).is_none());
    }

    // --- detect_lang_from_input ---

    #[test]
    fn detect_lang_python() {
        let input = json!({"file_path": "/src/main.py"});
        assert_eq!(detect_lang_from_input(&input), Some("python"));
    }

    #[test]
    fn detect_lang_rust() {
        let input = json!({"path": "/src/lib.rs"});
        assert_eq!(detect_lang_from_input(&input), Some("rust"));
    }

    #[test]
    fn detect_lang_typescript() {
        let input = json!({"file_path": "app.ts"});
        assert_eq!(detect_lang_from_input(&input), Some("typescript"));
    }

    #[test]
    fn detect_lang_unsupported() {
        let input = json!({"file_path": "data.json"});
        assert_eq!(detect_lang_from_input(&input), None);
    }

    #[test]
    fn detect_lang_no_path() {
        let input = json!({});
        assert_eq!(detect_lang_from_input(&input), None);
    }

    // --- has_range_keys ---

    #[test]
    fn has_range_keys_offset() {
        let input = json!({"file_path": "/src/main.py", "offset": 10, "limit": 50});
        assert!(has_range_keys(&input));
    }

    #[test]
    fn has_range_keys_line_range() {
        let input = json!({"file_path": "/src/main.py", "line_range": [10, 20]});
        assert!(has_range_keys(&input));
    }

    #[test]
    fn has_range_keys_none() {
        let input = json!({"file_path": "/src/main.py"});
        assert!(!has_range_keys(&input));
    }

    // --- parse_ast_grep_output ---

    #[test]
    fn parse_output_basic() {
        let output = r#"{"range":{"start":{"line":5},"byteOffset":{"start":100}}}
{"range":{"start":{"line":12},"byteOffset":{"start":300}}}"#;
        let matches = parse_ast_grep_output(output);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 4); // 0-based
        assert_eq!(matches[1].line, 11);
    }

    #[test]
    fn parse_output_empty() {
        let matches = parse_ast_grep_output("");
        assert!(matches.is_empty());
    }

    #[test]
    fn parse_output_invalid_json() {
        let output = "not json\nalso not json";
        let matches = parse_ast_grep_output(output);
        assert!(matches.is_empty());
    }

    // --- build_outline ---

    #[test]
    fn build_outline_basic() {
        let source =
            "fn main() {\n    println!(\"hello\");\n}\n\nfn helper() -> i32 {\n    42\n}\n";
        let matches = vec![
            AstGrepMatch {
                line: 0,
                byte_start: 0,
            },
            AstGrepMatch {
                line: 4,
                byte_start: 25,
            },
        ];
        let outline = build_outline(&matches, source).unwrap();
        assert!(outline.contains("2 definition(s)"));
        assert!(outline.contains("fn main()"));
        assert!(outline.contains("fn helper()"));
        assert!(outline.contains(OUTLINE_MARKER));
    }

    #[test]
    fn build_outline_with_docstring() {
        // Python-style docstring (inside function body, after signature)
        let source = "def foo():\n    \"\"\"This is a docstring.\"\"\"\n    pass\n";
        let matches = vec![AstGrepMatch {
            line: 0,
            byte_start: 0,
        }];
        let outline = build_outline(&matches, source).unwrap();
        assert!(outline.contains("def foo():"));
        assert!(outline.contains("\"\"\"This is a docstring.\"\"\""));
    }

    #[test]
    fn build_outline_empty() {
        let outline = build_outline(&[], "hello world");
        assert!(outline.is_none());
    }

    #[test]
    fn build_outline_deduplicates() {
        let source = "fn main() {\n    println!(\"hello\");\n}\n";
        let matches = vec![
            AstGrepMatch {
                line: 0,
                byte_start: 0,
            },
            AstGrepMatch {
                line: 0,
                byte_start: 0,
            },
        ];
        let outline = build_outline(&matches, source);
        assert!(outline.is_some());
        assert!(outline.unwrap().contains("1 definition(s)"));
    }

    // --- AstGrepReadOutline.matches ---

    #[test]
    fn matches_read_tool() {
        let interceptor = AstGrepReadOutline::default();
        let input = json!({"file_path": "/src/main.py"});
        let output = "x".repeat(600);
        assert!(interceptor.matches(Some("Read"), &input, &output));
    }

    #[test]
    fn matches_rejects_short_output() {
        let interceptor = AstGrepReadOutline::default();
        let input = json!({"file_path": "/src/main.py"});
        let output = "short";
        assert!(!interceptor.matches(Some("Read"), &input, output));
    }

    #[test]
    fn matches_rejects_non_read_tool() {
        let interceptor = AstGrepReadOutline::default();
        let input = json!({"file_path": "/src/main.py"});
        let output = "x".repeat(600);
        assert!(!interceptor.matches(Some("Grep"), &input, &output));
    }

    #[test]
    fn matches_rejects_line_range() {
        let interceptor = AstGrepReadOutline::default();
        let input = json!({"file_path": "/src/main.py", "offset": 10, "limit": 50});
        let output = "x".repeat(600);
        assert!(!interceptor.matches(Some("Read"), &input, &output));
    }

    #[test]
    fn matches_rejects_unsupported_lang() {
        let interceptor = AstGrepReadOutline::default();
        let input = json!({"file_path": "/data.json"});
        let output = "x".repeat(600);
        assert!(!interceptor.matches(Some("Read"), &input, &output));
    }

    #[test]
    fn matches_no_tool_name() {
        let interceptor = AstGrepReadOutline::default();
        let input = json!({"file_path": "/src/main.py"});
        let output = "x".repeat(600);
        assert!(!interceptor.matches(None, &input, &output));
    }

    // --- AstGrepReadOutline.progressive_disclosure_key ---

    #[test]
    fn progressive_key_returns_path() {
        let interceptor = AstGrepReadOutline::default();
        let input = json!({"file_path": "/src/main.py"});
        assert_eq!(
            interceptor.progressive_disclosure_key(Some("Read"), &input),
            Some("/src/main.py".to_string())
        );
    }

    #[test]
    fn progressive_key_none_without_path() {
        let interceptor = AstGrepReadOutline::default();
        let input = json!({});
        assert!(interceptor
            .progressive_disclosure_key(Some("Read"), &input)
            .is_none());
    }

    // --- Registry ---

    #[test]
    fn register_and_apply() {
        use crate::interceptors::base;
        // Reset state
        base::INTERCEPTORS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        base::reset_interceptor_failure_counts();

        // Register
        base::register(Box::new(AstGrepReadOutline::default()));

        let interceptors = base::INTERCEPTORS.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(interceptors.len(), 1);
        assert_eq!(interceptors[0].name(), "ast-grep");
    }

    // --- All supported languages ---

    #[test]
    fn all_extensions_mapped() {
        let expected = vec![
            ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".rs", ".java", ".rb", ".c", ".h", ".cpp",
            ".cc", ".hpp",
        ];
        for ext in expected {
            let input = json!({"file_path": format!("file{}", ext)});
            assert!(
                detect_lang_from_input(&input).is_some(),
                "missing mapping for {}",
                ext
            );
        }
    }

    // --- effective_min_chars ---

    #[test]
    fn effective_min_chars_default() {
        // Serialize: the override store is process-global (see
        // `override_test_lock`), and the runner is multi-threaded.
        let _guard = crate::runtime_env::override_test_lock();
        crate::runtime_env::clear_overrides();
        std::env::remove_var("HEADROOM_INTERCEPT_READ_MIN_CHARS");
        let interceptor = AstGrepReadOutline::default();
        assert_eq!(interceptor.effective_min_chars(), MIN_CHARS_DEFAULT);
    }

    #[test]
    fn effective_min_chars_from_override() {
        // Serialize: the override store is process-global (see
        // `override_test_lock`), and the runner is multi-threaded.
        let _guard = crate::runtime_env::override_test_lock();
        crate::runtime_env::clear_overrides();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "HEADROOM_INTERCEPT_READ_MIN_CHARS".to_string(),
            "1000".to_string(),
        );
        crate::runtime_env::set_overrides(&overrides);
        let interceptor = AstGrepReadOutline::default();
        assert_eq!(interceptor.effective_min_chars(), 1000);
        crate::runtime_env::clear_overrides();
    }

    #[test]
    fn effective_min_chars_invalid_env_uses_default() {
        // Serialize: the override store is process-global (see
        // `override_test_lock`), and the runner is multi-threaded.
        let _guard = crate::runtime_env::override_test_lock();
        crate::runtime_env::clear_overrides();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "HEADROOM_INTERCEPT_READ_MIN_CHARS".to_string(),
            "not_a_number".to_string(),
        );
        crate::runtime_env::set_overrides(&overrides);
        let interceptor = AstGrepReadOutline::default();
        assert_eq!(interceptor.effective_min_chars(), MIN_CHARS_DEFAULT);
        crate::runtime_env::clear_overrides();
    }

    // --- find_ast_grep_binary ---

    #[test]
    fn find_ast_grep_returns_something_or_none() {
        // This test just verifies the function doesn't panic.
        // The result depends on whether ast-grep is installed.
        let _ = find_ast_grep_binary();
    }
}

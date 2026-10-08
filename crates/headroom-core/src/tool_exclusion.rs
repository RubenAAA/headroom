//! Tool-name exclusion matching (Rust port of the exclusion helpers in
//! `headroom/config.py`).
//!
//! A tool result can be excluded from compression either because the operator
//! asked for it (`--exclude-tools`) or because rewriting it would break
//! something. The second case is why the defaults exist: `headroom_retrieve`
//! returns bytes the model asked to get *back*, so recompressing them reopens
//! the retrieval loop the retrieval was meant to close.
//!
//! Matching is deliberately forgiving about spelling, because the same logical
//! tool reaches us under several names depending on the client.

use serde_json::Value;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Tools whose results are never compressed by default.
/// Mirrors Python `DEFAULT_EXCLUDE_TOOLS`.
pub const DEFAULT_EXCLUDE_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "Write",
    "Edit",
    "WebSearch",
    "WebFetch",
    "view",
    "headroom_retrieve",
    // Cursor's file-read tool, the same category as `Read` and `view`:
    // without this its raw source takes the LOSSY path, which is strictly
    // worse than the fold `DEFAULT_BYTE_EXACT_EXCLUDE_TOOLS` protects it
    // from below.
    "read_file",
    // Skill bodies (Claude Code's `Skill` tool) are INSTRUCTIONS, not tool
    // output: lossy compression on a directive does not degrade it, it
    // inverts it. Named below as well, because protecting a tool needs
    // both halves.
    "Skill",
    // Caveman's recovery tool: like headroom_retrieve, it returns the stored
    // original of something Caveman compressed upstream of us. Compressing it
    // again hands the model a marker-wrapped original and breaks its
    // "complete original" contract, so keep the whole result out.
    "caveman_retrieve",
    // Lowercase variants for case-insensitive matching.
    "read",
    "glob",
    "grep",
    "skill",
    "write",
    "edit",
    "web_search",
    "web_fetch",
];

/// [`DEFAULT_EXCLUDE_TOOLS`] in the comma-separated shape `--exclude-tools`
/// takes, so the flag's default and the constant above cannot drift apart.
///
/// The lowercase spellings are left out on purpose: matching is
/// case-insensitive, so listing them again would only pad `--help`.
///
/// These are the tools whose results the model is most likely to need
/// verbatim — the contents of a file it is about to edit, the exact lines a
/// search matched. Compressing those trades a few thousand tokens for the risk
/// of the model acting on a summary of a file rather than the file. Pass
/// `--exclude-tools ""` to compress them anyway.
pub const DEFAULT_EXCLUDE_TOOLS_CSV: &str = "Read,Glob,Grep,Write,Edit,WebSearch,WebFetch,view,read_file,Skill,headroom_retrieve,caveman_retrieve";

/// Excluded tools whose results must stay *byte-faithful* — not merely
/// uncompressed. Even the excluded-tool lossless fold rewrites formatted JSON,
/// which is enough to break them.
///
/// Mirrors Python `DEFAULT_VERBATIM_EXCLUDE_TOOLS`. Removing `headroom_retrieve`
/// from this set silently reopens the retrieval loop for the cross-turn-dedup
/// path, which has no guard of its own.
pub const DEFAULT_VERBATIM_EXCLUDE_TOOLS: &[&str] = &[
    "WebSearch",
    "WebFetch",
    "web_search",
    "web_fetch",
    "view",
    "headroom_retrieve",
    // Caveman's recovery tool: returns a stored original, so even lossless
    // folding would corrupt the "complete original" it promises.
    "caveman_retrieve",
];

/// File-READ tools whose output the excluded-tool lossless fold must never
/// REWRITE. A strictly weaker protection than
/// [`DEFAULT_VERBATIM_EXCLUDE_TOOLS`]: this gates only the fold, so these
/// tools keep cross-turn dedup and the age-based lossy fall-through.
///
/// Mirrors Python `DEFAULT_BYTE_EXACT_EXCLUDE_TOOLS`. A read result hands
/// the fold raw file bytes; the fold inverts on our side, but the model
/// types `Edit(old_string=…)` from the bytes it was SHOWN, so a rewrite
/// breaks the anchor and the retry turn costs more than the fold saved.
/// Protecting a read tool takes both halves: excluded above so it never
/// compresses lossily, and named here so it is never folded either.
pub const DEFAULT_BYTE_EXACT_EXCLUDE_TOOLS: &[&str] =
    &["Read", "read", "read_file", "Skill", "skill"];

/// Equivalent spellings of a tool name, for exclusion matching.
/// Mirrors Python `_tool_name_aliases`.
///
/// OpenAI-style MCP wrappers use `mcp__server__tool`. Custom agents that speak
/// Anthropic sometimes emit the same wrapper as `mcp_Server_tool`. Both forms —
/// and the bare tool name — must match an entry written in any of them.
pub fn tool_name_aliases(name: &str) -> Vec<String> {
    let mut aliases = vec![name.to_string()];
    let lname = name.to_ascii_lowercase();

    // OpenCode prefixes its server name onto the tool name (upstream
    // `c7ebaedd`), so the retrieve tool arrives doubled.
    if lname == "headroom_headroom_retrieve" {
        aliases.push("headroom_retrieve".to_string());
    } else if lname.starts_with("mcp__") {
        let parts: Vec<&str> = name.splitn(3, "__").collect();
        if parts.len() == 3 && !parts[1].is_empty() && !parts[2].is_empty() {
            aliases.push(format!("mcp_{}_{}", parts[1], parts[2]));
            aliases.push(parts[2].to_string());
        }
    } else if lname.starts_with("mcp_") {
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        if parts.len() == 3 && !parts[1].is_empty() && !parts[2].is_empty() {
            aliases.push(format!("mcp__{}__{}", parts[1], parts[2]));
            aliases.push(parts[2].to_string());
        }
    }

    aliases.dedup();
    aliases
}

/// True when `pattern` contains a glob metacharacter.
fn is_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

/// Compile an fnmatch-style pattern to an anchored regex.
///
/// `fnmatchcase` semantics: `*` is any run, `?` is one character, `[...]` is a
/// character class with `!` (not `^`) for negation. Everything else is literal,
/// so it must be escaped — an unescaped `.` in a tool name would otherwise
/// match any character and over-exclude.
fn glob_to_regex(pattern: &str) -> Option<regex::Regex> {
    let mut out = String::with_capacity(pattern.len() * 2 + 4);
    out.push('^');
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '[' => {
                let mut class = String::from("[");
                // fnmatch spells negation `[!...]`; regex spells it `[^...]`.
                if chars.peek() == Some(&'!') {
                    chars.next();
                    class.push('^');
                } else if chars.peek() == Some(&'^') {
                    // A literal '^' first in the class; escape so regex does
                    // not read it as negation.
                    chars.next();
                    class.push_str("\\^");
                }
                let mut closed = false;
                for cc in chars.by_ref() {
                    if cc == ']' {
                        closed = true;
                        break;
                    }
                    if cc == '\\' {
                        class.push_str("\\\\");
                    } else {
                        class.push(cc);
                    }
                }
                if !closed {
                    // Unterminated class — treat the '[' as a literal, like
                    // fnmatch does.
                    out.push_str("\\[");
                    out.push_str(&regex::escape(&class[1..]));
                    continue;
                }
                class.push(']');
                out.push_str(&class);
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out.push('$');
    regex::Regex::new(&out).ok()
}

/// True if `name` matches the tool-exclusion set.
/// Mirrors Python `is_tool_excluded`.
///
/// Plain entries match by exact (case-insensitive) name, so the common case
/// stays a set lookup. Entries containing a glob metacharacter are matched
/// fnmatch-style, letting one pattern such as `mcp__*` cover every tool an MCP
/// server exposes without listing each name.
pub fn is_tool_excluded<'a, I>(name: &str, exclude_tools: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    let patterns: Vec<&str> = exclude_tools.into_iter().collect();
    if patterns.is_empty() || name.is_empty() {
        return false;
    }
    let aliases = tool_name_aliases(name);

    // Exact, then case-insensitive exact.
    let exact: HashSet<&str> = patterns.iter().copied().collect();
    let lower_exact: HashSet<String> = patterns.iter().map(|p| p.to_lowercase()).collect();
    for alias in &aliases {
        if exact.contains(alias.as_str()) || lower_exact.contains(&alias.to_lowercase()) {
            return true;
        }
    }

    // Glob entries only.
    patterns.iter().filter(|p| is_glob(p)).any(|pat| {
        glob_to_regex(&pat.to_lowercase())
            .map(|re| aliases.iter().any(|a| re.is_match(&a.to_lowercase())))
            .unwrap_or(false)
    })
}

/// True if `name` is excluded by the built-in verbatim set — results that must
/// not be rewritten at all, including by lossless folds.
pub fn is_verbatim_excluded(name: &str) -> bool {
    is_tool_excluded(name, DEFAULT_VERBATIM_EXCLUDE_TOOLS.iter().copied())
}

/// True if `name` is a file-read tool whose output must pass through the
/// excluded-tool lossless fold byte-exact.
pub fn is_byte_exact_excluded(name: &str) -> bool {
    is_tool_excluded(name, DEFAULT_BYTE_EXACT_EXCLUDE_TOOLS.iter().copied())
}

/// Env var tuning the small-MCP-result exemption; `0` disables it.
pub const MCP_RESULT_MIN_CHARS_ENV: &str = "HEADROOM_MCP_RESULT_MIN_CHARS";

/// Below this many characters an MCP tool result is never compressed.
/// Mirrors Python `_mcp_result_min_chars`'s default.
pub const DEFAULT_MCP_RESULT_MIN_CHARS: usize = 4000;

/// Parse the small-MCP-result threshold the way Python's
/// `_mcp_result_min_chars` does: missing, blank, or unparseable reads as
/// the 4000 default; negatives clamp to 0 (disabled — no text is
/// shorter than 0 characters). Pure so tests need no process-env games.
pub fn mcp_result_min_chars_from(raw: Option<&str>) -> usize {
    match raw {
        None => DEFAULT_MCP_RESULT_MIN_CHARS,
        Some(s) if s.trim().is_empty() => DEFAULT_MCP_RESULT_MIN_CHARS,
        Some(s) => s
            .trim()
            .parse::<i64>()
            .map(|v| v.max(0) as usize)
            .unwrap_or(DEFAULT_MCP_RESULT_MIN_CHARS),
    }
}

/// The live threshold, read from the environment once per request.
pub fn mcp_result_min_chars() -> usize {
    mcp_result_min_chars_from(std::env::var(MCP_RESULT_MIN_CHARS_ENV).ok().as_deref())
}

/// True if `name` is an MCP tool: `mcp__server__tool` (Claude Code) or
/// `mcp--server--tool`. Case-insensitive, like Python's
/// `_is_small_mcp_result`.
///
/// Note the double dash: Python's docstring names `mcp-server-tool` as a
/// seen spelling, but its `startswith(("mcp_", "mcp--"))` never matches a
/// single dash, so neither do we — the code is the contract.
pub fn is_mcp_tool_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 4
        && b[0].eq_ignore_ascii_case(&b'm')
        && b[1].eq_ignore_ascii_case(&b'c')
        && b[2].eq_ignore_ascii_case(&b'p')
        && (b[3] == b'_' || (b.len() >= 5 && b[3] == b'-' && b[4] == b'-'))
}

/// True when `text` is short enough to keep verbatim: strictly fewer
/// than `min_chars` characters. Python `len(text)` counts characters,
/// so `.chars()` keeps parity on non-ASCII text where bytes would
/// overcount.
pub fn mcp_result_is_small(text: &str, min_chars: usize) -> bool {
    text.chars().count() < min_chars
}

/// True if this tool result must stay verbatim (#4003): an MCP tool's
/// output under the size threshold. Small MCP results (tool schemas,
/// status reads) compress badly for the tokens they cost, so both
/// live-zone paths keep them byte-identical instead of rewriting them.
/// Mirrors Python `_is_small_mcp_result`.
pub fn is_small_mcp_result(tool_name: &str, text: &str, min_chars: usize) -> bool {
    is_mcp_tool_name(tool_name) && mcp_result_is_small(text, min_chars)
}

/// The CCR retrieval tool's canonical name. Results carrying this name are the
/// bytes the model just asked to have restored; compressing them again reopens
/// the loop the retrieval closed.
pub const CCR_TOOL_NAME: &str = "headroom_retrieve";

/// Tool names that resolve to the CCR retrieval tool, in any client spelling.
/// Cached because it is consulted per content block.
pub fn ccr_retrieve_aliases() -> &'static HashSet<String> {
    static ALIASES: OnceLock<HashSet<String>> = OnceLock::new();
    ALIASES.get_or_init(|| {
        [
            CCR_TOOL_NAME.to_string(),
            format!("mcp__Headroom__{CCR_TOOL_NAME}"),
            format!("mcp_Headroom_{CCR_TOOL_NAME}"),
        ]
        .into_iter()
        .collect()
    })
}

/// True if `name` is the CCR retrieval tool under any spelling.
pub fn is_ccr_retrieve_tool(name: &str) -> bool {
    is_tool_excluded(name, std::iter::once(CCR_TOOL_NAME))
}

/// Orchestrator wrappers whose payload is a script that may itself invoke the
/// retrieval tool. Port of Python `_ORCHESTRATOR_WRAPPER_NAMES`: OpenCode V2
/// Code Mode runs every MCP call inside its built-in `execute` tool, and
/// Codex code mode sends calls as `exec` / `functions.exec` custom tool
/// calls. On the wire the name is the wrapper — the inner tool name never
/// reaches the proxy — so without unwrapping, a retrieval delivered through
/// one of these loses the [`is_ccr_retrieve_tool`] exemption and its bytes
/// are re-offloaded into a marker the agent can never redeem.
const ORCHESTRATOR_WRAPPER_NAMES: &[&str] = &["execute", "exec", "functions.exec"];

fn js_call_chain_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Port of Python `_JS_CALL_CHAIN_RE`, minus the leading negative
        // lookbehind (`regex` has no look-around): the caller drops matches
        // whose preceding character is a word character, `$`, or `.`.
        regex::Regex::new(r"([A-Za-z_$][\w$]*(?:\??\.[A-Za-z_$][\w$]*)*)\s*(?:\?\.)?\s*\(")
            .expect("JS call-chain pattern")
    })
}

fn js_bracket_key_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Port of Python `_JS_BRACKET_KEY_RE`.
        regex::Regex::new(
            r#"\[\s*\\?(?:\"([^\"\n\\]{1,200})\\?\"|'([^'\n\\]{1,200})\\?')\s*\]\s*(?:\?\.)?\s*\("#,
        )
        .expect("JS bracket-key pattern")
    })
}

/// Every string nested in a decoded payload (objects and arrays walk
/// through); scalars contribute nothing. Port of Python `_decoded_strings`.
fn decoded_strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => out.push(s.as_str()),
        Value::Array(items) => {
            for item in items {
                decoded_strings(item, out);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                decoded_strings(item, out);
            }
        }
        _ => {}
    }
}

/// True when the trailing dotted segment (or bracket key) names the CCR
/// retrieval tool under any spelling. Port of Python `_names_retrieve`.
fn names_retrieve(candidate: &str) -> bool {
    let last = candidate.rsplit('.').next().unwrap_or(candidate);
    is_ccr_retrieve_tool(last)
}

/// True when a call-chain match starts at a real callee boundary: the
/// character before it is not a word character, `$`, or `.`. This is the
/// half of Python `_JS_CALL_CHAIN_RE`'s leading `(?<![\w$.])` that the
/// `regex` crate cannot express.
fn at_callee_boundary(text: &str, match_start: usize) -> bool {
    if match_start == 0 {
        return true;
    }
    match text[..match_start].chars().next_back() {
        None => true,
        Some(c) => !(c.is_alphanumeric() || c == '_' || c == '$' || c == '.'),
    }
}

/// True when decoded script `text` invokes the retrieval tool: a dotted
/// call chain (`headroom_retrieve(`, `tools.headroom.headroom_retrieve(`,
/// optional chaining included) or a bracket call (`tools["headroom_retrieve"](`).
fn script_invokes_retrieve(text: &str) -> bool {
    // Cheap gate first, as upstream: regexes only run on texts that name
    // the tool at all.
    if !text.contains(CCR_TOOL_NAME) {
        return false;
    }
    if js_call_chain_re()
        .captures_iter(text)
        .filter(|caps| {
            caps.get(0)
                .is_some_and(|m| at_callee_boundary(text, m.start()))
        })
        .filter_map(|caps| caps.get(1))
        .any(|callee| names_retrieve(callee.as_str()))
    {
        return true;
    }
    js_bracket_key_re().captures_iter(text).any(|caps| {
        caps.iter()
            .skip(1)
            .flatten()
            .any(|key| names_retrieve(key.as_str()))
    })
}

/// True when an orchestrator wrapper's payload invokes the retrieval tool.
/// Port of Python `_orchestrator_invokes_retrieve`.
///
/// Only a call whose own payload invokes the retrieval tool counts: the
/// wrapper is never exempted as a whole, and the match keys on the
/// model-authored call arguments — never on the result content.
pub fn orchestrator_invokes_retrieve(name: &str, arguments: Option<&Value>) -> bool {
    if !ORCHESTRATOR_WRAPPER_NAMES.contains(&name) {
        return false;
    }
    let Some(args) = arguments else {
        return false;
    };
    // A JSON string argument decodes to its payload; raw JavaScript for a
    // custom_tool_call is scanned as-is. Anything else (object or raw script
    // on the Anthropic wire) is walked for nested strings.
    let owned: Option<Value> = match args {
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(object @ Value::Object(_)) => Some(object),
            // A JSON scalar, string, or array: no orchestrator payload shape.
            Ok(_) => return false,
            Err(_) => None,
        },
        _ => None,
    };
    let payload: &Value = match (&owned, args) {
        (Some(object), _) => object,
        (None, Value::String(raw)) => return script_invokes_retrieve(raw),
        (None, Value::Object(_)) => args,
        (None, _) => return false,
    };
    let mut texts = Vec::new();
    decoded_strings(payload, &mut texts);
    texts.iter().any(|text| script_invokes_retrieve(text))
}

/// Resolve a wrapper tool call to its effective name: the retrieval tool's
/// own name when — and only when — the call's payload invokes it, otherwise
/// `name` unchanged. Port of the orchestrator-wrapper half of Python
/// `unwrap_tool_call_name`.
///
/// (The Hermes `tool_call` deferred-tool bridge has no Rust counterpart yet;
// This function does not unwrap it.)
pub fn unwrap_tool_call_name<'a>(name: &'a str, arguments: Option<&Value>) -> &'a str {
    if orchestrator_invokes_retrieve(name, arguments) {
        CCR_TOOL_NAME
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_is_case_insensitive() {
        assert!(is_tool_excluded(
            "Read",
            DEFAULT_EXCLUDE_TOOLS.iter().copied()
        ));
        assert!(is_tool_excluded(
            "READ",
            DEFAULT_EXCLUDE_TOOLS.iter().copied()
        ));
        assert!(!is_tool_excluded(
            "Bash",
            DEFAULT_EXCLUDE_TOOLS.iter().copied()
        ));
    }

    #[test]
    fn empty_inputs_never_match() {
        assert!(!is_tool_excluded("Read", std::iter::empty()));
        assert!(!is_tool_excluded("", DEFAULT_EXCLUDE_TOOLS.iter().copied()));
    }

    /// One `mcp__*` entry must cover every tool an MCP server exposes, rather
    /// than the operator listing each name.
    #[test]
    fn glob_covers_a_whole_mcp_server() {
        let pats = ["mcp__*"];
        assert!(is_tool_excluded("mcp__github__create_issue", pats));
        assert!(is_tool_excluded("mcp__Headroom__headroom_retrieve", pats));
        assert!(!is_tool_excluded("Bash", pats));
    }

    /// The same logical tool arrives spelled three ways depending on client.
    #[test]
    fn mcp_wrappers_match_through_aliases() {
        for name in [
            "headroom_retrieve",
            "mcp__Headroom__headroom_retrieve",
            "mcp_Headroom_headroom_retrieve",
            "headroom_headroom_retrieve",
        ] {
            assert!(
                is_ccr_retrieve_tool(name),
                "{name} must resolve to the CCR retrieval tool"
            );
        }
        assert!(!is_ccr_retrieve_tool("mcp__Other__something_else"));
        assert!(!is_ccr_retrieve_tool("my_headroom_headroom_retrieve"));
        assert!(!is_ccr_retrieve_tool("headroom_headroom_retrieve_v2"));
    }

    /// A literal '.' in a pattern must not act as a regex wildcard.
    #[test]
    fn literal_dot_is_not_a_wildcard() {
        assert!(!is_tool_excluded("axb", ["a.b*"]));
        assert!(is_tool_excluded("a.bc", ["a.b*"]));
    }

    #[test]
    fn char_classes_and_negation() {
        assert!(is_tool_excluded("tool1", ["tool[0-9]"]));
        assert!(!is_tool_excluded("toolx", ["tool[0-9]"]));
        assert!(is_tool_excluded("toolx", ["tool[!0-9]"]));
    }

    #[test]
    fn verbatim_set_covers_retrieval_and_web_tools() {
        assert!(is_verbatim_excluded("headroom_retrieve"));
        assert!(is_verbatim_excluded("caveman_retrieve"));
        assert!(is_verbatim_excluded("WebFetch"));
        // Read is excluded from compression but is not verbatim-protected.
        assert!(!is_verbatim_excluded("Read"));
    }

    #[test]
    fn lossy_exclusion_covers_caveman_retrieve() {
        // Caveman's recovery tool returns a stored original: keep it out of
        // lossy compression like headroom_retrieve (#4010).
        for name in ["caveman_retrieve", "CAVEMAN_RETRIEVE"] {
            assert!(
                is_tool_excluded(name, DEFAULT_EXCLUDE_TOOLS.iter().copied()),
                "{name} must be excluded from lossy compression"
            );
        }
    }

    #[test]
    fn lossy_exclusion_covers_read_file_and_skill() {
        for name in ["read_file", "READ_FILE", "Skill", "skill", "SKILL"] {
            assert!(
                is_tool_excluded(name, DEFAULT_EXCLUDE_TOOLS.iter().copied()),
                "{name} must be excluded from lossy compression"
            );
        }
    }

    /// Port of `TestOrchestratorUnwrap`: a `headroom_retrieve` call delivered
    /// through a generic orchestrator wrapper resolves to the retrieval tool
    /// when — and only when — the call's own payload invokes it.
    fn unwrap<'a>(name: &'a str, arguments: Option<&'a Value>) -> &'a str {
        unwrap_tool_call_name(name, arguments)
    }

    fn json_args(code: &str) -> Value {
        Value::String(serde_json::to_string(&serde_json::json!({"code": code})).unwrap())
    }

    #[test]
    fn opencode_execute_wrapper_resolves_to_retrieve() {
        let code = r#"const r = await tools.headroom.headroom_retrieve({hash: "abc123def456"});
return r;"#;
        assert_eq!(unwrap("execute", Some(&json_args(code))), CCR_TOOL_NAME);
    }

    #[test]
    fn qualified_mcp_name_inside_script_resolves() {
        let code = r#"const r = await tools.mcp__headroom__headroom_retrieve({hash: "abc"});"#;
        assert_eq!(unwrap("execute", Some(&json_args(code))), CCR_TOOL_NAME);
    }

    #[test]
    fn bracket_access_inside_script_resolves() {
        let code = r#"const r = await tools["headroom_retrieve"]({hash: "abc"});"#;
        assert_eq!(unwrap("execute", Some(&json_args(code))), CCR_TOOL_NAME);
    }

    #[test]
    fn execute_wrapper_without_retrieve_call_keeps_name() {
        let code = "const r = await tools.bash.bash({command: \"ls\"});\nreturn r;";
        assert_eq!(unwrap("execute", Some(&json_args(code))), "execute");
    }

    #[test]
    fn non_wrapper_mentioning_retrieve_keeps_name() {
        // A non-orchestrator tool whose arguments merely name the tool is not
        // recovery output; only recognized wrappers resolve.
        let args = Value::String(r#"{"command": "grep -n headroom_retrieve( README.md"}"#.into());
        assert_eq!(unwrap("Bash", Some(&args)), "Bash");
    }

    #[test]
    fn wrapper_payload_mentioning_retrieve_without_a_call_keeps_name() {
        let args = json_args("// see headroom_retrieve");
        assert_eq!(unwrap("execute", Some(&args)), "execute");
    }

    #[test]
    fn json_encoded_whitespace_before_call_resolves() {
        // The OpenAI wire JSON-encodes the payload, so the script's real
        // newline between the callee and `(` arrives escaped; the call must be
        // recognized on the decoded script.
        for sep in ["\n", "\t"] {
            let code = format!(
                r#"const r = await tools.headroom.headroom_retrieve{sep}({{hash: "abc"}});"#
            );
            assert_eq!(
                unwrap("execute", Some(&json_args(&code))),
                CCR_TOOL_NAME,
                "sep {sep:?}"
            );
            let code =
                format!(r#"const r = await tools["headroom_retrieve"]{sep}({{hash: "abc"}});"#);
            assert_eq!(
                unwrap("execute", Some(&json_args(&code))),
                CCR_TOOL_NAME,
                "sep {sep:?}"
            );
        }
    }

    #[test]
    fn anthropic_dict_payload_with_newline_resolves() {
        // The Anthropic wire hands over a decoded dict; it must be scanned
        // the same way.
        let args = serde_json::json!({"code": "const r = await tools.headroom.headroom_retrieve\n({hash: \"abc\"});"});
        assert_eq!(unwrap("exec", Some(&args)), CCR_TOOL_NAME);
    }

    #[test]
    fn nested_containers_in_dict_payload_resolve() {
        let args = serde_json::json!({
            "meta": {"attempt": 1},
            "calls": [{"code": "const r = await tools.headroom.headroom_retrieve\n({hash: \"abc\"});"}],
        });
        assert_eq!(unwrap("functions.exec", Some(&args)), CCR_TOOL_NAME);
    }

    #[test]
    fn raw_javascript_payload_with_newline_resolves() {
        // A custom_tool_call payload that is not JSON is scanned as-is.
        let args = Value::String(
            "const r = await tools.headroom.headroom_retrieve\n({hash: \"abc\"});".into(),
        );
        assert_eq!(unwrap("functions.exec", Some(&args)), CCR_TOOL_NAME);
    }

    #[test]
    fn string_values_without_the_name_are_skipped() {
        let args = serde_json::json!({
            "note": "no tool call here",
            "code": "const r = await tools.headroom.headroom_retrieve({hash: \"abc\"});",
        });
        assert_eq!(unwrap("execute", Some(&args)), CCR_TOOL_NAME);
    }

    #[test]
    fn dict_payload_without_string_scripts_keeps_name() {
        let args =
            serde_json::json!({"code": 42, "meta": {"attempt": 1}, "flags": [true, Value::Null]});
        assert_eq!(unwrap("execute", Some(&args)), "execute");
    }

    #[test]
    fn non_string_non_object_arguments_fail_closed() {
        for args in [None, Some(&Value::Null), Some(&serde_json::json!([1, 2]))] {
            assert_eq!(unwrap("execute", args), "execute");
        }
    }

    #[test]
    fn byte_exact_set_covers_file_reads_and_skills_only() {
        for name in [
            "Read",
            "read",
            "READ",
            "read_file",
            "Skill",
            "skill",
            "SKILL",
        ] {
            assert!(
                is_byte_exact_excluded(name),
                "{name} must pass through the lossless fold byte-exact"
            );
        }
        for name in ["Bash", "Grep", "WebFetch", "view", "headroom_retrieve"] {
            assert!(
                !is_byte_exact_excluded(name),
                "{name} must keep reaching the lossless fold"
            );
        }
    }

    #[test]
    fn mcp_tool_name_matches_both_wire_spellings() {
        for name in [
            "mcp__shop__get-price",
            "mcp--shop--get-price",
            "MCP__X",
            "Mcp--X",
            "mcp_",
        ] {
            assert!(is_mcp_tool_name(name), "{name} must read as an MCP tool");
        }
        // A single dash never matches: Python's startswith(("mcp_",
        // "mcp--")) says so, whatever its docstring claims.
        for name in [
            "mcp-server-tool",
            "mcp",
            "mcp-",
            "Bash",
            "headroom_retrieve",
            "",
        ] {
            assert!(!is_mcp_tool_name(name), "{name} must not read as MCP");
        }
    }

    #[test]
    fn mcp_threshold_parse_matches_python() {
        assert_eq!(mcp_result_min_chars_from(None), 4000);
        assert_eq!(mcp_result_min_chars_from(Some("")), 4000);
        assert_eq!(mcp_result_min_chars_from(Some("   ")), 4000);
        assert_eq!(mcp_result_min_chars_from(Some("6000")), 6000);
        assert_eq!(mcp_result_min_chars_from(Some(" 4000 ")), 4000);
        assert_eq!(mcp_result_min_chars_from(Some("+100")), 100);
        // Zero and negatives disable the floor (Python's max(0, ...)).
        assert_eq!(mcp_result_min_chars_from(Some("0")), 0);
        assert_eq!(mcp_result_min_chars_from(Some("-5")), 0);
        // Unparseable falls back to the default.
        assert_eq!(mcp_result_min_chars_from(Some("soon")), 4000);
        assert_eq!(mcp_result_min_chars_from(Some("4.5")), 4000);
    }

    #[test]
    fn small_mcp_result_needs_mcp_name_and_short_text() {
        let short = "x".repeat(3999);
        let at_floor = "x".repeat(4000);
        assert!(is_small_mcp_result("mcp__shop__price", &short, 4000));
        // The floor is strict: exactly 4000 characters compresses.
        assert!(!is_small_mcp_result("mcp__shop__price", &at_floor, 4000));
        assert!(!is_small_mcp_result("Bash", &short, 4000));
        // A zero floor exempts nothing.
        assert!(!is_small_mcp_result("mcp__shop__price", &short, 0));
        assert!(!is_small_mcp_result("mcp__shop__price", "", 0));
        // Characters, not bytes: 3999 é's are 7998 bytes but still small.
        assert!(is_small_mcp_result(
            "mcp__shop__price",
            &"é".repeat(3999),
            4000
        ));
        assert!(!is_small_mcp_result(
            "mcp__shop__price",
            &"é".repeat(4000),
            4000
        ));
    }
}

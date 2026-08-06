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
    "headroom_retrieve",
    // Lowercase variants for case-insensitive matching.
    "read",
    "glob",
    "grep",
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
pub const DEFAULT_EXCLUDE_TOOLS_CSV: &str =
    "Read,Glob,Grep,Write,Edit,WebSearch,WebFetch,headroom_retrieve";

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
    "headroom_retrieve",
];

/// Equivalent spellings of a tool name, for exclusion matching.
/// Mirrors Python `_tool_name_aliases`.
///
/// OpenAI-style MCP wrappers use `mcp__server__tool`. Custom agents that speak
/// Anthropic sometimes emit the same wrapper as `mcp_Server_tool`. Both forms —
/// and the bare tool name — must match an entry written in any of them.
pub fn tool_name_aliases(name: &str) -> Vec<String> {
    let mut aliases = vec![name.to_string()];
    let lname = name.to_ascii_lowercase();

    if lname.starts_with("mcp__") {
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
        ] {
            assert!(
                is_ccr_retrieve_tool(name),
                "{name} must resolve to the CCR retrieval tool"
            );
        }
        assert!(!is_ccr_retrieve_tool("mcp__Other__something_else"));
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
        assert!(is_verbatim_excluded("WebFetch"));
        // Read is excluded from compression but is not verbatim-protected.
        assert!(!is_verbatim_excluded("Read"));
    }
}

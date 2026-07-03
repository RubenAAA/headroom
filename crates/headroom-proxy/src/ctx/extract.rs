//! Event extraction (CTX-2a skeleton + first slice).
//!
//! Ports the *structure* of `context-mode/src/session/extract.ts`: a set of
//! category extractors run over the request body's messages, each emitting
//! typed [`ExtractedEvent`]s. The TS file is a 26-category, ~2960-line spec
//! (`rule, file, cwd, error, git, task, plan, env, skill, constraint,
//! decision, subagent, data, intent, …`). CTX-2a implements **four** of those
//! categories end-to-end; every other category is a `// CTX-2b:` stub so the
//! dispatch shape is in place for the full port.
//!
//! Unlike the TS extractor (which consumes a per-tool `HookInput`), this reads
//! the Anthropic request body directly: it walks `messages[..]` content blocks
//! (`text`, `tool_use`, `tool_result`). The caller passes `from_index` so only
//! the *new* messages of a turn are scanned (diff-against-last-turn, computed
//! by `identity::classify` + `extract_from_index`).

use serde_json::Value;

/// One extracted session event, mirroring `SessionEvent` in extract.ts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedEvent {
    pub category: String,
    pub type_: String,
    pub data: String,
    pub priority: i64,
}

impl ExtractedEvent {
    fn new(category: &str, type_: &str, data: impl Into<String>, priority: i64) -> Self {
        Self {
            category: category.to_string(),
            type_: type_.to_string(),
            data: data.into(),
            priority,
        }
    }
}

/// Extract events from the messages at `messages[from_index..]`.
///
/// Runs every category extractor over each new message and flattens the
/// result. Order: messages ascending, then category order within a message.
pub fn extract_new_messages(parsed: &Value, from_index: usize) -> Vec<ExtractedEvent> {
    let Some(messages) = parsed.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for msg in messages.iter().skip(from_index) {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let blocks = content_blocks(msg);

        // ── Implemented categories (CTX-2a) ──
        extract_intent(role, msg, &mut out);
        for block in &blocks {
            extract_error(block, &mut out);
            extract_file(block, &mut out);
            extract_git(block, &mut out);

            // ── Deferred categories (CTX-2b stubs) ──
            extract_rule(block, &mut out);
            extract_cwd(block, &mut out);
            extract_task(block, &mut out);
            extract_plan(block, &mut out);
            extract_env(block, &mut out);
            extract_skill(block, &mut out);
            extract_constraint(block, &mut out);
            extract_decision(block, &mut out);
            extract_subagent(block, &mut out);
            extract_data(block, &mut out);
        }
    }
    out
}

/// A message's content as a slice of blocks. A string `content` is treated as
/// a single synthetic `text` block so downstream code is uniform.
fn content_blocks(msg: &Value) -> Vec<Value> {
    match msg.get("content") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => vec![serde_json::json!({"type":"text","text": s})],
        _ => Vec::new(),
    }
}

// ─────────────────────────────────────────────────────────
// Category: intent (user prompts) — IMPLEMENTED
// ─────────────────────────────────────────────────────────

/// User-authored prose is the conversation's *intent*. Tool-result-only user
/// messages (no text) produce nothing. Assistant messages are skipped.
fn extract_intent(role: &str, msg: &Value, out: &mut Vec<ExtractedEvent>) {
    if role != "user" {
        return;
    }
    let text = user_text(msg);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    // A compaction/resume preamble is not fresh user intent — skip it so it
    // doesn't pollute the intent stream (identity.rs classifies it separately).
    if crate::ctx::identity::has_compaction_marker(trimmed) {
        return;
    }
    out.push(ExtractedEvent::new("intent", "intent", trimmed.to_string(), 1));
}

/// Concatenated text blocks of a message.
fn user_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut out = String::new();
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            out
        }
        _ => String::new(),
    }
}

// ─────────────────────────────────────────────────────────
// Category: error — IMPLEMENTED (extract.ts:97-122, 334-345)
// ─────────────────────────────────────────────────────────

/// A `tool_result` block that signals failure — either an explicit `is_error`
/// flag or a bash-style error pattern in its content.
fn extract_error(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
        return;
    }
    let response = tool_result_text(block);
    if !is_tool_error(block, &response) {
        return;
    }
    out.push(ExtractedEvent::new("error", "error_tool", response, 2));
}

/// Port of `isToolError` (extract.ts:97). Excludes context-mode's own guidance
/// echo (whose copy legitimately mentions "fails"/"error"), then checks the
/// explicit error flag or the bash error-pattern set.
fn is_tool_error(block: &Value, response: &str) -> bool {
    // context-mode guidance echo is never a real error.
    if response.starts_with("context-mode:") {
        return false;
    }
    let is_error_flag = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    is_error_flag || matches_error_pattern(response)
}

/// `/exit code [1-9]|error:|Error:|FAIL|failed/i` — the extract.ts:120 set.
/// `FAIL`/`failed` collapse to a case-insensitive `fail` substring test.
fn matches_error_pattern(s: &str) -> bool {
    let lower = s.to_lowercase();
    if lower.contains("error:") || lower.contains("fail") {
        return true;
    }
    // "exit code" followed (after spaces) by a non-zero digit.
    let mut hay = lower.as_str();
    while let Some(pos) = hay.find("exit code ") {
        let rest = hay[pos + "exit code ".len()..].trim_start();
        if rest.chars().next().is_some_and(|c| ('1'..='9').contains(&c)) {
            return true;
        }
        hay = &hay[pos + "exit code ".len()..];
    }
    false
}

/// Text content of a `tool_result` block (`content` may be a string or an
/// array of `{type:"text", text}` blocks).
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            out
        }
        _ => String::new(),
    }
}

// ─────────────────────────────────────────────────────────
// Category: file — IMPLEMENTED (extract.ts:176-300)
// ─────────────────────────────────────────────────────────

/// File-touching tool_use blocks → a `file` event carrying the target path.
fn extract_file(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
    let input = block.get("input");
    let (type_, priority) = match name {
        "Read" => ("file_read", 1),
        "Write" => ("file_write", 1),
        "Edit" | "MultiEdit" | "NotebookEdit" => ("file_edit", 1),
        "Glob" => ("file_glob", 3),
        "Grep" => ("file_search", 3),
        _ => return,
    };
    // Prefer file_path, then path, then pattern (Glob/Grep).
    let data = input
        .and_then(|i| {
            i.get("file_path")
                .or_else(|| i.get("path"))
                .or_else(|| i.get("pattern"))
        })
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if data.is_empty() {
        return;
    }
    out.push(ExtractedEvent::new("file", type_, data, priority));
}

// ─────────────────────────────────────────────────────────
// Category: git — IMPLEMENTED (extract.ts:352-437)
// ─────────────────────────────────────────────────────────

/// `git <op>` operations invoked via a Bash tool_use.
fn extract_git(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if block.get("name").and_then(Value::as_str) != Some("Bash") {
        return;
    }
    let command = block
        .get("input")
        .and_then(|i| i.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(op) = git_operation(command) else {
        return;
    };
    out.push(ExtractedEvent::new("git", "git", format!("{op}: {command}"), 2));
}

/// First recognized `git <subcommand>` in a command string → operation label.
/// Mirrors the GIT_PATTERNS table (extract.ts:352) via word-boundary matching.
fn git_operation(command: &str) -> Option<&'static str> {
    const OPS: &[(&str, &str)] = &[
        ("checkout", "branch"),
        ("commit", "commit"),
        ("merge", "merge"),
        ("rebase", "rebase"),
        ("stash", "stash"),
        ("push", "push"),
        ("pull", "pull"),
        ("log", "log"),
        ("diff", "diff"),
        ("status", "status"),
        ("branch", "branch"),
        ("reset", "reset"),
    ];
    let tokens: Vec<&str> = command.split_whitespace().collect();
    for w in tokens.windows(2) {
        if w[0] == "git" {
            for (sub, op) in OPS {
                if w[1] == *sub {
                    return Some(op);
                }
            }
        }
    }
    None
}

// ─────────────────────────────────────────────────────────
// Deferred categories (CTX-2b) — stubs preserving dispatch shape
// ─────────────────────────────────────────────────────────

// CTX-2b: rule — CLAUDE.md / rule-file reads and inline rule content.
fn extract_rule(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: cwd — `cd`/pushd working-directory changes.
fn extract_cwd(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: task — TodoWrite / task-list state.
fn extract_task(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: plan — ExitPlanMode plan text.
fn extract_plan(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: env — environment / tool-version probes.
fn extract_env(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: skill — Skill invocations.
fn extract_skill(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: constraint — explicit user constraints ("must", "never").
fn extract_constraint(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: decision — recorded design decisions.
fn extract_decision(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: subagent — Task/subagent spawns.
fn extract_subagent(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}
// CTX-2b: data — structured data payloads.
fn extract_data(_block: &Value, _out: &mut Vec<ExtractedEvent>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn intent_from_user_text() {
        let req = json!({"messages":[
            {"role":"user","content":"please refactor the parser"}
        ]});
        let events = extract_new_messages(&req, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].category, "intent");
        assert_eq!(events[0].data, "please refactor the parser");
        assert_eq!(events[0].priority, 1);
    }

    #[test]
    fn intent_skips_compaction_preamble_and_assistant() {
        let req = json!({"messages":[
            {"role":"user","content":"This session is being continued from a previous conversation. ..."},
            {"role":"assistant","content":"working on it"}
        ]});
        assert!(extract_new_messages(&req, 0).is_empty());
    }

    #[test]
    fn error_from_is_error_flag() {
        let req = json!({"messages":[
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let errs: Vec<_> = events.iter().filter(|e| e.category == "error").collect();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].type_, "error_tool");
        assert_eq!(errs[0].data, "boom");
    }

    #[test]
    fn error_from_bash_pattern() {
        for body in ["command failed", "exit code 1", "Error: nope", "tests FAILED"] {
            let req = json!({"messages":[
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t","content": body}
                ]}
            ]});
            let events = extract_new_messages(&req, 0);
            assert!(
                events.iter().any(|e| e.category == "error"),
                "should flag error for {body:?}"
            );
        }
    }

    #[test]
    fn error_ignores_context_mode_guidance_and_success() {
        let req = json!({"messages":[
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t","content":"context-mode: retry if it fails"}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t2","content":"exit code 0 ok"}
            ]}
        ]});
        assert!(extract_new_messages(&req, 0).iter().all(|e| e.category != "error"));
    }

    #[test]
    fn file_events_by_tool() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"Read","input":{"file_path":"/a/b.rs"}},
                {"type":"tool_use","name":"Edit","input":{"file_path":"/a/c.rs"}},
                {"type":"tool_use","name":"Grep","input":{"pattern":"foo"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let files: Vec<_> = events.iter().filter(|e| e.category == "file").collect();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].type_, "file_read");
        assert_eq!(files[0].data, "/a/b.rs");
        assert_eq!(files[1].type_, "file_edit");
        assert_eq!(files[2].type_, "file_search");
        assert_eq!(files[2].data, "foo");
    }

    #[test]
    fn git_events_by_command() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"Bash","input":{"command":"git commit -m wip"}},
                {"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let git: Vec<_> = events.iter().filter(|e| e.category == "git").collect();
        assert_eq!(git.len(), 1);
        assert_eq!(git[0].data, "commit: git commit -m wip");
    }

    #[test]
    fn from_index_skips_old_messages() {
        let req = json!({"messages":[
            {"role":"user","content":"old intent"},
            {"role":"assistant","content":"reply"},
            {"role":"user","content":"new intent"}
        ]});
        let events = extract_new_messages(&req, 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "new intent");
    }
}

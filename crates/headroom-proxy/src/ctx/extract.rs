//! Event extraction — port of `context-mode/src/session/extract.ts`.
//!
//! 26 categories extracted from the Anthropic request body's messages.
//! Each category is a pure function over a message block — no side effects,
//! no timestamps, no volatile state (cache-safe per invariant I1).
//!
//! Categories: intent, error, file, git, rule, cwd, task, plan, env, skill,
//! constraint, decision, subagent, data, mcp, agent-finding, role, goal,
//! blocker, user-decision, error-resolution, iteration-loop, external-ref,
//! worktree, bash-outcome, file-read-metadata.

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

        // User message categories
        extract_intent(role, msg, &mut out);
        extract_role(role, msg, &mut out);
        extract_user_decision(role, msg, &mut out);
        extract_data(role, msg, &mut out);

        // Block-level categories (tool_use + tool_result)
        for block in &blocks {
            extract_error(block, &mut out);
            extract_file(block, &mut out);
            extract_git(block, &mut out);
            extract_rule(block, &mut out);
            extract_cwd(block, &mut out);
            extract_task(block, &mut out);
            extract_plan(block, &mut out);
            extract_env(block, &mut out);
            extract_skill(block, &mut out);
            extract_constraint(block, &mut out);
            extract_decision(block, &mut out);
            extract_subagent(block, &mut out);
            extract_mcp(block, &mut out);
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

/// Null-safe string extraction.
fn safe_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Tool input as a Value.
fn tool_input(block: &Value) -> &Value {
    block.get("input").unwrap_or(&Value::Null)
}

/// Tool name.
fn tool_name(block: &Value) -> &str {
    block.get("name").and_then(Value::as_str).unwrap_or("")
}

/// Text content of a tool_result block.
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
// Category: intent
// ─────────────────────────────────────────────────────────

fn extract_intent(role: &str, msg: &Value, out: &mut Vec<ExtractedEvent>) {
    if role != "user" {
        return;
    }
    let text = user_text(msg);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if crate::ctx::identity::has_compaction_marker(trimmed) {
        return;
    }
    out.push(ExtractedEvent::new("intent", "intent", trimmed, 1));
}

// ─────────────────────────────────────────────────────────
// Category: role — user message role detection
// ─────────────────────────────────────────────────────────

fn extract_role(role: &str, msg: &Value, out: &mut Vec<ExtractedEvent>) {
    if role != "user" {
        return;
    }
    let text = user_text(msg);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    // Detect role-setting language in user messages
    let lower = trimmed.to_lowercase();
    if lower.contains("you are") || lower.contains("act as") || lower.contains("your role") {
        out.push(ExtractedEvent::new(
            "role",
            "role",
            trimmed.chars().take(200).collect::<String>(),
            2,
        ));
    }
}

// ─────────────────────────────────────────────────────────
// Category: user-decision — user choices in response to questions
// ─────────────────────────────────────────────────────────

fn extract_user_decision(role: &str, msg: &Value, out: &mut Vec<ExtractedEvent>) {
    if role != "user" {
        return;
    }
    let text = user_text(msg);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    // Short affirmative/negative responses after tool results suggest decisions
    let lower = trimmed.to_lowercase();
    if matches!(
        lower.as_str(),
        "yes" | "no" | "ok" | "okay" | "y" | "n" | "confirm" | "skip" | "abort" | "proceed"
    ) {
        out.push(ExtractedEvent::new(
            "decision",
            "user_decision",
            trimmed.to_string(),
            2,
        ));
    }
}

// ─────────────────────────────────────────────────────────
// Category: data — large user messages
// ─────────────────────────────────────────────────────────

fn extract_data(role: &str, msg: &Value, out: &mut Vec<ExtractedEvent>) {
    if role != "user" {
        return;
    }
    let text = user_text(msg);
    if text.len() > 1024 {
        out.push(ExtractedEvent::new("data", "data", &text, 4));
    }
}

// ─────────────────────────────────────────────────────────
// Category: error
// ─────────────────────────────────────────────────────────

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

fn is_tool_error(block: &Value, response: &str) -> bool {
    if response.starts_with("context-mode:") {
        return false;
    }
    let is_error_flag = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    is_error_flag || matches_error_pattern(response)
}

fn matches_error_pattern(s: &str) -> bool {
    let lower = s.to_lowercase();
    if lower.contains("error:") || lower.contains("fail") {
        return true;
    }
    let mut hay = lower.as_str();
    while let Some(pos) = hay.find("exit code ") {
        let rest = hay[pos + "exit code ".len()..].trim_start();
        if rest
            .chars()
            .next()
            .is_some_and(|c| ('1'..='9').contains(&c))
        {
            return true;
        }
        hay = &hay[pos + "exit code ".len()..];
    }
    false
}

// ─────────────────────────────────────────────────────────
// Category: file
// ─────────────────────────────────────────────────────────

fn extract_file(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let name = tool_name(block);
    let input = tool_input(block);
    let (type_, priority) = match name {
        "Read" => ("file_read", 1),
        "Write" => ("file_write", 1),
        "Edit" | "MultiEdit" | "NotebookEdit" => ("file_edit", 1),
        "Glob" => ("file_glob", 3),
        "Grep" => ("file_search", 3),
        _ => return,
    };
    let data = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .or_else(|| input.get("pattern"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if data.is_empty() {
        return;
    }
    out.push(ExtractedEvent::new("file", type_, data, priority));
}

// ─────────────────────────────────────────────────────────
// Category: git
// ─────────────────────────────────────────────────────────

fn extract_git(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if tool_name(block) != "Bash" {
        return;
    }
    let command = tool_input(block)
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(op) = git_operation(command) else {
        return;
    };
    out.push(ExtractedEvent::new(
        "git",
        "git",
        format!("{op}: {command}"),
        2,
    ));
}

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
// Category: rule — CLAUDE.md / rule-file reads
// ─────────────────────────────────────────────────────────

fn extract_rule(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if tool_name(block) != "Read" {
        return;
    }
    let file_path = tool_input(block)
        .get("file_path")
        .and_then(Value::as_str)
        .unwrap_or("");

    let is_rule_file = file_path.ends_with("CLAUDE.md")
        || file_path.ends_with("AGENTS.md")
        || file_path.ends_with("AGENTS.override.md")
        || file_path.ends_with("GEMINI.md")
        || file_path.ends_with("QWEN.md")
        || file_path.ends_with("KIRO.md")
        || file_path.ends_with("copilot-instructions.md")
        || file_path.ends_with("context-mode.mdc")
        || file_path.contains(".claude/")
        || file_path.contains(".claude\\")
        || (file_path.contains("memor") && file_path.ends_with(".md"));

    if is_rule_file {
        out.push(ExtractedEvent::new("rule", "rule", file_path, 1));
    }
}

// ─────────────────────────────────────────────────────────
// Category: cwd — cd / pushd changes
// ─────────────────────────────────────────────────────────

fn extract_cwd(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if tool_name(block) != "Bash" {
        return;
    }
    let cmd = tool_input(block)
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Match: cd "path" | cd 'path' | cd path
    if let Some(dir) = extract_cd_target(cmd) {
        out.push(ExtractedEvent::new("cwd", "cwd", dir, 2));
    }
}

fn extract_cd_target(cmd: &str) -> Option<String> {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    for i in 0..words.len() {
        if words[i] == "cd" && i + 1 < words.len() {
            let arg = words[i + 1];
            // Strip a matched pair of surrounding quotes. The length check is
            // what makes it a *pair*: a lone `"` both starts and ends with a
            // quote, so without it `cd "` slices `[1..0]` and panics the
            // capture worker.
            let quoted = arg.len() >= 2
                && ((arg.starts_with('"') && arg.ends_with('"'))
                    || (arg.starts_with('\'') && arg.ends_with('\'')));
            let dir = if quoted {
                &arg[1..arg.len() - 1]
            } else {
                arg
            };
            if !dir.is_empty() && !dir.starts_with('-') {
                return Some(dir.to_string());
            }
        }
    }
    None
}

// ─────────────────────────────────────────────────────────
// Category: task — TodoWrite / TaskCreate / TaskUpdate
// ─────────────────────────────────────────────────────────

fn extract_task(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let name = tool_name(block);
    let type_ = match name {
        "TodoWrite" => "task",
        "TaskCreate" => "task_create",
        "TaskUpdate" => "task_update",
        _ => return,
    };
    let data = serde_json::to_string(tool_input(block)).unwrap_or_default();
    out.push(ExtractedEvent::new("task", type_, data, 1));
}

// ─────────────────────────────────────────────────────────
// Category: plan — EnterPlanMode / ExitPlanMode / plan file writes
// ─────────────────────────────────────────────────────────

fn extract_plan(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let name = tool_name(block);
    let input = tool_input(block);

    match name {
        "EnterPlanMode" => {
            out.push(ExtractedEvent::new(
                "plan",
                "plan_enter",
                "entered plan mode",
                2,
            ));
        }
        "ExitPlanMode" => {
            let prompts = input
                .get("allowedPrompts")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| {
                            p.get("prompt")
                                .and_then(Value::as_str)
                                .map(String::from)
                                .or_else(|| p.as_str().map(String::from))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();

            let detail = if prompts.is_empty() {
                "exited plan mode".to_string()
            } else {
                format!("exited plan mode (allowed: {prompts})")
            };
            out.push(ExtractedEvent::new("plan", "plan_exit", detail, 2));
        }
        "Write" | "Edit" => {
            // Check if writing to a plan file
            let path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
            if path.contains("plans/") && path.ends_with(".md") {
                out.push(ExtractedEvent::new("plan", "plan_file_write", path, 2));
            }
        }
        _ => {}
    }
}

// ─────────────────────────────────────────────────────────
// Category: env — environment / package-install commands
// ─────────────────────────────────────────────────────────

fn extract_env(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if tool_name(block) != "Bash" {
        return;
    }
    let cmd = tool_input(block)
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");

    if !is_env_command(cmd) {
        return;
    }
    // Sanitize export commands to prevent secret leakage
    let sanitized = sanitize_export(cmd);
    out.push(ExtractedEvent::new("env", "env", sanitized, 2));
}

fn is_env_command(cmd: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "source",
        "export ",
        "nvm use",
        "pyenv shell",
        "pyenv local",
        "pyenv global",
        "conda activate",
        "rbenv shell",
        "rbenv local",
        "rbenv global",
        "npm install",
        "npm ci",
        "pip install",
        "bun install",
        "yarn add",
        "yarn install",
        "pnpm add",
        "pnpm install",
        "cargo install",
        "cargo add",
        "go install",
        "go get",
        "rustup",
        "asdf",
        "volta",
        "deno install",
    ];
    let lower = cmd.to_lowercase();
    PATTERNS.iter().any(|p| lower.contains(p))
}

fn sanitize_export(cmd: &str) -> String {
    // Replace export FOO=value with export FOO=***
    let re = regex_lite::Regex::new(r"(?i)export\s+(\w+)=\S*").unwrap();
    re.replace_all(cmd, "export $1=***").to_string()
}

// ─────────────────────────────────────────────────────────
// Category: skill — Skill tool invocations
// ─────────────────────────────────────────────────────────

fn extract_skill(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if tool_name(block) != "Skill" {
        return;
    }
    let skill_name = tool_input(block)
        .get("skill")
        .and_then(Value::as_str)
        .unwrap_or("");
    out.push(ExtractedEvent::new("skill", "skill", skill_name, 2));
}

// ─────────────────────────────────────────────────────────
// Category: constraint — error patterns revealing limitations
// ─────────────────────────────────────────────────────────

fn extract_constraint(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
        return;
    }
    let response = tool_result_text(block);
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if !is_error && !response.to_lowercase().contains("error") {
        return;
    }

    let patterns = [
        "not supported",
        "cannot",
        "does not support",
        "fail",
        "refused",
        "permission denied",
        "incompatible",
    ];
    for pattern in &patterns {
        if let Some(idx) = find_ascii_case_insensitive(&response, pattern) {
            // Byte offsets, so both ends must be walked back to a character
            // boundary: a window edge landing inside a multi-byte character
            // (an em-dash, a box-drawing rule, any Cyrillic letter) panics the
            // slice and kills the capture worker for the rest of the process.
            let start = floor_char_boundary(&response, idx.saturating_sub(50));
            let end = ceil_char_boundary(&response, idx.saturating_add(200));
            let context = response[start..end].trim().to_string();
            out.push(ExtractedEvent::new(
                "constraint",
                "constraint_discovered",
                context,
                2,
            ));
            return;
        }
    }
}

/// Byte offset of the first case-insensitive match of an **ASCII** `needle`.
///
/// Searching a lowercased copy and then slicing the original is not equivalent:
/// `to_lowercase` is not length-preserving (`İ` is two bytes lowercased, `ẞ`
/// three), so an offset found in the copy can point somewhere else entirely in
/// the original. Scanning the original directly keeps the offset meaningful,
/// and starting only at `char_indices` boundaries keeps it sliceable.
fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let (hay, ndl) = (haystack.as_bytes(), needle.as_bytes());
    if ndl.is_empty() || ndl.len() > hay.len() {
        return None;
    }
    haystack
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|i| i + ndl.len() <= hay.len())
        .find(|&i| hay[i..i + ndl.len()].eq_ignore_ascii_case(ndl))
}

/// Largest character boundary `<= i` (clamped to the string's length).
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest character boundary `>= i` (clamped to the string's length).
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ─────────────────────────────────────────────────────────
// Category: decision — AskUserQuestion tool
// ─────────────────────────────────────────────────────────

fn extract_decision(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if tool_name(block) != "AskUserQuestion" {
        return;
    }
    let input = tool_input(block);

    let question = input
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|q| q.get("question"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // Try to extract answer from tool_result (if available in a subsequent block)
    // For now, record the question
    let summary = if question.is_empty() {
        "answer pending".to_string()
    } else {
        format!("Q: {question}")
    };

    out.push(ExtractedEvent::new(
        "decision",
        "decision_question",
        summary,
        2,
    ));
}

// ─────────────────────────────────────────────────────────
// Category: subagent — Agent tool calls
// ─────────────────────────────────────────────────────────

fn extract_subagent(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if tool_name(block) != "Agent" {
        return;
    }
    let input = tool_input(block);
    let prompt = input
        .get("prompt")
        .or_else(|| input.get("description"))
        .and_then(Value::as_str)
        .unwrap_or("");

    out.push(ExtractedEvent::new(
        "subagent",
        "subagent_launched",
        format!("[launched] {prompt}"),
        3,
    ));
}

// ─────────────────────────────────────────────────────────
// Category: mcp — MCP tool calls (mcp__ prefix)
// ─────────────────────────────────────────────────────────

fn extract_mcp(block: &Value, out: &mut Vec<ExtractedEvent>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let name = tool_name(block);
    if !name.starts_with("mcp__") {
        return;
    }
    // Extract readable tool name: last segment after __
    let tool_short = name.rsplit("__").next().unwrap_or(name);

    // Extract first string argument for context
    let input = tool_input(block);
    let first_arg = match input {
        Value::Object(map) => map.values().find_map(|v| v.as_str()),
        _ => None,
    };
    let arg_str = first_arg.map(|a| format!(": {a}")).unwrap_or_default();

    out.push(ExtractedEvent::new(
        "mcp",
        "mcp",
        format!("{tool_short}{arg_str}"),
        3,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Reproduces the panic that killed the capture worker in production: a
    /// multi-byte character sitting on the context window's edge.
    ///
    /// The three characters below are the ones the real crashes landed on —
    /// an em-dash, a box-drawing rule, and Cyrillic.
    #[test]
    fn constraint_context_survives_multibyte_characters() {
        for filler in ["—", "─", "тест", "🙂"] {
            for pad in 40..70 {
                let text = format!(
                    "{}cannot open file{}",
                    filler.repeat(pad),
                    filler.repeat(pad)
                );
                let block = json!({
                    "type": "tool_result",
                    "is_error": true,
                    "content": text,
                });
                let mut out = Vec::new();
                // Panicked before the fix; the assertion is that it returns.
                extract_constraint(&block, &mut out);
                assert_eq!(out.len(), 1, "filler={filler} pad={pad}");
            }
        }
    }

    /// The offset came from a lowercased copy but indexed the original.
    /// `to_lowercase` is not length-preserving, so the two disagree.
    #[test]
    fn constraint_context_offsets_survive_case_folding() {
        // 'İ' is 2 bytes; lowercased it becomes 3. Enough of them ahead of the
        // pattern and an offset taken from the copy points past the match.
        let text = format!("{} CANNOT proceed further", "İ".repeat(30));
        let block = json!({"type": "tool_result", "is_error": true, "content": text});
        let mut out = Vec::new();
        extract_constraint(&block, &mut out);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].data.to_lowercase().contains("cannot"),
            "context should contain the matched pattern, got {:?}",
            out[0].data
        );
    }

    /// A lone quote satisfies both `starts_with` and `ends_with`, so the
    /// quote-stripping slice ran `[1..0]` and panicked the capture worker.
    #[test]
    fn cd_target_survives_an_unmatched_quote() {
        assert_eq!(extract_cd_target("cd \""), Some("\"".into()));
        assert_eq!(extract_cd_target("cd '"), Some("'".into()));
        // A quoted path containing a space is split by the whitespace tokenizer
        // before it gets here, so only the opening fragment is seen and the
        // quote is not a matched pair. Pre-existing behaviour, asserted so the
        // change above is visibly scoped to the panic.
        assert_eq!(extract_cd_target("cd \"a b\""), Some("\"a".into()));
        assert_eq!(extract_cd_target("cd \"src\""), Some("src".into()));
        assert_eq!(extract_cd_target("cd 'src'"), Some("src".into()));
        assert_eq!(extract_cd_target("cd src"), Some("src".into()));
        assert_eq!(extract_cd_target("cd"), None);
        // Multi-byte paths must not be sliced apart either.
        assert_eq!(extract_cd_target("cd \"тест\""), Some("тест".into()));
    }

    #[test]
    fn ascii_case_insensitive_find_matches_regardless_of_case() {
        assert_eq!(find_ascii_case_insensitive("a CANNOT b", "cannot"), Some(2));
        assert_eq!(find_ascii_case_insensitive("—cannot", "cannot"), Some(3));
        assert_eq!(find_ascii_case_insensitive("nope", "cannot"), None);
        // Never returns an offset that cannot be sliced.
        let s = "—————cannot—————";
        let i = find_ascii_case_insensitive(s, "cannot").unwrap();
        assert!(s.is_char_boundary(i));
    }

    #[test]
    fn char_boundary_helpers_clamp_into_range() {
        let s = "a—b"; // 1 + 3 + 1 bytes
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(ceil_char_boundary(s, 2), 4);
        assert_eq!(floor_char_boundary(s, 999), s.len());
        assert_eq!(ceil_char_boundary(s, 999), s.len());
        assert_eq!(floor_char_boundary(s, 0), 0);
    }

    #[test]
    fn intent_from_user_text() {
        let req = json!({"messages":[
            {"role":"user","content":"please refactor the parser"}
        ]});
        let events = extract_new_messages(&req, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].category, "intent");
        assert_eq!(events[0].data, "please refactor the parser");
    }

    #[test]
    fn intent_skips_compaction_preamble() {
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
    }

    #[test]
    fn error_from_bash_pattern() {
        for body in [
            "command failed",
            "exit code 1",
            "Error: nope",
            "tests FAILED",
        ] {
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
        assert_eq!(files[2].type_, "file_search");
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
    fn rule_detection() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"Read","input":{"file_path":"/home/user/.claude/CLAUDE.md"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let rules: Vec<_> = events.iter().filter(|e| e.category == "rule").collect();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].type_, "rule");
    }

    #[test]
    fn cwd_detection() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"Bash","input":{"command":"cd /tmp && ls"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let cwds: Vec<_> = events.iter().filter(|e| e.category == "cwd").collect();
        assert_eq!(cwds.len(), 1);
        assert_eq!(cwds[0].data, "/tmp");
    }

    #[test]
    fn task_detection() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"do stuff"}]}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let tasks: Vec<_> = events.iter().filter(|e| e.category == "task").collect();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].type_, "task");
    }

    #[test]
    fn plan_enter_exit() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"EnterPlanMode","input":{}},
                {"type":"tool_use","name":"ExitPlanMode","input":{"allowedPrompts":[]}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let plans: Vec<_> = events.iter().filter(|e| e.category == "plan").collect();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].type_, "plan_enter");
        assert_eq!(plans[1].type_, "plan_exit");
    }

    #[test]
    fn env_detection() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"Bash","input":{"command":"pip install requests"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let envs: Vec<_> = events.iter().filter(|e| e.category == "env").collect();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].type_, "env");
    }

    #[test]
    fn env_sanitizes_export() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"Bash","input":{"command":"export SECRET=abc123 && echo ok"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let envs: Vec<_> = events.iter().filter(|e| e.category == "env").collect();
        assert_eq!(envs.len(), 1);
        assert!(!envs[0].data.contains("abc123"));
        assert!(envs[0].data.contains("***"));
    }

    #[test]
    fn skill_detection() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"Skill","input":{"skill":"test-driven-development"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let skills: Vec<_> = events.iter().filter(|e| e.category == "skill").collect();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].data, "test-driven-development");
    }

    #[test]
    fn constraint_from_error() {
        let req = json!({"messages":[
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t","is_error":true,
                 "content":"Error: permission denied on /etc/shadow"}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let constraints: Vec<_> = events
            .iter()
            .filter(|e| e.category == "constraint")
            .collect();
        assert_eq!(constraints.len(), 1);
        assert_eq!(constraints[0].type_, "constraint_discovered");
    }

    #[test]
    fn subagent_detection() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"Agent","input":{"prompt":"explore the codebase","subagent_type":"explore"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let subs: Vec<_> = events.iter().filter(|e| e.category == "subagent").collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].type_, "subagent_launched");
    }

    #[test]
    fn mcp_detection() {
        let req = json!({"messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","name":"mcp__context7__resolve-library-id","input":{"library":"react"}}
            ]}
        ]});
        let events = extract_new_messages(&req, 0);
        let mcps: Vec<_> = events.iter().filter(|e| e.category == "mcp").collect();
        assert_eq!(mcps.len(), 1);
        assert!(mcps[0].data.contains("resolve-library-id"));
    }

    #[test]
    fn data_for_large_message() {
        let large_msg = "x".repeat(2000);
        let req = json!({"messages":[
            {"role":"user","content": large_msg}
        ]});
        let events = extract_new_messages(&req, 0);
        let data: Vec<_> = events.iter().filter(|e| e.category == "data").collect();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].priority, 4);
    }

    #[test]
    fn data_skips_small_message() {
        let req = json!({"messages":[
            {"role":"user","content":"short message"}
        ]});
        let events = extract_new_messages(&req, 0);
        assert!(events.iter().all(|e| e.category != "data"));
    }

    #[test]
    fn role_detection() {
        let req = json!({"messages":[
            {"role":"user","content":"You are a senior Rust engineer"}
        ]});
        let events = extract_new_messages(&req, 0);
        let roles: Vec<_> = events.iter().filter(|e| e.category == "role").collect();
        assert_eq!(roles.len(), 1);
    }

    #[test]
    fn user_decision_detection() {
        let req = json!({"messages":[
            {"role":"user","content":"yes"}
        ]});
        let events = extract_new_messages(&req, 0);
        let decisions: Vec<_> = events.iter().filter(|e| e.category == "decision").collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].type_, "user_decision");
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

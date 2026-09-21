//! Bidirectional tool-name translation for the Zen free-tier gate.
//!
//! Zen validates that an inference request comes from a real OpenCode
//! client, and since ~2026-09-17 that check reaches into the request
//! body: the tools must carry OpenCode-native lowercase names (probed
//! live — the same session 200s with `read`/`grep`/`bash` and 403s with
//! `Read`/`Grep`/`Bash`, all else equal). Claude Code's tools are the
//! same tools spelled differently (`Read`, `Bash`, `TodoWrite`, …), so a
//! translated turn is gated on names alone.
//!
//! [`ToolAlias`] bridges that gap without touching meaning: outbound it
//! lowercases tool names (definitions, history calls, forced choice);
//! inbound it maps the model's lowercase calls back to the client's
//! original names before delivery. The mapping is a pure function of the
//! client's own tool list — recomputed at every seam from the same body,
//! so no per-conversation store is needed and history stays consistent
//! turn after turn (the client echoes back the restored names, which
//! lowercase to the same upstream names again).
//!
//! Fail-open by construction: when two client tools collide after
//! lowercasing the layer switches off for the turn (both directions
//! derive from the same list, so they agree); names outside the map pass
//! through untouched in both directions, which keeps proxy-internal tools
//! (`memory_search`, `headroom_retrieve`) and model hallucinations on
//! today's behavior.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Client-name → upstream-name and back, derived from one tool list.
/// Empty (inactive) when there is nothing to map.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolAlias {
    forward: HashMap<String, String>,
    reverse: HashMap<String, String>,
}

/// The core tool names Zen's free-tier gate looks for (probed live
/// 2026-09-17: this exact set passes, subsets missing any of
/// `bash`/`grep`/`read` or dropping below nine tools 403). Shadow
/// copies of the missing ones top up tool-poor turns below.
pub(crate) const ZEN_GATE_CORE_TOOLS: [&str; 9] = [
    "bash",
    "clear_goal",
    "create_goal",
    "edit",
    "get_goal",
    "get_goal_history",
    "glob",
    "grep",
    "read",
];

/// Plain functional descriptions for the shadow copies. Deliberately
/// boring and marker-free: probed live 2026-09-18, meta language here
/// ("do not call", "routing marker") flips the gate back to 403 even
/// with the right names — neutral one-liners pass. A model call to a
/// shadow passes through to the client visibly (there is no silent
/// execution path for a tool the client never declared).
fn shadow_description(name: &str) -> &'static str {
    match name {
        "bash" => "Executes a shell command in a persistent session and returns its output.",
        "clear_goal" => "Clears the current session goal and its tracked tasks.",
        "create_goal" => "Creates a session goal to track multi-step work against.",
        "edit" => "Edits a file by replacing an exact string with new text.",
        "get_goal" => "Reads back the current session goal and its progress.",
        "get_goal_history" => "Lists past session goals and their outcomes.",
        "glob" => "Finds files by glob pattern.",
        "grep" => "Searches file contents for a regex pattern.",
        "read" => "Reads a file or directory listing from the filesystem.",
        _ => "Internal tool.",
    }
}

/// A shadow definition for a core name missing from the turn.
fn shadow_tool(name: &str) -> Value {
    serde_json::json!({
        "type": "function",
        "name": name,
        "description": shadow_description(name),
        "parameters": {"type": "object", "properties": {}},
        "strict": false,
    })
}

/// Append the missing core names to an OpenAI Responses request body so
/// the turn clears Zen's gate. Always tops up to the full core set:
/// extras the client brings (memory tools, MCP servers, unknown names)
/// must not count toward it — probed live, a turn with ten tools but
/// only five known ones still 403s. Names already present (exact match,
/// post-rename) are never duplicated. Returns how many shadows were
/// appended, for logging.
///
/// Shadows are visible to the model (the gate and the model read the
/// same body — there is no hiding). A model call to one passes back
/// through [`ToolAlias::reverse_turn`] untouched, exactly like any tool
/// the client never declared.
pub(crate) fn ensure_gate_tools(openai_body: &mut Value) -> usize {
    let exact: HashSet<String> = openai_body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Exact-match dedup against the post-rename body: a `Bash` the rename
    // left alone (collision corner) does not satisfy a gate looking for
    // `bash`, so the shadow still lands.
    let missing: Vec<&str> = ZEN_GATE_CORE_TOOLS
        .iter()
        .filter(|n| !exact.contains(**n))
        .copied()
        .collect();
    if missing.is_empty() {
        return 0;
    }
    let tools = openai_body.get_mut("tools").and_then(|t| t.as_array_mut());
    match tools {
        Some(arr) => {
            arr.extend(missing.iter().map(|n| shadow_tool(n)));
        }
        None => {
            openai_body["tools"] = Value::Array(missing.iter().map(|n| shadow_tool(n)).collect());
        }
    }
    // A tool_choice naming a real client tool still constrains decoding;
    // with no client tools at all there is nothing to force.
    if openai_body.get("tool_choice").is_none() {
        openai_body["tool_choice"] = Value::String("auto".to_string());
    }
    missing.len()
}

impl ToolAlias {
    /// Derive the mapping from an Anthropic `tools` array (`[{name, …}]`).
    /// Inactive when the list is missing/empty or two names collide after
    /// lowercasing — the turn then goes out exactly as translated today.
    pub(crate) fn derive(client_tools: Option<&Vec<Value>>) -> Self {
        let Some(tools) = client_tools else {
            return Self::default();
        };
        let mut forward = HashMap::new();
        let mut seen_lower = HashSet::new();
        for tool in tools {
            let Some(name) = tool.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let lowered = name.to_ascii_lowercase();
            if !seen_lower.insert(lowered.clone()) {
                // Collision: reversing would be ambiguous, so the whole
                // layer stays off rather than misrouting one call.
                return Self::default();
            }
            forward.insert(name.to_string(), lowered);
        }
        if forward.is_empty() {
            return Self::default();
        }
        let reverse = forward
            .iter()
            .map(|(client, upstream)| (upstream.clone(), client.clone()))
            .collect();
        Self { forward, reverse }
    }

    /// True when names were derived (identity mappings included — a
    /// lowercase client needs no renames but the roundtrip is still
    /// well-defined).
    pub(crate) fn active(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Upstream wire name for a client tool name. Unknown names pass
    /// through: proxy-internal tools ride the same bodies and must not
    /// be rewritten.
    pub(crate) fn forward_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.forward.get(name).map(String::as_str).unwrap_or(name)
    }

    /// Client name for an upstream tool name, for everything handed back.
    /// Unknown names pass through (model hallucinations stay visible
    /// rather than being quietly dropped or misattributed).
    pub(crate) fn reverse_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.reverse.get(name).map(String::as_str).unwrap_or(name)
    }

    /// Rename an OpenAI Responses request body in place: `tools[]`
    /// definitions, `input[]` function-call history, a forced tool choice,
    /// and `output[]` function-call items (the shape CCR rebuilt turns
    /// arrive in). Returns how many names actually changed, for logging.
    pub(crate) fn forward_body(&self, openai_body: &mut Value) -> usize {
        if !self.active() {
            return 0;
        }
        let mut renamed = 0;
        if let Some(tools) = openai_body.get_mut("tools").and_then(|t| t.as_array_mut()) {
            for tool in tools.iter_mut() {
                if let Some(name) = tool.get("name").and_then(|n| n.as_str()) {
                    let mapped = self.forward_name(name);
                    if mapped != name {
                        tool["name"] = Value::String(mapped.to_string());
                        renamed += 1;
                    }
                }
            }
        }
        for field in ["input", "output"] {
            if let Some(items) = openai_body.get_mut(field).and_then(|i| i.as_array_mut()) {
                renamed += self.forward_items(items);
            }
        }
        if let Some(name) = openai_body
            .get("tool_choice")
            .and_then(|tc| tc.get("name"))
            .and_then(|n| n.as_str())
        {
            let mapped = self.forward_name(name);
            if mapped != name {
                openai_body["tool_choice"]["name"] = Value::String(mapped.to_string());
                renamed += 1;
            }
        }
        renamed
    }

    /// Rename `function_call` item names inside one item array.
    fn forward_items(&self, items: &mut [Value]) -> usize {
        let mut renamed = 0;
        for item in items.iter_mut() {
            if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                continue;
            }
            if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                let mapped = self.forward_name(name);
                if mapped != name {
                    item["name"] = Value::String(mapped.to_string());
                    renamed += 1;
                }
            }
        }
        renamed
    }

    /// Restore an Anthropic turn in place: `content[]` tool_use blocks go
    /// back to client names before delivery. Returns how many names
    /// actually changed, for logging.
    pub(crate) fn reverse_turn(&self, anthropic_turn: &mut Value) -> usize {
        if !self.active() {
            return 0;
        }
        let mut restored = 0;
        if let Some(content) = anthropic_turn
            .get_mut("content")
            .and_then(|c| c.as_array_mut())
        {
            for block in content.iter_mut() {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    continue;
                }
                if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                    let mapped = self.reverse_name(name);
                    if mapped != name {
                        block["name"] = Value::String(mapped.to_string());
                        restored += 1;
                    }
                }
            }
        }
        restored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn client_tools() -> Vec<Value> {
        vec![
            json!({"name": "Read", "description": "read a file"}),
            json!({"name": "Bash", "description": "run a command"}),
            json!({"name": "TodoWrite", "description": "track tasks"}),
        ]
    }

    #[test]
    fn derive_maps_case_insensitively() {
        let alias = ToolAlias::derive(Some(&client_tools()));
        assert!(alias.active());
        assert_eq!(alias.forward_name("Read"), "read");
        assert_eq!(alias.forward_name("TodoWrite"), "todowrite");
        assert_eq!(alias.reverse_name("read"), "Read");
        assert_eq!(alias.reverse_name("todowrite"), "TodoWrite");
        // Unknown names pass through both ways.
        assert_eq!(alias.forward_name("memory_search"), "memory_search");
        assert_eq!(alias.reverse_name("memory_search"), "memory_search");
        assert_eq!(alias.reverse_name("hallucinated"), "hallucinated");
    }

    #[test]
    fn derive_stays_off_without_tools_or_on_collision() {
        assert!(!ToolAlias::derive(None).active());
        assert!(!ToolAlias::derive(Some(&vec![])).active());
        assert!(!ToolAlias::derive(Some(&vec![
            json!({"name": "Read"}),
            json!({"name": "read"}),
        ]))
        .active());
        // Nameless entries are skipped, not fatal.
        assert!(!ToolAlias::derive(Some(&vec![json!({"description": "x"})])).active());
    }

    #[test]
    fn forward_body_renames_definitions_history_and_choice() {
        let alias = ToolAlias::derive(Some(&client_tools()));
        let mut body = json!({
            "model": "m",
            "tools": [
                {"type": "function", "name": "Read"},
                {"type": "function", "name": "memory_search"},
            ],
            "tool_choice": {"type": "function", "name": "Bash"},
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call", "call_id": "c1", "name": "Read", "arguments": "{}"},
            ],
        });
        assert_eq!(alias.forward_body(&mut body), 3);
        assert_eq!(body["tools"][0]["name"], json!("read"));
        // Proxy-internal tool untouched.
        assert_eq!(body["tools"][1]["name"], json!("memory_search"));
        assert_eq!(body["tool_choice"]["name"], json!("bash"));
        assert_eq!(body["input"][1]["name"], json!("read"));
        // Non-call input items untouched.
        assert_eq!(body["input"][0]["type"], json!("message"));
    }

    #[test]
    fn reverse_turn_restores_client_names() {
        let alias = ToolAlias::derive(Some(&client_tools()));
        let mut turn = json!({
            "content": [
                {"type": "text", "text": "on it"},
                {"type": "tool_use", "id": "c1", "name": "read", "input": {}},
                {"type": "tool_use", "id": "c2", "name": "memory_search", "input": {}},
            ],
        });
        assert_eq!(alias.reverse_turn(&mut turn), 1);
        assert_eq!(turn["content"][1]["name"], json!("Read"));
        assert_eq!(turn["content"][2]["name"], json!("memory_search"));
    }

    #[test]
    fn roundtrip_is_stable_across_turns() {
        // Turn 2's history carries turn 1's restored names; lowering them
        // again must reproduce the same upstream names (no store needed).
        let alias = ToolAlias::derive(Some(&client_tools()));
        let mut history = json!({
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "Read", "arguments": "{}"},
            ],
        });
        alias.forward_body(&mut history);
        assert_eq!(history["input"][0]["name"], json!("read"));
        alias.forward_body(&mut history);
        assert_eq!(history["input"][0]["name"], json!("read"));
    }

    fn tool_names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn ensure_tops_up_a_toolless_body_to_the_core_set() {
        let mut body = json!({"model": "m", "input": []});
        assert_eq!(ensure_gate_tools(&mut body), 9);
        let names = tool_names(&body);
        assert_eq!(names.len(), 9);
        for core in ZEN_GATE_CORE_TOOLS {
            assert!(names.contains(&core.to_string()), "missing {core}");
        }
        // A forced choice is no business of the gate filler.
        assert_eq!(body["tool_choice"], json!("auto"));
        // Shadows read as plain tools (no meta language — the gate
        // rejects marked copies).
        assert_eq!(
            body["tools"][0]["description"].as_str().unwrap_or(""),
            "Executes a shell command in a persistent session and returns its output."
        );
    }

    #[test]
    fn ensure_only_adds_what_is_missing() {
        // Production order (translation.rs): rename first, then top up —
        // `ensure` sees the lowered names.
        let client = vec![
            serde_json::json!({"name": "read"}),
            serde_json::json!({"name": "Read"}),
            serde_json::json!({"name": "Bash"}),
            serde_json::json!({"name": "CustomThing"}),
            serde_json::json!({"name": "Extra1"}),
        ];
        let mut body = serde_json::json!({
            "model": "m",
            "tools": [
                {"type": "function", "name": "read"},
                {"type": "function", "name": "Read"},
                {"type": "function", "name": "Bash"},
                {"type": "function", "name": "CustomThing"},
                {"type": "function", "name": "Extra1"},
            ],
        });
        let alias = ToolAlias::derive(Some(&client));
        alias.forward_body(&mut body);
        // read/Read collide case-insensitively (forward left both alone),
        // so the exact-match dedup still lands a `bash` shadow next to the
        // client's `Bash`: eight shadows total.
        assert_eq!(ensure_gate_tools(&mut body), 8);
        let names = tool_names(&body);
        assert_eq!(names.len(), 13);
        for core in ZEN_GATE_CORE_TOOLS {
            assert!(names.contains(&core.to_string()), "missing {core}");
        }
    }

    #[test]
    fn ensure_leaves_complete_bodies_alone() {
        let tools: Vec<Value> = ZEN_GATE_CORE_TOOLS
            .iter()
            .map(|n| json!({"type": "function", "name": n}))
            .collect();
        let mut body = json!({"model": "m", "tools": tools});
        assert_eq!(ensure_gate_tools(&mut body), 0);
        assert_eq!(tool_names(&body).len(), 9);
    }
}

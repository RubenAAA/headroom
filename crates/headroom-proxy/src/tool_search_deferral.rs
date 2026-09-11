//! Server-side tool-search deferral for Anthropic `/v1/messages`.
//!
//! Port of upstream `helpers.inject_tool_search_deferral`,
//! `helpers.strip_unsupported_tool_search_blocks`,
//! `helpers.strip_first_party_tool_search_tools_for_third_party_upstream`,
//! `providers.claude.runtime.is_custom_anthropic_base_url` (gate view), and
//! the `HEADROOM_TOOL_SEARCH` env gate in `handlers/anthropic.py`.
//!
//! When a request carries enough tools, non-core schemas are marked
//! `defer_loading: true` behind an injected `tool_search_tool_regex_*` search
//! tool, so first-party Anthropic excludes them from context billing until
//! the model searches for one. Every tool stays callable, output is
//! deterministic (the tools prefix still prompt-caches), and the tools cache
//! breakpoint moves to the last resident tool.
//!
//! Deferred schemas never pass through token counting, so their savings are
//! recorded in per-request tags (see `tool_schema_savings`) rather than
//! folded into `tokens_saved`.

use serde_json::Value;

/// Tool `type` prefix identifying a tool-search mechanism. Typed search tools
/// are the mechanism itself: never deferred, never a `tool_reference` target.
pub const TOOL_SEARCH_TYPE_PREFIX: &str = "tool_search_tool_";

/// Injected search tool shape (first-party Claude API, GA, no beta header).
pub const TOOL_SEARCH_TYPE: &str = "tool_search_tool_regex_20251119";
/// Name of the injected search tool.
pub const TOOL_SEARCH_NAME: &str = "tool_search_tool_regex";

/// Below this many tools the search round-trip isn't worth it (Anthropic's
/// own guidance: standard calling is better under ~10 tools).
pub const MIN_TOOLS: usize = 12;

/// Core coding tools kept resident so routine edit/read/run loops never pay a
/// search round-trip. Includes the client's own tool-search/schema-fetch tool
/// (`toolsearch`): it resolves tools the client keeps in its local registry
/// and never puts in the request body, so deferring it would hide the only
/// tool that can load them and they would become permanently unreachable.
pub const CORE_TOOLS: &[&str] = &[
    "bash",
    "bash_background",
    "bash_background_output",
    "bash_background_wait",
    "bash_background_kill",
    "read",
    "write",
    "edit",
    "multiedit",
    "apply_patch",
    "glob",
    "grep",
    "task",
    "todowrite",
    "todoread",
    "webfetch",
    "question",
    "skill",
    "toolsearch",
];

/// Env var gating deferral injection. Default on; `0`/`false`/`no`/`off`
/// (and anything outside the truthy set) opts out. Read per request so
/// operators can flip it without a restart.
pub const TOOL_SEARCH_ENV: &str = "HEADROOM_TOOL_SEARCH";

/// Normalize a client tool name for resident-tool membership checks.
///
/// Clients disagree on casing and leading namespace markers for the same
/// tool (`Bash` vs `bash` vs `_bash`); only leading markers are stripped so
/// internal separators such as `mcp__server__read` stay intact.
pub fn resident_key(name: &str) -> String {
    name.to_lowercase().trim_start_matches('_').to_string()
}

/// Whether deferral injection is enabled for `HEADROOM_TOOL_SEARCH`.
/// Pure over the env value so tests don't mutate the process environment.
pub fn tool_search_enabled_in(raw: Option<&str>) -> bool {
    matches!(
        raw.unwrap_or("1").trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "auto"
    )
}

/// Whether deferral injection is enabled (reads [`TOOL_SEARCH_ENV`]).
pub fn tool_search_enabled() -> bool {
    tool_search_enabled_in(std::env::var(TOOL_SEARCH_ENV).ok().as_deref())
}

/// Best-effort host extraction for the custom-upstream gate view.
///
/// Scheme, port, path, and trailing slash are ignored; matching is exact —
/// a lookalike such as `api.anthropic.com.evil.com` is custom.
/// Scheme-less values (`myproxy.local:8080`, `127.0.0.1:8787`) are re-parsed
/// as a network location. Unparseable input yields no host (not custom).
fn gate_view_host(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    if let Ok(url) = url::Url::parse(raw) {
        if let Some(host) = url.host_str().filter(|h| !h.is_empty()) {
            return host.to_lowercase();
        }
    }
    if !raw.contains("://") {
        if let Ok(url) = url::Url::parse(&format!("http://{raw}")) {
            return url.host_str().unwrap_or("").to_lowercase();
        }
    }
    String::new()
}

/// Whether an Anthropic base URL is custom (not first-party).
///
/// Only first-party Claude API supports the `tool_search_tool_*` type +
/// `defer_loading` shape (GA, no beta header); custom Anthropic-compatible
/// gateways reject it, so third-party routes strip client-originated search
/// tools and skip Headroom's own injection.
pub fn is_custom_anthropic_base_url(value: Option<&str>) -> bool {
    let raw = value.unwrap_or("").trim();
    if raw.is_empty() {
        return false;
    }
    !matches!(gate_view_host(raw).as_str(), "" | "api.anthropic.com")
}

/// Whether `tools` already carries a tool-search mechanism (typed or
/// name-prefixed): the client defers on its own, so leave it alone.
fn client_uses_tool_search(tools: &[Value]) -> bool {
    tools.iter().any(|t| {
        t.get("type")
            .and_then(Value::as_str)
            .is_some_and(|ty| ty.starts_with(TOOL_SEARCH_TYPE_PREFIX))
            || t.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.to_lowercase().starts_with(TOOL_SEARCH_TYPE_PREFIX))
    })
}

/// Whether `tools` carries a typed search tool (the mechanism itself).
/// Name-prefixed collisions don't count: a stale history reference to a
/// typeless client tool that happens to share the injected name must not
/// read as mechanism support.
fn has_typed_search_tool(tools: &[Value]) -> bool {
    tools.iter().any(|t| {
        t.get("type")
            .and_then(Value::as_str)
            .is_some_and(|ty| ty.starts_with(TOOL_SEARCH_TYPE_PREFIX))
    })
}

/// Outcome of [`strip_for_third_party_upstream`].
pub struct StripOutcome {
    /// Tools with first-party search entries removed, or the input moved
    /// back unchanged when there was nothing to strip.
    pub tools: Vec<Value>,
    /// Number of entries removed. Zero means unchanged.
    pub removed: usize,
}

/// Remove first-party tool-search tools for a third-party upstream.
///
/// Returns the input unchanged (`removed == 0`) when no typed search tool
/// is present — callers rely on the count to skip the write-back.
pub fn strip_for_third_party_upstream(tools: Vec<Value>) -> StripOutcome {
    if !has_typed_search_tool(&tools) {
        return StripOutcome { tools, removed: 0 };
    }
    let before = tools.len();
    let kept: Vec<Value> = tools
        .into_iter()
        .filter(|t| {
            !t.get("type")
                .and_then(Value::as_str)
                .is_some_and(|ty| ty.starts_with(TOOL_SEARCH_TYPE_PREFIX))
        })
        .collect();
    let removed = before - kept.len();
    StripOutcome {
        tools: kept,
        removed,
    }
}

/// Outcome of [`inject_deferral`].
pub struct InjectOutcome {
    /// New tools array (search tool first), or the input moved back
    /// unchanged when injection didn't apply. Callers must only
    /// re-serialize when `changed` to keep no-op turns byte-identical.
    pub tools: Vec<Value>,
    /// Whether the array changed.
    pub changed: bool,
    /// Deferred tools, serializable for token counting. Empty unless
    /// `changed`.
    pub deferred: Vec<Value>,
}

/// Defer non-core tool schemas behind an injected search tool.
///
/// No-op (input returned unchanged, `changed == false`) when the array is
/// not a deferral candidate: fewer than [`MIN_TOOLS`] tools, the client
/// already defers, or nothing would be deferred (so the cache prefix is
/// never perturbed for zero benefit).
///
/// Invariants enforced (else Anthropic 400s): the search tool is never
/// deferred; at least one tool stays non-deferred (the search tool itself);
/// a deferred tool never carries `cache_control` — if the client's tools
/// cache breakpoint sat on a now-deferred tool, it moves to the last
/// non-deferred real tool so the (smaller) tools prefix still caches. The
/// moved marker itself is preserved, not replaced with a bare ephemeral:
/// re-placing a bare marker would downgrade a 1h breakpoint to 5m.
pub fn inject_deferral(tools: Vec<Value>) -> InjectOutcome {
    let unchanged = |tools: Vec<Value>| InjectOutcome {
        tools,
        changed: false,
        deferred: Vec::new(),
    };
    if tools.len() < MIN_TOOLS || client_uses_tool_search(&tools) {
        return unchanged(tools);
    }

    let is_core = |tool: &Value| {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        CORE_TOOLS.contains(&resident_key(name).as_str())
    };

    let mut out: Vec<Value> = Vec::with_capacity(tools.len() + 1);
    out.push(serde_json::json!({"type": TOOL_SEARCH_TYPE, "name": TOOL_SEARCH_NAME}));
    let mut deferred = 0usize;
    let mut dropped_marker: Option<Value> = None;
    let mut dropped_cache_control = false;
    // Index into `out` of the last resident real (dict, typeless) tool, and
    // whether any resident tool already carries a breakpoint.
    let mut last_resident_real: Option<usize> = None;
    let mut resident_has_cache_control = false;

    for tool in tools {
        let typeless_dict = tool.as_object().is_some_and(|o| o.get("type").is_none());
        if !typeless_dict || is_core(&tool) {
            // Non-dict, server/typed tools (web_search, computer, …), and
            // core tools stay resident and unchanged.
            if typeless_dict {
                last_resident_real = Some(out.len());
                resident_has_cache_control = resident_has_cache_control
                    || tool.get("cache_control").is_some_and(|c| !c.is_null());
            }
            out.push(tool);
            continue;
        }
        let mut new_tool = tool;
        if let Some(obj) = new_tool.as_object_mut() {
            obj.insert("defer_loading".to_string(), Value::Bool(true));
            if let Some(dropped) = obj.remove("cache_control") {
                dropped_cache_control = true;
                if dropped.is_object() {
                    dropped_marker = Some(dropped);
                }
            }
        }
        out.push(new_tool);
        deferred += 1;
    }

    if deferred == 0 {
        // Nothing to defer → don't perturb the cache prefix. `out` holds
        // only the search tool plus the originals; drop it and hand back an
        // array equal to the input.
        let mut original = Vec::with_capacity(out.len() - 1);
        original.extend(out.into_iter().skip(1));
        return unchanged(original);
    }
    if dropped_cache_control && !resident_has_cache_control {
        if let Some(idx) = last_resident_real {
            if let Some(obj) = out[idx].as_object_mut() {
                obj.insert(
                    "cache_control".to_string(),
                    dropped_marker.unwrap_or_else(|| serde_json::json!({"type": "ephemeral"})),
                );
            }
        }
    }
    let deferred_tools: Vec<Value> = out
        .iter()
        .filter(|t| {
            t.get("defer_loading")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    InjectOutcome {
        tools: out,
        changed: true,
        deferred: deferred_tools,
    }
}

/// Names referenced by a `tool_search_tool_result` block.
///
/// Server-side results nest them under `content.tool_references`; a
/// client-side tool-search implementation returns the bare list. Entries are
/// `{"type": "tool_reference", "tool_name"|"name": ...}`.
fn tool_search_reference_names(content: &Value) -> Vec<String> {
    let entries = match content {
        Value::Object(map) => map.get("tool_references").and_then(Value::as_array),
        Value::Array(items) => Some(items),
        _ => None,
    };
    let Some(entries) = entries else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            if e.get("type").and_then(Value::as_str) != Some("tool_reference") {
                return None;
            }
            let name = e
                .get("tool_name")
                .or_else(|| e.get("name"))
                .and_then(Value::as_str)?;
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Outcome of [`strip_unsupported_blocks`].
pub struct RepairOutcome {
    /// Repaired messages, or the input moved back unchanged when nothing
    /// was removed. Callers must only re-serialize when `removed > 0`.
    pub messages: Vec<Value>,
    /// Number of blocks removed. Zero means unchanged.
    pub removed: usize,
}

/// Drop tool-search blocks the request's tools array cannot support.
///
/// A block pair is unsupportable when the request carries no typed search
/// tool, or when a `tool_reference` names a tool absent from `tools` — both
/// shapes Anthropic rejects. Both the `tool_search_tool_result` and its
/// paired `server_tool_use` are removed (an orphan of either 400s on its
/// own), and a message left with no content blocks is dropped rather than
/// sent empty. Only tool-search server calls are eligible: `web_search` and
/// code execution share the `server_tool_use` block type and must survive
/// untouched.
pub fn strip_unsupported_blocks(messages: Vec<Value>, tools: &[Value]) -> RepairOutcome {
    let unchanged = |messages: Vec<Value>| RepairOutcome {
        messages,
        removed: 0,
    };
    let available: std::collections::HashSet<&str> = tools
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?;
            if name.is_empty()
                || t.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|ty| ty.starts_with(TOOL_SEARCH_TYPE_PREFIX))
            {
                return None;
            }
            Some(name)
        })
        .collect();
    let has_search_tool = has_typed_search_tool(tools);

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut removed = 0usize;
    let mut changed = false;
    for mut message in messages {
        let content = match message.get_mut("content").and_then(|c| c.as_array_mut()) {
            Some(content) => content,
            None => {
                out.push(message);
                continue;
            }
        };
        let mut drop_indexes: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut orphaned_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (index, block) in content.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("tool_search_tool_result") {
                continue;
            }
            let names = tool_search_reference_names(block.get("content").unwrap_or(&Value::Null));
            if has_search_tool && names.iter().all(|n| available.contains(n.as_str())) {
                continue;
            }
            drop_indexes.insert(index);
            if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                orphaned_ids.insert(id.to_string());
            }
        }
        // The search call itself precedes its result, so pair it up in a
        // second pass. Only tool-search server calls are eligible.
        for (index, block) in content.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("server_tool_use") {
                continue;
            }
            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
            let is_search_call = name.starts_with(TOOL_SEARCH_TYPE_PREFIX);
            let id = block.get("id").and_then(Value::as_str).unwrap_or("");
            if orphaned_ids.contains(id) || (is_search_call && !has_search_tool) {
                drop_indexes.insert(index);
            }
        }
        if drop_indexes.is_empty() {
            out.push(message);
            continue;
        }
        changed = true;
        removed += drop_indexes.len();
        let mut kept: Vec<Value> = Vec::new();
        for (index, block) in content.drain(..).enumerate() {
            if !drop_indexes.contains(&index) {
                kept.push(block);
            }
        }
        if kept.is_empty() {
            continue; // the whole turn was tool-search bookkeeping
        }
        if let Some(obj) = message.as_object_mut() {
            obj.insert("content".to_string(), Value::Array(kept));
        }
        out.push(message);
    }
    if changed {
        RepairOutcome {
            messages: out,
            removed,
        }
    } else {
        unchanged(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Value {
        json!({"name": name, "input_schema": {"type": "object"}})
    }

    fn many_tools(names: &[&str]) -> Vec<Value> {
        names.iter().map(|n| tool(n)).collect()
    }

    // ── resident_key ──

    #[test]
    fn resident_key_normalizes_case_and_markers() {
        assert_eq!(resident_key("Bash"), "bash");
        assert_eq!(resident_key("_bash"), "bash");
        assert_eq!(resident_key("__BASH"), "bash");
        assert_eq!(resident_key("mcp__server__read"), "mcp__server__read");
    }

    // ── env gate ──

    #[test]
    fn gate_defaults_on_and_parses_truthy() {
        assert!(tool_search_enabled_in(None));
        for v in ["1", "true", "YES", " on ", "auto"] {
            assert!(tool_search_enabled_in(Some(v)), "{v}");
        }
        for v in ["0", "false", "no", "off", ""] {
            assert!(!tool_search_enabled_in(Some(v)), "{v}");
        }
    }

    // ── custom-URL predicate ──

    #[test]
    fn first_party_hosts_are_not_custom() {
        assert!(!is_custom_anthropic_base_url(None));
        assert!(!is_custom_anthropic_base_url(Some("")));
        assert!(!is_custom_anthropic_base_url(Some(
            "https://api.anthropic.com"
        )));
        assert!(!is_custom_anthropic_base_url(Some(
            "https://api.anthropic.com/v1/messages"
        )));
    }

    #[test]
    fn custom_hosts_are_custom() {
        assert!(is_custom_anthropic_base_url(Some(
            "https://gateway.internal/v1"
        )));
        assert!(is_custom_anthropic_base_url(Some("myproxy.local:8080")));
        assert!(is_custom_anthropic_base_url(Some("127.0.0.1:8787")));
        // Lookalike: exact match only, subdomains of evil don't count.
        assert!(is_custom_anthropic_base_url(Some(
            "https://api.anthropic.com.evil.com"
        )));
    }

    #[test]
    fn malformed_urls_are_not_custom() {
        assert!(!is_custom_anthropic_base_url(Some("http://[::1:8787")));
    }

    // ── inject no-ops ──

    #[test]
    fn fewer_than_twelve_tools_is_unchanged() {
        let tools = many_tools(&["read", "write", "grep"]);
        let before = tools.clone();
        let out = inject_deferral(tools);
        assert!(!out.changed);
        assert_eq!(out.tools, before);
        assert!(out.deferred.is_empty());
    }

    #[test]
    fn client_already_deferring_is_unchanged() {
        let mut tools = many_tools(&[
            "read",
            "write",
            "edit",
            "glob",
            "grep",
            "bash",
            "task",
            "webfetch",
            "question",
            "skill",
            "todowrite",
            "grep2",
        ]);
        tools.push(json!({"type": TOOL_SEARCH_TYPE, "name": TOOL_SEARCH_NAME}));
        let before = tools.clone();
        let out = inject_deferral(tools);
        assert!(!out.changed);
        assert_eq!(out.tools, before);
    }

    #[test]
    fn all_core_tools_means_nothing_to_defer() {
        // 12+ tools but every one resident: no perturbation of the prefix.
        let names = [
            "bash",
            "read",
            "write",
            "edit",
            "glob",
            "grep",
            "task",
            "webfetch",
            "question",
            "skill",
            "todowrite",
            "todoread",
        ];
        let tools = many_tools(&names);
        let before = tools.clone();
        let out = inject_deferral(tools);
        assert!(!out.changed);
        assert_eq!(out.tools, before);
    }

    // ── inject happy path ──

    fn mixed_tools() -> Vec<Value> {
        // 8 core + 6 third-party: over the threshold with deferral candidates.
        let mut tools = many_tools(&[
            "Bash",
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "Task",
            "WebFetch",
            "Slack_post",
            "Linear_get",
            "Sentry_get",
            "Notion_read",
            "Snowflake_q",
            "PagerDuty_get",
        ]);
        // Client breakpoint sits on a tool about to be deferred.
        tools[8] = json!({
            "name": "Slack_post",
            "input_schema": {"type": "object"},
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
        });
        tools
    }

    #[test]
    fn injection_defers_non_core_and_leads_with_search() {
        let out = inject_deferral(mixed_tools());
        assert!(out.changed);
        assert_eq!(out.tools.len(), 15);
        assert_eq!(out.tools[0]["type"], json!(TOOL_SEARCH_TYPE));
        assert_eq!(out.tools[0]["name"], json!(TOOL_SEARCH_NAME));
        assert_eq!(out.deferred.len(), 6);
        for t in &out.deferred {
            assert_eq!(t["defer_loading"], json!(true));
            assert!(t.get("cache_control").is_none());
        }
        // Core tools untouched.
        assert!(out.tools[1].get("defer_loading").is_none());
    }

    #[test]
    fn breakpoint_moves_to_last_resident_with_marker_preserved() {
        let out = inject_deferral(mixed_tools());
        assert!(out.changed);
        // Last resident real tool is WebFetch (index 8: search + 8 core).
        assert_eq!(
            out.tools[8]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn typed_server_tools_stay_resident() {
        let mut tools = mixed_tools();
        tools.push(json!({"type": "web_search_202602", "name": "web_search"}));
        let out = inject_deferral(tools);
        assert!(out.changed);
        let ws = out
            .tools
            .iter()
            .find(|t| t["name"] == json!("web_search"))
            .unwrap();
        assert!(ws.get("defer_loading").is_none());
    }

    // ── third-party strip ──

    #[test]
    fn strip_removes_only_typed_search_tools() {
        let tools = vec![
            tool("read"),
            json!({"type": TOOL_SEARCH_TYPE, "name": TOOL_SEARCH_NAME}),
            tool("write"),
        ];
        let out = strip_for_third_party_upstream(tools);
        assert_eq!(out.removed, 1);
        assert_eq!(out.tools.len(), 2);
    }

    #[test]
    fn strip_without_search_tools_is_unchanged() {
        let tools = many_tools(&["read", "write"]);
        let before = tools.clone();
        let out = strip_for_third_party_upstream(tools);
        assert_eq!(out.removed, 0);
        assert_eq!(out.tools, before);
    }

    // ── history repair ──

    fn search_result_block(id: &str, refs: Vec<Value>) -> Value {
        json!({
            "type": "tool_search_tool_result",
            "tool_use_id": id,
            "content": refs,
        })
    }

    fn search_call_block(id: &str) -> Value {
        json!({"type": "server_tool_use", "id": id, "name": TOOL_SEARCH_NAME})
    }

    fn tool_ref(name: &str) -> Value {
        json!({"type": "tool_reference", "tool_name": name})
    }

    #[test]
    fn supported_history_is_untouched() {
        let tools = {
            let mut t = many_tools(&["Slack_post"]);
            t.insert(
                0,
                json!({"type": TOOL_SEARCH_TYPE, "name": TOOL_SEARCH_NAME}),
            );
            t
        };
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                search_call_block("srv_1"),
                search_result_block("srv_1", vec![tool_ref("Slack_post")]),
            ],
        })];
        let before = messages.clone();
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.removed, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn unresolvable_references_drop_the_pair() {
        // Side-request tools array cannot resolve the reference: both the
        // result and its paired search call go, nothing else moves.
        let tools = many_tools(&["read"]);
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "hi"},
                search_call_block("srv_1"),
                search_result_block("srv_1", vec![tool_ref("Slack_post")]),
            ],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.removed, 2);
        assert_eq!(out.messages[0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn missing_search_tool_drops_search_calls_but_keeps_web_search() {
        let tools = many_tools(&["read", "web_search"]);
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                search_call_block("srv_9"),
                {"type": "server_tool_use", "id": "ws_1", "name": "web_search", "input": {}},
            ],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        // Search call dropped (no mechanism); web_search shares the block
        // type but is not a search call and has no orphan id → survives.
        assert_eq!(out.removed, 1);
        assert_eq!(out.messages[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(out.messages[0]["content"][0]["name"], json!("web_search"));
    }

    #[test]
    fn message_left_empty_is_dropped() {
        let tools = many_tools(&["read"]);
        let messages = vec![json!({
            "role": "assistant",
            "content": [search_call_block("srv_1")],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.removed, 1);
        assert!(out.messages.is_empty());
    }

    #[test]
    fn non_list_content_passes_through() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let before = messages.clone();
        let out = strip_unsupported_blocks(messages, &many_tools(&["read"]));
        assert_eq!(out.removed, 0);
        assert_eq!(out.messages, before);
    }
}

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

/// Env var overriding which tools stay resident. Unset → [`CORE_TOOLS`];
/// set-but-empty → defer everything non-typed; otherwise a comma-separated
/// list normalized via [`resident_key`]. Read per request, no restart needed.
/// `toolsearch` is always resident regardless (see [`CORE_TOOLS`]).
pub const CORE_TOOLS_ENV: &str = "HEADROOM_TOOL_SEARCH_CORE_TOOLS";

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

/// Tools that stay resident, with a deployment override.
///
/// Pure over the env value so tests don't mutate the process environment.
pub fn resolved_core_tools_in(raw: Option<&str>) -> Vec<String> {
    match raw {
        None => CORE_TOOLS.iter().map(|s| s.to_string()).collect(),
        Some(raw) => {
            let mut out: Vec<String> = raw
                .split(',')
                .filter_map(|part| {
                    let part = part.trim();
                    if part.is_empty() {
                        None
                    } else {
                        Some(resident_key(part))
                    }
                })
                .collect();
            // The client's own tool-search/schema-fetch tool resolves tools
            // the client keeps in its local registry and never puts in the
            // request body; deferring it orphans them (see [`CORE_TOOLS`]).
            if !out.iter().any(|t| t == "toolsearch") {
                out.push("toolsearch".to_string());
            }
            out
        }
    }
}

/// Tools that stay resident (reads [`CORE_TOOLS_ENV`]).
pub fn resolved_core_tools() -> Vec<String> {
    resolved_core_tools_in(std::env::var(CORE_TOOLS_ENV).ok().as_deref())
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
///
/// Visible crate-wide so the forward path can tag `tool_search_mode`:
/// `client` when the client already defers (stand-down, book nothing),
/// `headroom` only when we actually deferred something.
pub(crate) fn client_uses_tool_search(tools: &[Value]) -> bool {
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
    /// Number of surviving tools that had `defer_loading` cleared.
    pub undeferred: usize,
}

/// Remove first-party tool-search tools for a third-party upstream.
///
/// `defer_loading` is cleared at the same time, and that half is
/// load-bearing: removing only the search tool leaves every other tool
/// marked deferred with nothing left that can resolve it — the model has
/// no mechanism to load them, and a `tool_reference` naming one is a
/// documented 400. The tools go out eager instead.
///
/// Returns the input unchanged (`removed == 0`) when no typed search tool
/// is present — callers rely on the count to skip the write-back.
pub fn strip_for_third_party_upstream(tools: Vec<Value>) -> StripOutcome {
    if !has_typed_search_tool(&tools) {
        return StripOutcome {
            tools,
            removed: 0,
            undeferred: 0,
        };
    }
    let before = tools.len();
    let mut undeferred = 0usize;
    let mut kept: Vec<Value> = Vec::with_capacity(before);
    for mut t in tools {
        if t.get("type")
            .and_then(Value::as_str)
            .is_some_and(|ty| ty.starts_with(TOOL_SEARCH_TYPE_PREFIX))
        {
            continue;
        }
        if t.get("defer_loading").and_then(Value::as_bool) == Some(true) {
            if let Some(obj) = t.as_object_mut() {
                obj.remove("defer_loading");
                undeferred += 1;
            }
        }
        kept.push(t);
    }
    let removed = before - kept.len();
    StripOutcome {
        tools: kept,
        removed,
        undeferred,
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
    /// Deferred tools resolved as core under the active core set
    /// ([`resolved_core_tools`]). Disjoint slice of `deferred`: with a custom
    /// core set, deferring a core tool means the override put it outside the
    /// resident set. Empty unless `changed`.
    pub core_deferred: Vec<Value>,
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
    inject_deferral_with_core(tools, &resolved_core_tools())
}

/// [`inject_deferral`] with an explicit core set. Pure so tests can pass a
/// custom set without mutating the process environment.
pub fn inject_deferral_with_core(tools: Vec<Value>, core_tools: &[String]) -> InjectOutcome {
    let unchanged = |tools: Vec<Value>| InjectOutcome {
        tools,
        changed: false,
        deferred: Vec::new(),
        core_deferred: Vec::new(),
    };
    if tools.len() < MIN_TOOLS || client_uses_tool_search(&tools) {
        return unchanged(tools);
    }

    let is_core = |tool: &Value| {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        let key = resident_key(name);
        // Hard-exempt: resolves client-local tools never in the body;
        // deferring it orphans them (see [`CORE_TOOLS`]).
        key == "toolsearch" || core_tools.iter().any(|c| c == &key)
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
    // Core-vs-noncore split of the same deferred total: a deferred tool
    // counts as core when the DEFAULT set calls it core, so the experiment
    // override shows up as core savings against the default baseline.
    // Disjoint from the non-core remainder; never added to the headline
    // alongside `deferred` (see `tool_schema_savings`).
    let core_deferred: Vec<Value> = deferred_tools
        .iter()
        .filter(|t| {
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            CORE_TOOLS.contains(&resident_key(name).as_str())
        })
        .cloned()
        .collect();
    InjectOutcome {
        tools: out,
        changed: true,
        deferred: deferred_tools,
        core_deferred,
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

/// Stand-in for a tool-search block the outbound tools array cannot support.
/// Text so it is inert to every validator, short so it costs ~10 tokens, and
/// constant so the repaired prefix stays byte-stable across turns (the
/// provider cache needs the same bytes every time). Mirrors upstream
/// `_TOOL_SEARCH_PLACEHOLDER_BLOCK`.
const TOOL_SEARCH_PLACEHOLDER_TEXT: &str = "[tool search omitted: unavailable in this request]";

fn placeholder_block() -> Value {
    serde_json::json!({"type": "text", "text": TOOL_SEARCH_PLACEHOLDER_TEXT})
}

/// Stand-in for a non-tool-search server result the outbound request cannot
/// support. Separate text so the two shapes stay distinguishable in logs
/// and cached prefixes.
const SERVER_RESULT_PLACEHOLDER_TEXT: &str = "[search result omitted: unavailable in this request]";

fn server_result_placeholder_block() -> Value {
    serde_json::json!({"type": "text", "text": SERVER_RESULT_PLACEHOLDER_TEXT})
}

/// Server-tool family for pairing results with their calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerToolFamily {
    Search,
    WebSearch,
    CodeExec,
}

/// Server result block types sharing the pair-with-`server_tool_use` rule.
/// `tool_search_tool_result` 400s without its call (seen live); the other
/// two share the validator shape, so an orphan there fails the same way.
fn server_result_family(block_type: &str) -> Option<ServerToolFamily> {
    match block_type {
        "tool_search_tool_result" => Some(ServerToolFamily::Search),
        "web_search_tool_result" => Some(ServerToolFamily::WebSearch),
        "code_execution_tool_result" => Some(ServerToolFamily::CodeExec),
        _ => None,
    }
}

/// Family of a `server_tool_use` call by name. Unknown names yield `None`:
/// pairing stays permissive for flows this proxy has never seen rather
/// than neutralizing calls it cannot classify.
fn server_call_family(name: &str) -> Option<ServerToolFamily> {
    if name.starts_with(TOOL_SEARCH_TYPE_PREFIX) {
        Some(ServerToolFamily::Search)
    } else if name.starts_with("web_search") {
        Some(ServerToolFamily::WebSearch)
    } else if name.starts_with("code_execution") {
        Some(ServerToolFamily::CodeExec)
    } else {
        None
    }
}

/// Outcome of [`strip_unsupported_blocks`].
pub struct RepairOutcome {
    /// Repaired messages, or the input moved back unchanged when nothing
    /// was neutralized. Callers must only re-serialize when `neutralized > 0`.
    pub messages: Vec<Value>,
    /// Number of blocks neutralized. Zero means unchanged.
    pub neutralized: usize,
}

/// Neutralize server-tool blocks the request's tools array cannot support.
///
/// A block pair is unsupportable when the request carries no typed search
/// tool, when a `tool_reference` names a tool absent from `tools`, or when a
/// server result (`tool_search_tool_result`, `web_search_tool_result`,
/// `code_execution_tool_result`) names a `tool_use_id` no `server_tool_use`
/// before it carries — all shapes Anthropic rejects (the last as
/// `unexpected tool_use_id ... must have a corresponding server_tool_use
/// block before it`; seen live when a client transcript kept bare results
/// but never stored their calls). A result with a missing id is
/// unsupportable too: every server result shape requires one.
/// Both the result and its paired `server_tool_use` are handled (an orphan
/// of either 400s on its own). Only tool-search server calls are eligible
/// for call-side neutralization: `web_search` and code execution calls
/// stand alone and must survive untouched.
///
/// Replace in place rather than remove (upstream #3456). The block indexes
/// of a message are load-bearing: the signed-thinking guard keys a thinking
/// block by its position, so deleting a block that sits before a thinking
/// block in the same message — or deleting a whole message ahead of one —
/// moves that block, the guard reports the reasoning altered, and the
/// repair is discarded in favour of the client's original bytes. The
/// request that needed repairing is exactly the one that loses it, and
/// upstream 400s on the reference already found. Swapping each block for a
/// short text block keeps every thinking block at its original coordinates,
/// so the repair survives to the wire. Same reasoning as the CCR sibling
/// [`crate::ccr_retrieve_repair::strip_unsupported_ccr_blocks`]
/// ("neutralize rather than drop").
pub fn strip_unsupported_blocks(messages: Vec<Value>, tools: &[Value]) -> RepairOutcome {
    let unchanged = |messages: Vec<Value>| RepairOutcome {
        messages,
        neutralized: 0,
    };
    let available = available_tool_names(tools);
    let has_search_tool = has_typed_search_tool(tools);

    // Server calls seen in earlier messages: id -> call family (None for
    // names outside the known families). A result is only paired when its
    // call precedes it — calls usually live in an earlier assistant
    // message than their results, so a same-message-only check would
    // orphan every split pair.
    let mut seen_server_calls: std::collections::HashMap<String, Option<ServerToolFamily>> =
        std::collections::HashMap::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut neutralized = 0usize;
    let mut changed = false;
    for mut message in messages {
        let content = match message.get_mut("content").and_then(|c| c.as_array_mut()) {
            Some(content) => content,
            None => {
                out.push(message);
                continue;
            }
        };
        let mut neutralize_indexes: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        // Indexes neutralized with the generic server-result placeholder
        // rather than the tool-search one.
        let mut generic_placeholder_indexes: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        let mut orphaned_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Server calls earlier in THIS message: (index, id, family). A call
        // neutralized below must not pair a later result (its placeholder
        // is not a call upstream).
        let mut message_server_calls: Vec<(usize, String, Option<ServerToolFamily>)> = Vec::new();
        // Ids of the above seen so far, for pairing results below.
        let mut message_call_families: std::collections::HashMap<String, Option<ServerToolFamily>> =
            std::collections::HashMap::new();
        for (index, block) in content.iter().enumerate() {
            let block_type = block.get("type").and_then(Value::as_str);
            if block_type == Some("server_tool_use") {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let family = server_call_family(name);
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        message_call_families.insert(id.to_string(), family);
                        message_server_calls.push((index, id.to_string(), family));
                    }
                }
                continue;
            }
            let Some(family) = block_type.and_then(server_result_family) else {
                continue;
            };
            let id = block.get("tool_use_id").and_then(Value::as_str);
            let paired = match id {
                Some(id) if !id.is_empty() => {
                    let pair_ok =
                        |m: &std::collections::HashMap<String, Option<ServerToolFamily>>| {
                            match m.get(id) {
                                // Same family pairs; an unknown call name
                                // stays permissive (see server_call_family).
                                Some(Some(f)) => *f == family,
                                Some(None) => true,
                                None => false,
                            }
                        };
                    pair_ok(&seen_server_calls) || pair_ok(&message_call_families)
                }
                // A missing id never pairs: every server result shape
                // requires one.
                _ => false,
            };
            let supported = match family {
                ServerToolFamily::Search => {
                    let names =
                        tool_search_reference_names(block.get("content").unwrap_or(&Value::Null));
                    has_search_tool && names.iter().all(|n| available.contains(n.as_str()))
                }
                ServerToolFamily::WebSearch | ServerToolFamily::CodeExec => true,
            };
            if supported && paired {
                continue;
            }
            neutralize_indexes.insert(index);
            if family != ServerToolFamily::Search {
                generic_placeholder_indexes.insert(index);
            }
            if let Some(id) = id {
                if !id.is_empty() {
                    orphaned_ids.insert(id.to_string());
                }
            }
        }
        neutralize_unpaired_calls(
            content,
            &orphaned_ids,
            has_search_tool,
            &mut neutralize_indexes,
        );
        neutralize_results_of_dropped_calls(
            content,
            &message_server_calls,
            &mut neutralize_indexes,
            &mut generic_placeholder_indexes,
        );
        if neutralize_indexes.is_empty() {
            seen_server_calls.extend(message_call_families);
            out.push(message);
            continue;
        }
        changed = true;
        neutralized += neutralize_indexes.len();
        // Neutralize in place: every block keeps its index, so signed
        // thinking blocks downstream keep their coordinates and the
        // tampering guard still sees them byte-identical. No message is
        // ever dropped — an assistant turn that was pure tool-search
        // bookkeeping keeps its slot as text.
        apply_placeholders(content, &neutralize_indexes, &generic_placeholder_indexes);
        // Only surviving calls pair later results: a neutralized call is
        // placeholder text upstream, not a call.
        seen_server_calls.extend(
            message_server_calls
                .iter()
                .filter(|(i, _, _)| !neutralize_indexes.contains(i))
                .map(|(_, id, family)| (id.clone(), *family)),
        );
        out.push(message);
    }
    if changed {
        RepairOutcome {
            messages: out,
            neutralized,
        }
    } else {
        unchanged(out)
    }
}

type Indexes = std::collections::HashSet<usize>;

/// Client tool names a tool-search reference may point at: named, and not a
/// tool-search tool itself.
fn available_tool_names(tools: &[Value]) -> std::collections::HashSet<&str> {
    tools
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
        .collect()
}

/// The search call itself precedes its result, so pair it up in a
/// second pass. Only tool-search server calls are eligible: other
/// families stand alone and a lone call is valid upstream.
fn neutralize_unpaired_calls(
    content: &[Value],
    orphaned_ids: &std::collections::HashSet<String>,
    has_search_tool: bool,
    neutralize_indexes: &mut Indexes,
) {
    for (index, block) in content.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("server_tool_use") {
            continue;
        }
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        let is_search_call = server_call_family(name) == Some(ServerToolFamily::Search);
        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
        if orphaned_ids.contains(id) || (is_search_call && !has_search_tool) {
            neutralize_indexes.insert(index);
        }
    }
}

/// A kept result whose call was neutralized above is orphaned
/// after all (duplicate ids in a corrupt transcript): neutralize
/// it rather than forward a result pointing at placeholder text.
fn neutralize_results_of_dropped_calls(
    content: &[Value],
    message_server_calls: &[(usize, String, Option<ServerToolFamily>)],
    neutralize_indexes: &mut Indexes,
    generic_placeholder_indexes: &mut Indexes,
) {
    for (index, block) in content.iter().enumerate() {
        if neutralize_indexes.contains(&index)
            || block
                .get("type")
                .and_then(Value::as_str)
                .and_then(server_result_family)
                .is_none()
        {
            continue;
        }
        if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
            if message_server_calls
                .iter()
                .any(|(i, cid, _)| cid == id && neutralize_indexes.contains(i))
            {
                neutralize_indexes.insert(index);
                if block
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| server_result_family(t) != Some(ServerToolFamily::Search))
                {
                    generic_placeholder_indexes.insert(index);
                }
            }
        }
    }
}

fn apply_placeholders(content: &mut [Value], neutralize: &Indexes, generic: &Indexes) {
    for (index, block) in content.iter_mut().enumerate() {
        if neutralize.contains(&index) {
            *block = if generic.contains(&index) {
                server_result_placeholder_block()
            } else {
                placeholder_block()
            };
        }
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
        assert_eq!(out.undeferred, 0);
        assert_eq!(out.tools, before);
    }

    #[test]
    fn strip_clears_defer_loading_on_survivors() {
        // The orphan shape: client sent the deferred form (ENABLE_TOOL_SEARCH
        // workaround), the search tool is stripped, and every other tool is
        // still marked deferred with nothing left that can resolve it.
        let mut deferred = tool("read");
        deferred["defer_loading"] = json!(true);
        let tools = vec![
            deferred,
            json!({"type": TOOL_SEARCH_TYPE, "name": TOOL_SEARCH_NAME}),
            tool("write"),
        ];
        let out = strip_for_third_party_upstream(tools);
        assert_eq!(out.removed, 1);
        assert_eq!(out.undeferred, 1);
        assert_eq!(out.tools.len(), 2);
        assert!(out.tools.iter().all(|t| t.get("defer_loading").is_none()));
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
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn orphan_result_with_resolvable_refs_is_neutralized() {
        // Live 400 (2026-09-22, conv 607588fd): messages.1.content.4 was a
        // tool_search_tool_result whose tool_use_id had no server_tool_use
        // before it, while the tools array carried the search mechanism and
        // resolvable references — so the old refs-only check waved it
        // through and Anthropic rejected the turn.
        let tools = {
            let mut t = many_tools(&["Slack_post"]);
            t.insert(
                0,
                json!({"type": TOOL_SEARCH_TYPE, "name": TOOL_SEARCH_NAME}),
            );
            t
        };
        let messages = vec![
            json!({"role": "user", "content": "find it"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "looking"},
                    {"type": "text", "text": "still"},
                    {"type": "text", "text": "almost"},
                    {"type": "text", "text": "done"},
                    search_result_block("srvtoolu_014GQB8PfhW2xzW8GVLSzM2a", vec![tool_ref("Slack_post")]),
                ],
            }),
        ];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 1);
        let content = out.messages[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 5);
        assert_eq!(
            content[4]["text"],
            json!("[tool search omitted: unavailable in this request]")
        );
        // Neighbor blocks untouched.
        assert_eq!(content[0]["text"], json!("looking"));
    }

    #[test]
    fn orphan_result_with_empty_reference_object_is_neutralized() {
        // Exact live shape (2026-09-22, conv 607588fd, messages.1.content.4):
        // the client persists bare result blocks with no server_tool_use
        // anywhere in the transcript, and empty tool_references — which the
        // old refs-only check passed vacuously (`all` over nothing).
        let tools = {
            let mut t = many_tools(&["memory_search"]);
            t.insert(
                0,
                json!({"type": TOOL_SEARCH_TYPE, "name": TOOL_SEARCH_NAME}),
            );
            t
        };
        let messages = vec![json!({
            "role": "assistant",
            "content": [{
                "type": "tool_search_tool_result",
                "tool_use_id": "srvtoolu_014GQB8PfhW2xzW8GVLSzM2a",
                "content": {
                    "type": "tool_search_tool_search_result",
                    "tool_references": [],
                },
            }],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 1);
        assert_eq!(
            out.messages[0]["content"][0]["text"],
            json!("[tool search omitted: unavailable in this request]")
        );
    }

    #[test]
    fn split_pair_across_messages_is_untouched() {
        // The valid shape: the search call lives in an earlier assistant
        // message than its result (server executes, client echoes back).
        // Pairing must span messages, not just blocks.
        let tools = {
            let mut t = many_tools(&["Slack_post"]);
            t.insert(
                0,
                json!({"type": TOOL_SEARCH_TYPE, "name": TOOL_SEARCH_NAME}),
            );
            t
        };
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "searching"},
                    search_call_block("srv_1"),
                ],
            }),
            json!({
                "role": "user",
                "content": [search_result_block("srv_1", vec![tool_ref("Slack_post")])],
            }),
        ];
        let before = messages.clone();
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    fn web_call_block(id: &str) -> Value {
        json!({"type": "server_tool_use", "id": id, "name": "web_search"})
    }

    fn web_result_block(id: &str) -> Value {
        json!({"type": "web_search_tool_result", "tool_use_id": id, "content": []})
    }

    #[test]
    fn web_search_split_pair_is_untouched() {
        // Pairing generalizes past tool_search: a web result paired with
        // its call across messages is valid with no mechanism involved.
        let tools = many_tools(&["read"]);
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "looking up"}, web_call_block("ws_1")],
            }),
            json!({
                "role": "user",
                "content": [web_result_block("ws_1")],
            }),
        ];
        let before = messages.clone();
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn web_search_orphan_result_is_neutralized_generically() {
        // Same 400 shape as the tool_search orphan, other family: the
        // result goes, the unrelated call survives, and the placeholder
        // is the generic one (not the tool-search text, keeping the two
        // shapes distinguishable in cached prefixes).
        let tools = many_tools(&["read"]);
        let messages = vec![json!({
            "role": "assistant",
            "content": [web_call_block("ws_9"), web_result_block("ws_orphan")],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 1);
        let content = out.messages[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["name"], json!("web_search"));
        assert_eq!(
            content[1]["text"],
            json!("[search result omitted: unavailable in this request]")
        );
    }

    #[test]
    fn family_mismatch_neutralizes_the_pair() {
        // Same id, wrong family: a tool_search result pointing at a
        // web_search call is corrupt either way, and a call left dangling
        // beside a neutralized result is its own 400 risk — so the pair
        // goes together, like every other orphan id.
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
                web_call_block("srv_x"),
                search_result_block("srv_x", vec![tool_ref("Slack_post")]),
            ],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 2);
        let content = out.messages[0]["content"].as_array().unwrap();
        assert!(content
            .iter()
            .all(|b| b.get("type").and_then(Value::as_str) == Some("text")));
    }

    #[test]
    fn missing_result_id_is_neutralized() {
        // A server result without tool_use_id cannot pair by construction;
        // forwarding it risks the same 400 class.
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
            "content": [{"type": "tool_search_tool_result", "content": []}],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 1);
        assert_eq!(out.messages[0]["content"][0]["type"], json!("text"));
    }

    #[test]
    fn neutralized_call_does_not_pair_a_later_result() {
        // No mechanism: the call in message 0 is neutralized, so the later
        // result with the same id must not treat it as a pair.
        let tools = many_tools(&["read"]);
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [search_call_block("srv_1")],
            }),
            json!({
                "role": "user",
                "content": [search_result_block("srv_1", vec![tool_ref("Slack_post")])],
            }),
        ];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 2);
        assert_eq!(out.messages[0]["content"][0]["type"], json!("text"));
        assert_eq!(out.messages[1]["content"][0]["type"], json!("text"));
    }

    #[test]
    fn unresolvable_references_neutralize_the_pair_in_place() {
        // Side-request tools array cannot resolve the reference: both the
        // result and its paired search call become text, and every block
        // keeps its index so signed thinking downstream is undisturbed.
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
        assert_eq!(out.neutralized, 2);
        let content = out.messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["text"], json!("hi"));
        assert_eq!(content[1]["type"], json!("text"));
        assert_eq!(content[2]["type"], json!("text"));
    }

    #[test]
    fn missing_search_tool_neutralizes_search_calls_but_keeps_web_search() {
        let tools = many_tools(&["read", "web_search"]);
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                search_call_block("srv_9"),
                {"type": "server_tool_use", "id": "ws_1", "name": "web_search", "input": {}},
            ],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        // Search call neutralized (no mechanism); web_search shares the block
        // type but is not a search call and has no orphan id → survives.
        assert_eq!(out.neutralized, 1);
        let content = out.messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], json!("text"));
        assert_eq!(content[1]["name"], json!("web_search"));
    }

    #[test]
    fn message_left_empty_keeps_its_slot_as_text() {
        // A turn that was pure tool-search bookkeeping keeps its message
        // slot: dropping it would shift message indexes for every signed
        // thinking block after it.
        let tools = many_tools(&["read"]);
        let messages = vec![json!({
            "role": "assistant",
            "content": [search_call_block("srv_1")],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 1);
        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0]["content"][0]["type"], json!("text"));
    }

    #[test]
    fn repair_leaves_neighbor_blocks_untouched() {
        // The repair swaps only the unsupportable pair: a thinking block
        // after it must be byte-identical and at the same index, so the
        // signed-reasoning guard still sees it unchanged.
        let thinking = json!({
            "type": "thinking",
            "thinking": "let me look that up",
            "signature": "sig-abc",
        });
        let tools = many_tools(&["read"]);
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                search_call_block("srv_1"),
                search_result_block("srv_1", vec![tool_ref("Slack_post")]),
                thinking,
            ],
        })];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 2);
        let content = out.messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[2], thinking);
    }

    #[test]
    fn repair_preserves_role_alternation() {
        // Dropping a message that was pure tool-search bookkeeping leaves
        // two adjacent same-role messages, which Anthropic 400s — and the
        // client replays the same transcript every turn, so the session
        // wedges permanently. The slot must survive as text.
        let tools = many_tools(&["read"]);
        let messages = vec![
            json!({"role": "user", "content": "find it"}),
            json!({
                "role": "assistant",
                "content": [
                    search_call_block("srv_1"),
                    search_result_block("srv_1", vec![tool_ref("gone_tool")]),
                ],
            }),
            json!({"role": "user", "content": "anything?"}),
        ];
        let out = strip_unsupported_blocks(messages, &tools);
        assert_eq!(out.neutralized, 2);
        assert_eq!(out.messages.len(), 3);
        let roles: Vec<&str> = out
            .messages
            .iter()
            .map(|m| m.get("role").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
        assert!(out.messages[1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b.get("type").and_then(Value::as_str) == Some("text")));
    }

    #[test]
    fn non_list_content_passes_through() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let before = messages.clone();
        let out = strip_unsupported_blocks(messages, &many_tools(&["read"]));
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    // ── core-tools env override ──

    #[test]
    fn unresolved_env_returns_defaults() {
        let core = resolved_core_tools_in(None);
        assert_eq!(core.len(), CORE_TOOLS.len());
        for t in CORE_TOOLS {
            assert!(core.iter().any(|c| c == t), "{t}");
        }
    }

    #[test]
    fn empty_env_defers_everything_non_typed() {
        let core = resolved_core_tools_in(Some(""));
        assert!(core.contains(&"toolsearch".to_string()));
        // With an empty set, a default-core tool is no longer resident.
        let tools = many_tools(&[
            "Bash",
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "Task",
            "WebFetch",
            "Skill",
            "TodoWrite",
            "Extra_a",
            "Extra_b",
        ]);
        let out = inject_deferral_with_core(tools, &core);
        assert!(out.changed);
        let names: Vec<&str> = out
            .deferred
            .iter()
            .map(|t| t.get("name").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert!(names.contains(&"Bash"), "{names:?}");
    }

    #[test]
    fn custom_list_normalizes_entries() {
        let core = resolved_core_tools_in(Some(" Bash , _READ,,"));
        assert!(core.contains(&"bash".to_string()));
        assert!(core.contains(&"read".to_string()));
        assert!(core.contains(&"toolsearch".to_string()));
    }

    #[test]
    fn toolsearch_stays_resident_under_any_override() {
        for raw in [Some(""), Some("bash,read"), Some("toolsearch")] {
            let core = resolved_core_tools_in(raw);
            let tools = many_tools(&[
                "ToolSearch",
                "Bash",
                "Read",
                "Write",
                "Edit",
                "Glob",
                "Grep",
                "Task",
                "WebFetch",
                "Skill",
                "TodoWrite",
                "Extra_a",
            ]);
            assert!(tools.len() >= MIN_TOOLS);
            let out = inject_deferral_with_core(tools, &core);
            let deferred: Vec<&str> = out
                .deferred
                .iter()
                .map(|t| t.get("name").and_then(Value::as_str).unwrap_or(""))
                .collect();
            assert!(
                !deferred.iter().any(|n| resident_key(n) == "toolsearch"),
                "toolsearch deferred under {raw:?}: {deferred:?}"
            );
        }
    }

    #[test]
    fn default_run_books_no_core_deferral() {
        let out = inject_deferral(mixed_tools());
        assert!(out.changed);
        assert!(out.core_deferred.is_empty());
    }
}

//! PR-A4: deterministic tool-array pruning (subscription token reduction).
//!
//! The `tools[]` array is ~46% of a Claude Code request payload, and on a
//! typical session the overwhelming majority of it is whole MCP servers the
//! session never calls (browser automation, tmux, research tools, …). On a
//! subscription those tool schemas are re-sent every turn and re-read as a
//! cached prefix; dropping the ones the operator knows they aren't using
//! shrinks both the one-time cache-creation and every subsequent cache-read.
//!
//! Measured ceiling on a real 14-turn capture: pruning 81 tools to the 2
//! actually called cut weighted usage 469,643 → 325,769 (−30.6%), reducing
//! BOTH cache_creation and cache_read — so the win survives any cache-bucket
//! weighting.
//!
//! # Cache-safety contract
//!
//! Pruning mutates the `tools` prefix, so it is only cache-safe under two
//! preconditions the caller must honour:
//!
//! 1. **Determinism.** The pruned set MUST be identical every turn, derived
//!    only from operator config (a fixed allowlist / server drop-list), never
//!    from per-request state. A set that changes between turns busts the
//!    prefix cache exactly like the position-keyed live-zone bug did.
//! 2. **Marker preservation.** A tool carrying a top-level `cache_control`
//!    marker is a customer-placed cache breakpoint; removing it shifts cache
//!    scope. Such tools are never pruned regardless of policy.
//!
//! Survivors keep their original relative order, so combined with the existing
//! deterministic sort (PR-E1) the emitted prefix is byte-stable across turns.
//!
//! Unlike PR-E1/E3 this is **not** auto-enabled and **not** auth-gated by
//! force: it only fires when the operator explicitly configures a policy
//! (default-off, opt-in), because only the operator knows which tools their
//! session actually needs. Dropping a stable allowlist looks like normal
//! per-session tool variation (Claude Code users enable different MCP servers
//! every session), unlike the reordering PR-E1 deliberately avoids on
//! subscription.

use serde_json::Value;
use std::collections::BTreeSet;

/// Operator-configured pruning policy. Deterministic by construction: both
/// fields are fixed config, so the same input `tools[]` always yields the
/// same survivors.
#[derive(Debug, Clone, Default)]
pub struct PrunePolicy {
    /// Exact tool-name allowlist. When non-empty, ONLY tools whose name is in
    /// this set survive (the most aggressive mode). Takes precedence over
    /// `drop_mcp_servers`.
    pub keep_names: BTreeSet<String>,
    /// MCP server names to drop wholesale. A tool named `mcp__<server>__<fn>`
    /// is dropped when `<server>` is in this set. Used only when `keep_names`
    /// is empty (the conservative mode: never touches built-in tools).
    pub drop_mcp_servers: BTreeSet<String>,
}

impl PrunePolicy {
    /// True when the policy would never remove anything — the caller skips the
    /// JSON walk entirely so default-off requests are pure passthrough.
    pub fn is_noop(&self) -> bool {
        self.keep_names.is_empty() && self.drop_mcp_servers.is_empty()
    }
}

/// Extract a tool's name from either provider shape (Anthropic `name`,
/// OpenAI `function.name`). Returns `None` for malformed/unnamed tools, which
/// are always kept (we never drop something we can't identify).
fn tool_name(tool: &Value) -> Option<&str> {
    if let Some(name) = tool.get("name").and_then(Value::as_str) {
        return Some(name);
    }
    tool.get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
}

/// True if `name` is `mcp__<server>__<anything>` for some `<server>` in the
/// drop set. The MCP tool-name convention is `mcp__{server}__{tool}`.
fn matches_dropped_mcp_server(name: &str, drop_servers: &BTreeSet<String>) -> bool {
    let Some(rest) = name.strip_prefix("mcp__") else {
        return false;
    };
    // server is the segment up to the next `__`
    let Some(idx) = rest.find("__") else {
        return false;
    };
    let server = &rest[..idx];
    drop_servers.contains(server)
}

/// Decide whether a single tool survives the policy. A tool with a top-level
/// `cache_control` marker always survives (marker preservation).
fn should_keep(tool: &Value, policy: &PrunePolicy) -> bool {
    if tool.get("cache_control").is_some() {
        return true;
    }
    let Some(name) = tool_name(tool) else {
        // unidentifiable -> keep (never drop what we can't name)
        return true;
    };
    if !policy.keep_names.is_empty() {
        return policy.keep_names.contains(name);
    }
    !matches_dropped_mcp_server(name, &policy.drop_mcp_servers)
}

/// Prune `tools[]` in place per `policy`, preserving the relative order of
/// survivors. Returns the number of tools removed.
///
/// Deterministic and idempotent: re-running on already-pruned input removes
/// nothing and yields byte-identical bytes.
pub fn prune_tools(tools: &mut Vec<Value>, policy: &PrunePolicy) -> usize {
    if policy.is_noop() {
        return 0;
    }
    let before = tools.len();
    tools.retain(|tool| should_keep(tool, policy));
    before - tools.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(name: &str) -> Value {
        json!({"name": name, "input_schema": {"type": "object"}})
    }

    fn names(tools: &[Value]) -> Vec<String> {
        tools
            .iter()
            .map(|x| tool_name(x).unwrap().to_string())
            .collect()
    }

    #[test]
    fn keep_allowlist_drops_everything_else() {
        let mut tools = vec![t("Read"), t("Bash"), t("WebSearch")];
        let policy = PrunePolicy {
            keep_names: ["Read", "Bash"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let removed = prune_tools(&mut tools, &policy);
        assert_eq!(removed, 1);
        assert_eq!(names(&tools), vec!["Read", "Bash"]);
    }

    #[test]
    fn drop_mcp_server_removes_only_that_server() {
        let mut tools = vec![
            t("Read"),
            t("mcp__chrome__click"),
            t("mcp__chrome__navigate"),
            t("mcp__tmux__capture"),
            t("Bash"),
        ];
        let policy = PrunePolicy {
            drop_mcp_servers: ["chrome"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let removed = prune_tools(&mut tools, &policy);
        assert_eq!(removed, 2);
        assert_eq!(names(&tools), vec!["Read", "mcp__tmux__capture", "Bash"]);
    }

    #[test]
    fn never_drops_tool_with_cache_control_marker() {
        let mut marked = t("WebSearch");
        marked["cache_control"] = json!({"type": "ephemeral"});
        let mut tools = vec![t("Read"), marked];
        let policy = PrunePolicy {
            keep_names: ["Read"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let removed = prune_tools(&mut tools, &policy);
        assert_eq!(
            removed, 0,
            "marker-bearing tool must survive even off-allowlist"
        );
        assert_eq!(names(&tools), vec!["Read", "WebSearch"]);
    }

    #[test]
    fn preserves_order_and_is_idempotent() {
        let policy = PrunePolicy {
            drop_mcp_servers: ["chrome"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let mut tools = vec![t("Bash"), t("mcp__chrome__x"), t("Read")];
        prune_tools(&mut tools, &policy);
        assert_eq!(names(&tools), vec!["Bash", "Read"]);
        let snapshot = tools.clone();
        let removed2 = prune_tools(&mut tools, &policy);
        assert_eq!(removed2, 0);
        assert_eq!(tools, snapshot, "second pass must be byte-identical");
    }

    #[test]
    fn noop_policy_changes_nothing() {
        let policy = PrunePolicy::default();
        assert!(policy.is_noop());
        let mut tools = vec![t("Read"), t("mcp__chrome__x")];
        let removed = prune_tools(&mut tools, &policy);
        assert_eq!(removed, 0);
        assert_eq!(names(&tools), vec!["Read", "mcp__chrome__x"]);
    }

    #[test]
    fn unnamed_tool_is_always_kept() {
        let mut tools = vec![json!({"input_schema": {}}), t("Bash")];
        let policy = PrunePolicy {
            keep_names: ["Read"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        prune_tools(&mut tools, &policy);
        assert_eq!(
            tools.len(),
            1,
            "unnamed kept, Bash dropped (not in allowlist)"
        );
    }
}

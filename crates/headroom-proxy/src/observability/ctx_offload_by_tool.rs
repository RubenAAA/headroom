//! Offloaded `tool_result` blocks, broken down by the tool that produced them.
//!
//! `ctx_offloaded_blocks_total` counts conversions and nothing else, so it
//! cannot answer the one question that matters after `--exclude-tools` stopped
//! gating `ctx_offload`: are the file and search results — `Read`, `Grep`,
//! `Glob` — actually converting now, or is the total still all `Bash`? A flat
//! count rises either way. This one names the tool.
//!
//! # Cardinality
//!
//! The tool name arrives in the request body, so a client can invent as many as
//! it likes and every distinct string would become a permanent time series in a
//! process-global registry. [`bucket`] therefore maps anything outside a fixed
//! allowlist — the old default exclusion list plus `Bash` — to `other`, capping
//! the label at nine values whatever the request says.

use std::sync::OnceLock;

use prometheus::{IntCounterVec, Opts, Registry};

use super::metric_names::{
    LABEL_TOOL, METRIC_CTX_OFFLOADED_BLOCKS_BY_TOOL_TOTAL,
    METRIC_CTX_OFFLOADED_BLOCKS_BY_TOOL_TOTAL_HELP,
};

/// Every tool name this metric will ever emit, apart from `other`. The old
/// `--exclude-tools` default list, because those are the results the change was
/// made for, plus `Bash` because it is the bulk of what already offloaded and
/// is the baseline the rest is read against.
const KNOWN_TOOLS: [&str; 8] = [
    "Read",
    "Grep",
    "Glob",
    "Write",
    "Edit",
    "WebSearch",
    "WebFetch",
    "Bash",
];

/// The `other` bucket, also used when the `tool_use` pairing found no name.
const OTHER: &str = "other";

/// Collapse a request-supplied tool name onto the bounded label set.
fn bucket(tool: &str) -> &'static str {
    KNOWN_TOOLS
        .into_iter()
        .find(|known| *known == tool)
        .unwrap_or(OTHER)
}

fn offloaded_by_tool(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                METRIC_CTX_OFFLOADED_BLOCKS_BY_TOOL_TOTAL,
                METRIC_CTX_OFFLOADED_BLOCKS_BY_TOOL_TOTAL_HELP,
            ),
            &[LABEL_TOOL],
        )
        .expect("ctx_offloaded_blocks_by_tool_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("ctx_offloaded_blocks_by_tool_total registers exactly once");
        c
    })
}

/// Record one offloaded block produced by `tool`. Called from
/// `ctx_offload::offload_anthropic_request` where a block actually converts.
pub fn observe_offloaded_tool(tool: &str) {
    offloaded_by_tool(super::prometheus::registry())
        .with_label_values(&[bucket(tool)])
        .inc();
}

/// Blocks offloaded for `tool` so far, after bucketing. Used by tests.
pub fn offloaded_tool_get(tool: &str) -> u64 {
    offloaded_by_tool(super::prometheus::registry())
        .with_label_values(&[bucket(tool)])
        .get()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deltas, not absolutes: the registry is process-global and `cargo test`
    /// runs this binary's tests in parallel, so another test offloading a
    /// `Read` mid-assertion would otherwise fail this one at random.
    #[test]
    fn a_known_tool_counts_under_its_own_name() {
        let before = offloaded_tool_get("Read");
        let before_other = offloaded_tool_get(OTHER);
        observe_offloaded_tool("Read");
        assert_eq!(offloaded_tool_get("Read"), before + 1);
        assert_eq!(offloaded_tool_get(OTHER), before_other, "not bucketed");
    }

    /// The cardinality guard. Tool names come from the request body, so an MCP
    /// server or a client with per-call tool names could otherwise mint an
    /// unbounded number of series that never get collected.
    #[test]
    fn an_unknown_tool_collapses_into_other() {
        let before = offloaded_tool_get(OTHER);
        observe_offloaded_tool("mcp__some__server__tool");
        observe_offloaded_tool("");
        assert_eq!(offloaded_tool_get(OTHER), before + 2);
    }
}

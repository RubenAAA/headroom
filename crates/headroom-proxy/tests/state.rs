//! Merged integration suites: one test binary for this group
//! instead of one per file. Link time dominates full-suite
//! rebuilds and every extra binary re-links the whole tree —
//! see AGENTS.md "Testing (fast loop)". Suites live in
//! `tests/suites/state/` (a plain directory, invisible to
//! cargo's auto-discovery); this file is the only target.
//! Each suite shares the umbrella's `common`.

#[path = "common/mod.rs"]
mod common;

#[path = "suites/state/memory_round_trip.rs"]
mod memory_round_trip;

#[path = "suites/state/memory_real_corpus.rs"]
mod memory_real_corpus;

#[path = "suites/state/memory_routed_mixed_turn.rs"]
mod memory_routed_mixed_turn;

#[path = "suites/state/ctx_endpoints.rs"]
mod ctx_endpoints;

#[path = "suites/state/e2e_simulators.rs"]
mod e2e_simulators;

#[path = "suites/state/integration_offload_arrived_history.rs"]
mod integration_offload_arrived_history;

#[path = "suites/state/integration_offload_preview_cap.rs"]
mod integration_offload_preview_cap;

#[path = "suites/state/integration_offload_tool_use.rs"]
mod integration_offload_tool_use;

#[path = "suites/state/integration_prior_thinking.rs"]
mod integration_prior_thinking;

#[path = "suites/state/withdrawn_scaffolding_span_alignment.rs"]
mod withdrawn_scaffolding_span_alignment;

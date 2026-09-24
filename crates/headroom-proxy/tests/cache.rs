//! Merged integration suites: one test binary for this group
//! instead of one per file. Link time dominates full-suite
//! rebuilds and every extra binary re-links the whole tree —
//! see AGENTS.md "Testing (fast loop)". Suites live in
//! `tests/suites/cache/` (a plain directory, invisible to
//! cargo's auto-discovery); this file is the only target.
//! Each suite shares the umbrella's `common`.

#[path = "common/mod.rs"]
mod common;

#[path = "suites/cache/cache_key_contract.rs"]
mod cache_key_contract;

#[path = "suites/cache/ctx_cache_stability.rs"]
mod ctx_cache_stability;

#[path = "suites/cache/integration_b1_cache_ttl.rs"]
mod integration_b1_cache_ttl;

#[path = "suites/cache/integration_b2_tool_order.rs"]
mod integration_b2_tool_order;

#[path = "suites/cache/integration_e4_openai_cache_key.rs"]
mod integration_e4_openai_cache_key;

#[path = "suites/cache/integration_tool_roster_pin.rs"]
mod integration_tool_roster_pin;

#[path = "suites/cache/integration_tool_sort.rs"]
mod integration_tool_sort;

#[path = "suites/cache/integration_tool_invariant.rs"]
mod integration_tool_invariant;

#[path = "suites/cache/integration_prefix_replay.rs"]
mod integration_prefix_replay;

#[path = "suites/cache/integration_prefix_adoption.rs"]
mod integration_prefix_adoption;

#[path = "suites/cache/integration_shared_scaffolding_prefix.rs"]
mod integration_shared_scaffolding_prefix;

#[path = "suites/cache/confirmed_prefix_floor.rs"]
mod confirmed_prefix_floor;

#[path = "suites/cache/context_editing_inject.rs"]
mod context_editing_inject;

#[path = "suites/cache/ctx_cross_session_seed.rs"]
mod ctx_cross_session_seed;

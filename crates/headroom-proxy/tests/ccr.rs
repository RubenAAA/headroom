//! Merged integration suites: one test binary for this group
//! instead of one per file. Link time dominates full-suite
//! rebuilds and every extra binary re-links the whole tree —
//! see AGENTS.md "Testing (fast loop)". Suites live in
//! `tests/suites/ccr/` (a plain directory, invisible to
//! cargo's auto-discovery); this file is the only target.
//! Each suite shares the umbrella's `common`.

#[path = "common/mod.rs"]
mod common;

#[path = "suites/ccr/integration_ccr_routed.rs"]
mod integration_ccr_routed;

#[path = "suites/ccr/integration_ccr_streaming.rs"]
mod integration_ccr_streaming;

#[path = "suites/ccr/integration_responses_buffered_ccr.rs"]
mod integration_responses_buffered_ccr;

#[path = "suites/ccr/integration_responses_streaming.rs"]
mod integration_responses_streaming;

#[path = "suites/ccr/memory_continuation.rs"]
mod memory_continuation;

#[path = "suites/ccr/continuation_cache_prefix.rs"]
mod continuation_cache_prefix;

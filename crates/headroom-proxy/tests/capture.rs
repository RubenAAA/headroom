//! Merged integration suites: one test binary for this group
//! instead of one per file. Link time dominates full-suite
//! rebuilds and every extra binary re-links the whole tree —
//! see AGENTS.md "Testing (fast loop)". Suites live in
//! `tests/suites/capture/` (a plain directory, invisible to
//! cargo's auto-discovery); this file is the only target.
//! Each suite shares the umbrella's `common`.

#[path = "common/mod.rs"]
mod common;

#[path = "suites/capture/integration_cache_control.rs"]
mod integration_cache_control;

#[path = "suites/capture/integration_cache_drift.rs"]
mod integration_cache_drift;

#[path = "suites/capture/integration_compression.rs"]
mod integration_compression;

#[path = "suites/capture/integration_e3_anthropic_cache_control.rs"]
mod integration_e3_anthropic_cache_control;

#[path = "suites/capture/integration_volatile_detector.rs"]
mod integration_volatile_detector;

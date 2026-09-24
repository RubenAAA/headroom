//! Merged integration suites: one test binary for this group
//! instead of one per file. Link time dominates full-suite
//! rebuilds and every extra binary re-links the whole tree —
//! see AGENTS.md "Testing (fast loop)". Suites live in
//! `tests/suites/routing/` (a plain directory, invisible to
//! cargo's auto-discovery); this file is the only target.
//! Each suite shares the umbrella's `common`.

#[path = "common/mod.rs"]
mod common;

#[path = "suites/routing/routed_early_stream_retry.rs"]
mod routed_early_stream_retry;

#[path = "suites/routing/integration_sidecar.rs"]
mod integration_sidecar;

#[path = "suites/routing/integration_stampede_gate.rs"]
mod integration_stampede_gate;

#[path = "suites/routing/integration_concurrency_cap.rs"]
mod integration_concurrency_cap;

#[path = "suites/routing/integration_stream_lanes.rs"]
mod integration_stream_lanes;

#[path = "suites/routing/chat_dedup_stream_gate.rs"]
mod chat_dedup_stream_gate;

#[path = "suites/routing/compression_decision_gate.rs"]
mod compression_decision_gate;

#[path = "suites/routing/early_reminder_drift_proof.rs"]
mod early_reminder_drift_proof;

#[path = "suites/routing/integration_http.rs"]
mod integration_http;

#[path = "suites/routing/integration_nonstreaming_outcome.rs"]
mod integration_nonstreaming_outcome;

#[path = "suites/routing/integration_beta_header_sticky.rs"]
mod integration_beta_header_sticky;

#[path = "suites/routing/integration_upstream_override_ssrf.rs"]
mod integration_upstream_override_ssrf;

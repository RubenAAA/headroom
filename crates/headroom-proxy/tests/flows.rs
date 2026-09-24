//! Merged integration suites: one test binary for this group
//! instead of one per file. Link time dominates full-suite
//! rebuilds and every extra binary re-links the whole tree —
//! see AGENTS.md "Testing (fast loop)". Suites live in
//! `tests/suites/flows/` (a plain directory, invisible to
//! cargo's auto-discovery); this file is the only target.
//! Each suite shares the umbrella's `common`.

#[path = "common/mod.rs"]
mod common;

#[path = "suites/flows/integration_health.rs"]
mod integration_health;

#[path = "suites/flows/integration_metrics.rs"]
mod integration_metrics;

#[path = "suites/flows/integration_headers.rs"]
mod integration_headers;

#[path = "suites/flows/integration_body.rs"]
mod integration_body;

#[path = "suites/flows/integration_body_size.rs"]
mod integration_body_size;

#[path = "suites/flows/integration_conversations.rs"]
mod integration_conversations;

#[path = "suites/flows/integration_digit_integrity.rs"]
mod integration_digit_integrity;

#[path = "suites/flows/integration_request_id.rs"]
mod integration_request_id;

#[path = "suites/flows/integration_schema_sort.rs"]
mod integration_schema_sort;

#[path = "suites/flows/proxy_auth_gate.rs"]
mod proxy_auth_gate;

#[path = "suites/flows/tls_client_wiring.rs"]
mod tls_client_wiring;

#[path = "suites/flows/integration_ws.rs"]
mod integration_ws;

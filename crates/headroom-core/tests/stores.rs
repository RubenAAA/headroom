//! Merged integration suites: one test binary for this group
//! instead of one per file. See AGENTS.md "Testing (fast loop)".
//! Suites live in `tests/suites/<group>/` (a plain directory,
//! invisible to cargo's auto-discovery); this file is the only target.

// Edition 2024 makes std::env::set_var and remove_var unsafe. Tests call them
// to set up config; non-test code stays free of unsafe.
#![allow(unsafe_code, clippy::undocumented_unsafe_blocks)]

#[path = "suites/stores/ccr_backends.rs"]
mod ccr_backends;

#[path = "suites/stores/ccr_roundtrip.rs"]
mod ccr_roundtrip;

#[path = "suites/stores/auth_mode.rs"]
mod auth_mode;

#[path = "suites/stores/recommendations_loader.rs"]
mod recommendations_loader;

#[path = "suites/stores/read_protection_wiring.rs"]
mod read_protection_wiring;

#[path = "suites/stores/kompress_parity.rs"]
mod kompress_parity;

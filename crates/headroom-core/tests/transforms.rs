//! Merged integration suites: one test binary for this group
//! instead of one per file. See AGENTS.md "Testing (fast loop)".
//! Suites live in `tests/suites/<group>/` (a plain directory,
//! invisible to cargo's auto-discovery); this file is the only target.

#[path = "suites/transforms/live_zone_dispatch.rs"]
mod live_zone_dispatch;

#[path = "suites/transforms/live_zone_all_messages.rs"]
mod live_zone_all_messages;

#[path = "suites/transforms/live_zone_ccr.rs"]
mod live_zone_ccr;

#[path = "suites/transforms/live_zone_thresholds.rs"]
mod live_zone_thresholds;

#[path = "suites/transforms/live_zone_token_validation.rs"]
mod live_zone_token_validation;

#[path = "suites/transforms/code_compressor_anchor.rs"]
mod code_compressor_anchor;

#[path = "suites/transforms/code_compressor_parity.rs"]
mod code_compressor_parity;

#[path = "suites/transforms/code_compressor_perl.rs"]
mod code_compressor_perl;

#[path = "suites/transforms/code_aware_off_arm.rs"]
mod code_aware_off_arm;

#[path = "suites/transforms/code_quality_eval.rs"]
mod code_quality_eval;

#[path = "suites/transforms/tokenizer_proptest.rs"]
mod tokenizer_proptest;

#[path = "suites/transforms/cache_control.rs"]
mod cache_control;

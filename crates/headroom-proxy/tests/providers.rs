//! Merged integration suites: one test binary for this group
//! instead of one per file. Link time dominates full-suite
//! rebuilds and every extra binary re-links the whole tree —
//! see AGENTS.md "Testing (fast loop)". Suites live in
//! `tests/suites/providers/` (a plain directory, invisible to
//! cargo's auto-discovery); this file is the only target.
//! Each suite shares the umbrella's `common`.

#[path = "common/mod.rs"]
mod common;

#[path = "suites/providers/integration_bedrock_authmode.rs"]
mod integration_bedrock_authmode;

#[path = "suites/providers/integration_bedrock_invoke.rs"]
mod integration_bedrock_invoke;

#[path = "suites/providers/integration_bedrock_metrics.rs"]
mod integration_bedrock_metrics;

#[path = "suites/providers/integration_bedrock_streaming.rs"]
mod integration_bedrock_streaming;

#[path = "suites/providers/integration_vertex_raw_predict.rs"]
mod integration_vertex_raw_predict;

#[path = "suites/providers/integration_foundry.rs"]
mod integration_foundry;

#[path = "suites/providers/integration_anthropic_batch.rs"]
mod integration_anthropic_batch;

#[path = "suites/providers/integration_anthropic_model_sanitize.rs"]
mod integration_anthropic_model_sanitize;

#[path = "suites/providers/integration_chat_completions.rs"]
mod integration_chat_completions;

#[path = "suites/providers/integration_responses.rs"]
mod integration_responses;

#[path = "suites/providers/integration_responses_additional_tools.rs"]
mod integration_responses_additional_tools;

#[path = "suites/providers/integration_count_tokens.rs"]
mod integration_count_tokens;

#[path = "suites/providers/integration_spark_context.rs"]
mod integration_spark_context;

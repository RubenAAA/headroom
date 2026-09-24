//! Merged integration suites: one test binary for this group
//! instead of one per file. Link time dominates full-suite
//! rebuilds and every extra binary re-links the whole tree —
//! see AGENTS.md "Testing (fast loop)". Suites live in
//! `tests/suites/sse/` (a plain directory, invisible to
//! cargo's auto-discovery); this file is the only target.
//! Each suite shares the umbrella's `common`.

#[path = "common/mod.rs"]
mod common;

#[path = "suites/sse/sse_anthropic.rs"]
mod sse_anthropic;

#[path = "suites/sse/sse_framing.rs"]
mod sse_framing;

#[path = "suites/sse/sse_openai_chat.rs"]
mod sse_openai_chat;

#[path = "suites/sse/sse_openai_responses.rs"]
mod sse_openai_responses;

#[path = "suites/sse/integration_sse.rs"]
mod integration_sse;

#[path = "suites/sse/integration_inband_sse_retry.rs"]
mod integration_inband_sse_retry;

#[path = "suites/sse/integration_stream_incomplete.rs"]
mod integration_stream_incomplete;

#[path = "suites/sse/integration_stream_drop_retry.rs"]
mod integration_stream_drop_retry;

#[path = "suites/sse/integration_nonstream_sse_answer.rs"]
mod integration_nonstream_sse_answer;

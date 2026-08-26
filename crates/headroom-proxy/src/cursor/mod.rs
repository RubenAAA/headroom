//! Running Cursor's `agent` CLI as an Anthropic Messages provider.
//!
//! Cursor has no REST model API — the desktop client talks Connect RPC over
//! protobuf behind an `x-cursor-checksum` anti-abuse header — so the only
//! supported way to reach its models on a subscription is the `agent` CLI. That
//! makes this a subprocess transport rather than an HTTP upstream.
pub(crate) mod agent;
pub mod bridge;
pub mod endpoint;
pub(crate) mod handler;
pub(crate) mod translate;
pub(crate) mod turn;

//! Shared machinery for the routed (non-`forward_http`) request paths.

pub(crate) mod auth;
pub(crate) mod ccr;
pub(crate) mod early_stream_retry;
pub(crate) mod outcome;
pub(crate) mod prepare;
pub(crate) mod quirks;
pub(crate) mod redaction;
pub(crate) mod response_arms;
pub(crate) mod retry;
pub(crate) mod routing;
pub(crate) mod sidecar;
pub(crate) mod tool_alias;
pub(crate) mod transforms;
pub(crate) mod translation;
pub(crate) mod upstream_gate;
pub(crate) mod zen_hold;

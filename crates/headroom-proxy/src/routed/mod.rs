//! Shared machinery for the routed (non-`forward_http`) request paths.

pub(crate) mod auth;
pub(crate) mod ccr;
pub(crate) mod outcome;
pub(crate) mod prepare;
pub(crate) mod redaction;
pub(crate) mod response_arms;
pub(crate) mod retry;
pub(crate) mod routing;
pub(crate) mod sidecar;
pub(crate) mod transforms;
pub(crate) mod translation;

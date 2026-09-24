//! The stages of [`super::forward_http`], split by phase: request stages
//! and compression, CTX inject/offload, pre-send observation, the buffered
//! send and its retries, memory/CCR tool plumbing, session analysis, and
//! response assembly.
//!
//! Each child module does `use super::*`, which reaches the proxy's items
//! through the glob import below, and is re-exported here, so callers keep
//! writing `forward::name`.

mod buffered_send;
mod compression_stage;
mod ctx;
mod memory;
mod presend;
mod response;
mod session;
mod stages;

// Globs cap each item at its own visibility: `pub` items stay public
// API, the rest stay in-crate. Modules with no `pub` item would
// otherwise warn that they re-export nothing.
#[allow(unused_imports)]
pub use self::{
    buffered_send::*, compression_stage::*, ctx::*, memory::*, presend::*, response::*, session::*,
    stages::*,
};

use super::*;

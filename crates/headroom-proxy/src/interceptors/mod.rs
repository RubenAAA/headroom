//! Tool-result interceptors.
//!
//! An interceptor rewrites a single tool_result's text before it reaches the
//! model. Each interceptor is self-contained: implement the [`ToolResultInterceptor`]
//! trait, register it, and the proxy pipeline calls it automatically.
//!
//! Mirrors Python's `headroom.proxy.interceptors`.

pub mod astgrep;
pub mod base;

pub use base::{
    apply_to_messages, InterceptionResult, ToolResultInterceptor, TransformSpan, INTERCEPTORS,
};

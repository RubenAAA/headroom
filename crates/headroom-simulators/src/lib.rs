//! Deterministic provider simulators for local and CI Headroom validation.
//!
//! The crate is intentionally standalone: it behaves like a provider upstream
//! that Headroom can proxy to, but it never calls a real LLM service.

pub mod application;
pub mod config;
pub mod domain;
pub mod presentation;

pub use application::Simulator;
pub use config::{SimulatorConfig, load_config};
pub use domain::{ProviderPath, SimulatedResponse};
pub use presentation::build_app;

//! Helpers shared by tests in more than one module.
//!
//! `EventCapture` reads back the `tracing` events a booked turn emits, and
//! `test_state` builds an `AppState` whose savings ledger points at a
//! throwaway file rather than the developer's real one.

use crate::proxy::AppState;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(crate) struct EventCapture(pub(crate) Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        use tracing::field::{Field, Visit};
        struct Visitor(String);
        impl Visit for Visitor {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                self.0.push_str(&format!("{}={value} ", field.name()));
            }
        }
        let mut visitor = Visitor(String::new());
        event.record(&mut visitor);
        self.0.lock().unwrap().push(visitor.0);
    }
}

/// Point the durable savings ledger at a throwaway file.
///
/// `emit_request_outcome` appends to it whenever a turn saved tokens, and
/// it resolves to `~/.headroom/savings_events.jsonl` by default — so
/// without this the tests below write fake savings into the developer's
/// real ledger, which `headroom savings` then reports. The temp dir is
/// leaked deliberately: the path has to stay valid for the whole test
/// binary, and the OS reclaims it.
pub(crate) fn test_state(configure: impl FnOnce(&mut crate::config::Config)) -> AppState {
    let mut config =
        crate::config::Config::for_test(url::Url::parse("http://upstream:8080").unwrap());
    configure(&mut config);
    AppState {
        started_at: std::time::Instant::now(),
        config: std::sync::Arc::new(config),
        client: reqwest::Client::new(),
        bedrock_credentials: None,
        drift_state: crate::cache_stabilization::drift_detector::DriftState::new(8),
        outbound_drift_state: crate::cache_stabilization::drift_detector::DriftState::new(8),
        tool_order_state: crate::cache_stabilization::tool_order::ToolOrderStore::default(),
        beta_sticky: crate::cache_stabilization::beta_sticky::BetaStickyState::new(8),
        replay_store: crate::cache_stabilization::prefix_replay::SessionReplayStore::new(8),
        working_dir_pins: crate::cache_stabilization::working_dir::WorkingDirPins::new(8),
        usage_observer: std::sync::Arc::new(
            crate::cache_stabilization::usage_observer::UsageObserver::new(),
        ),
        codex_rate_limits: crate::codex_rate_limits::CodexRateLimitStore::new(),
        ctx_observer: None,
        ctx_offload: None,
        ctx_inject: None,
        ccr_context_tracker: None,
        cost_tracker: std::sync::Arc::new(headroom_core::cost_tracker::CostTracker::new(
            None, "monthly",
        )),
        savings_tracker: std::sync::Arc::new(
            headroom_core::savings_tracker::SavingsTracker::new(None, false),
        ),
        request_logger: std::sync::Arc::new(crate::request_logger::RequestLogger::new(None)),
        vertex_token_source: std::sync::Arc::new(crate::vertex::StaticTokenSource::new(
            "test".to_string(),
        )),
        dynamic_upstream: crate::cc_switch_reconciler::new_dynamic_upstream(),
        ws_sessions: std::sync::Arc::new(std::sync::Mutex::new(
            crate::ws_session_registry::WebSocketSessionRegistry::new(),
        )),
        rate_limiter: None,
        semantic_cache: None,
        memory_handler: None,
        probe_recorder: None,
        compression_feedback: None,
        trusted_gateway_cidrs: vec![],
        background_compressor: None,
        compression_failure_action: crate::compression_failure::CompressionFailureAction {
            refuse: false,
            reason: "test".into(),
            frame_bytes: 0,
        },
        batch_context_store: std::sync::Arc::new(headroom_core::ccr::BatchContextStore::new(
            std::time::Duration::from_secs(86_400),
            10_000,
        )),
    }
}

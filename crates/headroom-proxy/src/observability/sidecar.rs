//! Spinner-text sidecars answered on a shrunk request, by kind.
//!
//! One increment per request that [`crate::sidecar`] recognised and diverted.
//! The count is the denominator for the saving: each one used to read the whole
//! conversation prefix and write a cache tail, and each one used to leave a
//! stored prefix that made the following real turn look like a re-cache.
//!
//! `kind` comes from a closed set in `sidecar`, never from request input.

use std::sync::OnceLock;

use prometheus::{IntCounterVec, Opts, Registry};

use super::metric_names::{
    LABEL_KIND, METRIC_PROXY_SIDECAR_TOTAL, METRIC_PROXY_SIDECAR_TOTAL_HELP,
};

fn sidecars(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(METRIC_PROXY_SIDECAR_TOTAL, METRIC_PROXY_SIDECAR_TOTAL_HELP),
            &[LABEL_KIND],
        )
        .expect("proxy_sidecar_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_sidecar_total registers exactly once");
        c
    })
}

/// Record one detected sidecar of `kind`.
pub fn observe_detected(kind: &str) {
    sidecars(super::prometheus::registry())
        .with_label_values(&[kind])
        .inc();
}

/// Sidecars of `kind` seen so far. Used by tests.
pub fn detected_get(kind: &str) -> u64 {
    sidecars(super::prometheus::registry())
        .with_label_values(&[kind])
        .get()
}

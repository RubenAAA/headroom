//! Whether the tail-breakpoint move is reaching real requests.
//!
//! The stage is a no-op on most traffic by design: the client already puts its
//! message marker on the last content block on 97% of captured requests, and
//! the stage refuses any other placement it was not measured against. So
//! "moved nothing all day" is the expected reading and is indistinguishable
//! from "never ran" without a count of both. `applied` against `skipped` is
//! the check — a few percent applied is healthy, zero over a long run means
//! the stage is not being reached, and a sharp rise means the client changed
//! where it puts its marker.
use std::sync::OnceLock;

use prometheus::{IntCounterVec, Opts, Registry};

use super::metric_names::{
    LABEL_OUTCOME, METRIC_PROXY_CACHE_BREAKPOINT_SPREAD_TOTAL,
    METRIC_PROXY_CACHE_BREAKPOINT_SPREAD_TOTAL_HELP,
};

fn spreads(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                METRIC_PROXY_CACHE_BREAKPOINT_SPREAD_TOTAL,
                METRIC_PROXY_CACHE_BREAKPOINT_SPREAD_TOTAL_HELP,
            ),
            &[LABEL_OUTCOME],
        )
        .expect("proxy_cache_tail_breakpoint_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_cache_tail_breakpoint_total registers exactly once");
        c
    })
}

/// Record one request the stage looked at, and whether it moved the marker.
pub fn observe(applied: bool) {
    let outcome = if applied { "applied" } else { "skipped" };
    spreads(super::prometheus::registry())
        .with_label_values(&[outcome])
        .inc();
}

/// Requests recorded under `outcome` so far. Used by tests.
pub fn spread_get(outcome: &str) -> u64 {
    spreads(super::prometheus::registry())
        .with_label_values(&[outcome])
        .get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_outcomes_are_counted_separately() {
        let (a, s) = (spread_get("applied"), spread_get("skipped"));
        observe(true);
        observe(false);
        observe(false);
        assert_eq!(spread_get("applied"), a + 1);
        assert_eq!(spread_get("skipped"), s + 2);
    }
}

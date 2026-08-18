//! What became of each buffered `headroom_retrieve`.
//!
//! The proxy answers a retrieval by posting a continuation upstream and
//! splicing the tool_result back. Two shapes broke that and both failed
//! quietly:
//!
//! - a turn that mixes the retrieval with a real client tool call cannot be
//!   continued, because appending the assistant message would leave the
//!   client's `tool_use` unanswered. The old code skipped the retrieval and
//!   the model got nothing.
//! - a continuation that upstream refused was a single attempt, so an
//!   overload burst cost the retrieval outright.
//!
//! Neither showed up in `ctx_retrieval_hits_total`, which counts the store
//! lookup and not whether the content ever reached the model. `outcome` here
//! is the terminal fate of the call: `continuation` and `spliced_mixed` both
//! mean the model was answered, `unresolved` means it was not.
use std::sync::OnceLock;

use prometheus::{IntCounter, IntCounterVec, Opts, Registry};

use super::metric_names::{
    METRIC_PROXY_CCR_CONTINUATION_RETRIES_TOTAL,
    METRIC_PROXY_CCR_CONTINUATION_RETRIES_TOTAL_HELP,
    METRIC_PROXY_CCR_RETRIEVAL_OUTCOMES_TOTAL, METRIC_PROXY_CCR_RETRIEVAL_OUTCOMES_TOTAL_HELP,
};

/// Answered by a continuation turn carrying a real tool_result.
pub const OUTCOME_CONTINUATION: &str = "continuation";
/// Answered as text, because a real client tool call shared the turn.
pub const OUTCOME_SPLICED_MIXED: &str = "spliced_mixed";
/// Not answered: out of rounds, upstream refused, or an unsupported shape.
pub const OUTCOME_UNRESOLVED: &str = "unresolved";

fn outcomes(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                METRIC_PROXY_CCR_RETRIEVAL_OUTCOMES_TOTAL,
                METRIC_PROXY_CCR_RETRIEVAL_OUTCOMES_TOTAL_HELP,
            ),
            &[super::metric_names::LABEL_OUTCOME],
        )
        .expect("proxy_ccr_retrieval_outcomes_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_ccr_retrieval_outcomes_total registers exactly once");
        c
    })
}

fn retries(registry: &Registry) -> &'static IntCounter {
    static COUNTER: OnceLock<IntCounter> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounter::new(
            METRIC_PROXY_CCR_CONTINUATION_RETRIES_TOTAL,
            METRIC_PROXY_CCR_CONTINUATION_RETRIES_TOTAL_HELP,
        )
        .expect("proxy_ccr_continuation_retries_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_ccr_continuation_retries_total registers exactly once");
        c
    })
}

/// Record `count` retrievals ending in `outcome`. `outcome` comes from the
/// three constants above, never from request input.
pub fn observe_outcome(outcome: &str, count: u64) {
    outcomes(super::prometheus::registry())
        .with_label_values(&[outcome])
        .inc_by(count);
}

/// Retrievals recorded under `outcome` so far. Used by tests.
pub fn outcome_get(outcome: &str) -> u64 {
    outcomes(super::prometheus::registry())
        .with_label_values(&[outcome])
        .get()
}

/// Record one continuation POST re-sent after a retryable failure.
pub fn observe_continuation_retry() {
    retries(super::prometheus::registry()).inc();
}

/// Continuation retries so far. Used by tests.
pub fn continuation_retries_get() -> u64 {
    retries(super::prometheus::registry()).get()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters live in a process-global registry every test in this
    /// binary shares, so read a delta rather than an absolute.
    #[test]
    fn outcomes_accumulate_per_label() {
        let before = outcome_get(OUTCOME_SPLICED_MIXED);
        observe_outcome(OUTCOME_SPLICED_MIXED, 2);
        assert_eq!(outcome_get(OUTCOME_SPLICED_MIXED), before + 2);
    }

    #[test]
    fn retries_count_one_at_a_time() {
        let before = continuation_retries_get();
        observe_continuation_retry();
        observe_continuation_retry();
        assert_eq!(continuation_retries_get(), before + 2);
    }
}

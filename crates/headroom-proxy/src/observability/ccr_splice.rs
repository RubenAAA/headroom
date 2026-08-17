//! Blocks the CCR splice refused to put back on the wire, by reason.
//!
//! `sse::ccr_stream` resolves a mid-stream `headroom_retrieve` with a second
//! upstream call and splices that call's content onto a turn the client is
//! already receiving. Some of that content must never be forwarded, and the
//! two interesting reasons are defects that cost a day to find:
//!
//! - `continuation_thinking` — reasoning signed for the continuation request.
//!   The client stores it and replays it next turn, where the signature cannot
//!   verify and the API refuses the whole conversation.
//! - `already_streamed` — a block the client already has, re-sent under a new
//!   index, including a `tool_use` id the client is already acting on.
//!
//! Both fail on the *next* request, so the log line at the splice and the
//! rejection are a turn apart and were never joined up. Counting them by
//! reason is what makes the pair visible: a rising
//! `continuation_thinking` count alongside a rising rejection rate
//! (`upstream_health`) names the cause without a capture corpus.
//!
//! `unresolved_proxy_tool` is the routine reason and is counted for the
//! denominator, not as a fault.

use std::sync::OnceLock;

use prometheus::{IntCounterVec, Opts, Registry};

use super::metric_names::{
    LABEL_REASON, METRIC_PROXY_CCR_SPLICE_DROPPED_BLOCKS_TOTAL,
    METRIC_PROXY_CCR_SPLICE_DROPPED_BLOCKS_TOTAL_HELP,
};

fn dropped_blocks(registry: &Registry) -> &'static IntCounterVec {
    static COUNTER: OnceLock<IntCounterVec> = OnceLock::new();
    COUNTER.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                METRIC_PROXY_CCR_SPLICE_DROPPED_BLOCKS_TOTAL,
                METRIC_PROXY_CCR_SPLICE_DROPPED_BLOCKS_TOTAL_HELP,
            ),
            &[LABEL_REASON],
        )
        .expect("proxy_ccr_splice_dropped_blocks_total descriptor is well-formed");
        registry
            .register(Box::new(c.clone()))
            .expect("proxy_ccr_splice_dropped_blocks_total registers exactly once");
        c
    })
}

/// Record `count` blocks dropped for `reason`. `reason` comes from a closed
/// set in `ccr_stream`, never from request input, so label cardinality is
/// bounded at three.
pub fn observe_dropped(reason: &str, count: u64) {
    dropped_blocks(super::prometheus::registry())
        .with_label_values(&[reason])
        .inc_by(count);
}

/// Blocks dropped for `reason` so far. Used by `/cache-health` and tests.
pub fn dropped_get(reason: &str) -> u64 {
    dropped_blocks(super::prometheus::registry())
        .with_label_values(&[reason])
        .get()
}

/// The two counts worth watching, for the health snapshot. Both should stay at
/// zero; neither has a legitimate cause.
pub fn unusable_blocks_get() -> u64 {
    dropped_get("continuation_thinking") + dropped_get("already_streamed")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A label of its own, because the counters live in a process-global
    /// registry that every test in this binary shares and `cargo test` runs
    /// them in parallel. This test used `already_streamed`, which
    /// [`unusable_blocks_get`] sums, so its +3 landed between the two reads in
    /// the test below and failed it — in the full suite only, which made it look
    /// like whatever else had just changed.
    const TEST_ONLY_REASON: &str = "test_only_accumulation";

    #[test]
    fn counts_accumulate_per_reason() {
        let before = dropped_get(TEST_ONLY_REASON);
        observe_dropped(TEST_ONLY_REASON, 3);
        assert_eq!(dropped_get(TEST_ONLY_REASON), before + 3);
    }

    /// The health summary must count both defect reasons and ignore the
    /// routine one — a proxy tool the continuation could not run is not a sign
    /// that anything is broken.
    #[test]
    fn the_summary_ignores_the_routine_reason() {
        let before = unusable_blocks_get();
        observe_dropped("unresolved_proxy_tool", 5);
        assert_eq!(unusable_blocks_get(), before);
        observe_dropped("continuation_thinking", 1);
        assert_eq!(unusable_blocks_get(), before + 1);
    }
}

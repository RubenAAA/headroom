//! usage_observer::summed — a turn whose usage the provider summed over
//! several sampling iterations.
use super::*;

impl UsageObserver {
    /// Close a turn whose usage spans more than one provider iteration.
    ///
    /// A server-side tool call (tool search, web search) makes the provider
    /// sample again inside one request and report usage summed over the
    /// iterations, so the read and write counts describe no single prefix. On
    /// 2026-09-29 the turn after such a call read normally, was scored against
    /// the summed figure and flagged as a recache: 12 of 12 residual
    /// `unexplained_after_replay` events, 68,000+ wasted tokens that were never
    /// lost. The turn is billed and ledgered like any other, but it is neither
    /// scored nor stored as the baseline, so the turn before it stays the one
    /// the next turn is compared against.
    pub fn complete_summed(
        &self,
        request_id: &str,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_write_ttl_split: Option<(u64, u64)>,
    ) {
        let now_instant = Instant::now();
        let mut inner = self.lock();
        Self::record_hit_rate(
            &mut inner,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            true,
        );
        let Some(pending) = Self::pop_pending(&mut inner, request_id) else {
            return;
        };
        Self::note_completion(&mut inner, &pending, now_instant);
        Self::record_savings_placement(
            request_id,
            &pending,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        );
        Self::record_cost_ledger(
            &mut inner,
            request_id,
            &pending,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_write_ttl_split,
        );
        tracing::info!(
            event = "usage_baseline_skipped",
            request_id = %request_id,
            conversation_key = %pending.conversation_key,
            reason = "summed_iterations",
            input_tokens = input_tokens,
            cache_read_input_tokens = cache_read_input_tokens,
            cache_creation_input_tokens = cache_creation_input_tokens,
            "turn usage spans several provider iterations; not used as a cache baseline"
        );
    }
}

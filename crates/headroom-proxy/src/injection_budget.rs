//! One byte ceiling shared by everything that appends to a turn.
//!
//! Three stages add content to the same request: CCR proactive expansion, ctx
//! recall injection, and memory injection. Each was bounded on its own terms —
//! expansion by a count of expansions, recall by a result count, memory by an
//! entry count and a token cap — and nothing summed them. Three stages that
//! each look small can still inflate one turn, and the per-stage counters all
//! report success while it happens.
//!
//! This is a per-request ceiling drawn down in the order the stages run, so a
//! stage that fires early cannot be starved by one that fires later, and the
//! total is bounded whatever the mix.
//!
//! # What is deliberately *not* clipped
//!
//! Ctx recall injection is decided once per conversation and replayed
//! byte-for-byte on every later turn (invariant I4). It sits in the cached
//! prefix, so clipping it on turn 40 would rewrite bytes the provider had
//! already cached and bust the prefix — costing far more than the injection
//! saves. Recall therefore *reserves* against the budget without being
//! truncated: it reports what it used so later stages see a smaller ceiling,
//! and is never itself cut.
//!
//! Expansion and memory both append to the live tail, which is re-sent every
//! turn anyway, so clipping them is cache-safe.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Default ceiling: bytes one request may gain across all injection stages.
///
/// 32 KB is roughly 8k tokens — enough for a resume snapshot plus a few
/// expansions, and small enough that a runaway stage is caught before it
/// doubles a request. Override with `--max-injection-bytes`.
pub const DEFAULT_MAX_INJECTION_BYTES: usize = 32_768;

/// Which stage is spending. Ordered as they run on the request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionStage {
    /// CCR proactive expansion — appends previously-offloaded content back.
    ProactiveExpansion,
    /// Ctx recall / resume injection — reserves, never clipped (see above).
    Recall,
    /// Memory injection — appends memory entries to the user tail.
    Memory,
}

impl InjectionStage {
    /// Label used in metrics and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProactiveExpansion => "proactive_expansion",
            Self::Recall => "recall",
            Self::Memory => "memory",
        }
    }

    /// Whether this stage's bytes may be truncated to fit.
    ///
    /// Recall lands in the cached prefix and is replayed verbatim, so cutting
    /// it busts the prefix it was meant to ride along with.
    pub fn is_clippable(self) -> bool {
        !matches!(self, Self::Recall)
    }
}

/// Per-request injection ceiling, drawn down as stages spend.
///
/// Shared by reference across the stages of one request; not shared between
/// requests. The atomic is for interior mutability behind a `&`, not for
/// cross-thread contention — one request draws down its own budget.
#[derive(Debug)]
pub struct InjectionBudget {
    remaining: AtomicUsize,
    total: usize,
    request_id: String,
}

impl InjectionBudget {
    /// A budget of `max_bytes`. Zero disables injection entirely, which is a
    /// legitimate way to turn all three stages off at once.
    pub fn new(max_bytes: usize) -> Self {
        Self::for_request(max_bytes, "")
    }

    /// A request-correlated budget for production paths. Keeping [`Self::new`]
    /// makes small unit tests terse; every live caller should use this form so
    /// clipping and overrun events join the rest of the turn.
    pub fn for_request(max_bytes: usize, request_id: impl Into<String>) -> Self {
        Self {
            remaining: AtomicUsize::new(max_bytes),
            total: max_bytes,
            request_id: request_id.into(),
        }
    }

    /// Bytes still available.
    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Relaxed)
    }

    /// Bytes spent so far.
    pub fn spent(&self) -> usize {
        self.total.saturating_sub(self.remaining())
    }

    /// Take `text` down to what the budget allows for `stage`, and charge it.
    ///
    /// Returns `None` when nothing fits — the caller should skip the stage
    /// rather than append an empty block. A clippable stage is cut at a line
    /// boundary where one exists, so a truncated block still reads as whole
    /// lines rather than ending mid-token.
    ///
    /// A non-clippable stage is charged in full and never cut: if it overruns,
    /// the budget floors at zero and every later stage is skipped instead.
    /// That is the intended trade — the uncuttable stage wins, and the ones
    /// that can yield do.
    pub fn take(&self, stage: InjectionStage, text: String) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        let available = self.remaining();

        // Non-clippable stages are settled first, before the empty-budget exit.
        // Dropping one is not a cheaper version of clipping it — it is the same
        // prefix bust. Recall sits in `messages[0]`, so a turn that omits it
        // rewrites byte zero of the conversation and re-caches everything.
        // Whether an earlier stage left 1 byte or 0 cannot decide that.
        if !stage.is_clippable() {
            if text.len() <= available {
                self.remaining.fetch_sub(text.len(), Ordering::Relaxed);
            } else {
                // Charge what is left and let it through whole. The prefix
                // stays byte-stable; later stages find an empty budget and
                // stand down.
                self.remaining.store(0, Ordering::Relaxed);
                tracing::debug!(
                    event = "injection_budget_overrun",
                    request_id = %self.request_id,
                    stage = stage.as_str(),
                    wanted = text.len(),
                    available,
                    "stage exceeded the injection budget but cannot be clipped; \
                     later stages will be skipped"
                );
            }
            return Some(text);
        }

        if available == 0 {
            self.record_clip(stage, text.len(), 0);
            return None;
        }

        if text.len() <= available {
            self.remaining.fetch_sub(text.len(), Ordering::Relaxed);
            return Some(text);
        }

        let cut = clip_at_line_boundary(&text, available);
        self.record_clip(stage, text.len(), cut.len());
        if cut.is_empty() {
            self.remaining.store(0, Ordering::Relaxed);
            return None;
        }
        self.remaining.fetch_sub(cut.len(), Ordering::Relaxed);
        Some(cut)
    }

    fn record_clip(&self, stage: InjectionStage, wanted: usize, kept: usize) {
        crate::observability::ctx_metrics::observe_injection_clipped(
            stage.as_str(),
            (wanted - kept) as u64,
        );
        tracing::info!(
            event = "injection_budget_clipped",
            request_id = %self.request_id,
            stage = stage.as_str(),
            wanted,
            kept,
            dropped = wanted - kept,
            "injection budget clipped a stage; raise --max-injection-bytes to keep it whole"
        );
    }
}

/// Longest prefix of `text` within `max_bytes`, cut on a line boundary when
/// there is one and always on a UTF-8 char boundary.
fn clip_at_line_boundary(text: &str, max_bytes: usize) -> String {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    match head.rfind('\n') {
        Some(pos) if pos > 0 => head[..pos + 1].to_string(),
        _ => head.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spending_draws_the_budget_down() {
        let budget = InjectionBudget::new(100);
        assert_eq!(
            budget.take(InjectionStage::Memory, "a".repeat(30)),
            Some("a".repeat(30))
        );
        assert_eq!(budget.remaining(), 70);
        assert_eq!(budget.spent(), 30);
    }

    #[test]
    fn production_budget_carries_request_correlation() {
        let budget = InjectionBudget::for_request(10, "req-budget");
        assert_eq!(budget.request_id, "req-budget");
    }

    /// The whole point: three stages that each fit on their own must not
    /// collectively exceed the ceiling.
    #[test]
    fn three_stages_cannot_together_exceed_the_ceiling() {
        let budget = InjectionBudget::new(100);
        budget.take(InjectionStage::ProactiveExpansion, "a".repeat(60));
        budget.take(InjectionStage::Recall, "b".repeat(30));
        let third = budget.take(InjectionStage::Memory, "c".repeat(50));
        assert_eq!(
            third.map(|t| t.len()),
            Some(10),
            "third stage clipped to fit"
        );
        assert_eq!(budget.remaining(), 0);
        assert!(budget.spent() <= 100);
    }

    #[test]
    fn an_exhausted_budget_skips_the_stage() {
        let budget = InjectionBudget::new(10);
        budget.take(InjectionStage::ProactiveExpansion, "a".repeat(10));
        assert_eq!(budget.take(InjectionStage::Memory, "b".repeat(5)), None);
    }

    /// Recall is replayed byte-for-byte into the cached prefix. Clipping it
    /// would rewrite bytes the provider already cached and bust the prefix,
    /// so it goes out whole and the stages that can yield do.
    #[test]
    fn recall_is_never_clipped_and_starves_later_stages_instead() {
        let budget = InjectionBudget::new(20);
        let recall = budget.take(InjectionStage::Recall, "r".repeat(50));
        assert_eq!(recall.map(|t| t.len()), Some(50), "recall goes out whole");
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.take(InjectionStage::Memory, "m".repeat(5)), None);
    }

    /// The case that burned a full re-cache on every flip: proactive expansion
    /// runs first and can spend the whole ceiling, and recall then found an
    /// empty budget and stood down. Recall lives in `messages[0]`, so the turns
    /// that dropped it rewrote byte zero and re-cached the conversation — then
    /// the next turn, with a smaller expansion, put it back and re-cached again.
    #[test]
    fn recall_survives_a_budget_an_earlier_stage_already_spent() {
        let budget = InjectionBudget::new(100);
        // Expansion is clippable and eats the ceiling whole.
        budget.take(InjectionStage::ProactiveExpansion, "e".repeat(500));
        assert_eq!(budget.remaining(), 0);

        let recall = budget.take(InjectionStage::Recall, "r".repeat(40));
        assert_eq!(
            recall.map(|t| t.len()),
            Some(40),
            "recall must go out whole even on an exhausted budget"
        );
    }

    #[test]
    fn clipping_prefers_a_line_boundary() {
        let budget = InjectionBudget::new(10);
        let text = "one\ntwo\nthree\n".to_string();
        assert_eq!(
            budget.take(InjectionStage::Memory, text),
            Some("one\ntwo\n".to_string())
        );
    }

    #[test]
    fn clipping_never_splits_a_utf8_char() {
        let budget = InjectionBudget::new(4);
        // Three-byte chars: a naive cut at 4 bytes would split the second.
        let got = budget
            .take(InjectionStage::Memory, "☃☃☃".to_string())
            .unwrap();
        assert_eq!(got, "☃", "cut lands on a char boundary");
    }

    #[test]
    fn a_zero_budget_turns_every_stage_off() {
        let budget = InjectionBudget::new(0);
        assert_eq!(budget.take(InjectionStage::Memory, "x".to_string()), None);
        assert_eq!(
            budget.take(InjectionStage::ProactiveExpansion, "y".to_string()),
            None
        );
    }
}

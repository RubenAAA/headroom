//! Per-block totals from a live-zone rewrite, shared by the three providers.
//!
//! Each provider dispatcher used to walk `manifest.block_outcomes` itself, and
//! the three walks had already drifted: Anthropic counted declined-no-shrink
//! blocks but not compressor errors, while the two OpenAI shapes counted
//! compressor errors but not declines. Both gaps were silent — the metric
//! simply never appeared for that provider. One walk, so one set of counters
//! fires whichever path the request took.

use headroom_core::transforms::{BlockAction, CompressionManifest};

use crate::compression::PerStrategyTokens;

/// What a live-zone rewrite produced, in the shape both the structured log and
/// `Outcome::Compressed` want.
pub(crate) struct ManifestTotals {
    pub(crate) original_bytes: usize,
    pub(crate) compressed_bytes: usize,
    pub(crate) original_tokens: usize,
    pub(crate) compressed_tokens: usize,
    /// Distinct strategies that actually shrank a block, in first-seen order.
    pub(crate) strategies: Vec<&'static str>,
    /// One entry per strategy; several blocks of the same strategy sum into it.
    /// The proxy emits one `proxy_compression_ratio_by_strategy` sample per
    /// entry, which is why an aggregate ratio will not do.
    pub(crate) per_strategy_tokens: Vec<PerStrategyTokens>,
    pub(crate) had_compressor_error: bool,
}

/// Walk the manifest, summing what compressed and counting what did not.
///
/// `path` names the request path for the error log; it is the only part of
/// this that varies by provider.
pub(crate) fn aggregate(
    manifest: &CompressionManifest,
    request_id: &str,
    path: &'static str,
) -> ManifestTotals {
    let mut totals = ManifestTotals {
        original_bytes: 0,
        compressed_bytes: 0,
        original_tokens: 0,
        compressed_tokens: 0,
        strategies: Vec::new(),
        per_strategy_tokens: Vec::new(),
        had_compressor_error: false,
    };

    for entry in &manifest.block_outcomes {
        match entry.action {
            BlockAction::Compressed {
                strategy,
                original_bytes,
                compressed_bytes,
                original_tokens,
                compressed_tokens,
            } => {
                totals.original_bytes += original_bytes;
                totals.compressed_bytes += compressed_bytes;
                totals.original_tokens += original_tokens;
                totals.compressed_tokens += compressed_tokens;
                totals.push_strategy(strategy);
                if let Some(slot) = totals
                    .per_strategy_tokens
                    .iter_mut()
                    .find(|s| s.strategy == strategy)
                {
                    slot.original_tokens += original_tokens;
                    slot.compressed_tokens += compressed_tokens;
                } else {
                    totals.per_strategy_tokens.push(PerStrategyTokens {
                        strategy,
                        original_tokens,
                        compressed_tokens,
                    });
                }
            }
            BlockAction::RejectedNotSmaller { strategy, .. } => {
                // The compressor ran and the tokenizer said the result was no
                // smaller, so the original stands. Attributed separately from a
                // decline: here the work was done and thrown away.
                crate::observability::record_compression_rejected_by_token_check(strategy);
            }
            BlockAction::NoCompressionApplied {
                declined_by: Some(ref strategy),
                ..
            } => {
                // The size gate declined the block before the tokenizer saw it.
                // If this rises while accepted compression holds steady the gate
                // is doing its job; if accepted compression falls with it, the
                // gate is declining work that pays.
                crate::observability::record_compression_declined_no_shrink(strategy.as_str());
            }
            BlockAction::CompressorError {
                strategy,
                ref error,
            } => {
                totals.had_compressor_error = true;
                tracing::error!(
                    event = "compression_error",
                    request_id = %request_id,
                    path = path,
                    strategy = strategy,
                    error = %error,
                    "compressor error on a block; that block reverts to original"
                );
            }
            _ => {}
        }
    }

    totals
}

impl ManifestTotals {
    /// Record a strategy tag once, keeping first-seen order.
    ///
    /// Callers stitch in the cache-stabilization tags (tool-array sort,
    /// cache-control auto-placement) this way, so dashboards attribute them to
    /// their own surface rather than to a live-zone compressor that never ran.
    pub(crate) fn push_strategy(&mut self, strategy: &'static str) {
        if !self.strategies.contains(&strategy) {
            self.strategies.push(strategy);
        }
    }
}

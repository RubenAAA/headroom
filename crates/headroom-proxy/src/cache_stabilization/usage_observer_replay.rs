//! usage_observer::replay — split from usage_observer.rs (pure move, no logic change).
use super::*;

/// Provenance for the histories compared by prefix replay.
///
/// Keep this explicit: a final-message difference is a branch/tail build only
/// when it came from the client histories entering the proxy. A comparison of
/// transformed/forwarded messages must never receive that attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayComparisonOrigin {
    InboundOriginalHistories,
}

/// Structured evidence from a prefix-replay decline.
///
/// The replay stage has the typed reason plus both message slices. Parking all
/// of the evidence here avoids collapsing `PrefixContentDiverged` to a string
/// before the response-side usage counters can distinguish a replaced live
/// tail from a deeper edit inside the cached prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaySkipEvidence {
    pub(super) reason: ReplaySkip,
    comparison_origin: ReplayComparisonOrigin,
    prior_message_count: Option<usize>,
    current_message_count: usize,
}

/// Evidence that a stored prefix was selected and actually serialized onto
/// the upstream request. This deliberately stops at the proxy/provider
/// boundary: it proves what the proxy sent, without guessing why the provider
/// subsequently failed to read all of it from cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayAppliedEvidence {
    pub(super) chain_id: u64,
    pub(super) breakpoints_placed: usize,
    pub(super) system_markers_dropped: usize,
}

impl ReplayAppliedEvidence {
    pub fn new(chain_id: u64, breakpoints_placed: usize, system_markers_dropped: usize) -> Self {
        Self {
            chain_id,
            breakpoints_placed,
            system_markers_dropped,
        }
    }
}

impl ReplaySkipEvidence {
    /// Message index where the prefix first differed, when that was the reason.
    ///
    /// The whole cost story turns on this number: an edit in the first quarter
    /// of the prefix destroys far more than one in the third, and the index is
    /// what separates "the client deleted mid-history" from "the opener churns
    /// every turn". It was computed and then dropped before reaching the log.
    pub fn first_diff_index(&self) -> Option<usize> {
        match self.reason {
            ReplaySkip::PrefixContentDiverged {
                first_diff_index, ..
            } => Some(first_diff_index),
            _ => None,
        }
    }

    /// Messages the stored turn carried, and this one carries. A count that
    /// holds steady or falls while the content changed is the deletion
    /// signature; one that grows is ordinary appending.
    pub fn message_counts(&self) -> (Option<usize>, usize) {
        (self.prior_message_count, self.current_message_count)
    }

    /// Evidence produced by comparing the prior and current inbound originals.
    pub fn from_inbound_original_histories(
        reason: ReplaySkip,
        prior: Option<&[serde_json::Value]>,
        current: &[serde_json::Value],
    ) -> Self {
        Self {
            reason,
            comparison_origin: ReplayComparisonOrigin::InboundOriginalHistories,
            prior_message_count: prior.map(|messages| messages.len()),
            current_message_count: current.len(),
        }
    }

    pub(super) fn is_inbound_tail_replacement(self) -> bool {
        let Some(prior_count) = self.prior_message_count else {
            return false;
        };
        if prior_count == 0 || prior_count != self.current_message_count {
            return false;
        }
        matches!(
            (self.comparison_origin, self.reason),
            (
                ReplayComparisonOrigin::InboundOriginalHistories,
                ReplaySkip::PrefixContentDiverged { first_diff_index, .. }
            ) if first_diff_index == prior_count - 1
        )
    }
}

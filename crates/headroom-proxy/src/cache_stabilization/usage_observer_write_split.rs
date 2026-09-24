//! usage_observer::write_split — split from usage_observer.rs (pure move, no logic change).

/// Token slack for the healthy-turn comparison. `cache_read` can
/// legitimately undershoot the expectation by a few tokens
/// (breakpoint rounding); anything inside the slack is Healthy.
pub const RECACHE_SLACK_TOKENS: u64 = 64;

/// Below this, an unearned write is breakpoint rounding and not worth a line.
/// Chosen against 2026-09-07: a floor of 1,024 logs 188 turns of the day and
/// still names 96% of the unearned tokens, where a floor at the slack would
/// log 331 turns to catch the last 4%.
pub const UNEARNED_WRITE_FLOOR_TOKENS: u64 = 1_024;

/// Split a healthy turn's cache write into the part that bought new cached
/// footprint and the part that re-wrote footprint the conversation already
/// had.
///
/// A turn that reads its whole expected prefix is `Healthy` however much it
/// writes, because the classifier only ever asked whether the *read* fell
/// short. That left 65% of one day's written tokens in a bucket with no name
/// — mostly the breakpoint advancing over genuinely new content, which is the
/// mechanism working and money well spent, but not only that. Growth in the
/// cached footprint is what a write is supposed to buy; anything written
/// beyond it went over ground already covered.
///
/// Deliberately conservative: `growth` is the *whole* footprint increase, so
/// a write is called earned whenever it plausibly paid for one. This
/// undercounts rather than accuses.
pub fn split_cache_write(
    previous_footprint: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> (u64, u64) {
    let footprint = cache_read_input_tokens.saturating_add(cache_creation_input_tokens);
    let growth = footprint.saturating_sub(previous_footprint);
    let earned = cache_creation_input_tokens.min(growth);
    (earned, cache_creation_input_tokens - earned)
}

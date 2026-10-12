//! What a token costs on the Claude subscription meter, in percent of one
//! 5-hour window. Subscription users are never billed API dollars; the limit
//! they hit is this meter, and it weights token classes differently from the
//! API price list (output about 2.5x heavier, cache reads about half).
//!
//! Fitted against Anthropic's own utilization samples, 2026-09-29 to
//! 2026-10-12: `docs/notes/learnings/subscription-meter-weights.md`,
//! scripts in `scripts/meter-fit/`. Refit after a plan change.

/// 5h-meter percent per token, by token class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterRates {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub output: f64,
}

/// The 7-day window fills about 7.84x slower than the 5h one (API-shaped fit;
/// the 7d window had too few resets for a free one).
pub const WEEK_PER_5H_WINDOW: f64 = 1.0 / 7.84;

const PER_MTOK: f64 = 1e-6;

/// Every write in the fit was 1h, so the 5m weight cannot be measured; it
/// takes the API ratio of the two (1.25x vs 2.0x input).
const WRITE_5M_OF_1H: f64 = 1.25 / 2.0;

/// Opus, the bulk of the fitted traffic. Callers that need one set of
/// weights for every model use this one.
pub const OPUS: MeterRates = MeterRates {
    input: 1.7 * PER_MTOK,
    cache_read: 0.070 * PER_MTOK,
    cache_write_5m: 2.69 * WRITE_5M_OF_1H * PER_MTOK,
    cache_write_1h: 2.69 * PER_MTOK,
    output: 18.7 * PER_MTOK,
};

const SONNET: MeterRates = MeterRates {
    input: 1.5 * PER_MTOK,
    cache_read: 0.051 * PER_MTOK,
    cache_write_5m: 2.83 * WRITE_5M_OF_1H * PER_MTOK,
    cache_write_1h: 2.83 * PER_MTOK,
    output: 9.4 * PER_MTOK,
};

/// Haiku had too few turns to fit; it takes Sonnet's weights scaled by the
/// API price ratio of the two (1/3).
const HAIKU_OF_SONNET: f64 = 1.0 / 3.0;

/// Meter rates for `model`, or `None` when the model does not draw on the
/// Claude subscription meter (Codex, Spark, other providers) or has no fit
/// to price it with (Fable).
pub fn rates(model: &str) -> Option<MeterRates> {
    let m = model.to_ascii_lowercase();
    if !m.contains("claude") {
        return None;
    }
    if m.contains("opus") {
        Some(OPUS)
    } else if m.contains("sonnet") {
        Some(SONNET)
    } else if m.contains("haiku") {
        let s = SONNET;
        Some(MeterRates {
            input: s.input * HAIKU_OF_SONNET,
            cache_read: s.cache_read * HAIKU_OF_SONNET,
            cache_write_5m: s.cache_write_5m * HAIKU_OF_SONNET,
            cache_write_1h: s.cache_write_1h * HAIKU_OF_SONNET,
            output: s.output * HAIKU_OF_SONNET,
        })
    } else {
        None
    }
}

/// Meter percent for a token split, or `None` off the meter. `write_1h` is
/// the part of `write` billed at the 1h TTL.
pub fn meter_pct(
    model: &str,
    input: i64,
    output: i64,
    cache_read: i64,
    write: i64,
    write_1h: i64,
) -> Option<f64> {
    let r = rates(model)?;
    let write_1h = write_1h.clamp(0, write.max(0));
    Some(
        input.max(0) as f64 * r.input
            + output.max(0) as f64 * r.output
            + cache_read.max(0) as f64 * r.cache_read
            + (write.max(0) - write_1h) as f64 * r.cache_write_5m
            + write_1h as f64 * r.cache_write_1h,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_claude_models_draw_on_the_meter() {
        assert_eq!(rates("claude-opus-5-5"), Some(OPUS));
        assert_eq!(rates("claude-sonnet-5"), Some(SONNET));
        assert!(rates("claude-haiku-4-5-20251001").is_some());
        assert_eq!(rates("muse-spark-1.3-contributor-free"), None);
        assert_eq!(rates("gpt-6-luna"), None);
        assert_eq!(rates("claude-fable-5-1"), None);
    }

    #[test]
    fn prices_each_class_at_its_own_rate() {
        let pct = meter_pct("claude-opus-5-5", 0, 1_000_000, 0, 0, 0).unwrap();
        assert!((pct - 18.7).abs() < 1e-9);
        let w = meter_pct("claude-opus-5-5", 0, 0, 0, 1_000_000, 1_000_000).unwrap();
        assert!((w - 2.69).abs() < 1e-9);
    }
}

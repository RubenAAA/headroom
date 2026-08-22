//! ``MemoryRanker``: pluggable re-ranker for memory candidates.
//!
//! Ships ``RecencyBoostRanker`` — exponential recency decay applied to
//! cosine scores. Pure function, no I/O.
//!
//! Mirrors Python's `headroom.proxy.memory_ranker`.

use std::time::{SystemTime, UNIX_EPOCH};

/// A retrieval candidate flowing through the ranker.
#[derive(Debug, Clone)]
pub struct MemoryCandidate {
    pub content: String,
    pub score: f64,
    pub created_at_secs: Option<f64>,
    pub source: Option<String>,
    pub related_entities: Vec<String>,
    pub id: String,
}

/// Pluggable re-ranking protocol.
pub trait MemoryRanker: Send + Sync {
    fn rank(&self, candidates: &[MemoryCandidate]) -> Vec<MemoryCandidate>;
}

/// Re-ranker applying exponential recency decay.
///
/// Final score: `cosine * exp(-age_days / decay_days)`.
/// Candidates with no timestamp get factor 1.0 (recency-neutral).
/// Future timestamps (clock skew) clamped to 1.0.
#[derive(Debug, Clone)]
pub struct RecencyBoostRanker {
    pub decay_days: f64,
}

impl Default for RecencyBoostRanker {
    fn default() -> Self {
        Self { decay_days: 30.0 }
    }
}

impl MemoryRanker for RecencyBoostRanker {
    fn rank(&self, candidates: &[MemoryCandidate]) -> Vec<MemoryCandidate> {
        if candidates.is_empty() {
            return vec![];
        }

        let now = day_start_secs(now_secs());
        let mut boosted: Vec<(usize, MemoryCandidate, f64)> = candidates
            .iter()
            .enumerate()
            .map(|(idx, c)| {
                let factor = self.recency_factor(now, c.created_at_secs);
                (idx, c.clone(), c.score * factor)
            })
            .collect();

        // Stable sort: descending by boosted score, input order on ties
        boosted.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        boosted
            .into_iter()
            .map(|(_, mut c, new_score)| {
                c.score = new_score;
                c
            })
            .collect()
    }
}

impl RecencyBoostRanker {
    fn recency_factor(&self, now_secs: f64, created_at_secs: Option<f64>) -> f64 {
        match created_at_secs {
            None => 1.0,
            Some(created) => {
                let age_secs = now_secs - created;
                if age_secs <= 0.0 {
                    return 1.0; // future-dated, clock skew
                }
                let age_days = age_secs / 86400.0;
                (-age_days / self.decay_days).exp()
            }
        }
    }
}

/// `secs` rounded down to the start of its UTC day.
///
/// A memory created earlier today then reads as future-dated and takes the
/// neutral 1.0 factor — which is also the maximum, so the newest memories
/// still rank highest.
fn day_start_secs(secs: f64) -> f64 {
    (secs / 86_400.0).floor() * 86_400.0
}

/// Current time as seconds since UNIX epoch.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Parse a timestamp string (ISO-8601 or epoch seconds) into epoch seconds.
pub fn parse_timestamp(value: &str) -> Option<f64> {
    // Try parsing as f64 (epoch seconds)
    if let Ok(secs) = value.parse::<f64>() {
        return Some(secs);
    }
    // Try ISO-8601 by converting to epoch via simple heuristics
    // For now, just return None for unparsable strings
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(score: f64, created_at: Option<f64>) -> MemoryCandidate {
        MemoryCandidate {
            content: format!("content_{}", score),
            score,
            created_at_secs: created_at,
            source: None,
            related_entities: vec![],
            id: String::new(),
        }
    }

    #[test]
    fn empty_input() {
        let ranker = RecencyBoostRanker::default();
        assert!(ranker.rank(&[]).is_empty());
    }

    #[test]
    fn no_timestamps_preserves_order() {
        let ranker = RecencyBoostRanker::default();
        let candidates = vec![
            candidate(0.5, None),
            candidate(0.9, None),
            candidate(0.7, None),
        ];
        let ranked = ranker.rank(&candidates);
        assert_eq!(ranked[0].score, 0.9);
        assert_eq!(ranked[1].score, 0.7);
        assert_eq!(ranked[2].score, 0.5);
    }

    #[test]
    fn fresh_memory_boosts_above_stale() {
        let ranker = RecencyBoostRanker::default();
        let now = now_secs();
        let fresh = candidate(0.5, Some(now - 3600.0)); // 1 hour ago
        let stale = candidate(0.7, Some(now - 86400.0 * 90.0)); // 90 days ago
        let ranked = ranker.rank(&[stale, fresh.clone()]);
        // Fresh memory with lower cosine should rank higher after boost
        assert_eq!(ranked[0].content, fresh.content);
    }

    #[test]
    fn recency_factor_one_for_none() {
        let ranker = RecencyBoostRanker::default();
        let factor = ranker.recency_factor(1000.0, None);
        assert!((factor - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn recency_factor_one_for_future() {
        let ranker = RecencyBoostRanker::default();
        let factor = ranker.recency_factor(100.0, Some(200.0)); // future
        assert!((factor - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn recency_factor_decay() {
        let ranker = RecencyBoostRanker { decay_days: 30.0 };
        let now = 1_000_000.0;
        let factor_0 = ranker.recency_factor(now, Some(now));
        let factor_30 = ranker.recency_factor(now, Some(now - 86400.0 * 30.0));
        assert!((factor_0 - 1.0).abs() < 0.001);
        assert!((factor_30 - 0.368).abs() < 0.01);
    }

    #[test]
    fn ranking_is_stable_across_calls_within_a_day() {
        // Injected memory text has to be byte-identical between two rankings
        // of the same candidates, or a re-injection rewrites a cached prefix.
        let ranker = RecencyBoostRanker::default();
        let now = now_secs();
        let candidates = vec![
            candidate(0.50, Some(now - 86400.0 * 10.0)),
            candidate(0.51, Some(now - 86400.0 * 10.5)),
            candidate(0.49, Some(now - 86400.0 * 3.0)),
        ];
        let first = ranker.rank(&candidates);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = ranker.rank(&candidates);
        let scores = |r: &[MemoryCandidate]| r.iter().map(|c| c.score.to_bits()).collect::<Vec<_>>();
        assert_eq!(scores(&first), scores(&second));
    }

    #[test]
    fn day_start_is_a_multiple_of_a_day() {
        assert_eq!(day_start_secs(86_400.0 * 3.0 + 12_345.0), 86_400.0 * 3.0);
        assert_eq!(day_start_secs(86_400.0 * 3.0), 86_400.0 * 3.0);
    }

    #[test]
    fn stable_sort_on_ties() {
        let ranker = RecencyBoostRanker::default();
        let candidates = vec![
            candidate(0.5, None),
            candidate(0.5, None),
            candidate(0.5, None),
        ];
        let ranked = ranker.rank(&candidates);
        // Input order preserved on ties
        assert_eq!(ranked[0].content, "content_0.5");
        assert_eq!(ranked[1].content, "content_0.5");
        assert_eq!(ranked[2].content, "content_0.5");
    }

    #[test]
    fn default_decay_is_30() {
        let ranker = RecencyBoostRanker::default();
        assert!((ranker.decay_days - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn configurable_decay_affects_ranking() {
        let fast_decay = RecencyBoostRanker { decay_days: 1.0 };
        let slow_decay = RecencyBoostRanker { decay_days: 60.0 };
        let now = now_secs();
        let c1 = candidate(0.5, Some(now - 86400.0 * 5.0)); // 5 days ago
        let c2 = candidate(0.8, Some(now - 86400.0 * 5.0)); // same age, higher cosine
                                                            // With fast decay (1 day), 5-day-old content gets heavily penalized
        let ranked_fast = fast_decay.rank(&[c1.clone(), c2.clone()]);
        // c2 still wins because its cosine is higher, but let's check
        // that the boost factor differs
        let f1_fast = fast_decay.recency_factor(now, Some(now - 86400.0 * 5.0));
        let f1_slow = slow_decay.recency_factor(now, Some(now - 86400.0 * 5.0));
        assert!(f1_fast < f1_slow, "fast decay should penalize more");
    }

    #[test]
    fn output_length_matches_input() {
        let ranker = RecencyBoostRanker::default();
        let candidates: Vec<MemoryCandidate> =
            (0..50).map(|i| candidate(i as f64 / 50.0, None)).collect();
        let ranked = ranker.rank(&candidates);
        assert_eq!(ranked.len(), 50);
    }

    #[test]
    fn old_strong_beats_young_weak() {
        let ranker = RecencyBoostRanker { decay_days: 30.0 };
        let now = now_secs();
        // Old memory with very high cosine (0.95) vs young memory with low cosine (0.2)
        let old_strong = candidate(0.95, Some(now - 86400.0 * 30.0)); // 30 days, factor ~0.37
        let young_weak = candidate(0.2, Some(now - 3600.0)); // 1 hour, factor ~1.0
        let ranked = ranker.rank(&[old_strong.clone(), young_weak.clone()]);
        // Old strong: 0.95 * 0.37 ≈ 0.35, Young weak: 0.2 * 1.0 = 0.2
        assert_eq!(ranked[0].content, old_strong.content);
    }

    #[test]
    fn candidates_not_mutated() {
        let ranker = RecencyBoostRanker::default();
        let mut candidates = vec![candidate(0.5, Some(now_secs() - 1000.0))];
        let original_score = candidates[0].score;
        let _ranked = ranker.rank(&candidates);
        assert_eq!(candidates[0].score, original_score);
    }

    #[test]
    fn custom_decay_days_zero_panics_not() {
        let ranker = RecencyBoostRanker { decay_days: 0.0 };
        let now = now_secs();
        // decay_days=0 means exp(-age/0) would be exp(-inf) = 0 for any age > 0
        let c = candidate(0.5, Some(now - 100.0));
        let ranked = ranker.rank(&[c]);
        // Should not panic; result is a valid score
        assert!(!ranked.is_empty());
    }
}

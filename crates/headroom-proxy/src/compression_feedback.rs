//! Compression Feedback Loop for learning optimal compression strategies.
//!
//! Analyzes retrieval patterns to learn what compression works well and
//! provides hints to SmartCrusher. Ports `headroom.cache.compression_feedback`.
//!
//! Key insight: when compression causes the LLM to retrieve more data,
//! that signals we compressed too aggressively.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

/// Cap on retained strategy labels. Labels are content-derived, so an
/// unbounded map grows for the life of the process.
const MAX_STRATEGIES: usize = 50;
/// How many top entries to keep from EACH counter before the combined
/// tie-break. Taking the union means a rarely-compressed but heavily-retrieved
/// strategy — exactly the signal worth keeping — isn't dropped.
const TOP_STRATEGIES_PER_COUNTER: usize = 40;
/// Compressions needed before a strategy is eligible for `best_strategy`.
const MIN_SAMPLES_FOR_RECOMMENDATION: u64 = 3;

// ─── Data models ─────────────────────────────────────────────────────────

/// Learned patterns for a specific tool type.
#[derive(Debug, Clone)]
pub struct LocalToolPattern {
    pub tool_name: String,
    pub total_compressions: u64,
    pub total_retrievals: u64,
    pub full_retrievals: u64,
    pub search_retrievals: u64,
    pub common_queries: HashMap<String, u64>,
    pub queried_fields: HashMap<String, u64>,
    pub strategy_compressions: HashMap<String, u64>,
    pub strategy_retrievals: HashMap<String, u64>,
    pub signature_hashes: Vec<String>,
    pub last_compression: f64,
    pub last_retrieval: f64,
}

impl LocalToolPattern {
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            total_compressions: 0,
            total_retrievals: 0,
            full_retrievals: 0,
            search_retrievals: 0,
            common_queries: HashMap::new(),
            queried_fields: HashMap::new(),
            strategy_compressions: HashMap::new(),
            strategy_retrievals: HashMap::new(),
            signature_hashes: Vec::new(),
            last_compression: 0.0,
            last_retrieval: 0.0,
        }
    }

    pub fn retrieval_rate(&self) -> f64 {
        if self.total_compressions == 0 {
            return 0.0;
        }
        self.total_retrievals as f64 / self.total_compressions as f64
    }

    pub fn full_retrieval_rate(&self) -> f64 {
        if self.total_retrievals == 0 {
            return 0.0;
        }
        self.full_retrievals as f64 / self.total_retrievals as f64
    }

    pub fn search_rate(&self) -> f64 {
        if self.total_retrievals == 0 {
            return 0.0;
        }
        self.search_retrievals as f64 / self.total_retrievals as f64
    }

    pub fn strategy_retrieval_rate(&self, strategy: &str) -> f64 {
        let compressions = self
            .strategy_compressions
            .get(strategy)
            .copied()
            .unwrap_or(0);
        if compressions == 0 {
            return 0.0;
        }
        let retrievals = self.strategy_retrievals.get(strategy).copied().unwrap_or(0);
        retrievals as f64 / compressions as f64
    }

    /// Record one compression for `strategy`, then bound the counters.
    pub fn record_strategy_compression(&mut self, strategy: &str) {
        *self
            .strategy_compressions
            .entry(strategy.to_string())
            .or_insert(0) += 1;
        self.prune_strategies();
    }

    /// Record one retrieval for `strategy`, then bound the counters.
    ///
    /// A retrieval means the compression wasn't good enough — the model had to
    /// go back for the original — so this is the signal `best_strategy`
    /// minimizes.
    pub fn record_strategy_retrieval(&mut self, strategy: &str) {
        *self
            .strategy_retrievals
            .entry(strategy.to_string())
            .or_insert(0) += 1;
        self.prune_strategies();
    }

    /// Bound the counters while preserving the highest-signal strategies.
    ///
    /// Without this the maps grow unbounded for the lifetime of the process —
    /// strategy labels are derived from content, so a long-running proxy
    /// accumulates them indefinitely. Keeps the top entries of EACH counter
    /// (union, so a strategy that is heavily retrieved but rarely compressed
    /// still survives), then breaks any overflow by combined volume.
    fn prune_strategies(&mut self) {
        if self.strategy_compressions.len() <= MAX_STRATEGIES
            && self.strategy_retrievals.len() <= MAX_STRATEGIES
        {
            return;
        }
        let keep = self.strategy_keys_to_keep();
        self.strategy_compressions.retain(|k, _| keep.contains(k));
        self.strategy_retrievals.retain(|k, _| keep.contains(k));
    }

    fn strategy_keys_to_keep(&self) -> std::collections::HashSet<String> {
        let top = |counts: &HashMap<String, u64>| -> std::collections::HashSet<String> {
            let mut items: Vec<(&String, &u64)> = counts.iter().collect();
            // Descending by (count, name) — the name is the tie-break, so the
            // result is deterministic rather than HashMap-order dependent.
            items.sort_by(|a, b| b.1.cmp(a.1).then_with(|| b.0.cmp(a.0)));
            items
                .into_iter()
                .take(TOP_STRATEGIES_PER_COUNTER)
                .map(|(k, _)| k.clone())
                .collect()
        };
        let mut candidates = top(&self.strategy_compressions);
        candidates.extend(top(&self.strategy_retrievals));
        if candidates.len() <= MAX_STRATEGIES {
            return candidates;
        }
        let mut ranked: Vec<String> = candidates.into_iter().collect();
        let count = |m: &HashMap<String, u64>, k: &str| m.get(k).copied().unwrap_or(0);
        ranked.sort_by(|a, b| {
            let (ca, ra) = (
                count(&self.strategy_compressions, a),
                count(&self.strategy_retrievals, a),
            );
            let (cb, rb) = (
                count(&self.strategy_compressions, b),
                count(&self.strategy_retrievals, b),
            );
            (cb + rb)
                .cmp(&(ca + ra))
                .then_with(|| cb.cmp(&ca))
                .then_with(|| rb.cmp(&ra))
                .then_with(|| b.cmp(a))
        });
        ranked.into_iter().take(MAX_STRATEGIES).collect()
    }

    pub fn best_strategy(&self) -> Option<&str> {
        if self.strategy_compressions.is_empty() {
            return None;
        }
        let mut best: Option<&str> = None;
        let mut best_rate = 1.0_f64;
        for (strategy, &compressions) in &self.strategy_compressions {
            if compressions < MIN_SAMPLES_FOR_RECOMMENDATION {
                continue;
            }
            let rate = self.strategy_retrieval_rate(strategy);
            if rate < best_rate {
                best_rate = rate;
                best = Some(strategy.as_str());
            }
        }
        best
    }
}

/// Hints for optimizing compression of a specific tool's output.
#[derive(Debug, Clone)]
pub struct CompressionHints {
    pub max_items: usize,
    pub min_items: usize,
    pub suggested_items: Option<usize>,
    pub preserve_fields: Vec<String>,
    pub aggressiveness: f64,
    pub reason: String,
    pub skip_compression: bool,
    pub recommended_strategy: Option<String>,
}

impl Default for CompressionHints {
    fn default() -> Self {
        Self {
            max_items: 15,
            min_items: 3,
            suggested_items: None,
            preserve_fields: Vec::new(),
            aggressiveness: 0.7,
            reason: String::new(),
            skip_compression: false,
            recommended_strategy: None,
        }
    }
}

// ─── Feedback engine ─────────────────────────────────────────────────────

const HIGH_RETRIEVAL_THRESHOLD: f64 = 0.5;
const MEDIUM_RETRIEVAL_THRESHOLD: f64 = 0.2;
const MIN_SAMPLES_FOR_HINTS: u64 = 5;
const MAX_STRATEGY_ENTRIES: usize = 50;
const MAX_QUERY_ENTRIES: usize = 100;
const MAX_FIELD_ENTRIES: usize = 50;
const MAX_SIGNATURE_HASHES: usize = 100;

/// Learn from retrieval patterns to improve compression.
pub struct CompressionFeedback {
    inner: Mutex<FeedbackInner>,
}

struct FeedbackInner {
    tool_patterns: HashMap<String, LocalToolPattern>,
    total_compressions: u64,
    total_retrievals: u64,
    enable_learning: bool,
}

impl CompressionFeedback {
    pub fn new(enable_learning: bool) -> Self {
        Self {
            inner: Mutex::new(FeedbackInner {
                tool_patterns: HashMap::new(),
                total_compressions: 0,
                total_retrievals: 0,
                enable_learning,
            }),
        }
    }

    pub fn record_compression(
        &self,
        tool_name: Option<&str>,
        _original_count: usize,
        _compressed_count: usize,
        strategy: Option<&str>,
        tool_signature_hash: Option<&str>,
    ) {
        let tool_name = match tool_name {
            Some(n) if !n.is_empty() => n,
            _ => return,
        };

        let mut inner = self.inner.lock().unwrap();
        if !inner.enable_learning {
            return;
        }

        inner.total_compressions += 1;
        let pattern = inner
            .tool_patterns
            .entry(tool_name.to_string())
            .or_insert_with(|| LocalToolPattern::new(tool_name));

        pattern.total_compressions += 1;
        pattern.last_compression = now_secs();

        if let Some(s) = strategy {
            *pattern
                .strategy_compressions
                .entry(s.to_string())
                .or_insert(0) += 1;
            if pattern.strategy_compressions.len() > MAX_STRATEGY_ENTRIES {
                truncate_strategy_dicts(pattern);
            }
        }

        if let Some(hash) = tool_signature_hash {
            if !pattern.signature_hashes.contains(&hash.to_string()) {
                pattern.signature_hashes.push(hash.to_string());
            }
            if pattern.signature_hashes.len() > MAX_SIGNATURE_HASHES {
                pattern.signature_hashes.sort();
                pattern.signature_hashes.truncate(MAX_SIGNATURE_HASHES);
            }
        }
    }

    pub fn record_retrieval(
        &self,
        tool_name: Option<&str>,
        retrieval_type: &str,
        query: Option<&str>,
        strategy: Option<&str>,
    ) {
        let tool_name = match tool_name {
            Some(n) if !n.is_empty() => n,
            _ => return,
        };

        let mut inner = self.inner.lock().unwrap();
        if !inner.enable_learning {
            return;
        }

        inner.total_retrievals += 1;
        let pattern = inner
            .tool_patterns
            .entry(tool_name.to_string())
            .or_insert_with(|| LocalToolPattern::new(tool_name));

        pattern.total_retrievals += 1;
        pattern.last_retrieval = now_secs();

        if retrieval_type == "full" {
            pattern.full_retrievals += 1;
        } else {
            pattern.search_retrievals += 1;
        }

        if let Some(s) = strategy {
            *pattern
                .strategy_retrievals
                .entry(s.to_string())
                .or_insert(0) += 1;
            if pattern.strategy_retrievals.len() > MAX_STRATEGY_ENTRIES {
                truncate_strategy_dicts(pattern);
            }
        }

        if let Some(q) = query {
            let q_lower = q.to_lowercase();
            *pattern.common_queries.entry(q_lower).or_insert(0) += 1;
            if pattern.common_queries.len() > MAX_QUERY_ENTRIES {
                keep_top_by_value(&mut pattern.common_queries, MAX_QUERY_ENTRIES);
            }
            extract_field_hints(pattern, q);
        }
    }

    pub fn get_compression_hints(&self, tool_name: Option<&str>) -> CompressionHints {
        let tool_name = match tool_name {
            Some(n) if !n.is_empty() => n,
            _ => {
                return CompressionHints {
                    reason: "No tool name provided, using defaults".to_string(),
                    ..Default::default()
                };
            }
        };

        let inner = self.inner.lock().unwrap();
        let pattern = match inner.tool_patterns.get(tool_name) {
            Some(p) => p,
            None => {
                return CompressionHints {
                    reason: format!("No pattern data for {tool_name}, using defaults"),
                    ..Default::default()
                };
            }
        };

        if pattern.total_compressions < MIN_SAMPLES_FOR_HINTS {
            return CompressionHints {
                reason: format!(
                    "Insufficient data ({} samples), need {MIN_SAMPLES_FOR_HINTS}",
                    pattern.total_compressions
                ),
                ..Default::default()
            };
        }

        let retrieval_rate = pattern.retrieval_rate();
        let mut hints = CompressionHints::default();

        if retrieval_rate > HIGH_RETRIEVAL_THRESHOLD {
            if pattern.full_retrieval_rate() > 0.8 {
                hints.skip_compression = true;
                hints.reason = format!(
                    "Very high full retrieval rate ({:.0}%), recommending skip compression",
                    pattern.full_retrieval_rate() * 100.0
                );
            } else {
                hints.max_items = 50;
                hints.suggested_items = Some(40);
                hints.aggressiveness = 0.3;
                hints.reason = format!(
                    "High retrieval rate ({:.0}%), recommending less aggressive compression",
                    retrieval_rate * 100.0
                );
            }
        } else if retrieval_rate > MEDIUM_RETRIEVAL_THRESHOLD {
            hints.max_items = 30;
            hints.suggested_items = Some(25);
            hints.aggressiveness = 0.5;
            hints.reason = format!(
                "Medium retrieval rate ({:.0}%), recommending moderate compression",
                retrieval_rate * 100.0
            );
        } else {
            hints.max_items = 15;
            hints.suggested_items = Some(10);
            hints.aggressiveness = 0.7;
            hints.reason = format!(
                "Low retrieval rate ({:.0}%), current compression is effective",
                retrieval_rate * 100.0
            );
        }

        // Field preservation from common queries
        if !pattern.queried_fields.is_empty() {
            let mut fields: Vec<(String, u64)> = pattern
                .queried_fields
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect();
            fields.sort_by(|a, b| b.1.cmp(&a.1));
            hints.preserve_fields = fields.into_iter().take(5).map(|(k, _)| k).collect();
        }

        if let Some(best) = pattern.best_strategy() {
            hints.recommended_strategy = Some(best.to_string());
        }

        hints
    }

    pub fn get_stats(&self) -> FeedbackStats {
        let inner = self.inner.lock().unwrap();
        let global_rate = if inner.total_compressions > 0 {
            inner.total_retrievals as f64 / inner.total_compressions as f64
        } else {
            0.0
        };

        let tool_patterns: HashMap<String, ToolStats> = inner
            .tool_patterns
            .iter()
            .map(|(name, p)| {
                (
                    name.clone(),
                    ToolStats {
                        compressions: p.total_compressions,
                        retrievals: p.total_retrievals,
                        retrieval_rate: p.retrieval_rate(),
                        full_rate: p.full_retrieval_rate(),
                        search_rate: p.search_rate(),
                    },
                )
            })
            .collect();

        FeedbackStats {
            total_compressions: inner.total_compressions,
            total_retrievals: inner.total_retrievals,
            global_retrieval_rate: global_rate,
            tools_tracked: inner.tool_patterns.len(),
            tool_patterns,
        }
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.tool_patterns.clear();
        inner.total_compressions = 0;
        inner.total_retrievals = 0;
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FeedbackStats {
    pub total_compressions: u64,
    pub total_retrievals: u64,
    pub global_retrieval_rate: f64,
    pub tools_tracked: usize,
    pub tool_patterns: HashMap<String, ToolStats>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolStats {
    pub compressions: u64,
    pub retrievals: u64,
    pub retrieval_rate: f64,
    pub full_rate: f64,
    pub search_rate: f64,
}

// ─── Helpers ─────────────────────────────────────────────────────────────

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn truncate_strategy_dicts(pattern: &mut LocalToolPattern) {
    let top_compressions: Vec<String> = sorted_keys(&pattern.strategy_compressions, 40);
    let top_retrievals: Vec<String> = sorted_keys(&pattern.strategy_retrievals, 40);
    let keep: std::collections::HashSet<String> =
        top_compressions.into_iter().chain(top_retrievals).collect();
    pattern
        .strategy_compressions
        .retain(|k, _| keep.contains(k));
    pattern.strategy_retrievals.retain(|k, _| keep.contains(k));
}

fn sorted_keys(map: &HashMap<String, u64>, limit: usize) -> Vec<String> {
    let mut entries: Vec<(String, u64)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.into_iter().take(limit).map(|(k, _)| k).collect()
}

fn keep_top_by_value(map: &mut HashMap<String, u64>, limit: usize) {
    let top = sorted_keys(map, limit);
    let keep: std::collections::HashSet<String> = top.into_iter().collect();
    map.retain(|k, _| keep.contains(k));
}

fn extract_field_hints(pattern: &mut LocalToolPattern, query: &str) {
    // Field:value or field=value patterns (matches Python's re.findall(r"(\w+)[=:]", query))
    let mut chars = query.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if (c == '=' || c == ':') && i > 0 {
            // Walk backwards to find the start of the word
            let start = query[..i]
                .rfind(|ch: char| !ch.is_alphanumeric() && ch != '_')
                .map(|p| p + 1)
                .unwrap_or(0);
            let field = &query[start..i];
            if !field.is_empty() {
                *pattern.queried_fields.entry(field.to_string()).or_insert(0) += 1;
            }
        }
    }

    // Common JSON field names
    const COMMON_FIELDS: &[&str] = &[
        "id", "name", "status", "error", "message", "type", "code", "result", "value", "data",
        "items", "count",
    ];
    let query_lower = query.to_lowercase();
    for field in COMMON_FIELDS {
        if query_lower.contains(field) {
            *pattern.queried_fields.entry(field.to_string()).or_insert(0) += 1;
        }
    }

    if pattern.queried_fields.len() > MAX_FIELD_ENTRIES {
        keep_top_by_value(&mut pattern.queried_fields, MAX_FIELD_ENTRIES);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_tool_pattern_defaults() {
        let p = LocalToolPattern::new("test_tool");
        assert_eq!(p.tool_name, "test_tool");
        assert_eq!(p.total_compressions, 0);
        assert_eq!(p.retrieval_rate(), 0.0);
    }

    #[test]
    fn retrieval_rate_calculation() {
        let mut p = LocalToolPattern::new("t");
        p.total_compressions = 10;
        p.total_retrievals = 3;
        assert!((p.retrieval_rate() - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn full_retrieval_rate() {
        let mut p = LocalToolPattern::new("t");
        p.total_retrievals = 10;
        p.full_retrievals = 7;
        assert!((p.full_retrieval_rate() - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn best_strategy_requires_min_samples() {
        let mut p = LocalToolPattern::new("t");
        p.strategy_compressions.insert("A".into(), 2);
        p.strategy_retrievals.insert("A".into(), 0);
        assert!(p.best_strategy().is_none()); // < 3 samples

        p.strategy_compressions.insert("A".into(), 5);
        assert_eq!(p.best_strategy(), Some("A"));
    }

    #[test]
    fn record_compression_tracks_stats() {
        let fb = CompressionFeedback::new(true);
        fb.record_compression(Some("tool_a"), 100, 30, Some("SMART_SAMPLE"), None);
        fb.record_compression(Some("tool_a"), 100, 30, Some("SMART_SAMPLE"), None);

        let stats = fb.get_stats();
        assert_eq!(stats.total_compressions, 2);
        assert_eq!(stats.tools_tracked, 1);
        let ts = stats.tool_patterns.get("tool_a").unwrap();
        assert_eq!(ts.compressions, 2);
    }

    #[test]
    fn record_retrieval_tracks_types() {
        let fb = CompressionFeedback::new(true);
        fb.record_retrieval(Some("tool_b"), "full", Some("status:ok"), None);
        fb.record_retrieval(Some("tool_b"), "search", Some("id=123"), None);

        let stats = fb.get_stats();
        assert_eq!(stats.total_retrievals, 2);
        let ts = stats.tool_patterns.get("tool_b").unwrap();
        assert_eq!(ts.full_rate, 0.5);
        assert_eq!(ts.search_rate, 0.5);
    }

    #[test]
    fn hints_default_for_unknown_tool() {
        let fb = CompressionFeedback::new(true);
        let hints = fb.get_compression_hints(Some("unknown"));
        assert_eq!(hints.max_items, 15);
        assert!(!hints.skip_compression);
    }

    #[test]
    fn hints_skip_compression_on_high_full_retrieval() {
        let fb = CompressionFeedback::new(true);
        // Record 20 compressions and 18 full retrievals (> 50% retrieval, > 80% full)
        for _ in 0..20 {
            fb.record_compression(Some("t"), 100, 30, None, None);
        }
        for _ in 0..18 {
            fb.record_retrieval(Some("t"), "full", None, None);
        }
        let hints = fb.get_compression_hints(Some("t"));
        assert!(hints.skip_compression);
    }

    #[test]
    fn hints_less_aggressive_on_medium_retrieval() {
        let fb = CompressionFeedback::new(true);
        for _ in 0..20 {
            fb.record_compression(Some("t"), 100, 30, None, None);
        }
        // 5 retrievals out of 20 compressions = 25% retrieval rate (> 20%)
        for _ in 0..5 {
            fb.record_retrieval(Some("t"), "search", None, None);
        }
        let hints = fb.get_compression_hints(Some("t"));
        assert_eq!(hints.max_items, 30);
        assert!((hints.aggressiveness - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn hints_default_on_low_retrieval() {
        let fb = CompressionFeedback::new(true);
        for _ in 0..20 {
            fb.record_compression(Some("t"), 100, 30, None, None);
        }
        // 1 retrieval out of 20 = 5% (< 20%)
        fb.record_retrieval(Some("t"), "search", None, None);
        let hints = fb.get_compression_hints(Some("t"));
        assert_eq!(hints.max_items, 15);
    }

    #[test]
    fn clear_resets_state() {
        let fb = CompressionFeedback::new(true);
        fb.record_compression(Some("t"), 10, 5, None, None);
        fb.clear();
        let stats = fb.get_stats();
        assert_eq!(stats.total_compressions, 0);
        assert_eq!(stats.tools_tracked, 0);
    }

    #[test]
    fn disabled_learning_skips_records() {
        let fb = CompressionFeedback::new(false);
        fb.record_compression(Some("t"), 10, 5, None, None);
        let stats = fb.get_stats();
        assert_eq!(stats.total_compressions, 0);
    }

    #[test]
    fn field_hints_extracted_from_queries() {
        let fb = CompressionFeedback::new(true);
        for _ in 0..10 {
            fb.record_compression(Some("t"), 100, 30, None, None);
        }
        fb.record_retrieval(Some("t"), "search", Some("status:open id=42"), None);
        fb.record_retrieval(Some("t"), "search", Some("status:closed"), None);
        let hints = fb.get_compression_hints(Some("t"));
        assert!(hints.preserve_fields.contains(&"status".to_string()));
    }

    // ─── Strategy outcome recording + pruning (upstream addition) ────────

    #[test]
    fn strategy_counters_are_bounded_like_python() {
        // 60 strategies with descending counts. Pruning runs after EVERY
        // record, so entries added since the last prune survive regardless of
        // count — Python keeps 50 and drops exactly s40..s49.
        let mut p = LocalToolPattern::new("Read");
        for i in 0..60u64 {
            for _ in 0..(60 - i) {
                p.record_strategy_compression(&format!("s{i:02}"));
            }
        }
        assert_eq!(p.strategy_compressions.len(), 50, "must be bounded at 50");
        assert!(
            p.strategy_compressions.contains_key("s00"),
            "highest count kept"
        );
        assert!(
            p.strategy_compressions.contains_key("s59"),
            "most recent kept"
        );
        for i in 40..50u64 {
            assert!(
                !p.strategy_compressions.contains_key(&format!("s{i:02}")),
                "s{i:02} should have been pruned"
            );
        }
    }

    #[test]
    fn record_strategy_outcomes_and_best_strategy_match_python() {
        let mut p = LocalToolPattern::new("Read");
        for _ in 0..5 {
            p.record_strategy_compression("good");
        }
        p.record_strategy_retrieval("good");
        // Below the sample floor: must not be recommended despite a 0.0 rate.
        for _ in 0..2 {
            p.record_strategy_compression("rare");
        }
        assert_eq!(p.best_strategy(), Some("good"));
        assert!((p.strategy_retrieval_rate("good") - 0.2).abs() < 1e-9);
        assert_eq!(p.strategy_retrieval_rate("never_seen"), 0.0);
    }

    #[test]
    fn a_heavily_retrieved_strategy_survives_pruning() {
        // The union-of-both-counters rule exists so a strategy that is rarely
        // compressed but heavily RETRIEVED — the strongest "this compression is
        // bad" signal — isn't evicted by compression volume alone.
        let mut p = LocalToolPattern::new("Read");
        for i in 0..60u64 {
            for _ in 0..(60 - i) {
                p.record_strategy_compression(&format!("s{i:02}"));
            }
        }
        for _ in 0..500 {
            p.record_strategy_retrieval("s45");
        }
        assert!(
            p.strategy_retrievals.contains_key("s45"),
            "a top-retrieved strategy must be retained"
        );
    }
}

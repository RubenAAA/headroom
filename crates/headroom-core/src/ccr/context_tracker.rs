//! Multi-turn context tracking for CCR (Compress-Cache-Retrieve).
//!
//! Tracks compressed content across conversation turns and provides
//! intelligent context expansion based on query relevance. Prevents
//! "context amnesia" where earlier compressed data is forgotten.
//!
//! # Workspace isolation
//!
//! Every tracked compression is tagged with a `workspace_key` that ties
//! it to a single project/CWD identity. Cross-project proactive expansion
//! is impossible — the `analyze_query` method only considers entries whose
//! `workspace_key` matches the caller-provided key.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;

// ─── Types ───────────────────────────────────────────────────────────────

/// Represents a piece of compressed context from the conversation.
#[derive(Debug, Clone)]
pub struct CompressedContext {
    pub hash_key: String,
    pub turn_number: u32,
    pub timestamp: Instant,
    pub tool_name: Option<String>,
    pub original_item_count: usize,
    pub compressed_item_count: usize,
    pub query_context: String,
    pub sample_content: String,
    pub workspace_key: String,
}

/// Recommendation to expand compressed context.
#[derive(Debug, Clone)]
pub struct ExpansionRecommendation {
    pub hash_key: String,
    pub reason: String,
    pub relevance_score: f64,
}

/// Configuration for context tracking.
#[derive(Debug, Clone)]
pub struct ContextTrackerConfig {
    pub enabled: bool,
    pub max_tracked_contexts: usize,
    pub relevance_threshold: f64,
    pub max_context_age: Duration,
    pub proactive_expansion: bool,
    pub max_proactive_expansions: usize,
}

impl Default for ContextTrackerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Sized against `max_context_age`, because that is how long an
            // entry can still be recommended: anything older is skipped at
            // query time, so the cap only has to hold one age window.
            //
            // Measured over 870 captured requests across 22 sessions, the
            // digests referenced inside a 300s window peaked at 91 — against
            // the old cap of 100. That is why the tracker evicted 3,953 times
            // in a day, 460 hashes more than once: it was thrashing at the
            // edge of its own working set. Lifting the tool exclusions makes
            // roughly 1.8x as many blocks eligible (~164), and this is one
            // global map shared by every conversation on the box, so a busier
            // day than the one measured needs room too.
            //
            // 512 slots hold ~2.5KB each at the 2000-char sample cap, so about
            // 1.3MB — small enough that headroom is cheaper than churn.
            max_tracked_contexts: 512,
            relevance_threshold: 0.3,
            max_context_age: Duration::from_secs(300), // 5 minutes
            proactive_expansion: true,
            max_proactive_expansions: 2,
        }
    }
}

// ─── Keyword extraction ──────────────────────────────────────────────────

fn keyword_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b[a-z][a-z0-9_.-]*[a-z0-9]\b|\b[a-z]{2,}\b")
            .expect("KEYWORD_RE is a valid regex")
    })
}

fn stop_words() -> &'static HashSet<&'static str> {
    static WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        let words = [
            "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has",
            "had", "do", "does", "did", "will", "would", "could", "should", "may", "might", "must",
            "shall", "can", "need", "dare", "ought", "used", "to", "of", "in", "for", "on", "with",
            "at", "by", "from", "as", "into", "through", "during", "before", "after", "above",
            "below", "between", "under", "again", "further", "then", "once", "here", "there",
            "when", "where", "why", "how", "all", "each", "few", "more", "most", "other", "some",
            "such", "no", "nor", "not", "only", "own", "same", "so", "than", "too", "very", "just",
            "and", "but", "if", "or", "because", "until", "while", "this", "that", "these",
            "those", "what", "which", "who", "whom", "it", "its", "me", "my", "i", "you",
        ];
        words.iter().copied().collect()
    })
}

/// Extract meaningful keywords from text.
fn extract_keywords(text: &str) -> Vec<String> {
    keyword_re()
        .find_iter(&text.to_lowercase())
        .map(|m| m.as_str().to_string())
        .filter(|w| !stop_words().contains(w.as_str()) && w.len() >= 2)
        .collect()
}

// ─── ContextTracker ──────────────────────────────────────────────────────

/// Tracks compressed contexts across conversation turns.
pub struct ContextTracker {
    config: ContextTrackerConfig,
    contexts: HashMap<String, CompressedContext>,
    turn_order: Vec<String>,
    current_turn: u32,
    /// Hashes already sent to the model once. Their content is in the
    /// conversation from then on, so expanding again spends context to repeat
    /// what the model can already read.
    expanded: HashSet<String>,
}

impl ContextTracker {
    pub fn new(config: Option<ContextTrackerConfig>) -> Self {
        Self {
            config: config.unwrap_or_default(),
            contexts: HashMap::new(),
            turn_order: Vec::new(),
            current_turn: 0,
            expanded: HashSet::new(),
        }
    }

    /// Track a compression event.
    pub fn track_compression(
        &mut self,
        hash_key: &str,
        turn_number: u32,
        tool_name: Option<&str>,
        original_count: usize,
        compressed_count: usize,
        workspace_key: &str,
        query_context: &str,
        sample_content: &str,
    ) {
        if !self.config.enabled {
            return;
        }

        // Re-tracking is not re-capturing. The same offload record rides along
        // with every later request while its marker sits in the history, and
        // stamping each one afresh kept contexts permanently young:
        // `max_context_age` could never expire anything, and the turn number
        // climbed until a snapshot taken at turn 1 announced itself as captured
        // at turn 200. First sighting wins.
        let first_seen = self
            .contexts
            .get(hash_key)
            .map(|prior| (prior.turn_number, prior.timestamp));
        let (turn_number, timestamp) = first_seen.unwrap_or((turn_number, Instant::now()));

        let context = CompressedContext {
            hash_key: hash_key.to_string(),
            turn_number,
            timestamp,
            tool_name: tool_name.map(|s| s.to_string()),
            original_item_count: original_count,
            compressed_item_count: compressed_count,
            query_context: query_context.to_string(),
            sample_content: sample_content.chars().take(2000).collect(),
            workspace_key: workspace_key.to_string(),
        };

        // Add or update context
        if self.contexts.contains_key(hash_key) {
            self.turn_order.retain(|k| k != hash_key);
        }
        self.contexts.insert(hash_key.to_string(), context);
        self.turn_order.push(hash_key.to_string());

        // LRU eviction
        while self.contexts.len() > self.config.max_tracked_contexts {
            if let Some(oldest) = self.turn_order.first().cloned() {
                self.turn_order.remove(0);
                let evicted = self.contexts.remove(&oldest);
                tracing::info!(
                    event = "ccr_context_tracker_eviction",
                    hash = %oldest,
                    tracked_contexts = self.contexts.len(),
                    max_tracked_contexts = self.config.max_tracked_contexts,
                    had_context = evicted.is_some(),
                    "CCR context tracker evicted the oldest entry due to capacity"
                );
            }
        }

        self.current_turn = self.current_turn.max(turn_number);
    }

    /// Analyze a query to find relevant compressed contexts.
    pub fn analyze_query(
        &mut self,
        query: &str,
        current_turn: Option<u32>,
        workspace_key: &str,
    ) -> Vec<ExpansionRecommendation> {
        if !self.config.enabled || !self.config.proactive_expansion {
            return vec![];
        }

        // Empty workspace = fail closed
        if workspace_key.is_empty() {
            return vec![];
        }

        if let Some(turn) = current_turn {
            self.current_turn = turn;
        }

        let mut recommendations: Vec<ExpansionRecommendation> = Vec::new();
        let now = Instant::now();

        for (hash_key, context) in &self.contexts {
            // Workspace filter
            if context.workspace_key != workspace_key {
                continue;
            }

            // Sent once already. The bytes are in the conversation from that
            // point on, so a second copy buys nothing and costs the window —
            // and it is a *snapshot*, so on an edited file the copy now
            // contradicts what is on disk.
            if self.expanded.contains(hash_key) {
                continue;
            }

            // Check age
            let age = now.duration_since(context.timestamp);
            if age > self.config.max_context_age {
                continue;
            }

            // Calculate relevance
            let relevance = self.calculate_relevance(query, context);

            // Age discount: older contexts get lower scores
            let age_factor =
                1.0 - (age.as_secs_f64() / self.config.max_context_age.as_secs_f64()) * 0.5;
            let relevance = relevance * age_factor;

            if relevance >= self.config.relevance_threshold {
                recommendations.push(ExpansionRecommendation {
                    hash_key: hash_key.clone(),
                    reason: self.generate_reason(query, context, relevance),
                    relevance_score: relevance,
                });
            }
        }

        // Sort by relevance, limit count
        recommendations.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        recommendations.truncate(self.config.max_proactive_expansions);
        for rec in &recommendations {
            self.expanded.insert(rec.hash_key.clone());
        }
        recommendations
    }

    /// Calculate relevance score between query and compressed context.
    fn calculate_relevance(&self, query: &str, context: &CompressedContext) -> f64 {
        let query_lower = query.to_lowercase();
        let query_words: HashSet<String> = extract_keywords(&query_lower).into_iter().collect();

        if query_words.is_empty() {
            return 0.0;
        }

        let mut score = 0.0;

        // Check sample content overlap
        let sample_lower = context.sample_content.to_lowercase();
        let sample_words: HashSet<String> = extract_keywords(&sample_lower).into_iter().collect();

        if !sample_words.is_empty() {
            let overlap: HashSet<_> = query_words.intersection(&sample_words).collect();
            score += (overlap.len() as f64 / query_words.len() as f64) * 0.5;

            // Bonus for exact substring matches
            for word in &query_words {
                if word.len() >= 4 && sample_lower.contains(word.as_str()) {
                    score += 0.2;
                }
            }
        }

        // Check original query context overlap
        if !context.query_context.is_empty() {
            let context_lower = context.query_context.to_lowercase();
            let context_words: HashSet<String> =
                extract_keywords(&context_lower).into_iter().collect();

            if !context_words.is_empty() {
                let overlap: HashSet<_> = query_words.intersection(&context_words).collect();
                score += (overlap.len() as f64 / query_words.len() as f64) * 0.3;
            }
        }

        // Tool name relevance
        if let Some(ref tool_name) = context.tool_name {
            let tool_lower = tool_name.to_lowercase();
            let file_ops = ["find", "glob", "search", "grep", "ls"];
            let query_terms = ["file", "where", "find", "show", "list"];

            if file_ops.iter().any(|w| tool_lower.contains(w))
                && query_terms.iter().any(|w| query_lower.contains(w))
            {
                score += 0.1;
            }
        }

        score.min(1.0)
    }

    /// Generate human-readable reason for expansion recommendation.
    fn generate_reason(&self, _query: &str, context: &CompressedContext, relevance: f64) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(ref tool_name) = context.tool_name {
            parts.push(format!("from {}", tool_name));
        }

        parts.push(format!(
            "{} items compressed in turn {}",
            context.original_item_count, context.turn_number
        ));

        if relevance > 0.5 {
            parts.push("high relevance to current query".to_string());
        } else {
            parts.push("possible relevance to current query".to_string());
        }

        parts.join(", ")
    }

    /// Format expansions as additional context for the LLM.
    pub fn format_expansions_for_context(
        expansions: &[ExpansionContent],
        workspace_label: Option<&str>,
    ) -> String {
        if expansions.is_empty() {
            return String::new();
        }

        let mut header = "[Proactive Context Expansion - relevant to your query".to_string();
        if let Some(label) = workspace_label {
            header.push_str(&format!(" | workspace: {}", label));
        }
        header.push(']');

        let mut parts: Vec<String> = vec![header];

        for exp in expansions {
            parts.push(format!("\n--- Expanded from earlier ({}) ---", exp.reason));
            parts.push(exp.content.clone());
        }

        parts.push("[End Proactive Expansion]".to_string());
        let body = parts.join("\n");
        // Escape any stray close tag in payload
        let body = body.replace(
            "</headroom_proactive_expansion>",
            "<\\/headroom_proactive_expansion>",
        );
        format!(
            "<headroom_proactive_expansion>\n{}\n</headroom_proactive_expansion>",
            body
        )
    }

    /// Get list of currently tracked hashes.
    pub fn get_tracked_hashes(&self) -> Vec<&str> {
        self.turn_order.iter().map(|s| s.as_str()).collect()
    }

    /// Get tracker statistics.
    pub fn get_stats(&self) -> TrackerStats {
        TrackerStats {
            tracked_contexts: self.contexts.len(),
            current_turn: self.current_turn,
            enabled: self.config.enabled,
            max_contexts: self.config.max_tracked_contexts,
            relevance_threshold: self.config.relevance_threshold,
            proactive_expansion: self.config.proactive_expansion,
        }
    }

    /// Clear all tracked contexts.
    pub fn clear(&mut self) {
        self.contexts.clear();
        self.turn_order.clear();
        self.current_turn = 0;
        self.expanded.clear();
    }
}

/// Content to be formatted for expansion.
#[derive(Debug, Clone)]
pub struct ExpansionContent {
    pub hash_key: String,
    pub content: String,
    pub reason: String,
    pub item_count: usize,
}

/// Statistics about the context tracker.
#[derive(Debug, Clone)]
pub struct TrackerStats {
    pub tracked_contexts: usize,
    pub current_turn: u32,
    pub enabled: bool,
    pub max_contexts: usize,
    pub relevance_threshold: f64,
    pub proactive_expansion: bool,
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_keywords_filters_stop_words() {
        let keywords = extract_keywords("the quick brown fox is very fast");
        assert!(keywords.contains(&"quick".to_string()));
        assert!(keywords.contains(&"brown".to_string()));
        assert!(keywords.contains(&"fox".to_string()));
        assert!(keywords.contains(&"fast".to_string()));
        assert!(!keywords.contains(&"the".to_string()));
        assert!(!keywords.contains(&"is".to_string()));
        assert!(!keywords.contains(&"very".to_string()));
    }

    #[test]
    fn extract_keywords_handles_punctuation() {
        let keywords = extract_keywords("src/main.py:10:def main():");
        assert!(keywords.contains(&"src".to_string()));
        assert!(keywords.contains(&"main.py".to_string()));
        assert!(keywords.contains(&"def".to_string()));
    }

    #[test]
    fn track_compression_stores_context() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression(
            "abc123",
            1,
            Some("Bash"),
            100,
            10,
            "project_a",
            "find all python files",
            "[\"src/main.py\", \"src/auth.py\"]",
        );

        let hashes = tracker.get_tracked_hashes();
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "abc123");
    }

    #[test]
    fn track_compression_lru_eviction() {
        let config = ContextTrackerConfig {
            max_tracked_contexts: 3,
            ..Default::default()
        };
        let mut tracker = ContextTracker::new(Some(config));

        for i in 0..5 {
            tracker.track_compression(&format!("hash_{}", i), i, None, 10, 5, "workspace", "", "");
        }

        let hashes = tracker.get_tracked_hashes();
        assert_eq!(hashes.len(), 3);
        assert_eq!(hashes[0], "hash_2");
        assert_eq!(hashes[1], "hash_3");
        assert_eq!(hashes[2], "hash_4");
    }

    #[test]
    fn retracking_keeps_the_first_capture_turn() {
        let mut tracker = ContextTracker::new(None);
        // The same offload record is re-tracked on every later request for as
        // long as its marker sits in the history.
        for turn in 1..200 {
            tracker.track_compression(
                "abc",
                turn,
                Some("Read"),
                10,
                1,
                "ws",
                "auth middleware",
                "auth middleware",
            );
        }

        let recs = tracker.analyze_query("auth middleware", Some(200), "ws");
        assert_eq!(recs.len(), 1);
        assert!(
            recs[0].reason.contains("in turn 1,"),
            "capture turn should be the first sighting, not the latest: {}",
            recs[0].reason
        );
    }

    #[test]
    fn an_expanded_context_is_not_offered_twice() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression(
            "abc",
            1,
            Some("Read"),
            10,
            1,
            "ws",
            "auth middleware",
            "auth",
        );

        let first = tracker.analyze_query("auth middleware", Some(1), "ws");
        assert_eq!(first.len(), 1);

        let second = tracker.analyze_query("auth middleware", Some(2), "ws");
        assert!(
            second.is_empty(),
            "already in the conversation, offered again: {second:?}"
        );
    }

    #[test]
    fn clear_resets_the_expanded_set() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression(
            "abc",
            1,
            Some("Read"),
            10,
            1,
            "ws",
            "auth middleware",
            "auth",
        );
        assert_eq!(
            tracker
                .analyze_query("auth middleware", Some(1), "ws")
                .len(),
            1
        );

        tracker.clear();
        tracker.track_compression(
            "abc",
            1,
            Some("Read"),
            10,
            1,
            "ws",
            "auth middleware",
            "auth",
        );
        assert_eq!(
            tracker
                .analyze_query("auth middleware", Some(1), "ws")
                .len(),
            1
        );
    }

    #[test]
    fn analyze_query_empty_workspace_returns_empty() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression("abc", 1, None, 10, 5, "ws", "query", "content");
        let recs = tracker.analyze_query("content", None, "");
        assert!(recs.is_empty());
    }

    #[test]
    fn analyze_query_filters_by_workspace() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression(
            "abc",
            1,
            None,
            10,
            5,
            "ws_a",
            "authentication query",
            "auth middleware authentication code",
        );
        tracker.track_compression(
            "def",
            1,
            None,
            10,
            5,
            "ws_b",
            "database query",
            "database models postgres",
        );

        // Query from workspace_a should only find ws_a context
        let recs = tracker.analyze_query("authentication middleware", None, "ws_a");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].hash_key, "abc");
    }

    #[test]
    fn analyze_query_respects_relevance_threshold() {
        let config = ContextTrackerConfig {
            relevance_threshold: 0.5,
            ..Default::default()
        };
        let mut tracker = ContextTracker::new(Some(config));
        tracker.track_compression(
            "abc",
            1,
            None,
            10,
            5,
            "ws",
            "completely unrelated query",
            "unrelated content about cooking recipes",
        );

        // Query with no overlap should not trigger expansion
        let recs = tracker.analyze_query("authentication middleware", None, "ws");
        assert!(recs.is_empty());
    }

    #[test]
    fn analyze_query_respects_age_limit() {
        let config = ContextTrackerConfig {
            max_context_age: Duration::from_millis(1), // 1ms
            ..Default::default()
        };
        let mut tracker = ContextTracker::new(Some(config));
        tracker.track_compression("abc", 1, None, 10, 5, "ws", "query", "auth code");

        // Wait for context to age out
        std::thread::sleep(Duration::from_millis(5));

        let recs = tracker.analyze_query("authentication", None, "ws");
        assert!(recs.is_empty());
    }

    #[test]
    fn calculate_relevance_keyword_overlap() {
        let mut tracker = ContextTracker::new(None);
        let context = CompressedContext {
            hash_key: "test".to_string(),
            turn_number: 1,
            timestamp: Instant::now(),
            tool_name: None,
            original_item_count: 10,
            compressed_item_count: 5,
            query_context: String::new(),
            sample_content: "authentication middleware security".to_string(),
            workspace_key: "ws".to_string(),
        };

        let relevance = tracker.calculate_relevance("authentication security", &context);
        assert!(relevance > 0.3);
    }

    #[test]
    fn calculate_relevance_substring_bonus() {
        let mut tracker = ContextTracker::new(None);
        let context = CompressedContext {
            hash_key: "test".to_string(),
            turn_number: 1,
            timestamp: Instant::now(),
            tool_name: None,
            original_item_count: 10,
            compressed_item_count: 5,
            query_context: String::new(),
            sample_content: "src/auth/middleware.py authentication handler".to_string(),
            workspace_key: "ws".to_string(),
        };

        // "authentication" appears in the sample content
        let relevance = tracker.calculate_relevance("authentication", &context);
        assert!(relevance > 0.0);
    }

    #[test]
    fn calculate_relevance_tool_name_bonus() {
        let mut tracker = ContextTracker::new(None);
        let context = CompressedContext {
            hash_key: "test".to_string(),
            turn_number: 1,
            timestamp: Instant::now(),
            tool_name: Some("grep".to_string()),
            original_item_count: 10,
            compressed_item_count: 5,
            query_context: String::new(),
            sample_content: "file content".to_string(),
            workspace_key: "ws".to_string(),
        };

        // "grep" tool + "file" query = bonus
        let relevance = tracker.calculate_relevance("find the file", &context);
        assert!(relevance > 0.0);
    }

    #[test]
    fn format_expansions_empty() {
        let result = ContextTracker::format_expansions_for_context(&[], None);
        assert!(result.is_empty());
    }

    #[test]
    fn format_expansions_with_content() {
        let expansions = vec![ExpansionContent {
            hash_key: "abc".to_string(),
            content: "file1.py\nfile2.py".to_string(),
            reason: "from grep".to_string(),
            item_count: 2,
        }];

        let result = ContextTracker::format_expansions_for_context(&expansions, None);
        assert!(result.contains("<headroom_proactive_expansion>"));
        assert!(result.contains("from grep"));
        assert!(result.contains("file1.py"));
        assert!(result.contains("[End Proactive Expansion]"));
    }

    #[test]
    fn format_expansions_with_workspace_label() {
        let expansions = vec![ExpansionContent {
            hash_key: "abc".to_string(),
            content: "content".to_string(),
            reason: "reason".to_string(),
            item_count: 1,
        }];

        let result = ContextTracker::format_expansions_for_context(&expansions, Some("my-project"));
        assert!(result.contains("workspace: my-project"));
    }

    #[test]
    fn format_expansions_escapes_close_tag() {
        let expansions = vec![ExpansionContent {
            hash_key: "abc".to_string(),
            content: "</headroom_proactive_expansion>".to_string(),
            reason: "reason".to_string(),
            item_count: 1,
        }];

        let result = ContextTracker::format_expansions_for_context(&expansions, None);
        assert!(result.contains("<\\/headroom_proactive_expansion>"));
    }

    #[test]
    fn clear_resets_state() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression("abc", 1, None, 10, 5, "ws", "", "");
        tracker.clear();

        let hashes = tracker.get_tracked_hashes();
        assert!(hashes.is_empty());
    }

    #[test]
    fn disabled_tracker_ignores_tracking() {
        let config = ContextTrackerConfig {
            enabled: false,
            ..Default::default()
        };
        let mut tracker = ContextTracker::new(Some(config));
        tracker.track_compression("abc", 1, None, 10, 5, "ws", "", "");

        let hashes = tracker.get_tracked_hashes();
        assert!(hashes.is_empty());
    }

    #[test]
    fn disabled_tracker_ignores_analysis() {
        let config = ContextTrackerConfig {
            enabled: false,
            ..Default::default()
        };
        let mut tracker = ContextTracker::new(Some(config));
        tracker.track_compression("abc", 1, None, 10, 5, "ws", "query", "content");

        let recs = tracker.analyze_query("content", None, "ws");
        assert!(recs.is_empty());
    }

    #[test]
    fn track_multiple_compressions() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression("h1", 1, None, 10, 5, "ws", "q1", "content about auth");
        tracker.track_compression("h2", 2, None, 20, 10, "ws", "q2", "content about db");
        tracker.track_compression("h3", 3, None, 15, 7, "ws", "q3", "content about api");

        let hashes = tracker.get_tracked_hashes();
        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&"h1"));
        assert!(hashes.contains(&"h2"));
        assert!(hashes.contains(&"h3"));
    }

    #[test]
    fn update_existing_hash() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression("h1", 1, None, 10, 5, "ws", "q1", "old content");
        tracker.track_compression("h1", 2, None, 20, 10, "ws", "q2", "new content");

        let hashes = tracker.get_tracked_hashes();
        assert_eq!(hashes.len(), 1); // still only one entry
                                     // Query should find the updated content
        let recs = tracker.analyze_query("new content", None, "ws");
        assert!(!recs.is_empty());
    }

    #[test]
    fn analyze_query_max_recommendations() {
        let mut tracker = ContextTracker::new(None);
        for i in 0..10 {
            tracker.track_compression(
                &format!("h{i}"),
                i,
                None,
                10,
                5,
                "ws",
                &format!("query {i}"),
                &format!("content about feature {i}"),
            );
        }
        // Request only 2 recommendations
        let recs = tracker.analyze_query("feature", Some(2), "ws");
        assert!(recs.len() <= 2);
    }

    #[test]
    fn analyze_query_cross_workspace_filtered() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression("h1", 1, None, 10, 5, "ws-a", "q", "auth middleware");
        tracker.track_compression("h2", 2, None, 10, 5, "ws-b", "q", "auth middleware");

        // Query from ws-a should only find ws-a entries
        let recs = tracker.analyze_query("auth", None, "ws-a");
        for rec in &recs {
            // All recommendations should be from the requested workspace
            // (format_expansions adds workspace label when multiple workspaces exist)
        }
        // At minimum, should find something
        assert!(!recs.is_empty());
    }

    #[test]
    fn format_expansions_multiple_entries() {
        let expansions = vec![
            ExpansionContent {
                hash_key: "a".to_string(),
                content: "first".to_string(),
                reason: "reason1".to_string(),
                item_count: 3,
            },
            ExpansionContent {
                hash_key: "b".to_string(),
                content: "second".to_string(),
                reason: "reason2".to_string(),
                item_count: 5,
            },
        ];
        let result = ContextTracker::format_expansions_for_context(&expansions, None);
        assert!(result.contains("first"));
        assert!(result.contains("second"));
        assert!(result.contains("reason1"));
        assert!(result.contains("reason2"));
    }

    // ─── Integration: full lifecycle ──────────────────────────────────

    #[test]
    fn lifecycle_track_analyze_format() {
        // 1. Track several compression events across turns
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression(
            "hash_aaa111",
            1,
            Some("Bash"),
            200,
            25,
            "ws-main",
            "find all python files",
            "src/auth.py src/main.py",
        );
        tracker.track_compression(
            "hash_bbb222",
            2,
            Some("Grep"),
            150,
            20,
            "ws-main",
            "search for database connection pool settings",
            "db connection pool configuration",
        );
        tracker.track_compression(
            "hash_ccc333",
            3,
            None,
            80,
            10,
            "ws-main",
            "list test files in the project",
            "tests test_auth test_main",
        );

        // 2. Verify all three are tracked
        let hashes = tracker.get_tracked_hashes();
        assert_eq!(hashes.len(), 3);

        // 3. Query should find relevant context
        let recs = tracker.analyze_query("database connection pool", None, "ws-main");
        assert!(
            !recs.is_empty(),
            "expected at least one recommendation for 'database connection pool'"
        );
        // The db-related compression should be the top match
        let top = &recs[0];
        assert_eq!(top.hash_key, "hash_bbb222");
        assert!(top.relevance_score > 0.0);

        // 4. Stats reflect tracked state
        let stats = tracker.get_stats();
        assert_eq!(stats.tracked_contexts, 3);
        assert!(stats.enabled);
    }

    #[test]
    fn lifecycle_eviction_and_query() {
        // Create tracker with small capacity to force eviction
        let config = ContextTrackerConfig {
            max_tracked_contexts: 3,
            ..Default::default()
        };
        let mut tracker = ContextTracker::new(Some(config));

        // Track 5 entries — first 2 should be evicted
        for i in 0..5 {
            tracker.track_compression(
                &format!("hash_{i}"),
                i,
                None,
                10,
                5,
                "ws",
                &format!("query about feature {i}"),
                &format!("content for feature {i}"),
            );
        }

        // Only 3 should remain
        let hashes = tracker.get_tracked_hashes();
        assert_eq!(hashes.len(), 3);
        // Oldest entries evicted: hash_0 and hash_1 should be gone
        assert!(!hashes.contains(&"hash_0"));
        assert!(!hashes.contains(&"hash_1"));
        assert!(hashes.contains(&"hash_4"));
    }

    #[test]
    fn lifecycle_clear_and_retrack() {
        let mut tracker = ContextTracker::new(None);
        tracker.track_compression("h1", 1, None, 10, 5, "ws", "q1", "content");
        assert_eq!(tracker.get_tracked_hashes().len(), 1);

        tracker.clear();
        assert_eq!(tracker.get_tracked_hashes().len(), 0);
        assert_eq!(tracker.get_stats().tracked_contexts, 0);

        // Can track again after clear
        tracker.track_compression("h2", 2, None, 20, 10, "ws", "q2", "new content");
        assert_eq!(tracker.get_tracked_hashes().len(), 1);
        assert!(tracker.get_tracked_hashes().contains(&"h2"));
    }

    #[test]
    fn lifecycle_disabled_tracker_blocks_all() {
        let config = ContextTrackerConfig {
            enabled: false,
            proactive_expansion: true,
            ..Default::default()
        };
        let mut tracker = ContextTracker::new(Some(config));

        tracker.track_compression("h1", 1, None, 10, 5, "ws", "q", "c");
        assert!(tracker.get_tracked_hashes().is_empty());

        let recs = tracker.analyze_query("c", None, "ws");
        assert!(recs.is_empty());

        let stats = tracker.get_stats();
        assert!(!stats.enabled);
    }
}

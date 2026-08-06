//! ``MemoryInjectionBudget``: uniform token/entry cap on retrieved memory.
//!
//! Bounds the formatted injection block by tokens and entry count.
//! Pure value type — no I/O.
//!
//! Mirrors Python's `headroom.proxy.memory_injection`.

use serde_json::Value;

/// Chars-per-token heuristic for budget enforcement.
const CHARS_PER_TOKEN: usize = 4;

/// Frozen budget applied at the injection boundary.
#[derive(Debug, Clone)]
pub struct MemoryInjectionBudget {
    pub max_tokens: usize,
    pub max_entries: usize,
    pub min_similarity: f64,
}

impl Default for MemoryInjectionBudget {
    fn default() -> Self {
        Self {
            max_tokens: 1024,
            max_entries: 10,
            min_similarity: 0.3,
        }
    }
}

impl MemoryInjectionBudget {
    /// Bound a formatted injection block by max_tokens.
    /// Truncates at line boundaries when possible.
    pub fn apply_to_text(&self, text: &str) -> String {
        if text.is_empty() {
            return text.to_string();
        }
        let char_budget = self.max_tokens * CHARS_PER_TOKEN;
        if text.len() <= char_budget {
            return text.to_string();
        }
        // Truncate at the last newline at or before the budget
        match text[..char_budget].rfind('\n') {
            Some(pos) if pos > 0 => text[..pos + 1].to_string(),
            _ => text[..char_budget].to_string(),
        }
    }

    /// Cap a list of ranked entries by entry count + min similarity.
    pub fn apply_to_entries(&self, entries: &[Value]) -> Vec<Value> {
        entries
            .iter()
            .filter(|e| {
                e.get("score").and_then(Value::as_f64).unwrap_or(0.0) >= self.min_similarity
            })
            .take(self.max_entries)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_to_text_within_budget() {
        let b = MemoryInjectionBudget::default();
        let text = "short text";
        assert_eq!(b.apply_to_text(text), "short text");
    }

    #[test]
    fn apply_to_text_empty() {
        let b = MemoryInjectionBudget::default();
        assert_eq!(b.apply_to_text(""), "");
    }

    #[test]
    fn apply_to_text_truncates_at_newline() {
        let b = MemoryInjectionBudget {
            max_tokens: 10,
            ..Default::default()
        };
        // 10 tokens * 4 chars = 40 char budget
        let text = "line1\nline2\nline3\nline4\n";
        let result = b.apply_to_text(text);
        assert!(result.len() <= 40);
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn apply_to_text_hard_cut_when_no_newline() {
        let b = MemoryInjectionBudget {
            max_tokens: 2,
            ..Default::default()
        };
        let text = "abcdefghij"; // 10 chars, budget = 8
        let result = b.apply_to_text(text);
        assert_eq!(result, "abcdefgh");
    }

    #[test]
    fn apply_to_entries_filters_by_score() {
        let b = MemoryInjectionBudget::default();
        let entries = vec![
            json!({"content": "a", "score": 0.9}),
            json!({"content": "b", "score": 0.1}),
            json!({"content": "c", "score": 0.5}),
        ];
        let result = b.apply_to_entries(&entries);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["content"], "a");
        assert_eq!(result[1]["content"], "c");
    }

    #[test]
    fn apply_to_entries_caps_at_max() {
        let b = MemoryInjectionBudget {
            max_entries: 2,
            min_similarity: 0.0,
            ..Default::default()
        };
        let entries = vec![
            json!({"content": "a", "score": 0.9}),
            json!({"content": "b", "score": 0.8}),
            json!({"content": "c", "score": 0.7}),
        ];
        let result = b.apply_to_entries(&entries);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn apply_to_entries_empty() {
        let b = MemoryInjectionBudget::default();
        assert!(b.apply_to_entries(&[]).is_empty());
    }

    #[test]
    fn default_budget() {
        let b = MemoryInjectionBudget::default();
        assert_eq!(b.max_tokens, 1024);
        assert_eq!(b.max_entries, 10);
        assert!((b.min_similarity - 0.3).abs() < f64::EPSILON);
    }
}

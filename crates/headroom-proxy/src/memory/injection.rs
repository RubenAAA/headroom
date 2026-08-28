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
        // The budget counts bytes, but a memory entry holds whatever the user
        // wrote, in any script. Slicing at a byte that falls inside a
        // multi-byte character panics, and this runs on the request thread —
        // the task dies, hyper aborts the connection, and the client sees
        // ECONNRESET and retries into the same memory and the same panic.
        // Walk back to a boundary first.
        let mut end = char_budget;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        // Truncate at the last newline at or before the budget
        match text[..end].rfind('\n') {
            Some(pos) if pos > 0 => text[..pos + 1].to_string(),
            _ => text[..end].to_string(),
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

    /// A memory entry is whatever the user wrote, in any script, and the
    /// budget counts bytes. This panicked in production: a Cyrillic entry put
    /// a two-byte character across byte 4096, the slice panicked on the
    /// request thread, hyper aborted the connection, and the client retried
    /// into the same memory and the same panic — a reset loop that could not
    /// clear itself.
    #[test]
    fn apply_to_text_cuts_on_a_char_boundary_not_a_byte_count() {
        let b = MemoryInjectionBudget {
            max_tokens: 2,
            ..Default::default()
        };
        // Budget is 8 bytes. Four 2-byte characters put a boundary at 8, so
        // pad by one byte to push a character across it.
        let text = format!("x{}", "и".repeat(8));
        let out = b.apply_to_text(&text);
        assert!(text.starts_with(&out), "the cut must be a prefix");
        assert!(out.len() <= 8, "cut past the budget: {}", out.len());
    }

    /// The same, with no ASCII lead-in and no newline to fall back on.
    #[test]
    fn apply_to_text_survives_a_multibyte_run_with_no_newline() {
        let b = MemoryInjectionBudget {
            max_tokens: 1,
            ..Default::default()
        };
        for pad in 0..4 {
            let text = format!("{}{}", "x".repeat(pad), "\u{1f600}".repeat(4));
            let out = b.apply_to_text(&text);
            assert!(text.starts_with(&out), "pad={pad}");
        }
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

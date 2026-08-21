//! Parse Claude Code transcript JSONL files for per-window token breakdowns
//! (port of `headroom/subscription/session_tracking.py`).
//!
//! Reads `~/.claude/projects/**/*.jsonl` and aggregates token usage for entries
//! whose timestamp falls within a window. Model weights (Sonnet-normalised):
//! opus 2.0×, sonnet 1.0×, haiku 0.5×.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{json, Map, Value};

use super::models::WindowTokens;
use super::parse_timestamp;

/// Maximum bytes to read per transcript file (10 MB cap).
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

pub const DEFAULT_MODEL_WEIGHT: f64 = 1.0;

/// Sonnet-normalised model family weights, in match order.
const MODEL_FAMILY_WEIGHTS: &[(&str, f64)] = &[("opus", 2.0), ("sonnet", 1.0), ("haiku", 0.5)];

fn claude_config_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".claude")
        })
}

/// Sonnet-normalised weight for a model ID (word-boundary family match).
pub fn get_model_weight(model_id: &str) -> f64 {
    let lower = model_id.to_lowercase();
    for (family, weight) in MODEL_FAMILY_WEIGHTS {
        // Python: (?<![a-z])family(?![a-z]). Rust's regex crate has no
        // lookaround, so emulate with capture around the family token.
        let pattern = format!(r"(^|[^a-z]){}([^a-z]|$)", family);
        if Regex::new(&pattern).unwrap().is_match(&lower) {
            return *weight;
        }
    }
    DEFAULT_MODEL_WEIGHT
}

/// Return all `.jsonl` files under `~/.claude/projects`.
pub fn find_transcript_files() -> Vec<PathBuf> {
    let projects = claude_config_dir().join("projects");
    let mut results = Vec::new();
    walk_jsonl(&projects, &mut results);
    results
}

fn walk_jsonl(directory: &Path, results: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(directory) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, results);
        } else if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            results.push(path);
        }
    }
}

fn read_transcript_lines(path: &Path) -> Vec<String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let read_size = size.min(MAX_FILE_BYTES) as usize;
    let mut buf = vec![0u8; read_size];
    let mut handle = file;
    if handle.read_exact(&mut buf).is_err() {
        // Fall back to a best-effort read if the file shrank between stat/read.
        buf.clear();
        if handle.take(MAX_FILE_BYTES).read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
    }
    String::from_utf8_lossy(&buf)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

fn usage_i64(usage: &Value, key: &str) -> i64 {
    usage
        .get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

fn add_usage_to_tokens(dest: &mut WindowTokens, usage: &Value) {
    dest.input += usage_i64(usage, "input_tokens");
    dest.output += usage_i64(usage, "output_tokens");
    dest.cache_reads += usage_i64(usage, "cache_read_input_tokens");

    let cache_creation = usage.get("cache_creation").cloned().unwrap_or(json!({}));
    let w5m = usage_i64(&cache_creation, "ephemeral_5m_input_tokens");
    let w1h = usage_i64(&cache_creation, "ephemeral_1h_input_tokens");
    let total_writes = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(w5m + w1h);

    dest.cache_writes_5m += w5m;
    dest.cache_writes_1h += w1h;
    dest.cache_writes_total += total_writes;
}

fn total_token_count(t: &WindowTokens) -> i64 {
    t.input + t.output + t.cache_reads + t.cache_writes_total
}

fn window_tokens_to_map(t: &WindowTokens) -> Value {
    json!({
        "input": t.input,
        "output": t.output,
        "cache_reads": t.cache_reads,
        "cache_writes_5m": t.cache_writes_5m,
        "cache_writes_1h": t.cache_writes_1h,
        "cache_writes_total": t.cache_writes_total,
    })
}

/// Sum transcript token usage for entries in `[start_ts, end_ts)` (Unix seconds).
pub fn compute_window_tokens(start_ts: f64, end_ts: f64) -> WindowTokens {
    let mut totals = WindowTokens::default();
    // Preserve first-seen model order to mirror Python's dict iteration.
    let mut model_order: Vec<String> = Vec::new();
    let mut by_model: std::collections::HashMap<String, WindowTokens> =
        std::collections::HashMap::new();
    let mut unattributed = WindowTokens::default();
    // Claude Code can store one assistant response across several transcript
    // lines, one per content block, each carrying the SAME request-level
    // `message.usage`. Summing per line multiplies a single response's tokens
    // by its block count — 19x on one 420K response upstream. Count each
    // response once, keyed by the Anthropic `message.id`. Entries with no id
    // keep the per-line behaviour, so this only ever drops true duplicates.
    let mut seen_message_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for path in find_transcript_files() {
        for line in read_transcript_lines(&path) {
            let entry: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let ts_str = match entry.get("timestamp").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            let ts = match parse_timestamp(ts_str) {
                Some(dt) => dt.timestamp() as f64,
                None => continue,
            };
            if ts < start_ts || ts >= end_ts {
                continue;
            }

            let msg = entry.get("message").cloned().unwrap_or(json!({}));
            let usage = match msg.get("usage") {
                Some(u) if !u.is_null() => u.clone(),
                _ => continue,
            };

            if let Some(msg_id) = msg.get("id").and_then(|v| v.as_str()) {
                if !msg_id.is_empty() && !seen_message_ids.insert(msg_id.to_string()) {
                    continue;
                }
            }

            add_usage_to_tokens(&mut totals, &usage);

            match msg.get("model").and_then(|v| v.as_str()) {
                Some(model_id) if !model_id.is_empty() => {
                    if !by_model.contains_key(model_id) {
                        model_order.push(model_id.to_string());
                        by_model.insert(model_id.to_string(), WindowTokens::default());
                    }
                    add_usage_to_tokens(by_model.get_mut(model_id).unwrap(), &usage);
                }
                _ => add_usage_to_tokens(&mut unattributed, &usage),
            }
        }
    }

    let mut weighted = 0.0f64;
    let mut by_model_out = Map::new();
    for model_id in &model_order {
        let mt = &by_model[model_id];
        weighted += total_token_count(mt) as f64 * get_model_weight(model_id);
        by_model_out.insert(model_id.clone(), window_tokens_to_map(mt));
    }
    weighted += total_token_count(&unattributed) as f64 * DEFAULT_MODEL_WEIGHT;

    totals.weighted_token_equivalent = weighted;
    totals.by_model = by_model_out;
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn model_weights_by_family() {
        assert_eq!(get_model_weight("claude-opus-4-20250101"), 2.0);
        assert_eq!(get_model_weight("claude-sonnet-4"), 1.0);
        assert_eq!(get_model_weight("claude-haiku-3"), 0.5);
        assert_eq!(get_model_weight("gpt-4o"), DEFAULT_MODEL_WEIGHT);
    }

    #[test]
    fn compute_window_tokens_reads_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects").join("proj");
        std::fs::create_dir_all(&projects).unwrap();
        let line = json!({
            "timestamp": "2026-06-17T12:00:00Z",
            "message": {
                "model": "claude-opus-4",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "cache_read_input_tokens": 10,
                    "cache_creation_input_tokens": 5,
                }
            }
        })
        .to_string();
        std::fs::write(projects.join("a.jsonl"), format!("{line}\n")).unwrap();

        // Held across the read below, not just the write: `client`'s tests set
        // and clear the same variable, and this one is only correct while the
        // value it wrote is still there.
        let _guard = crate::subscription::env_guard();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir.path());
        let start = chrono::Utc
            .with_ymd_and_hms(2026, 6, 17, 0, 0, 0)
            .unwrap()
            .timestamp() as f64;
        let end = chrono::Utc
            .with_ymd_and_hms(2026, 6, 18, 0, 0, 0)
            .unwrap()
            .timestamp() as f64;
        let tokens = compute_window_tokens(start, end);
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        assert_eq!(tokens.input, 100);
        assert_eq!(tokens.output, 20);
        assert_eq!(tokens.cache_reads, 10);
        assert_eq!(tokens.cache_writes_total, 5);
        // opus weight 2.0 × (100+20+10+5) = 270
        assert_eq!(tokens.weighted_token_equivalent, 270.0);
        assert!(tokens.by_model.contains_key("claude-opus-4"));
    }

    #[test]
    fn compute_window_tokens_counts_each_message_id_once() {
        // One response can span several lines, one per content block, each
        // repeating the response's usage. Three lines carrying two ids must
        // count twice, not three times.
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects").join("proj");
        std::fs::create_dir_all(&projects).unwrap();
        let block = |id: &str| {
            json!({
                "timestamp": "2026-06-17T12:00:00Z",
                "message": {
                    "id": id,
                    "model": "claude-opus-4",
                    "usage": {"input_tokens": 100, "output_tokens": 20}
                }
            })
            .to_string()
        };
        let lines = format!(
            "{}\n{}\n{}\n",
            block("msg_a"),
            block("msg_a"),
            block("msg_b")
        );
        std::fs::write(projects.join("a.jsonl"), lines).unwrap();

        let _guard = crate::subscription::env_guard();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir.path());
        let start = chrono::Utc
            .with_ymd_and_hms(2026, 6, 17, 0, 0, 0)
            .unwrap()
            .timestamp() as f64;
        let end = chrono::Utc
            .with_ymd_and_hms(2026, 6, 18, 0, 0, 0)
            .unwrap()
            .timestamp() as f64;
        let tokens = compute_window_tokens(start, end);
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        assert_eq!(tokens.input, 200, "msg_a once plus msg_b once");
        assert_eq!(tokens.output, 40);
    }
}

//! Memory helper functions ported from `headroom/proxy/memory_handler.py`.
//!
//! * `background_dedup` — fire-and-forget auto-dedup of near-duplicates
//!   (cosine similarity >= 0.92). Mirrors `MemoryHandler._background_dedup`.
//! * `append_to_latest_user_tail` — provider-aware dispatch for injecting
//!   memory context into the last user message. Mirrors
//!   `MemoryHandler._append_to_latest_user_tail`.
//! * `resolve_native_path` — path traversal prevention for native file
//!   operations. Mirrors `MemoryHandler._resolve_native_path`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::memory::backend::MemoryBackend;
use crate::memory::backend::MemorySearchResult;

/// Cosine-similarity threshold above which memories are auto-deleted as
/// duplicates. Matches Python's `MemoryHandler.DEDUP_AUTO_THRESHOLD`.
const DEDUP_AUTO_THRESHOLD: f64 = 0.92;

/// Fire-and-forget auto-dedup of near-duplicates.
///
/// Iterates `similar_results`; if any have cosine similarity >=
/// [`DEDUP_AUTO_THRESHOLD`] to the newly saved memory, the older
/// duplicate is deleted. This runs asynchronously and never blocks
/// the tool response.
///
/// How alike two memories are, from 0 (nothing shared) to 1 (same words).
///
/// A Dice coefficient over lowercased word sets, ignoring words of three
/// characters or fewer. Deterministic, needs no model, and — unlike the BM25
/// ranks this module used to threshold — actually bounded in 0..1.
///
/// Calibrated on the 37 real memories, all 666 pairs: the most alike *distinct*
/// pair scores **0.397** and the median pair 0.255, while a reworded duplicate
/// scores 0.706 and a punctuation-only one 1.000. That gap is what makes the
/// thresholds in `handler` safe.
pub fn text_similarity(a: &str, b: &str) -> f64 {
    let words = |t: &str| -> std::collections::HashSet<String> {
        t.to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            // Short words carry no signal, but short *numbers* carry most of it
            // here: "511 percent" and "55 percent" are different findings, and
            // dropping the figure would merge them.
            .filter(|w| w.len() > 3 || w.chars().any(|c| c.is_ascii_digit()))
            .map(str::to_string)
            .collect()
    };
    let (a, b) = (words(a), words(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    2.0 * a.intersection(&b).count() as f64 / (a.len() + b.len()) as f64
}

/// Mirrors `MemoryHandler._background_dedup`.
///
/// **No caller since 2026-08-18, deliberately.** `execute_save` used to spawn
/// this on every save. The threshold it takes is a cosine similarity, and the
/// only backend we run scores with BM25, whose ranks sit near 0.03 even for
/// near-identical text — so the comparison was meaningless and it deleted
/// unrelated memories: 8 of 12 in a concurrency test. Saving now merges
/// duplicates with [`text_similarity`] instead, which is bounded and reported.
/// Do not wire this back.
pub async fn background_dedup(
    new_memory_id: &str,
    similar_results: &[MemorySearchResult],
    backend: &dyn MemoryBackend,
) {
    for result in similar_results {
        if result.score < DEDUP_AUTO_THRESHOLD {
            continue;
        }
        if result.memory.id == new_memory_id {
            continue;
        }

        // Skip if already superseded.
        if result
            .memory
            .metadata
            .get("superseded_by")
            .and_then(Value::as_str)
            .is_some()
        {
            continue;
        }

        match backend.delete_memory(&result.memory.id).await {
            Ok(_) => {
                let preview = result.memory.content.chars().take(50).collect::<String>();
                let agent = result
                    .memory
                    .metadata
                    .get("source_agent")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                tracing::info!(
                    old_id = %result.memory.id,
                    preview = %preview,
                    score = result.score,
                    superseded_by = %new_memory_id,
                    agent = %agent,
                    "memory dedup: removed duplicate"
                );
            }
            Err(e) => {
                tracing::warn!(
                    old_id = %result.memory.id,
                    error = %e,
                    "memory background dedup failed"
                );
            }
        }
    }
}

/// Provider-aware dispatch for injecting memory context into the last
/// user message.
///
/// Selects the correct tail-append helper for the provider's content
/// shape:
/// - `"anthropic"`: appends a text block to the latest non-frozen user
///   turn (respects `frozen_message_count`).
/// - `"openai"`: appends to the first text block of the latest user
///   chat message.
///
/// Returns `(new_messages, bytes_appended)`. `bytes_appended == 0` means
/// no eligible user text block was found; the message list is returned
/// unchanged.
///
/// Mirrors `MemoryHandler._append_to_latest_user_tail`.
pub fn append_to_latest_user_tail(
    messages: &mut Vec<Value>,
    context_text: &str,
    provider: &str,
    frozen_message_count: usize,
) -> usize {
    if messages.is_empty() || context_text.is_empty() {
        return 0;
    }

    match provider {
        "anthropic" => {
            // Find the latest user message outside the frozen prefix.
            let user_indices: Vec<usize> = messages
                .iter()
                .enumerate()
                .filter(|(_, msg)| msg.get("role").and_then(Value::as_str) == Some("user"))
                .map(|(i, _)| i)
                .collect();

            // The eligible user messages are those after the frozen prefix.
            let eligible: Vec<usize> = user_indices
                .into_iter()
                .filter(|&i| i >= frozen_message_count)
                .collect();

            let Some(&last_user_idx) = eligible.last() else {
                return 0;
            };

            let message = &mut messages[last_user_idx];
            match message.get_mut("content") {
                Some(Value::String(s)) => {
                    s.push_str("\n\n");
                    s.push_str(context_text);
                    context_text.len()
                }
                Some(Value::Array(blocks)) => {
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": context_text,
                    }));
                    context_text.len()
                }
                _ => {
                    message["content"] = Value::String(context_text.to_string());
                    context_text.len()
                }
            }
        }
        "openai" => {
            // OpenAI Chat Completions: body["messages"]
            crate::body::append_text_to_latest_user_chat_message(messages, context_text)
        }
        _ => 0,
    }
}

/// Resolve a path within the user's memory directory safely.
///
/// Prevents path traversal attacks by:
/// 1. Stripping the `/memories` prefix if present.
/// 2. Joining with the user-scoped directory.
/// 3. Normalizing (`.` and `..` components).
/// 4. Verifying the normalized result is still within the user directory.
///
/// Returns the resolved path on success, or an error describing the
/// traversal violation.
///
/// Mirrors `MemoryHandler._resolve_native_path`.
pub fn resolve_native_path(
    path: &str,
    user_id: &str,
    native_memory_dir: &Path,
) -> Result<PathBuf, String> {
    let user_dir = native_memory_dir.join(user_id);

    // Normalize: strip /memories prefix and leading slash.
    let mut normalized = path;
    if let Some(rest) = normalized.strip_prefix("/memories") {
        normalized = rest;
    }
    if let Some(rest) = normalized.strip_prefix('/') {
        normalized = rest;
    }

    // Join and normalize (Python's Path.resolve(strict=False)).
    let joined = user_dir.join(normalized);
    let resolved = normalize_path(&joined);

    // Security: ensure resolved path starts with user_dir.
    let user_dir_canonical = normalize_path(&user_dir);
    if resolved.starts_with(&user_dir_canonical) {
        Ok(resolved)
    } else {
        Err(format!("Path traversal detected: {path}"))
    }
}

/// Normalize a path by resolving `.` and `..` components without requiring
/// the path to exist. Matches Python's `Path.resolve(strict=False)`.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── background_dedup ─────────────────────────────────────────

    #[test]
    fn dedup_threshold_constant_matches_python() {
        assert_eq!(DEDUP_AUTO_THRESHOLD, 0.92);
    }

    // ── append_to_latest_user_tail ───────────────────────────────

    #[test]
    fn tail_empty_messages_returns_zero() {
        let mut msgs = vec![];
        let n = append_to_latest_user_tail(&mut msgs, "ctx", "anthropic", 0);
        assert_eq!(n, 0);
    }

    #[test]
    fn tail_empty_context_returns_zero() {
        let mut msgs = vec![json!({"role": "user", "content": "hello"})];
        let n = append_to_latest_user_tail(&mut msgs, "", "anthropic", 0);
        assert_eq!(n, 0);
    }

    #[test]
    fn tail_anthropic_string_content() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
            json!({"role": "user", "content": "question"}),
        ];
        let n = append_to_latest_user_tail(&mut msgs, "CTX", "anthropic", 0);
        assert_eq!(n, 3);
        let content = msgs[2]["content"].as_str().unwrap();
        assert!(content.ends_with("CTX"));
        assert!(content.starts_with("question"));
    }

    #[test]
    fn tail_anthropic_array_content() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}]
        })];
        let n = append_to_latest_user_tail(&mut msgs, "CTX", "anthropic", 0);
        assert_eq!(n, 3);
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["text"], "CTX");
    }

    #[test]
    fn tail_anthropic_respects_frozen_prefix() {
        let mut msgs = vec![
            json!({"role": "user", "content": "frozen"}),
            json!({"role": "user", "content": "live"}),
        ];
        // frozen_message_count=1 means index 0 is frozen.
        let n = append_to_latest_user_tail(&mut msgs, "CTX", "anthropic", 1);
        assert_eq!(n, 3);
        // Index 0 should be untouched.
        assert_eq!(msgs[0]["content"].as_str().unwrap(), "frozen");
        // Index 1 should have CTX appended.
        assert!(msgs[1]["content"].as_str().unwrap().ends_with("CTX"));
    }

    #[test]
    fn tail_anthropic_all_frozen_returns_zero() {
        let mut msgs = vec![
            json!({"role": "user", "content": "frozen1"}),
            json!({"role": "user", "content": "frozen2"}),
        ];
        let n = append_to_latest_user_tail(&mut msgs, "CTX", "anthropic", 2);
        assert_eq!(n, 0);
    }

    #[test]
    fn tail_openai_dispatches() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}]
        })];
        let n = append_to_latest_user_tail(&mut msgs, "CTX", "openai", 0);
        assert_eq!(n, 3);
    }

    #[test]
    fn tail_unknown_provider_returns_zero() {
        let mut msgs = vec![json!({"role": "user", "content": "hello"})];
        let n = append_to_latest_user_tail(&mut msgs, "CTX", "gemini", 0);
        assert_eq!(n, 0);
    }

    // ── resolve_native_path ──────────────────────────────────────

    #[test]
    fn resolve_simple_path() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_native_path("file.txt", "user1", dir.path()).unwrap();
        assert!(result.starts_with(dir.path().join("user1")));
        assert!(result.ends_with("file.txt"));
    }

    #[test]
    fn resolve_strips_memories_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_native_path("/memories/file.txt", "user1", dir.path()).unwrap();
        assert!(result.ends_with("file.txt"));
    }

    #[test]
    fn resolve_strips_leading_slash() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_native_path("/file.txt", "user1", dir.path()).unwrap();
        assert!(result.ends_with("file.txt"));
    }

    #[test]
    fn resolve_traversal_detected() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_native_path("../../etc/passwd", "user1", dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("traversal"));
    }

    #[test]
    fn resolve_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_native_path("subdir/file.txt", "user1", dir.path()).unwrap();
        assert!(result.ends_with("subdir/file.txt"));
    }
}

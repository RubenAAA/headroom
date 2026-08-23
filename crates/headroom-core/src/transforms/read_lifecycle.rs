//! Event-driven Read lifecycle management.
//!
//! Detects stale and superseded Read tool outputs in conversation messages and
//! replaces them with compact markers + CCR hashes. Fresh Reads are never touched.
//!
//! A Read becomes STALE when its file is subsequently edited — the content in
//! context is factually wrong. A Read becomes SUPERSEDED when the same file is
//! re-Read — the content is redundant. Both are provably safe to replace.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

use crate::ccr::CcrStore;

/// Lifecycle state of a Read output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadState {
    /// Latest read, no subsequent edit — leave untouched.
    Fresh,
    /// File was edited after this Read — content is wrong.
    Stale,
    /// File was re-Read after this Read — content is redundant.
    Superseded,
}

impl std::fmt::Display for ReadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadState::Fresh => write!(f, "fresh"),
            ReadState::Stale => write!(f, "stale"),
            ReadState::Superseded => write!(f, "superseded"),
        }
    }
}

/// A single file operation observed in the conversation.
#[derive(Debug, Clone)]
struct FileOperation {
    msg_index: usize,
    tool_call_id: String,
    tool_name: String,
    file_path: String,
    /// "read" | "edit" | "write"
    operation: String,
    content_size: usize,
    read_offset: Option<usize>,
    read_limit: Option<usize>,
}

/// Classification of a single Read output.
#[derive(Debug, Clone)]
pub struct ReadClassification {
    pub msg_index: usize,
    pub tool_call_id: String,
    pub file_path: String,
    pub state: ReadState,
    pub content_size: usize,
}

/// Format a read_lifecycle transform tag including the source file path.
///
/// Shape: `read_lifecycle:<state>:<file_path>`. Consumers splitting on `:`
/// must bound the split to 3 parts so paths containing `:` are preserved.
pub fn format_read_lifecycle_transform(classification: &ReadClassification) -> String {
    let path = if classification.file_path.is_empty() {
        ""
    } else {
        &classification.file_path
    };
    format!("read_lifecycle:{}:{}", classification.state, path)
}

/// Configuration for Read lifecycle management.
#[derive(Debug, Clone)]
pub struct ReadLifecycleConfig {
    /// On by default: stale/superseded Reads are provably safe to compress.
    pub enabled: bool,
    /// Replace Reads of files that were later edited.
    pub compress_stale: bool,
    /// Disabled by default: busts Anthropic prompt cache prefix.
    pub compress_superseded: bool,
    /// Skip tiny Read outputs (not worth the overhead).
    pub min_size_bytes: usize,
}

impl Default for ReadLifecycleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            compress_stale: true,
            compress_superseded: false,
            min_size_bytes: 512,
        }
    }
}

/// Tool names recognized as Read operations.
const READ_TOOL_NAMES: &[&str] = &["Read", "read"];

/// Tool names recognized as mutating operations.
const EDIT_TOOL_NAMES: &[&str] = &["Edit", "edit", "MultiEdit", "NotebookEdit"];
const WRITE_TOOL_NAMES: &[&str] = &["Write", "write"];

fn is_read_tool(name: &str) -> bool {
    READ_TOOL_NAMES.contains(&name)
}

fn is_mutating_tool(name: &str) -> bool {
    EDIT_TOOL_NAMES.contains(&name) || WRITE_TOOL_NAMES.contains(&name)
}

/// Output of lifecycle management pass.
#[derive(Debug, Clone, Default)]
pub struct ReadLifecycleResult {
    pub messages: Vec<Value>,
    pub reads_total: usize,
    pub reads_stale: usize,
    pub reads_superseded: usize,
    pub reads_fresh: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub transforms_applied: Vec<String>,
    pub ccr_hashes: Vec<String>,
}

/// Tool metadata extracted from a message: (name, file_path, offset, limit).
type ToolMeta = (String, Option<String>, Option<usize>, Option<usize>);

/// Event-driven Read lifecycle management.
///
/// Pre-processes messages to identify and replace stale/superseded Read outputs.
pub struct ReadLifecycleManager {
    config: ReadLifecycleConfig,
    store: Option<Arc<dyn CcrStore>>,
}

impl ReadLifecycleManager {
    pub fn new(config: ReadLifecycleConfig, compression_store: Option<Arc<dyn CcrStore>>) -> Self {
        Self {
            config,
            store: compression_store,
        }
    }

    /// Apply lifecycle management to messages.
    pub fn apply(&self, messages: &[Value], frozen_message_count: usize) -> ReadLifecycleResult {
        if !self.config.enabled {
            return ReadLifecycleResult {
                messages: messages.to_vec(),
                ..Default::default()
            };
        }

        let mut classifications = self.classify(messages);

        if classifications.is_empty() {
            return ReadLifecycleResult {
                messages: messages.to_vec(),
                ..Default::default()
            };
        }

        // Phase 3: Filter out replacements in frozen prefix
        if frozen_message_count > 0 {
            let frozen_skipped = classifications
                .iter()
                .filter(|c| c.state != ReadState::Fresh && c.msg_index < frozen_message_count)
                .count();

            if frozen_skipped > 0 {
                tracing::info!(
                    frozen_skipped,
                    frozen_message_count,
                    "ReadLifecycle: skipping stale/superseded replacements in frozen prefix"
                );
                for c in &mut classifications {
                    if c.msg_index < frozen_message_count && c.state != ReadState::Fresh {
                        c.state = ReadState::Fresh;
                    }
                }
            }
        }

        // Phase 4: Replace stale/superseded content
        self.apply_lifecycle(messages, &classifications)
    }

    /// Classify every Read in `messages` without rewriting anything.
    ///
    /// Split out of [`Self::apply`] so callers that only need to know which
    /// Reads have gone stale can ask without taking the rewrite — the CCR
    /// retrieval path warns on stale content but must not touch forwarded
    /// bytes, since a footer that flips mid-conversation breaks the prefix at
    /// that block.
    pub fn classify(&self, messages: &[Value]) -> Vec<ReadClassification> {
        let tool_metadata = self.build_tool_metadata(messages);
        let file_ops = self.build_file_operation_index(messages, &tool_metadata);
        self.classify_reads(&file_ops)
    }

    /// Build tool_call_id → (tool_name, file_path, offset, limit) mapping.
    fn build_tool_metadata(&self, messages: &[Value]) -> HashMap<String, ToolMeta> {
        let mut metadata = HashMap::new();

        for msg in messages {
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }

            // OpenAI format: tool_calls array
            if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let tc_id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                    let func = tc.get("function").unwrap_or(&Value::Null);
                    let name = func.get("name").and_then(Value::as_str).unwrap_or("");

                    if tc_id.is_empty() || name.is_empty() {
                        continue;
                    }

                    let (file_path, offset, limit) =
                        if let Some(args_str) = func.get("arguments").and_then(Value::as_str) {
                            if let Ok(args) = serde_json::from_str::<Value>(args_str) {
                                let fp = args
                                    .get("file_path")
                                    .or_else(|| args.get("path"))
                                    .and_then(Value::as_str)
                                    .map(String::from);
                                let off = args
                                    .get("offset")
                                    .and_then(Value::as_u64)
                                    .map(|v| v as usize);
                                let lim = args
                                    .get("limit")
                                    .and_then(Value::as_u64)
                                    .map(|v| v as usize);
                                (fp, off, lim)
                            } else {
                                (None, None, None)
                            }
                        } else {
                            (None, None, None)
                        };

                    metadata.insert(
                        tc_id.to_string(),
                        (name.to_string(), file_path, offset, limit),
                    );
                }
            }

            // Anthropic format: content blocks with type=tool_use
            if let Some(content) = msg.get("content").and_then(Value::as_array) {
                for block in content {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let tc_id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");

                    if tc_id.is_empty() || name.is_empty() {
                        continue;
                    }

                    let inp = block.get("input").unwrap_or(&Value::Null);
                    let file_path = inp
                        .get("file_path")
                        .or_else(|| inp.get("path"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let offset = inp
                        .get("offset")
                        .and_then(Value::as_u64)
                        .map(|v| v as usize);
                    let limit = inp.get("limit").and_then(Value::as_u64).map(|v| v as usize);

                    metadata.insert(
                        tc_id.to_string(),
                        (name.to_string(), file_path, offset, limit),
                    );
                }
            }
        }

        metadata
    }

    /// Build file_path → [FileOperation] index.
    fn build_file_operation_index(
        &self,
        messages: &[Value],
        tool_metadata: &HashMap<String, ToolMeta>,
    ) -> HashMap<String, Vec<FileOperation>> {
        let mut file_ops: HashMap<String, Vec<FileOperation>> = HashMap::new();

        for (tc_id, (name, file_path, offset, limit)) in tool_metadata {
            let file_path = match file_path {
                Some(fp) if !fp.is_empty() => fp,
                _ => continue,
            };

            let operation = if is_read_tool(name) {
                "read"
            } else if is_mutating_tool(name) {
                "edit"
            } else {
                continue;
            };

            let msg_idx = self.find_tool_call_msg_index(messages, tc_id);
            let msg_idx = match msg_idx {
                Some(idx) => idx,
                None => continue,
            };

            file_ops
                .entry(file_path.clone())
                .or_default()
                .push(FileOperation {
                    msg_index: msg_idx,
                    tool_call_id: tc_id.clone(),
                    tool_name: name.clone(),
                    file_path: file_path.clone(),
                    operation: operation.to_string(),
                    content_size: 0,
                    read_offset: if operation == "read" { *offset } else { None },
                    read_limit: if operation == "read" { *limit } else { None },
                });
        }

        file_ops
    }

    /// Find the message index containing a specific tool_call_id.
    fn find_tool_call_msg_index(&self, messages: &[Value], tool_call_id: &str) -> Option<usize> {
        for (i, msg) in messages.iter().enumerate() {
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }

            // OpenAI format
            if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    if tc.get("id").and_then(Value::as_str) == Some(tool_call_id) {
                        return Some(i);
                    }
                }
            }

            // Anthropic format
            if let Some(content) = msg.get("content").and_then(Value::as_array) {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && block.get("id").and_then(Value::as_str) == Some(tool_call_id)
                    {
                        return Some(i);
                    }
                }
            }
        }

        None
    }

    /// Check if `later` read fully covers the line range of `earlier`.
    fn read_covers(later: &FileOperation, earlier: &FileOperation) -> bool {
        // Full-file read supersedes anything
        if later.read_offset.is_none() && later.read_limit.is_none() {
            return true;
        }

        // If the earlier was a full-file read, a partial can't cover it
        if earlier.read_offset.is_none() && earlier.read_limit.is_none() {
            return false;
        }

        // Both are partial reads — check range containment
        let later_start = later.read_offset.unwrap_or(0);
        let later_end = later_start + later.read_limit.unwrap_or(2000);
        let earlier_start = earlier.read_offset.unwrap_or(0);
        let earlier_end = earlier_start + earlier.read_limit.unwrap_or(2000);

        later_start <= earlier_start && later_end >= earlier_end
    }

    /// Classify each Read as fresh, stale, or superseded.
    fn classify_reads(
        &self,
        file_ops: &HashMap<String, Vec<FileOperation>>,
    ) -> Vec<ReadClassification> {
        let mut classifications = Vec::new();

        for (_file_path, ops) in file_ops {
            let reads: Vec<&FileOperation> =
                ops.iter().filter(|op| op.operation == "read").collect();
            let edits: Vec<&FileOperation> =
                ops.iter().filter(|op| op.operation == "edit").collect();

            if reads.is_empty() {
                continue;
            }

            for read_op in &reads {
                // Check stale: any edit/write of this file AFTER this read?
                let is_stale = self.config.compress_stale
                    && edits.iter().any(|e| e.msg_index > read_op.msg_index);

                // Check superseded: any later read that FULLY COVERS this read's range?
                let is_superseded = self.config.compress_superseded
                    && reads
                        .iter()
                        .any(|r| r.msg_index > read_op.msg_index && Self::read_covers(r, read_op));

                let state = if is_stale {
                    ReadState::Stale
                } else if is_superseded {
                    ReadState::Superseded
                } else {
                    ReadState::Fresh
                };

                classifications.push(ReadClassification {
                    msg_index: read_op.msg_index,
                    tool_call_id: read_op.tool_call_id.clone(),
                    file_path: read_op.file_path.clone(),
                    state,
                    content_size: read_op.content_size,
                });
            }
        }

        classifications
    }

    /// Replace stale/superseded Read content with markers.
    fn apply_lifecycle(
        &self,
        messages: &[Value],
        classifications: &[ReadClassification],
    ) -> ReadLifecycleResult {
        // Build lookup: tool_call_id → classification (for non-fresh reads)
        let replacements: HashMap<&str, &ReadClassification> = classifications
            .iter()
            .filter(|c| c.state != ReadState::Fresh)
            .map(|c| (c.tool_call_id.as_str(), c))
            .collect();

        if replacements.is_empty() {
            return ReadLifecycleResult {
                messages: messages.to_vec(),
                reads_total: classifications.len(),
                reads_fresh: classifications.len(),
                ..Default::default()
            };
        }

        let mut result_messages = Vec::with_capacity(messages.len());
        let mut transforms = Vec::new();
        let mut ccr_hashes = Vec::new();
        let mut bytes_before = 0;
        let mut bytes_after = 0;

        let mut counts = HashMap::new();
        counts.insert(ReadState::Fresh, 0usize);
        counts.insert(ReadState::Stale, 0usize);
        counts.insert(ReadState::Superseded, 0usize);
        for c in classifications {
            *counts.entry(c.state).or_insert(0) += 1;
        }

        for msg in messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            let content = msg.get("content");

            // OpenAI format: role=tool with tool_call_id
            if role == "tool" {
                let tc_id = msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(classification) = replacements.get(tc_id) {
                    if let Some(content_str) = content.and_then(Value::as_str) {
                        let (replaced, marker, ccr_hash) =
                            self.replace_content(content_str, classification);
                        if replaced {
                            let mut new_msg = msg.clone();
                            new_msg["content"] = Value::String(marker.clone());
                            result_messages.push(new_msg);
                            transforms.push(format_read_lifecycle_transform(classification));
                            if let Some(hash) = ccr_hash {
                                ccr_hashes.push(hash);
                            }
                            bytes_before += content_str.len();
                            bytes_after += marker.len();
                            continue;
                        }
                    }
                }
            }

            // Anthropic format: content blocks list
            if let Some(content_arr) = content.and_then(Value::as_array) {
                let (new_blocks, block_replaced) = self.process_anthropic_blocks(
                    content_arr,
                    &replacements,
                    &mut transforms,
                    &mut ccr_hashes,
                );
                if block_replaced {
                    let mut new_msg = msg.clone();
                    new_msg["content"] = Value::Array(new_blocks);
                    result_messages.push(new_msg);
                    continue;
                }
            }

            result_messages.push(msg.clone());
        }

        ReadLifecycleResult {
            messages: result_messages,
            reads_total: classifications.len(),
            reads_stale: counts[&ReadState::Stale],
            reads_superseded: counts[&ReadState::Superseded],
            reads_fresh: counts[&ReadState::Fresh],
            bytes_before,
            bytes_after,
            transforms_applied: transforms,
            ccr_hashes,
        }
    }

    /// Process Anthropic-format content blocks for lifecycle replacement.
    fn process_anthropic_blocks<'a>(
        &self,
        content_blocks: &[Value],
        replacements: &HashMap<&str, &ReadClassification>,
        transforms: &mut Vec<String>,
        ccr_hashes: &mut Vec<String>,
    ) -> (Vec<Value>, bool) {
        let mut new_blocks = Vec::with_capacity(content_blocks.len());
        let mut any_replaced = false;

        for block in content_blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                new_blocks.push(block.clone());
                continue;
            }

            let tc_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let tool_content = block.get("content");

            if let Some(classification) = replacements.get(tc_id) {
                if let Some(content_str) = tool_content.and_then(Value::as_str) {
                    let (replaced, marker, ccr_hash) =
                        self.replace_content(content_str, classification);
                    if replaced {
                        let mut new_block = block.clone();
                        new_block["content"] = Value::String(marker);
                        new_blocks.push(new_block);
                        transforms.push(format_read_lifecycle_transform(classification));
                        if let Some(hash) = ccr_hash {
                            ccr_hashes.push(hash);
                        }
                        any_replaced = true;
                        continue;
                    }
                }
            }

            new_blocks.push(block.clone());
        }

        (new_blocks, any_replaced)
    }

    /// Replace Read content with a lifecycle marker.
    ///
    /// Returns (was_replaced, marker_text, ccr_hash).
    fn replace_content(
        &self,
        content: &str,
        classification: &ReadClassification,
    ) -> (bool, String, Option<String>) {
        let content_bytes = content.len();

        // Skip tiny outputs
        if content_bytes < self.config.min_size_bytes {
            return (false, content.to_string(), None);
        }

        // Compute CCR hash
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let result = hasher.finalize();
        let ccr_hash: String = result
            .iter()
            .take(12)
            .map(|b| format!("{:02x}", b))
            .collect();

        // Best-effort CCR persistence
        if let Some(ref store) = self.store {
            if !store.put(&ccr_hash, content) {
                tracing::warn!(
                    tool_call_id = %classification.tool_call_id,
                    "read_lifecycle: CCR store failed"
                );
            }
        }

        let file_display = if classification.file_path.is_empty() {
            "unknown"
        } else {
            &classification.file_path
        };

        // NOTE: the literal phrase "Retrieve original: hash=" is load-bearing
        let marker = match classification.state {
            ReadState::Stale => format!(
                "[Read content stale: {} was modified after this read — \
                 re-read the file for current content. \
                 Retrieve original: hash={}]",
                file_display, ccr_hash
            ),
            ReadState::Superseded => format!(
                "[Read content superseded: {} was re-read later — \
                 re-read the file if needed. \
                 Retrieve original: hash={}]",
                file_display, ccr_hash
            ),
            ReadState::Fresh => unreachable!("fresh reads are never replaced"),
        };

        (true, marker, Some(ccr_hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LARGE_CONTENT: &str = "x"; // Will be repeated to exceed min_size_bytes
    const SMALL_CONTENT: &str = "tiny";

    fn large_content() -> String {
        "x".repeat(2000)
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    fn make_openai_read(tc_id: &str, file_path: &str) -> Value {
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": tc_id,
                "type": "function",
                "function": {
                    "name": "Read",
                    "arguments": serde_json::to_string(&json!({"file_path": file_path})).unwrap()
                }
            }]
        })
    }

    fn make_openai_edit(tc_id: &str, file_path: &str) -> Value {
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": tc_id,
                "type": "function",
                "function": {
                    "name": "Edit",
                    "arguments": serde_json::to_string(&json!({
                        "file_path": file_path,
                        "old_string": "old",
                        "new_string": "new"
                    })).unwrap()
                }
            }]
        })
    }

    fn make_openai_write(tc_id: &str, file_path: &str) -> Value {
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": tc_id,
                "type": "function",
                "function": {
                    "name": "Write",
                    "arguments": serde_json::to_string(&json!({
                        "file_path": file_path,
                        "content": "new content"
                    })).unwrap()
                }
            }]
        })
    }

    fn make_openai_tool_result(tc_id: &str, content: &str) -> Value {
        json!({
            "role": "tool",
            "tool_call_id": tc_id,
            "content": content
        })
    }

    fn make_anthropic_read(tc_id: &str, file_path: &str) -> Value {
        json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": tc_id,
                "name": "Read",
                "input": {"file_path": file_path}
            }]
        })
    }

    fn make_anthropic_edit(tc_id: &str, file_path: &str) -> Value {
        json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": tc_id,
                "name": "Edit",
                "input": {
                    "file_path": file_path,
                    "old_string": "old",
                    "new_string": "new"
                }
            }]
        })
    }

    fn make_anthropic_tool_result(tc_id: &str, content: &str) -> Value {
        json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tc_id,
                "content": content
            }]
        })
    }

    // ── Disabled ─────────────────────────────────────────────────────────

    #[test]
    fn disabled_when_explicitly_off() {
        let config = ReadLifecycleConfig {
            enabled: false,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();
        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_total, 0);
        assert!(result.transforms_applied.is_empty());
        // Messages returned as-is (same content)
        assert_eq!(result.messages.len(), messages.len());
    }

    #[test]
    fn enabled_by_default() {
        let config = ReadLifecycleConfig::default();
        assert!(config.enabled);
    }

    // ── Stale Detection ─────────────────────────────────────────────────

    #[test]
    fn read_then_edit_makes_stale() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "edit success"),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_stale, 1);
        assert_eq!(result.reads_fresh, 0);

        let tool_result = &result.messages[1];
        let content = tool_result["content"].as_str().unwrap();
        assert!(content.to_lowercase().contains("stale"));
        assert!(content.contains("/src/app.py"));
        assert!(content.contains("hash="));
    }

    #[test]
    fn write_makes_read_stale() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_write("w1", "/src/app.py"),
            make_openai_tool_result("w1", "write success"),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_stale, 1);
        assert!(result.messages[1]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("stale"));
    }

    #[test]
    fn edit_different_file_not_stale() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/other.py"),
            make_openai_tool_result("e1", "edit success"),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_stale, 0);
        assert_eq!(result.reads_fresh, 1);
        assert_eq!(result.messages[1]["content"].as_str().unwrap(), &lc);
    }

    #[test]
    fn multiple_reads_all_stale() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_read("r2", "/src/app.py"),
            make_openai_tool_result("r2", &format!("{}_v2", &lc)),
            make_openai_read("r3", "/src/app.py"),
            make_openai_tool_result("r3", &format!("{}_v3", &lc)),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "edit success"),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_stale, 3);
        assert_eq!(result.reads_fresh, 0);
    }

    #[test]
    fn compress_stale_disabled() {
        let config = ReadLifecycleConfig {
            enabled: true,
            compress_stale: false,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "edit success"),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_fresh, 1);
        assert_eq!(result.messages[1]["content"].as_str().unwrap(), &lc);
    }

    // ── Superseded Detection ─────────────────────────────────────────────

    #[test]
    fn reread_makes_superseded() {
        let config = ReadLifecycleConfig {
            enabled: true,
            compress_superseded: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_read("r2", "/src/app.py"),
            make_openai_tool_result("r2", &format!("{}_updated", &lc)),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_superseded, 1);
        assert_eq!(result.reads_fresh, 1);
        assert!(result.messages[1]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("superseded"));
        assert_eq!(
            result.messages[3]["content"].as_str().unwrap(),
            format!("{}_updated", &lc)
        );
    }

    #[test]
    fn compress_superseded_disabled() {
        let config = ReadLifecycleConfig {
            enabled: true,
            compress_superseded: false,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_read("r2", "/src/app.py"),
            make_openai_tool_result("r2", &format!("{}_updated", &lc)),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_fresh, 2);
        assert_eq!(result.messages[1]["content"].as_str().unwrap(), &lc);
    }

    // ── Fresh Reads ──────────────────────────────────────────────────────

    #[test]
    fn single_read_stays_fresh() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_fresh, 1);
        assert_eq!(result.reads_stale, 0);
        assert_eq!(result.reads_superseded, 0);
        assert_eq!(result.messages[1]["content"].as_str().unwrap(), &lc);
    }

    #[test]
    fn read_edit_read_chain() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "edit success"),
            make_openai_read("r2", "/src/app.py"),
            make_openai_tool_result("r2", &format!("{}_v2", &lc)),
        ];

        let result = mgr.apply(&messages, 0);
        // First read: stale (edit happened after) AND superseded (re-read after)
        // → classified as stale (stale takes priority)
        assert_eq!(result.reads_stale, 1);
        // Second read: fresh (latest, no edit after)
        assert_eq!(result.reads_fresh, 1);
        assert!(result.messages[1]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("stale"));
        assert_eq!(
            result.messages[5]["content"].as_str().unwrap(),
            format!("{}_v2", &lc)
        );
    }

    // ── Multiple Files ───────────────────────────────────────────────────

    #[test]
    fn independent_files() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "edit success"),
            make_openai_read("r2", "/src/utils.py"),
            make_openai_tool_result("r2", &format!("{}_utils", &lc)),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_stale, 1);
        assert_eq!(result.reads_fresh, 1);
        assert!(result.messages[1]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("stale"));
        assert_eq!(
            result.messages[5]["content"].as_str().unwrap(),
            format!("{}_utils", &lc)
        );
    }

    // ── Size Gating ──────────────────────────────────────────────────────

    #[test]
    fn small_read_not_replaced() {
        let config = ReadLifecycleConfig {
            enabled: true,
            min_size_bytes: 512,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", SMALL_CONTENT),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "edit success"),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(
            result.messages[1]["content"].as_str().unwrap(),
            SMALL_CONTENT
        );
    }

    // ── Anthropic Format ─────────────────────────────────────────────────

    #[test]
    fn anthropic_stale_read() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_anthropic_read("r1", "/src/app.py"),
            make_anthropic_tool_result("r1", &lc),
            make_anthropic_edit("e1", "/src/app.py"),
            make_anthropic_tool_result("e1", "edit success"),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_stale, 1);

        let user_msg = &result.messages[1];
        let tool_result_block = &user_msg["content"][0];
        let content = tool_result_block["content"].as_str().unwrap();
        assert!(content.to_lowercase().contains("stale"));
        assert!(content.contains("hash="));
    }

    #[test]
    fn anthropic_fresh_read() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_anthropic_read("r1", "/src/app.py"),
            make_anthropic_tool_result("r1", &lc),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_fresh, 1);
        let user_msg = &result.messages[1];
        assert_eq!(user_msg["content"][0]["content"].as_str().unwrap(), &lc);
    }

    // ── Transform Tracking ───────────────────────────────────────────────

    #[test]
    fn transforms_recorded() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_read("r2", "/src/app.py"),
            make_openai_tool_result("r2", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "done"),
        ];

        let result = mgr.apply(&messages, 0);
        let stale_transforms: Vec<_> = result
            .transforms_applied
            .iter()
            .filter(|t| t.contains("stale"))
            .collect();
        assert_eq!(stale_transforms.len(), 2);
    }

    #[test]
    fn transform_tag_includes_file_path_openai() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "done"),
        ];

        let result = mgr.apply(&messages, 0);
        assert!(result
            .transforms_applied
            .contains(&"read_lifecycle:stale:/src/app.py".to_string()));
    }

    #[test]
    fn transform_tag_includes_file_path_anthropic() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_anthropic_read("r1", "/src/notes.md"),
            make_anthropic_tool_result("r1", &lc),
            make_anthropic_edit("e1", "/src/notes.md"),
            make_anthropic_tool_result("e1", "done"),
        ];

        let result = mgr.apply(&messages, 0);
        assert!(result
            .transforms_applied
            .contains(&"read_lifecycle:stale:/src/notes.md".to_string()));
    }

    #[test]
    fn transform_tag_preserves_colons_in_path() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();
        let weird_path = "/tmp/has:colon/file.py";

        let messages = vec![
            make_openai_read("r1", weird_path),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", weird_path),
            make_openai_tool_result("e1", "done"),
        ];

        let result = mgr.apply(&messages, 0);
        let tag = result
            .transforms_applied
            .iter()
            .find(|t| t.starts_with("read_lifecycle:stale"))
            .unwrap();
        let parts: Vec<&str> = tag.splitn(3, ':').collect();
        assert_eq!(parts, vec!["read_lifecycle", "stale", weird_path]);
    }

    // ── No File Path Handling ────────────────────────────────────────────

    #[test]
    fn read_without_file_path() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "r1",
                    "type": "function",
                    "function": {"name": "Read", "arguments": "{}"}
                }]
            }),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "done"),
        ];

        let result = mgr.apply(&messages, 0);
        // Can't match file_path, so Read is not classified at all
        assert_eq!(result.reads_total, 0);
        assert_eq!(result.messages[1]["content"].as_str().unwrap(), &lc);
    }

    // ── Frozen Prefix ────────────────────────────────────────────────────

    #[test]
    fn frozen_prefix_read_not_replaced() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "edit success"),
        ];

        // All messages frozen: nothing should be replaced
        let result = mgr.apply(&messages, messages.len());
        assert_eq!(result.reads_fresh, 1); // reclassified as fresh
        assert_eq!(result.reads_stale, 0);
        assert_eq!(result.messages[1]["content"].as_str().unwrap(), &lc);
    }

    // ── Partial Read Coverage ────────────────────────────────────────────

    #[test]
    fn partial_read_superseded_by_full_read() {
        let config = ReadLifecycleConfig {
            enabled: true,
            compress_superseded: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let mut messages = vec![
            make_anthropic_read("r1", "/src/app.py"),
            make_anthropic_tool_result("r1", &lc),
            // Second read is full-file (no offset/limit) — supersedes partial
            make_anthropic_read("r2", "/src/app.py"),
            make_anthropic_tool_result("r2", &format!("{}_v2", &lc)),
        ];

        // Override r1 to be a partial read
        let mut messages = messages;
        messages[0]["content"][0]["input"] =
            json!({"file_path": "/src/app.py", "offset": 10, "limit": 50});

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_superseded, 1);
        assert_eq!(result.reads_fresh, 1);
    }

    #[test]
    fn partial_read_not_superseded_by_different_partial() {
        let config = ReadLifecycleConfig {
            enabled: true,
            compress_superseded: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let mut messages = vec![
            make_anthropic_read("r1", "/src/app.py"),
            make_anthropic_tool_result("r1", &lc),
            make_anthropic_read("r2", "/src/app.py"),
            make_anthropic_tool_result("r2", &lc),
        ];

        // r1: offset=10, limit=50 → lines 10-60
        // r2: offset=200, limit=50 → lines 200-250
        // r2 does NOT cover r1's range
        messages[0]["content"][0]["input"] =
            json!({"file_path": "/src/app.py", "offset": 10, "limit": 50});
        messages[2]["content"][0]["input"] =
            json!({"file_path": "/src/app.py", "offset": 200, "limit": 50});

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_superseded, 0);
        assert_eq!(result.reads_fresh, 2);
    }

    // ── Bytes Tracking ───────────────────────────────────────────────────

    #[test]
    fn bytes_before_after_tracked() {
        let config = ReadLifecycleConfig {
            enabled: true,
            ..Default::default()
        };
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "edit success"),
        ];

        let result = mgr.apply(&messages, 0);
        assert!(result.bytes_before > 0);
        assert!(result.bytes_after > 0);
        assert!(result.bytes_before > result.bytes_after);
    }

    // ── No Tool Calls ────────────────────────────────────────────────────

    #[test]
    fn no_tool_calls_in_messages() {
        let config = ReadLifecycleConfig::default();
        let mgr = ReadLifecycleManager::new(config, None);

        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi there"}),
        ];

        let result = mgr.apply(&messages, 0);
        assert_eq!(result.reads_total, 0);
        assert_eq!(result.messages.len(), 2);
    }

    // ── Hash Format ──────────────────────────────────────────────────────

    #[test]
    fn marker_contains_24_char_hex_hash() {
        let config = ReadLifecycleConfig::default();
        let mgr = ReadLifecycleManager::new(config, None);
        let lc = large_content();

        let messages = vec![
            make_openai_read("r1", "/src/app.py"),
            make_openai_tool_result("r1", &lc),
            make_openai_edit("e1", "/src/app.py"),
            make_openai_tool_result("e1", "done"),
        ];

        let result = mgr.apply(&messages, 0);
        let content = result.messages[1]["content"].as_str().unwrap();

        // Extract hash from "Retrieve original: hash=<24 hex chars>"
        let hash_start = content.find("hash=").unwrap() + 5;
        let hash_end = content[hash_start..].find(']').unwrap() + hash_start;
        let hash = &content[hash_start..hash_end];

        assert_eq!(hash.len(), 24);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

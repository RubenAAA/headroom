//! Mechanism B: hold-back Read maturation — compress before cache entry.
//!
//! The prefix cache bills you for everything *after* the first changed byte,
//! so mutating an already-cached Read is ruinously expensive — but bytes that
//! have never been cache-written have no cache entry to bust. This module
//! exploits the one safe window: a fresh Read is deliberately held *out of*
//! the provider cache while its file is active. Once the file has been quiet
//! for `quiesce_turns`, the content is replaced with a CCR-backed marker.

use hex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

use crate::ccr::CcrStore;

/// Tool names whose results are eligible for maturation.
const READ_TOOLS: &[&str] = &["Read", "read"];

/// Tool names that count as file activity (reset the quiet clock).
const TOUCH_TOOLS: &[&str] = &[
    "Read",
    "read",
    "Edit",
    "edit",
    "Write",
    "write",
    "MultiEdit",
    "NotebookEdit",
];

fn is_read_tool(name: &str) -> bool {
    READ_TOOLS.contains(&name)
}

fn is_touch_tool(name: &str) -> bool {
    TOUCH_TOOLS.contains(&name)
}

/// Configuration for Read maturation.
#[derive(Debug, Clone)]
pub struct ReadMaturationConfig {
    /// Enabled by default while the mechanism is validated in pilots.
    pub enabled: bool,
    /// Mature a held Read once its FILE has had no activity for this many assistant turns.
    pub quiesce_turns: usize,
    /// Safety valve: mature regardless once held this many turns.
    pub max_hold_turns: usize,
    /// Only hold/mature Reads at least this large.
    pub min_size_bytes: usize,
}

impl Default for ReadMaturationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quiesce_turns: 5,
            max_hold_turns: 25,
            min_size_bytes: 2048,
        }
    }
}

/// Replayed replacement for a matured Read.
#[derive(Debug, Clone)]
pub struct MaturedRead {
    pub marker: String,
    pub ccr_hash: String,
}

/// Per-request scan of tool activity, in assistant-turn units.
#[derive(Debug, Default)]
struct Activity {
    /// tool_use_id → (file_path, assistant turn of the Read tool_use)
    read_calls: HashMap<String, (String, usize)>,
    /// file_path → assistant turn of its most recent touch (read or edit)
    file_last_touch: HashMap<String, usize>,
    /// Total assistant messages in the conversation ("now").
    assistant_count: usize,
}

/// Output of one per-request maturation pass.
#[derive(Debug, Clone)]
pub struct MaturationResult {
    pub messages: Vec<Value>,
    /// Message indices that contain still-holding Reads.
    pub holding_msg_indices: Vec<usize>,
    pub holding_reads: usize,
    pub newly_matured: usize,
    pub replacements_applied: usize,
    pub bytes_saved: usize,
}

impl Default for MaturationResult {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            holding_msg_indices: Vec::new(),
            holding_reads: 0,
            newly_matured: 0,
            replacements_applied: 0,
            bytes_saved: 0,
        }
    }
}

/// Per-session Read maturation state machine.
pub struct ReadMaturationManager {
    config: ReadMaturationConfig,
    store: Option<Arc<dyn CcrStore>>,
    matured: HashMap<String, MaturedRead>,
}

impl ReadMaturationManager {
    pub fn new(config: ReadMaturationConfig, compression_store: Option<Arc<dyn CcrStore>>) -> Self {
        Self {
            config,
            store: compression_store,
            matured: HashMap::new(),
        }
    }

    /// Hold active Reads, mature quiet ones, replay matured markers.
    pub fn apply(&mut self, messages: &[Value], frozen_message_count: usize) -> MaturationResult {
        let mut result = MaturationResult {
            messages: messages.to_vec(),
            ..Default::default()
        };

        if !self.config.enabled {
            return result;
        }

        let activity = self.scan_activity(messages);
        let mut out = Vec::with_capacity(messages.len());
        let mut any_changed = false;

        for (i, msg) in messages.iter().enumerate() {
            if i < frozen_message_count {
                out.push(msg.clone());
                continue;
            }
            let (new_msg, msg_holding) = self.process_message(msg, &activity, &mut result);
            out.push(new_msg.clone());
            if new_msg != *msg {
                any_changed = true;
            }
            if msg_holding {
                result.holding_msg_indices.push(i);
            }
        }

        if any_changed {
            result.messages = out;
        }
        result
    }

    /// One pass over assistant messages: read calls, per-file last touch,
    /// and the current assistant-turn count.
    fn scan_activity(&self, messages: &[Value]) -> Activity {
        let mut act = Activity::default();

        for msg in messages {
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            act.assistant_count += 1;
            let turn = act.assistant_count;

            // OpenAI format
            if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let func = tc.get("function").unwrap_or(&Value::Null);
                    let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                    if !is_touch_tool(name) {
                        continue;
                    }

                    let args: Value = func
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(Value::Null);

                    let fp = args
                        .get("file_path")
                        .or_else(|| args.get("path"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();

                    if !fp.is_empty() {
                        act.file_last_touch.insert(fp.clone(), turn);
                    }
                    if is_read_tool(name) {
                        let tc_id = tc
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        act.read_calls.insert(tc_id, (fp, turn));
                    }
                }
            }

            // Anthropic format
            if let Some(content) = msg.get("content").and_then(Value::as_array) {
                for b in content {
                    if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                    if !is_touch_tool(name) {
                        continue;
                    }

                    let inp = b.get("input").unwrap_or(&Value::Null);
                    let fp = inp
                        .get("file_path")
                        .or_else(|| inp.get("path"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();

                    if !fp.is_empty() {
                        act.file_last_touch.insert(fp.clone(), turn);
                    }
                    if is_read_tool(name) {
                        let tc_id = b
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        act.read_calls.insert(tc_id, (fp, turn));
                    }
                }
            }
        }

        act
    }

    /// Process a single message, returning (possibly-replaced message, message_still_holding).
    fn process_message(
        &mut self,
        msg: &Value,
        activity: &Activity,
        result: &mut MaturationResult,
    ) -> (Value, bool) {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg.get("content");

        // OpenAI format: whole message is one tool result.
        if role == "tool" {
            let tc_id = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(content_str) = content.and_then(Value::as_str) {
                if activity.read_calls.contains_key(tc_id) {
                    let (new_content, holding) =
                        self.handle_read(tc_id, content_str, activity, result);
                    if let Some(nc) = new_content {
                        let mut new_msg = msg.clone();
                        new_msg["content"] = Value::String(nc);
                        return (new_msg, holding);
                    }
                    return (msg.clone(), holding);
                }
            }
            return (msg.clone(), false);
        }

        // Anthropic format: tool_result blocks inside a user message.
        if let Some(content_arr) = content.and_then(Value::as_array) {
            let mut new_blocks = Vec::with_capacity(content_arr.len());
            let mut changed = false;
            let mut holding_any = false;

            for b in content_arr {
                if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let tc_id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    if let Some(content_str) = b.get("content").and_then(Value::as_str) {
                        if activity.read_calls.contains_key(tc_id) {
                            let (new_content, holding) =
                                self.handle_read(tc_id, content_str, activity, result);
                            holding_any = holding_any || holding;
                            if let Some(nc) = new_content {
                                let mut new_block = b.clone();
                                new_block["content"] = Value::String(nc);
                                new_blocks.push(new_block);
                                changed = true;
                                continue;
                            }
                        }
                    }
                }
                new_blocks.push(b.clone());
            }

            if changed {
                let mut new_msg = msg.clone();
                new_msg["content"] = Value::Array(new_blocks);
                return (new_msg, holding_any);
            }
            return (msg.clone(), holding_any);
        }

        (msg.clone(), false)
    }

    /// Handle a single Read tool result. Returns (replacement_content | None, still_holding).
    fn handle_read(
        &mut self,
        tc_id: &str,
        content: &str,
        activity: &Activity,
        result: &mut MaturationResult,
    ) -> (Option<String>, bool) {
        // Matured earlier: replay the recorded marker deterministically.
        if let Some(matured) = self.matured.get(tc_id) {
            if content == matured.marker {
                return (None, false);
            }
            result.replacements_applied += 1;
            result.bytes_saved += content.len().saturating_sub(matured.marker.len());
            return (Some(matured.marker.clone()), false);
        }

        let size = content.len(); // UTF-8 bytes
        if size < self.config.min_size_bytes {
            return (None, false);
        }

        // Lifecycle markers (stale/superseded) are already compact
        if content.contains("Retrieve original: hash=") || content.contains("Retrieve more: hash=")
        {
            return (None, false);
        }

        let (file_path, read_turn) = match activity.read_calls.get(tc_id) {
            Some(v) => v.clone(),
            None => return (None, false),
        };

        let last_touch = activity
            .file_last_touch
            .get(&file_path)
            .copied()
            .unwrap_or(read_turn);
        let quiet_turns = activity.assistant_count - last_touch;
        let held_turns = activity.assistant_count - read_turn;

        if quiet_turns < self.config.quiesce_turns && held_turns < self.config.max_hold_turns {
            result.holding_reads += 1;
            return (None, true); // file still active — keep verbatim, uncached
        }

        // File quiesced (or hold cap hit): mature.
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let hash_result = hasher.finalize();
        let ccr_hash: String = hash_result
            .iter()
            .take(12)
            .map(|b| format!("{:02x}", b))
            .collect();

        if let Some(ref store) = self.store {
            if !store.put(&ccr_hash, content) {
                tracing::warn!(
                    tool_call_id = %tc_id,
                    "read_maturation: CCR store failed"
                );
            }
        }

        let file_display = if file_path.is_empty() {
            "unknown"
        } else {
            &file_path
        };

        // NOTE: "Retrieve original: hash=" is load-bearing
        let marker = format!(
            "[Read of {} compressed after use — re-read the file \
             if needed. Retrieve original: hash={}]",
            file_display, ccr_hash
        );

        self.matured.insert(
            tc_id.to_string(),
            MaturedRead {
                marker: marker.clone(),
                ccr_hash: ccr_hash.clone(),
            },
        );
        result.newly_matured += 1;
        result.replacements_applied += 1;
        result.bytes_saved += content.len().saturating_sub(marker.len());
        (Some(marker), false)
    }
}

/// Park the trailing message-level cache breakpoint before held Reads.
///
/// Strips `cache_control` from every block at or after the earliest
/// holding message, and re-places that breakpoint — marker and all — on
/// the last block of the latest *eligible* message before it.
pub fn relocate_cache_breakpoint(messages: &[Value], holding_msg_indices: &[usize]) -> Vec<Value> {
    if holding_msg_indices.is_empty() {
        return messages.to_vec();
    }

    let earliest = *holding_msg_indices.iter().min().unwrap();
    let mut out: Vec<Value> = messages.to_vec();
    let mut stripped_any = false;
    // The marker we strip, kept whole. Re-anchoring with a bare
    // `{"type": "ephemeral"}` would silently downgrade a client's
    // `ttl: "1h"` breakpoint to the 5m default — the request still
    // succeeds, so the only symptom is a full prefix re-write on every
    // idle gap past five minutes.
    let mut held_marker: Option<Value> = None;

    // 1. Strip breakpoints from the held region [earliest:].
    for i in earliest..out.len() {
        let content = match out[i].get("content").and_then(Value::as_array) {
            Some(arr) => arr,
            None => continue,
        };

        let has_bp = content
            .iter()
            .any(|b| b.is_object() && b.get("cache_control").is_some());

        if has_bp {
            for b in content {
                if let Some(cc) = b.get("cache_control") {
                    if cc.is_object() {
                        held_marker = Some(cc.clone());
                    }
                }
            }
            let new_content: Vec<Value> = content
                .iter()
                .map(|b| {
                    if b.is_object() {
                        let mut cleaned = b.clone();
                        if let Some(obj) = cleaned.as_object_mut() {
                            obj.remove("cache_control");
                        }
                        cleaned
                    } else {
                        b.clone()
                    }
                })
                .collect();
            out[i]["content"] = Value::Array(new_content);
            stripped_any = true;
        }
    }

    if !stripped_any {
        return out;
    }

    // 2. Re-anchor: put the stripped breakpoint back on the last block of
    //    the latest block-style message before the held region. Fall back
    //    to a bare ephemeral only when the client sent no marker of its own.
    for i in (0..earliest).rev() {
        if let Some(content) = out[i].get("content").and_then(Value::as_array) {
            if let Some(last) = content.last() {
                if last.is_object() {
                    let mut new_content = content.clone();
                    let last_idx = new_content.len() - 1;
                    if let Some(obj) = new_content[last_idx].as_object_mut() {
                        let marker = held_marker
                            .clone()
                            .unwrap_or_else(|| json!({"type": "ephemeral"}));
                        obj.insert("cache_control".to_string(), marker);
                    }
                    out[i]["content"] = Value::Array(new_content);
                    break;
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONTENT: &str = "     1\tdef foo():\n     2\t    return 42\n"; // repeated to exceed 2048B
    const SMALL: &str = "     1\tok\n";

    fn large_content() -> String {
        CONTENT.repeat(60)
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    fn anthropic_read(tc_id: &str, file_path: &str, content: &str) -> Vec<Value> {
        vec![
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": tc_id,
                    "name": "Read",
                    "input": {"file_path": file_path}
                }]
            }),
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": tc_id, "content": content}]
            }),
        ]
    }

    fn anthropic_edit(tc_id: &str, file_path: &str) -> Vec<Value> {
        vec![
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": tc_id,
                    "name": "Edit",
                    "input": {"file_path": file_path, "old_string": "a", "new_string": "b"}
                }]
            }),
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": tc_id, "content": "ok"}]
            }),
        ]
    }

    fn openai_read(tc_id: &str, file_path: &str, content: &str) -> Vec<Value> {
        vec![
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": tc_id,
                    "function": {
                        "name": "Read",
                        "arguments": serde_json::to_string(&json!({"file_path": file_path})).unwrap()
                    }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": tc_id,
                "content": content
            }),
        ]
    }

    fn quiet(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                json!({
                    "role": "assistant",
                    "content": [{"type": "text", "text": format!("thinking {}", i)}]
                })
            })
            .collect()
    }

    fn base_conv(content: &str) -> Vec<Value> {
        let mut msgs = vec![json!({"role": "user", "content": "look"})];
        msgs.extend(anthropic_read("r1", "/x/foo.py", content));
        msgs
    }

    fn manager(quiesce_turns: usize, max_hold_turns: usize) -> ReadMaturationManager {
        let cfg = ReadMaturationConfig {
            enabled: true,
            quiesce_turns,
            max_hold_turns,
            ..Default::default()
        };
        ReadMaturationManager::new(cfg, None)
    }

    fn read_content(res: &MaturationResult, idx: usize) -> &str {
        res.messages[idx]["content"][0]["content"].as_str().unwrap()
    }

    // ── Activity Decision ────────────────────────────────────────────────

    #[test]
    fn disabled_is_noop() {
        let cfg = ReadMaturationConfig {
            enabled: false,
            ..Default::default()
        };
        let mut m = ReadMaturationManager::new(cfg, None);
        let lc = large_content();
        let msgs = base_conv(&lc);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.messages, msgs);
        assert!(res.holding_msg_indices.is_empty());
    }

    #[test]
    fn fresh_read_holds_verbatim() {
        let lc = large_content();
        let msgs = base_conv(&lc);
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert_eq!(read_content(&res, 2), &lc);
        assert_eq!(res.holding_msg_indices, vec![2]);
        assert_eq!(res.holding_reads, 1);
        assert_eq!(res.newly_matured, 0);
    }

    #[test]
    fn holds_while_file_quiet_below_quiesce() {
        let lc = large_content();
        let mut msgs = base_conv(&lc);
        msgs.extend(quiet(4)); // quiet = 4 < 5
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.holding_msg_indices, vec![2]);
        assert_eq!(read_content(&res, 2), &lc);
    }

    #[test]
    fn matures_after_quiesce() {
        let lc = large_content();
        let mut msgs = base_conv(&lc);
        msgs.extend(quiet(5)); // quiet = 5 >= 5
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.newly_matured, 1);
        assert!(res.holding_msg_indices.is_empty());
        let marker = read_content(&res, 2);
        assert!(marker.contains("compressed after use"));
        assert!(marker.contains("/x/foo.py"));
        assert!(marker.contains("Retrieve original: hash="));
        assert!(res.bytes_saved > 0);
    }

    #[test]
    fn file_activity_resets_quiet_clock() {
        let lc = large_content();
        let mut msgs = base_conv(&lc);
        msgs.extend(quiet(4));
        msgs.extend(anthropic_edit("e1", "/x/foo.py"));
        msgs.extend(quiet(4));
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.holding_msg_indices, vec![2]);
        // one more quiet turn → file quiesced → matures
        let mut msgs2 = msgs.clone();
        msgs2.extend(quiet(1));
        let res2 = m.apply(&msgs2, 0);
        assert_eq!(res2.newly_matured, 1);
    }

    #[test]
    fn activity_on_other_file_does_not_reset() {
        let lc = large_content();
        let mut msgs = base_conv(&lc);
        msgs.extend(quiet(3));
        msgs.extend(anthropic_edit("e1", "/x/OTHER.py"));
        msgs.extend(quiet(1));
        // foo.py quiet for 5 assistant turns (3 quiet + edit-turn + 1 quiet)
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.newly_matured, 1);
    }

    #[test]
    fn max_hold_caps_busy_files() {
        let lc = large_content();
        let mut msgs = base_conv(&lc);
        for i in 0..6 {
            msgs.extend(anthropic_edit(&format!("e{}", i), "/x/foo.py"));
        }
        let mut m = manager(100, 6);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.newly_matured, 1);
        assert!(res.holding_msg_indices.is_empty());
    }

    #[test]
    fn replay_is_deterministic_and_stateful() {
        let lc = large_content();
        let mut m = manager(5, 25);
        let matured_msgs: Vec<Value> = base_conv(&lc).into_iter().chain(quiet(5)).collect();
        let a = read_content(&m.apply(&matured_msgs, 0), 2).to_string();
        // Replay applies even when the conversation grows and the file is
        // touched again later (matured is final).
        let later: Vec<Value> = matured_msgs
            .iter()
            .cloned()
            .chain(anthropic_edit("e1", "/x/foo.py"))
            .collect();
        let b = read_content(&m.apply(&later, 0), 2).to_string();
        let c = read_content(
            &m.apply(&later.into_iter().chain(quiet(3)).collect::<Vec<_>>(), 0),
            2,
        )
        .to_string();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn small_reads_ignored() {
        let mut msgs = vec![json!({"role": "user", "content": "look"})];
        msgs.extend(anthropic_read("r1", "/x/a.py", SMALL));
        msgs.extend(quiet(10));
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert!(res.holding_msg_indices.is_empty());
        assert_eq!(read_content(&res, 2), SMALL);
    }

    #[test]
    fn frozen_prefix_untouched() {
        let lc = large_content();
        let mut msgs = base_conv(&lc);
        msgs.extend(quiet(10));
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, msgs.len());
        assert!(res.holding_msg_indices.is_empty());
        assert_eq!(read_content(&res, 2), &lc);
    }

    #[test]
    fn respects_lifecycle_markers() {
        let marker = format!(
            "[Read content stale: /x/foo.py ... Retrieve original: hash=abc123]{}",
            " ".repeat(2048)
        );
        let mut msgs = vec![json!({"role": "user", "content": "look"})];
        msgs.extend(anthropic_read("r1", "/x/foo.py", &marker));
        msgs.extend(quiet(10));
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert!(res.holding_msg_indices.is_empty());
        assert_eq!(res.newly_matured, 0);
    }

    #[test]
    fn client_breakpoint_on_fresh_read_is_held_and_relocated() {
        // Claude Code parks its tail breakpoint on the newest block —
        // right after a Read, that's the Read result itself. The read must
        // still be held (verbatim) and relocation must move the breakpoint
        // off it, otherwise the verbatim form gets cache-written.
        let lc = large_content();
        let mut msgs = base_conv(&lc);
        // Add cache_control to the Read result block
        let content = msgs[2]["content"].as_array().unwrap().clone();
        let mut new_content = content;
        new_content[0]["cache_control"] = json!({"type": "ephemeral"});
        msgs[2]["content"] = Value::Array(new_content);

        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.holding_msg_indices, vec![2]);
        assert_eq!(read_content(&res, 2), &lc);

        let out = relocate_cache_breakpoint(&res.messages, &res.holding_msg_indices);
        // Breakpoint stripped from the held read, re-anchored before it.
        assert!(out[2]["content"][0].get("cache_control").is_none());
        assert_eq!(
            out[1]["content"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .get("cache_control"),
            Some(&json!({"type": "ephemeral"}))
        );
    }

    #[test]
    fn openai_format() {
        let lc = large_content();
        let mut msgs = vec![json!({"role": "user", "content": "look"})];
        msgs.extend(openai_read("r1", "/x/foo.py", &lc));
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.holding_msg_indices, vec![2]);
        let later: Vec<Value> = msgs.into_iter().chain(quiet(5)).collect();
        let res2 = m.apply(&later, 0);
        assert!(res2.messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("compressed after use"));
    }

    #[test]
    fn files_mature_independently() {
        let lc = large_content();
        let mut msgs = base_conv(&lc);
        msgs.extend(quiet(6));
        msgs.extend(anthropic_read("r2", "/x/bar.py", &lc));
        let mut m = manager(5, 25);
        let res = m.apply(&msgs, 0);
        assert_eq!(res.newly_matured, 1); // foo.py
        assert!(read_content(&res, 2).contains("compressed after use"));
        assert_eq!(res.holding_msg_indices, vec![10]); // bar.py result message
        assert_eq!(read_content(&res, 10), &lc);
    }

    #[test]
    fn decision_survives_state_loss() {
        let lc = large_content();
        let msgs: Vec<Value> = base_conv(&lc).into_iter().chain(quiet(5)).collect();
        let first = {
            let mut m = manager(5, 25);
            read_content(&m.apply(&msgs, 0), 2).to_string()
        };
        let second = {
            let mut m = manager(5, 25);
            read_content(&m.apply(&msgs, 0), 2).to_string()
        };
        assert!(first.contains("compressed after use"));
        assert!(second.contains("compressed after use"));
        assert_eq!(first, second);
    }

    // ── Breakpoint Relocation ────────────────────────────────────────────

    fn msgs_with_tail_breakpoint() -> Vec<Value> {
        let lc = large_content();
        let mut msgs =
            vec![json!({"role": "user", "content": [{"type": "text", "text": "earlier turn"}]})];
        msgs.extend(anthropic_read("r1", "/x/foo.py", &lc));
        // Add cache_control to the last block
        let last = msgs.len() - 1;
        let content = msgs[last]["content"].as_array().unwrap().clone();
        let mut new_content = content;
        let last_block = new_content.last_mut().unwrap();
        last_block["cache_control"] = json!({"type": "ephemeral"});
        msgs[last]["content"] = Value::Array(new_content);
        msgs
    }

    fn breakpoint_indices(msgs: &[Value]) -> Vec<usize> {
        msgs.iter()
            .enumerate()
            .filter(|(_, m)| {
                m.get("content")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .any(|b| b.is_object() && b.get("cache_control").is_some())
                    })
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn relocate_noop_without_holds() {
        let msgs = msgs_with_tail_breakpoint();
        let out = relocate_cache_breakpoint(&msgs, &[]);
        assert_eq!(out, msgs);
    }

    #[test]
    fn relocate_before_held_read() {
        let msgs = msgs_with_tail_breakpoint();
        let out = relocate_cache_breakpoint(&msgs, &[2]);

        // Held region [2:] carries no breakpoint; re-anchored on the
        // latest eligible message before it (index 1 — the assistant
        // tool_use message).
        assert_eq!(breakpoint_indices(&out), vec![1]);
        assert_eq!(
            out[1]["content"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .get("cache_control"),
            Some(&json!({"type": "ephemeral"}))
        );
        assert!(breakpoint_indices(&out).len() <= breakpoint_indices(&msgs).len());
    }

    #[test]
    fn relocate_preserves_cache_control_ttl() {
        let mut msgs = msgs_with_tail_breakpoint();
        // Client asked for the 1h cache tier on the breakpoint we relocate.
        let last = msgs.len() - 1;
        let mut content = msgs[last]["content"].as_array().unwrap().clone();
        *content
            .last_mut()
            .unwrap()
            .get_mut("cache_control")
            .unwrap() = json!({"type": "ephemeral", "ttl": "1h"});
        msgs[last]["content"] = Value::Array(content);

        let out = relocate_cache_breakpoint(&msgs, &[2]);

        assert_eq!(breakpoint_indices(&out), vec![1]);
        assert_eq!(
            out[1]["content"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .get("cache_control"),
            Some(&json!({"type": "ephemeral", "ttl": "1h"}))
        );
    }

    #[test]
    fn relocate_noop_when_no_breakpoint_in_held_region() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "x"}]}),
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "r1", "content": "data"}]
            }),
        ];
        let out = relocate_cache_breakpoint(&msgs, &[1]);
        assert!(breakpoint_indices(&out).is_empty());
    }

    #[test]
    fn relocate_originals_not_mutated() {
        let msgs = msgs_with_tail_breakpoint();
        let before: Vec<String> = msgs.iter().map(|m| m.to_string()).collect();
        relocate_cache_breakpoint(&msgs, &[2]);
        let after: Vec<String> = msgs.iter().map(|m| m.to_string()).collect();
        assert_eq!(before, after);
    }
}

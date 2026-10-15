//! A block the model fetched back with `headroom_retrieve` goes out raw from
//! then on.
//!
//! Offload swaps a large tool result for a digest, and the model fetches the
//! original on a hidden continuation round. That round never reaches the
//! client, so the original never enters its history: next turn the model sees
//! the digest again and fetches it again, paying a hidden round every time.
//! Over 2026-09-23..24, 3,149 of 4,704 calls repeated an earlier fetch
//! (`docs/notes/learnings/offload-loses-at-2000-bytes.md`). On 2026-10-15 one
//! session fetched the same three files on six turns running, each turn
//! costing 2.6-3.5x what a stock client would have paid for it.
//!
//! So once a session has fetched a hash, the forwarded prefix carries the
//! client's raw block wherever that hash's digest stood. The first such turn
//! rewrites the cache from that block onward, once; the replay store then
//! holds the raw block, and later turns replay it byte-identical.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};

use headroom_core::transforms::live_zone::CTX_OFFLOAD_MARKER_PREFIX;
use lru::LruCache;
use serde_json::Value;

use crate::compression::ctx_offload::{marker_hash, tool_result_text};

/// Requests whose fetches have not reached their session yet. A request whose
/// stream never completes cleanly is never taken; the bound drops it.
const PENDING_FETCH_REQUESTS: usize = 1024;

/// Keyed by request because the fetch site knows nothing of sessions; the
/// replay store maps the request to its session when the turn completes.
fn pending() -> &'static Mutex<LruCache<String, HashSet<String>>> {
    static PENDING: OnceLock<Mutex<LruCache<String, HashSet<String>>>> = OnceLock::new();
    PENDING.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(PENDING_FETCH_REQUESTS).expect("non-zero"),
        ))
    })
}

/// Note that a hidden round of `request_id` fetched `hash`.
pub(crate) fn note_fetched(request_id: &str, hash: &str) {
    if let Ok(mut guard) = pending().lock() {
        guard
            .get_or_insert_mut(request_id.to_string(), HashSet::new)
            .insert(hash.to_string());
    }
}

/// The hashes `request_id` fetched, removed from the pending set.
pub(crate) fn take_fetched(request_id: &str) -> HashSet<String> {
    pending()
        .lock()
        .ok()
        .and_then(|mut guard| guard.pop(request_id))
        .unwrap_or_default()
}

/// Put the client's raw content back into every `tool_result` in `forwarded`
/// whose digest names a hash in `fetched`. Returns how many went back.
///
/// The client's block is found at the same message index by `tool_use_id`; a
/// message that does not line up simply has no match. A client block that is
/// itself a digest is left alone, since there is nothing raw to restore.
pub(crate) fn restore_fetched_blocks(
    forwarded: &mut [Value],
    originals: &[Value],
    fetched: &HashSet<String>,
) -> usize {
    let mut restored = 0;
    for (message, original) in forwarded.iter_mut().zip(originals) {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let Some(client_blocks) = original.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(hash) = block
                .get("content")
                .and_then(tool_result_text)
                .and_then(|text| marker_hash(&text))
            else {
                continue;
            };
            if !fetched.contains(&hash) {
                continue;
            }
            let Some(raw) = client_blocks
                .iter()
                .find(|b| {
                    b.get("type").and_then(Value::as_str) == Some("tool_result")
                        && b.get("tool_use_id") == block.get("tool_use_id")
                })
                .and_then(|b| b.get("content"))
            else {
                continue;
            };
            if tool_result_text(raw).is_some_and(|t| t.contains(CTX_OFFLOAD_MARKER_PREFIX)) {
                continue;
            }
            block["content"] = raw.clone();
            restored += 1;
        }
    }
    restored
}

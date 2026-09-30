//! Zen's encrypted reasoning, replayed until Zen refuses it.
//!
//! Zen binds a reasoning blob to the caller it issued it to (see
//! `learnings/zen-reasoning-blob-vs-exit-rotation.md`). A blob that crosses an
//! exit rotation or a lane failover comes back as
//! `reasoning encrypted_content was not issued to this caller`, a 400 that
//! repeats every turn because the client keeps the stale blob in its transcript.
//! So by default the translation strips every blob.
//!
//! With `--zen-reasoning-replay` the blobs go out instead, and Zen's own answer
//! decides which to keep. On that 400 the send drops every blob in the body,
//! remembers their fingerprints, and resends once. Later turns drop the
//! remembered blobs before sending, so a bad blob costs one refused request and
//! a good one, issued after the change, is replayed as usual. The rule does not
//! depend on what "caller" means (exit, session or both): Zen says which.
//!
//! Dropping keeps the item's `summary`, as the strip does: a reasoning item
//! with no `id` is still accepted.

use bytes::Bytes;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// What Zen says when a blob is not the caller's own.
const REFUSAL: &str = "was not issued to this caller";
/// Blobs remembered as refused. A turn carries at most a few dozen.
const REMEMBERED: usize = 4096;

/// `--zen-reasoning-replay`. Process-wide because the translation carries no
/// config; set once at startup.
static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn refused() -> &'static Mutex<lru::LruCache<u64, ()>> {
    static REFUSED: OnceLock<Mutex<lru::LruCache<u64, ()>>> = OnceLock::new();
    REFUSED.get_or_init(|| {
        Mutex::new(lru::LruCache::new(
            NonZeroUsize::new(REMEMBERED).expect("non-zero capacity"),
        ))
    })
}

fn fingerprint(blob: &str) -> u64 {
    let mut h = DefaultHasher::new();
    blob.hash(&mut h);
    h.finish()
}

/// The reasoning items of a translated Responses body that carry a blob.
fn blob_items(body: &mut Value) -> impl Iterator<Item = &mut serde_json::Map<String, Value>> {
    body.get_mut("input")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("reasoning")
                && item.get("encrypted_content").is_some_and(Value::is_string)
        })
        .filter_map(Value::as_object_mut)
}

fn drop_blob(item: &mut serde_json::Map<String, Value>) {
    item.remove("id");
    item.remove("encrypted_content");
}

/// Drops the blobs Zen has already refused; returns how many.
pub(crate) fn drop_refused(body: &mut Value) -> usize {
    let Ok(refused) = refused().lock() else {
        return 0;
    };
    let mut dropped = 0;
    for item in blob_items(body) {
        let blob = item["encrypted_content"].as_str().unwrap_or_default();
        if refused.contains(&fingerprint(blob)) {
            drop_blob(item);
            dropped += 1;
        }
    }
    dropped
}

/// Is this a 400 body that says the blob was not the caller's?
pub(crate) fn is_refusal(response_body: &[u8]) -> bool {
    response_body
        .windows(REFUSAL.len())
        .any(|w| w == REFUSAL.as_bytes())
}

/// The body without any blob, with every dropped blob remembered as refused.
/// `None` when the body carries no blob, so there is nothing to retry without.
pub(crate) fn drop_all_remembering(body: &Bytes) -> Option<Bytes> {
    let mut parsed: Value = serde_json::from_slice(body).ok()?;
    let mut dropped = 0;
    if let Ok(mut refused) = refused().lock() {
        for item in blob_items(&mut parsed) {
            refused.put(
                fingerprint(item["encrypted_content"].as_str().unwrap_or_default()),
                (),
            );
            drop_blob(item);
            dropped += 1;
        }
    }
    if dropped == 0 {
        return None;
    }
    serde_json::to_vec(&parsed).ok().map(Bytes::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: &str, blob: &str) -> Value {
        json!({"type": "reasoning", "id": id, "encrypted_content": blob,
               "summary": [{"type": "summary_text", "text": "thought"}]})
    }

    fn body_with(blobs: &[&str]) -> Value {
        let mut input = vec![json!({"type": "message", "role": "user", "content": "hi"})];
        input.extend(blobs.iter().map(|b| item(&format!("rs_{b}"), b)));
        json!({"model": "m", "include": ["reasoning.encrypted_content"], "input": input})
    }

    fn blobs_left(body: &Value) -> Vec<String> {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i.get("encrypted_content")?.as_str().map(str::to_string))
            .collect()
    }

    /// A refused blob is dropped on every later turn; a blob issued after the
    /// change is replayed. Fingerprints are unique to this test so the shared
    /// set cannot leak between tests.
    #[test]
    fn a_refused_blob_stays_dropped_and_a_later_one_is_kept() {
        let old =
            Bytes::from(serde_json::to_vec(&body_with(&["old-blob-a", "old-blob-b"])).unwrap());
        let resent = drop_all_remembering(&old).expect("blobs to drop");
        let resent: Value = serde_json::from_slice(&resent).unwrap();
        assert!(blobs_left(&resent).is_empty());
        assert!(
            resent["input"][1].get("id").is_none() && resent["input"][1]["summary"].is_array(),
            "the summary survives: {resent}"
        );
        assert_eq!(resent["include"][0], "reasoning.encrypted_content");

        let mut next = body_with(&["old-blob-a", "old-blob-b", "new-blob-c"]);
        assert_eq!(drop_refused(&mut next), 2);
        assert_eq!(blobs_left(&next), vec!["new-blob-c".to_string()]);
    }

    #[test]
    fn a_body_without_blobs_has_nothing_to_retry() {
        let plain = Bytes::from(serde_json::to_vec(&body_with(&[])).unwrap());
        assert!(drop_all_remembering(&plain).is_none());
        assert!(drop_all_remembering(&Bytes::from_static(b"not json")).is_none());
    }

    #[test]
    fn only_the_wrong_caller_400_counts_as_a_refusal() {
        let refusal = br#"{"error":{"message":"Error from provider (Console): reasoning `encrypted_content` was not issued to this caller"}}"#;
        assert!(is_refusal(refusal));
        assert!(!is_refusal(br#"{"error":{"message":"input too long"}}"#));
        assert!(!is_refusal(b""));
    }
}

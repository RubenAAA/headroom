//! What a routed turn forwarded, compared with the session's previous turn.
//!
//! A provider prompt cache reads a turn back only as far as its input matches
//! the last one byte for byte. Spark turns on Zen showed cached fractions of
//! about 0.99 in some turns and 0.13 in others with no visible cause, so this
//! compares where consecutive forwarded inputs first differ: the `head` (model,
//! instructions, tools) and each `input`/`messages` item are hashed.
//!
//! Only turns worth reading are logged: a changed head, or a broken prefix
//! whose first moved item is not a `function_call` (those do not cost cache).
//! Every other turn is counted, and a summary line goes out every
//! [`SUMMARY_EVERY`] turns. Read-only: the body is not touched.

use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

const SESSIONS: usize = 1024;
/// The first item is the largest (hundreds of KB when it is a system prompt),
/// so a session keeps a hash per block of it, not the text: enough to say where
/// it changed to within a block.
const BLOCK: usize = 1024;
/// Turns between summary lines.
const SUMMARY_EVERY: u64 = 100;

struct Prev {
    head: u64,
    items: Vec<u64>,
    /// Hash of each [`BLOCK`] of the first item's JSON, and its length.
    first_blocks: Vec<u64>,
    first_len: usize,
}

#[derive(Default)]
struct Tally {
    turns: AtomicU64,
    appended: AtomicU64,
    broken_tool_call: AtomicU64,
    broken_other: AtomicU64,
    head_changed: AtomicU64,
}

fn tally() -> &'static Tally {
    static TALLY: OnceLock<Tally> = OnceLock::new();
    TALLY.get_or_init(Tally::default)
}

fn store() -> &'static Mutex<lru::LruCache<u64, Prev>> {
    static STORE: OnceLock<Mutex<lru::LruCache<u64, Prev>>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(lru::LruCache::new(
            NonZeroUsize::new(SESSIONS).expect("non-zero capacity"),
        ))
    })
}

fn hash_of<T: Hash + ?Sized>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// Every item's hash, plus block hashes and length of the first item's JSON.
fn hash_items(items: &[Value]) -> (Vec<u64>, Vec<u64>, usize) {
    let mut hashes = Vec::with_capacity(items.len());
    let mut blocks = Vec::new();
    let mut first_len = 0;
    for (i, item) in items.iter().enumerate() {
        let bytes = serde_json::to_vec(item).unwrap_or_default();
        hashes.push(hash_of(&bytes));
        if i == 0 {
            first_len = bytes.len();
            blocks = bytes.chunks(BLOCK).map(hash_of).collect();
        }
    }
    (hashes, blocks, first_len)
}

/// The conversation array of a routed body: Responses `input`, else chat `messages`.
fn items_of(body: &Value) -> &[Value] {
    ["input", "messages"]
        .iter()
        .find_map(|k| body.get(*k).and_then(Value::as_array))
        .map_or(&[], Vec::as_slice)
}

/// Everything except the conversation array. A change here moves the whole
/// cache key, so it is compared as one unit.
fn head_hash(body: &Value) -> u64 {
    let Some(obj) = body.as_object() else {
        return 0;
    };
    let mut keys: Vec<&String> = obj
        .keys()
        .filter(|k| !matches!(k.as_str(), "input" | "messages"))
        .collect();
    keys.sort();
    let mut h = DefaultHasher::new();
    for k in keys {
        k.hash(&mut h);
        serde_json::to_vec(&obj[k]).unwrap_or_default().hash(&mut h);
    }
    h.finish()
}

/// How many leading items two hash sequences share.
fn common_prefix(a: &[u64], b: &[u64]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

pub(crate) fn note(session: &str, body: &Value, body_bytes: usize, request_id: &str) {
    let (items, first_blocks, first_len) = hash_items(items_of(body));
    let head = head_hash(body);
    let key = hash_of(&session);
    let prev = store().lock().ok().and_then(|mut m| {
        m.put(
            key,
            Prev {
                head,
                items: items.clone(),
                first_blocks: first_blocks.clone(),
                first_len,
            },
        )
    });
    let turns = tally().turns.fetch_add(1, Relaxed) + 1;
    if let Some(prev) = prev {
        let common = common_prefix(&prev.items, &items);
        let diverged = common < prev.items.len();
        let head_changed = prev.head != head;
        let moved = items_of(body).get(common);
        let tool_call = moved.map(kind_of).as_deref() == Some("function_call");
        count(diverged, tool_call, head_changed);
        let model = body.get("model").and_then(Value::as_str);
        if head_changed || (diverged && !tool_call) {
            tracing::info!(
                event = "routed_forward_continuity",
                request_id,
                model,
                session_hash = format_args!("{key:016x}"),
                items = items.len(),
                prev_items = prev.items.len(),
                common_prefix = common,
                prefix_broken = diverged,
                head_changed,
                first_moved_kind = moved.map(kind_of),
                first_moved_bytes = moved.map(|v| serde_json::to_vec(v).map_or(0, |b| b.len())),
                body_bytes,
                "routed forward continuity"
            );
        }
        if common == 0 && diverged {
            log_first_item_drift(&prev, &first_blocks, first_len, request_id);
        }
    }
    if turns.is_multiple_of(SUMMARY_EVERY) {
        log_summary(turns);
    }
}

/// Files one turn under the summary counters. A previous item that no longer
/// matches is a prefix break; a longer input whose old items all match is the
/// healthy append.
fn count(diverged: bool, tool_call: bool, head_changed: bool) {
    let tally = tally();
    match (diverged, tool_call) {
        (false, _) => &tally.appended,
        (true, true) => &tally.broken_tool_call,
        (true, false) => &tally.broken_other,
    }
    .fetch_add(1, Relaxed);
    if head_changed {
        tally.head_changed.fetch_add(1, Relaxed);
    }
}

fn log_summary(turns: u64) {
    let tally = tally();
    tracing::info!(
        event = "routed_forward_continuity_summary",
        turns,
        appended = tally.appended.load(Relaxed),
        broken_tool_call = tally.broken_tool_call.load(Relaxed),
        broken_other = tally.broken_other.load(Relaxed),
        head_changed = tally.head_changed.load(Relaxed),
        "routed forward continuity since the proxy started"
    );
}

/// The first item changed, so the whole cache prefix misses. Says roughly where.
fn log_first_item_drift(prev: &Prev, blocks: &[u64], len: usize, request_id: &str) {
    let Some(at) = first_difference(&prev.first_blocks, blocks) else {
        return;
    };
    tracing::info!(
        event = "routed_first_item_drift",
        request_id,
        differs_at_about = at * BLOCK,
        prev_len = prev.first_len,
        now_len = len,
        "first forwarded item changed since the last turn"
    );
}

/// Index of the first block where two block-hash lists part. `None` when equal.
fn first_difference(prev: &[u64], now: &[u64]) -> Option<usize> {
    let shared = common_prefix(prev, now);
    (shared < prev.len() || shared < now.len()).then_some(shared)
}

fn kind_of(item: &Value) -> String {
    let field = |k: &str| item.get(k).and_then(Value::as_str);
    match (field("type"), field("role")) {
        (Some(t), Some(r)) => format!("{t}:{r}"),
        (Some(t), None) => t.to_string(),
        (None, Some(r)) => r.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

// A drift cannot be provoked end to end: the lane key already folds in the
// client's system and opener, so a client edit there starts a new lane instead
// of comparing. Only a proxy-side change to the same input reaches this path,
// which is why it is logged.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drift_is_placed_in_its_block() {
        let prev = "a".repeat(3 * BLOCK);
        let mut now = prev.clone();
        now.replace_range(2 * BLOCK + 5..2 * BLOCK + 6, "b");
        let blocks = |t: &str| -> Vec<u64> { t.as_bytes().chunks(BLOCK).map(hash_of).collect() };
        assert_eq!(first_difference(&blocks(&prev), &blocks(&prev)), None);
        assert_eq!(first_difference(&blocks(&prev), &blocks(&now)), Some(2));
        // An append past the last full block is a difference at that block.
        let longer = format!("{prev}x");
        assert_eq!(first_difference(&blocks(&prev), &blocks(&longer)), Some(3));
    }
}

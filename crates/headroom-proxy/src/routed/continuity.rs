//! What a routed turn forwarded, compared with the session's recent turns.
//!
//! A provider prompt cache reads a turn back only as far as its input matches
//! the last one byte for byte. Spark turns on Zen showed cached fractions of
//! about 0.99 in some turns and 0.13 in others with no visible cause, so this
//! compares where forwarded inputs first differ: the `head` (model,
//! instructions, tools) and each `input`/`messages` item are hashed.
//!
//! A turn is compared with the last [`HISTORY`] turns of its session and
//! judged against the one it follows best. The provider caches every request's
//! prefix, so a side request that Claude Code sends between two real turns
//! (the spinner text) leaves the real turns' cache intact; comparing with the
//! last turn alone logged two breaks for each one.
//!
//! Only turns worth reading are logged: a changed head, or a broken prefix
//! whose first moved item is not a `function_call` (those do not cost cache).
//! Every other turn is counted, and a summary line goes out every
//! [`SUMMARY_EVERY`] turns. A logged break carries the start of the item that
//! moved and of what stood there before, so it shows what changed; those
//! previews are conversation text, so they reach only the local log.
//! Read-only: the body is not touched.

use serde_json::Value;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

const SESSIONS: usize = 1024;
/// Turns per session that a new turn is compared with.
const HISTORY: usize = 4;
/// The first item is the largest (hundreds of KB when it is a system prompt),
/// so a session keeps a hash per block of it, not the text: enough to say where
/// it changed to within a block.
const BLOCK: usize = 1024;
/// Turns between summary lines.
const SUMMARY_EVERY: u64 = 100;
/// Items at the end of an input whose start of text a session keeps, so a
/// break can show what the moved item was and what stood there before.
const TAIL: usize = 8;
/// Characters of an item's JSON kept for that preview.
const PREVIEW: usize = 160;

struct Prev {
    head: u64,
    items: Vec<u64>,
    /// Hash of each [`BLOCK`] of the first item's JSON, and its length.
    first_blocks: Vec<u64>,
    first_len: usize,
    /// Start of the last [`TAIL`] items' JSON; `tail[0]` is item `items.len() - tail.len()`.
    tail: Vec<String>,
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

type Store = Mutex<lru::LruCache<u64, VecDeque<Prev>>>;

fn store() -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
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

/// The first [`PREVIEW`] characters of an item's JSON.
fn preview(item: &Value) -> String {
    item.to_string().chars().take(PREVIEW).collect()
}

/// The start of the last [`TAIL`] items.
fn tail_previews(items: &[Value]) -> Vec<String> {
    items[items.len().saturating_sub(TAIL)..]
        .iter()
        .map(preview)
        .collect()
}

/// The preview a session kept for item `at`, if it is still in the tail.
fn kept_preview(prev: &Prev, at: usize) -> Option<&str> {
    let from = prev.items.len() - prev.tail.len();
    at.checked_sub(from)
        .and_then(|i| prev.tail.get(i))
        .map(String::as_str)
}

/// How many leading items two hash sequences share.
fn common_prefix(a: &[u64], b: &[u64]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// How a turn stands against the recent turns of its session.
struct Verdict {
    common: usize,
    prev_items: usize,
    head_changed: bool,
    moved_before: Option<String>,
    /// Where the first item changed and its length before and now, when `common == 0`.
    first_drift: Option<(usize, usize, usize)>,
}

/// Compares `now` with the recent turn it follows best: the newest one it
/// extends whole, else the one it shares the longest prefix with (newest on a
/// tie).
fn compare(recent: &VecDeque<Prev>, now: &Prev) -> Option<Verdict> {
    let mut best: Option<(&Prev, usize)> = None;
    for prev in recent {
        let common = common_prefix(&prev.items, &now.items);
        if common == prev.items.len() {
            best = Some((prev, common));
            break;
        }
        if best.is_none_or(|(_, most)| common > most) {
            best = Some((prev, common));
        }
    }
    let (prev, common) = best?;
    let diverged = common < prev.items.len();
    Some(Verdict {
        common,
        prev_items: prev.items.len(),
        head_changed: prev.head != now.head,
        moved_before: kept_preview(prev, common).map(str::to_string),
        first_drift: (common == 0 && diverged)
            .then(|| first_difference(&prev.first_blocks, &now.first_blocks))
            .flatten()
            .map(|at| (at * BLOCK, prev.first_len, now.first_len)),
    })
}

pub(crate) fn note(session: &str, body: &Value, body_bytes: usize, request_id: &str) {
    let all = items_of(body);
    let (items, first_blocks, first_len) = hash_items(all);
    let now = Prev {
        head: head_hash(body),
        items,
        first_blocks,
        first_len,
        tail: tail_previews(all),
    };
    let key = hash_of(&session);
    let verdict = store().lock().ok().and_then(|mut m| {
        let recent = m.get_or_insert_mut(key, VecDeque::new);
        let verdict = compare(recent, &now);
        recent.push_front(now);
        recent.truncate(HISTORY);
        verdict
    });
    let turns = tally().turns.fetch_add(1, Relaxed) + 1;
    if let Some(v) = verdict {
        let diverged = v.common < v.prev_items;
        let moved = all.get(v.common);
        let tool_call = moved.map(kind_of).as_deref() == Some("function_call");
        count(diverged, tool_call, v.head_changed);
        let model = body.get("model").and_then(Value::as_str);
        if v.head_changed || (diverged && !tool_call) {
            tracing::info!(
                event = "routed_forward_continuity",
                request_id,
                model,
                session_hash = format_args!("{key:016x}"),
                items = all.len(),
                prev_items = v.prev_items,
                common_prefix = v.common,
                prefix_broken = diverged,
                head_changed = v.head_changed,
                first_moved_kind = moved.map(kind_of),
                first_moved_bytes = moved.map(|v| serde_json::to_vec(v).map_or(0, |b| b.len())),
                moved_now = moved.map(preview),
                kinds_from_moved = ?all.iter().skip(v.common).take(TAIL).map(kind_of).collect::<Vec<_>>(),
                moved_before = v.moved_before,
                body_bytes,
                "routed forward continuity"
            );
        }
        if let Some((at, prev_len, now_len)) = v.first_drift {
            tracing::info!(
                event = "routed_first_item_drift",
                request_id,
                differs_at_about = at,
                prev_len,
                now_len,
                "first forwarded item changed since the last turn"
            );
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

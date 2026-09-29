//! What a routed turn forwarded, compared with the session's previous turn.
//!
//! A provider prompt cache reads a turn back only as far as its input matches
//! the last one byte for byte. Spark turns on Zen show cached fractions of
//! about 0.99 in some turns and 0.13 in others with no visible cause, so this
//! logs where consecutive forwarded inputs first differ: the `head` (model,
//! instructions, tools) and each `input`/`messages` item are hashed, and the
//! event names the first item that moved. Read-only: the body is not touched.

use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};

const SESSIONS: usize = 1024;
/// Sessions whose first item is kept as text, for [`log_first_item_drift`]. That
/// item is the whole system prompt (hundreds of KB), so the bound is small.
const TEXT_SESSIONS: usize = 32;
/// Bytes of context printed on each side of the first differing byte.
const DRIFT_CONTEXT: usize = 60;

struct Prev {
    head: u64,
    items: Vec<u64>,
}

fn store() -> &'static Mutex<lru::LruCache<u64, Prev>> {
    static STORE: OnceLock<Mutex<lru::LruCache<u64, Prev>>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(lru::LruCache::new(
            NonZeroUsize::new(SESSIONS).expect("non-zero capacity"),
        ))
    })
}

fn texts() -> &'static Mutex<lru::LruCache<u64, String>> {
    static TEXTS: OnceLock<Mutex<lru::LruCache<u64, String>>> = OnceLock::new();
    TEXTS.get_or_init(|| {
        Mutex::new(lru::LruCache::new(
            NonZeroUsize::new(TEXT_SESSIONS).expect("non-zero capacity"),
        ))
    })
}

fn hash_of(v: &impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn hash_value(v: &Value) -> u64 {
    hash_of(&serde_json::to_vec(v).unwrap_or_default())
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
    let items: Vec<u64> = items_of(body).iter().map(hash_value).collect();
    let head = head_hash(body);
    let key = hash_of(&session);
    let prev = store().lock().ok().and_then(|mut m| {
        m.put(
            key,
            Prev {
                head,
                items: items.clone(),
            },
        )
    });
    let prev_first = swap_first_item(key, items_of(body).first());
    let cache_key = body.get("prompt_cache_key").and_then(Value::as_str);
    let model = body.get("model").and_then(Value::as_str);
    let Some(prev) = prev else {
        tracing::info!(
            event = "routed_forward_continuity",
            request_id,
            model,
            session_hash = format_args!("{key:016x}"),
            items = items.len(),
            body_bytes,
            first_turn = true,
            prompt_cache_key_hash = cache_key.map(|k| format!("{:016x}", hash_of(&k))),
            "routed forward continuity"
        );
        return;
    };
    let common = common_prefix(&prev.items, &items);
    let diverged = common < prev.items.len();
    let moved = items_of(body).get(common);
    if common == 0 && diverged {
        log_first_item_drift(prev_first.as_deref(), items_of(body).first(), request_id);
    }
    tracing::info!(
        event = "routed_forward_continuity",
        request_id,
        model,
        session_hash = format_args!("{key:016x}"),
        items = items.len(),
        prev_items = prev.items.len(),
        common_prefix = common,
        // A previous item that no longer matches is a prefix break; a longer
        // input whose old items all match is the healthy append.
        prefix_broken = diverged,
        head_changed = prev.head != head,
        first_moved_kind = moved.map(kind_of),
        first_moved_bytes = moved.map(|v| serde_json::to_vec(v).map_or(0, |b| b.len())),
        body_bytes,
        prompt_cache_key_hash = cache_key.map(|k| format!("{:016x}", hash_of(&k))),
        "routed forward continuity"
    );
}

/// Stores this turn's first item and returns the previous turn's.
fn swap_first_item(key: u64, first: Option<&Value>) -> Option<String> {
    let text = serde_json::to_string(first?).ok()?;
    texts().lock().ok()?.put(key, text)
}

/// Where two first items first part, and how much they share at the end, which
/// tells an edited line from a moved block. `None` when they are equal.
fn drift(prev: &str, now: &str) -> Option<(usize, usize, usize)> {
    let start = prev
        .bytes()
        .zip(now.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    if start == prev.len() && start == now.len() {
        return None;
    }
    let tail = prev.as_bytes()[start..]
        .iter()
        .rev()
        .zip(now.as_bytes()[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    Some((start, tail, prev.len().abs_diff(now.len())))
}

/// `text` around byte `at`, cut back to character boundaries.
fn around(text: &str, at: usize) -> &str {
    let mut lo = at.saturating_sub(DRIFT_CONTEXT);
    let mut hi = (at + DRIFT_CONTEXT).min(text.len());
    while !text.is_char_boundary(lo) {
        lo -= 1;
    }
    while !text.is_char_boundary(hi) {
        hi += 1;
    }
    &text[lo..hi]
}

fn log_first_item_drift(prev: Option<&str>, now: Option<&Value>, request_id: &str) {
    let (Some(prev), Some(now)) = (prev, now.and_then(|v| serde_json::to_string(v).ok())) else {
        return;
    };
    let Some((at, same_tail, len_delta)) = drift(prev, &now) else {
        return;
    };
    tracing::info!(
        event = "routed_first_item_drift",
        request_id,
        differs_at = at,
        same_tail_bytes = same_tail,
        len_delta,
        prev_len = prev.len(),
        now_len = now.len(),
        prev_context = around(prev, at),
        now_context = around(&now, at),
        "first forwarded item changed since the last turn"
    );
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
// which is why it is logged. These pin the two ways the excerpt can go wrong.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_reports_the_first_difference_and_the_shared_tail() {
        assert_eq!(drift("abc", "abc"), None);
        assert_eq!(drift("cwd /one/x", "cwd /two/x"), Some((5, 2, 0)));
        assert_eq!(drift("abc", "abcdef"), Some((3, 0, 3)));
    }

    #[test]
    fn the_excerpt_never_splits_a_character() {
        let text = format!("{}é{}", "a".repeat(DRIFT_CONTEXT - 1), "b".repeat(200));
        for at in [0, DRIFT_CONTEXT - 1, DRIFT_CONTEXT, text.len()] {
            let _ = around(&text, at);
        }
    }
}

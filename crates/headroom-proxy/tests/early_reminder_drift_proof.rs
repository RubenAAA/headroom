//! Proof that the drift detector and the replay comparator disagree about
//! withdrawn `<system-reminder>` scaffolding, and that the disagreement turns
//! on message index alone.
//!
//! Shape taken from a live persisted prefix
//! (`~/.local/state/headroom/replay-prefixes`, 2026-08-26): 80 of 87 stored
//! conversations carry a reminder in `messages[0]`, and 34 carry a standalone
//! `role: "system"` bare-string reminder at `messages[1]`.

use headroom_proxy::cache_stabilization::drift_detector::{compute_structural_hash, ApiKind};
use headroom_proxy::cache_stabilization::prefix_replay::canonicalize_for_prefix_compare;
use serde_json::{json, Value};

/// `messages[1]` is the withdrawable standalone reminder; `messages[2]` is the
/// assistant turn that slides into its slot when it goes.
fn body_with_reminder_at(slot: usize) -> Value {
    let reminder = json!({
        "role": "system",
        "content": "<system-reminder>\nThe following skills are available.\n</system-reminder>"
    });
    let mut messages = vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>\nproject context\n</system-reminder>"},
            {"type": "text", "text": "the user's actual question"}
        ]}),
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "..."},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "file body"}
        ]}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
        json!({"role": "user", "content": [{"type": "text", "text": "next"}]}),
    ];
    messages.insert(slot, reminder);
    json!({"model": "claude-opus-4", "messages": messages})
}

fn without_reminder() -> Value {
    json!({"model": "claude-opus-4", "messages": [
        json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>\nproject context\n</system-reminder>"},
            {"type": "text", "text": "the user's actual question"}
        ]}),
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "..."},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "file body"}
        ]}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
        json!({"role": "user", "content": [{"type": "text", "text": "next"}]}),
    ]})
}

fn msgs(body: &Value) -> Vec<Value> {
    body["messages"].as_array().unwrap().clone()
}

/// The replay comparator's verdict: does every message the two bodies share,
/// once the withdrawn message is stepped over, canonicalize the same?
fn replay_sees_same_prefix(with: &Value, without: &Value, withdrawn_slot: usize) -> bool {
    let a = msgs(with);
    let b = msgs(without);
    a.iter()
        .enumerate()
        .filter(|(i, _)| *i != withdrawn_slot)
        .map(|(_, m)| canonicalize_for_prefix_compare(m))
        .eq(b.iter().map(canonicalize_for_prefix_compare))
}

#[test]
fn the_withdrawal_used_to_be_drift_at_slot_1_and_free_at_slot_4() {
    let bare = without_reminder();

    // Identical client behaviour, twice: the same reminder message withdrawn,
    // once from inside the detector's 3-message window and once past it. Before
    // the fix the verdict turned on the index alone, which is the whole defect.
    for (slot, drifted_before_the_fix) in [(1usize, true), (4usize, false)] {
        let decorated = body_with_reminder_at(slot);
        let before = compute_structural_hash(&decorated, ApiKind::Anthropic);
        let after = compute_structural_hash(&bare, ApiKind::Anthropic);

        assert_eq!(
            before.early_messages_legacy != after.early_messages_legacy,
            drifted_before_the_fix,
            "slot {slot}: the pre-2026-08-26 verdict"
        );

        // And now it does not, at either index.
        assert_eq!(
            before.early_messages, after.early_messages,
            "slot {slot}: a withdrawn reminder must not read as drift"
        );

        // The replay comparator always said the same thing. It was never asked,
        // because the invalidation ran first.
        assert!(
            replay_sees_same_prefix(&decorated, &bare, slot),
            "slot {slot}: replay comparator must see one unchanged prefix"
        );
    }
}

// ---------------------------------------------------------------------------
// The proposed fix, modelled here so its effect is measurable before it ships.
//
// `early_message_hashes` slots by raw index and hashes the raw message. Two
// changes make it ask the question the rest of the pipeline already asks:
//   1. skip messages that are nothing but client scaffolding, so a withdrawal
//      does not shift every later slot;
//   2. strip trailing reminder spans inside a message before hashing.
// Both predicates already exist in `prefix_replay`; the fix is to share one
// definition rather than add a second.
// ---------------------------------------------------------------------------

fn is_pure_scaffolding(m: &Value) -> bool {
    if m.get("role").and_then(Value::as_str) == Some("assistant") {
        return false;
    }
    let strip = |t: &str| t.replace(|_c: char| false, "");
    match m.get("content") {
        Some(Value::String(t)) => strip(t).trim().starts_with("<system-reminder>"),
        Some(Value::Array(b)) => {
            !b.is_empty()
                && b.iter().all(|blk| {
                    blk.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.trim().starts_with("<system-reminder>"))
                })
        }
        _ => false,
    }
}

/// What the detector would hash under the fix: the first three *substantive*
/// early messages.
fn early_slots_under_fix(body: &Value) -> Vec<Value> {
    msgs(body)
        .into_iter()
        .filter(|m| !is_pure_scaffolding(m))
        .take(3)
        .map(|m| canonicalize_for_prefix_compare(&m))
        .collect()
}

#[test]
fn the_fix_absorbs_the_withdrawal_without_blinding_the_detector() {
    let bare = without_reminder();
    let decorated = body_with_reminder_at(1);

    // 1. The withdrawal stops reading as drift.
    assert_eq!(
        early_slots_under_fix(&decorated),
        early_slots_under_fix(&bare),
        "a withdrawn scaffolding message must not shift the early window"
    );

    // 2. A real edit to settled early history still reads as drift. This is the
    //    half that matters: the detector must keep earning its invalidation.
    let mut edited = bare.clone();
    edited["messages"][0]["content"][1]["text"] = json!("a different question entirely");
    assert_ne!(
        early_slots_under_fix(&edited),
        early_slots_under_fix(&bare),
        "an edit to real early content must still be drift"
    );

    // 3. And so does a genuine change at the slot the log calls `2:block[0]`,
    //    which live data says is never a reminder (0 of 80 stored prefixes).
    let mut thinking_changed = bare.clone();
    thinking_changed["messages"][1]["content"][0]["thinking"] = json!("different reasoning");
    assert_ne!(
        early_slots_under_fix(&thinking_changed),
        early_slots_under_fix(&bare),
        "a rewritten thinking block must still be drift"
    );
}

// ---------------------------------------------------------------------------
// The offline measurement.
//
// Live `early_scaffolding_absorbed` events price the fix going forward, but
// they need traffic. This prices it against conversations already on disk, in
// seconds, by asking each one the question that costs money: if the client
// withdrew the scaffolding it is currently holding in the early window, would
// the detector drop the whole stored prefix?
//
//   cargo test -p headroom-proxy --test early_reminder_drift_proof \
//       -- --ignored --nocapture
// ---------------------------------------------------------------------------

use headroom_proxy::cache_stabilization::drift_detector::EARLY_MESSAGES_WINDOW;

fn early_window_moved(prev: &[Option<[u8; 32]>], curr: &[Option<[u8; 32]>]) -> bool {
    prev.iter().zip(curr.iter()).any(|slots| match slots {
        (Some(p), Some(c)) => p != c,
        (Some(_), None) => true,
        (None, _) => false,
    })
}

/// Is this message nothing but a `<system-reminder>`? Mirrors the predicate the
/// fix uses; kept local so the harness measures behaviour, not an import.
fn scaffolding_only(m: &Value) -> bool {
    if m.get("role").and_then(Value::as_str) == Some("assistant") {
        return false;
    }
    match m.get("content") {
        Some(Value::String(t)) => t.trim().starts_with("<system-reminder>"),
        Some(Value::Array(b)) => {
            !b.is_empty()
                && b.iter().all(|blk| {
                    blk.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.trim().starts_with("<system-reminder>"))
                })
        }
        _ => false,
    }
}

#[test]
#[ignore = "reads the operator's persisted replay prefixes; run on demand"]
fn price_the_fix_against_persisted_conversations() {
    let dir = std::env::var("HEADROOM_REPLAY_PREFIX_DIR").unwrap_or_else(|_| {
        format!(
            "{}/.local/state/headroom/replay-prefixes",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("no prefix directory at {dir}; nothing to measure");
        return;
    };

    let (mut total, mut eligible, mut before, mut after) = (0usize, 0usize, 0usize, 0usize);
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(stored) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(originals) = stored.get("originals").and_then(Value::as_array) else {
            continue;
        };
        total += 1;

        // The turn as stored, and the same turn with the scaffolding the client
        // is holding in the early window withdrawn — the move that costs money.
        let held: Vec<Value> = originals.clone();
        let withdrawn: Vec<Value> = originals
            .iter()
            .enumerate()
            .filter(|(i, m)| *i >= EARLY_MESSAGES_WINDOW * 2 || !scaffolding_only(m))
            .map(|(_, m)| m.clone())
            .collect();
        if withdrawn.len() == held.len() {
            continue; // nothing withdrawable in the window
        }
        eligible += 1;

        let body = |m: &Vec<Value>| json!({"model": "m", "messages": m});
        let h1 = compute_structural_hash(&body(&held), ApiKind::Anthropic);
        let h2 = compute_structural_hash(&body(&withdrawn), ApiKind::Anthropic);

        if early_window_moved(&h1.early_messages_legacy, &h2.early_messages_legacy) {
            before += 1;
        }
        if early_window_moved(&h1.early_messages, &h2.early_messages) {
            after += 1;
        }
    }

    println!("\n  persisted conversations read        : {total}");
    println!("  holding withdrawable scaffolding    : {eligible}");
    println!("  invalidated by a withdrawal, BEFORE : {before}");
    println!("  invalidated by a withdrawal, AFTER  : {after}");
    if before > 0 {
        let pct = 100.0 * (before - after) as f64 / before as f64;
        println!("  removed by the fix                  : {} ({pct:.0}%)\n", before - after);
    }
    assert!(after <= before, "the fix must not create new invalidations");
}

// ---------------------------------------------------------------------------
// The second measurement: the same withdrawal, judged by the replay
// comparator rather than the drift detector.
//
// `align_over_withdrawn_scaffolding` keyed on the `<system-reminder>` tag, and
// most of these messages do not carry one. This prices widening it to `role`.
//
//   cargo test -p headroom-proxy --test early_reminder_drift_proof \
//       -- --ignored --nocapture price_the_role_predicate
// ---------------------------------------------------------------------------

/// The predicate as it was: scaffolding only when the reminder tag is present.
fn tagged_scaffolding_only(_index: usize, m: &Value) -> bool {
    scaffolding_only(m)
}

/// The predicate as it ships: `role: "system"` past index 0 counts too.
fn role_is_the_marker(index: usize, m: &Value) -> bool {
    if index > 0 && m.get("role").and_then(Value::as_str) == Some("system") {
        return true;
    }
    scaffolding_only(m)
}

/// Does the stored prefix still line up with this turn once the client drops
/// the scaffolding it is holding? Mirrors `align_over_withdrawn_scaffolding`.
fn aligns(prev: &[Value], curr: &[Value], scaffolding: fn(usize, &Value) -> bool) -> bool {
    let mut j = 0usize;
    for (i, stored) in prev.iter().enumerate() {
        if curr.get(j).is_some_and(|c| c == stored) {
            j += 1;
            continue;
        }
        if scaffolding(i, stored) {
            continue;
        }
        return false;
    }
    true
}

#[test]
#[ignore = "reads the operator's persisted replay prefixes; run on demand"]
fn price_the_role_predicate_against_persisted_conversations() {
    let dir = std::env::var("HEADROOM_REPLAY_PREFIX_DIR").unwrap_or_else(|_| {
        format!(
            "{}/.local/state/headroom/replay-prefixes",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("no prefix directory at {dir}; nothing to measure");
        return;
    };

    let (mut held, mut before, mut after) = (0usize, 0usize, 0usize);
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(stored) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(originals) = stored.get("originals").and_then(Value::as_array) else {
            continue;
        };

        // What the client would send if it withdrew everything it is holding.
        let withdrawn: Vec<Value> = originals
            .iter()
            .enumerate()
            .filter(|(i, m)| !role_is_the_marker(*i, m))
            .map(|(_, m)| m.clone())
            .collect();
        if withdrawn.len() == originals.len() {
            continue;
        }
        held += 1;
        if aligns(originals, &withdrawn, tagged_scaffolding_only) {
            before += 1;
        }
        if aligns(originals, &withdrawn, role_is_the_marker) {
            after += 1;
        }
    }

    eprintln!("conversations holding scaffolding: {held}");
    eprintln!("  prefix survives the withdrawal BEFORE: {before}");
    eprintln!("  prefix survives the withdrawal AFTER : {after}");
    assert!(
        after >= before,
        "widening the predicate must never strand a prefix that used to survive"
    );
}

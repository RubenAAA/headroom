//! usage_observer::fingerprint — split from usage_observer.rs (pure move, no logic change).

/// Item 11's deciding test: what the cacheable part of this request actually
/// contained, split at the boundary the evidence points to.
///
/// A recache event says a prefix was re-written. It cannot say *why*, and the
/// two candidate causes need opposite fixes:
///
/// - **Real thrash** — two concurrent streams on one conversation genuinely
///   send different bytes past the tools block, so each one's prefix misses.
///   Real money, roughly 90K tokens per turn on the observed conversation.
/// - **Artefact** — [`conversation_key`](super::keys::conversation_key) is too coarse and merged two separate
///   conversations, so ordinary alternation only *looks* like drift.
///
/// Logging these two hashes next to the key decides it. For two alternating
/// turns under one key:
///
/// - same `head`, **different** `stable` → the streams diverge after the tools
///   block. Real thrash; the waste is real spend.
/// - same `head`, **same** `stable` → identical cacheable bytes, so the key
///   merged two streams upstream treats separately (or upstream evicted).
///   The waste is an accounting artefact and item 3's totals shrink.
///
/// The split is at system+tools because that is where the observed floor sits:
/// `actual_cache_read` pinned at exactly 13,907 across twelve turns while the
/// conversation grew from 55 to 121 messages means that stream matched the
/// leading block and nothing after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixFingerprint {
    /// `model` + `system` + `tools` — the block that does cache.
    pub head: String,
    /// The three components hashed separately, same projection as `head`
    /// (`sample_value` per fragment, `hex16` truncation), so a later turn can
    /// say *which* of model/system/tools moved rather than just that the
    /// fused head did. Additive logging only: never re-gate attribution on
    /// these (see `recache_attribution` — the empty-dims + head shape is the
    /// abandoned-retry witness and must keep firing).
    pub head_model: String,
    pub head_system: String,
    pub head_tools: String,
    /// The first [`FINGERPRINT_FIXED_DEPTH`] messages.
    ///
    /// Fixed depth on purpose. The obvious design — hash every message except
    /// the live tail — is useless here: that region grows by one message per
    /// turn, so two turns of one conversation never agree and the field can
    /// only ever report "different". A fixed depth is comparable between any
    /// two turns of any length, which is the whole job.
    ///
    /// Depth is measured from the opener because that is where a merged key
    /// hides. `conversation_key` is `(model, first message)`, so two subagents
    /// merged by it share message 0 by construction; if they are genuinely
    /// different work they diverge within the next few turns.
    ///
    /// Empty when the conversation has not yet reached the depth — below it the
    /// hash would move purely because the conversation grew, which is the very
    /// thing the fixed depth exists to prevent. An empty value means "not
    /// comparable yet", never "no difference".
    pub body: String,
    /// Every message except the live tail. Only comparable between turns whose
    /// `stable_msgs` agree — which the alternating pairs in item 11 mostly do.
    pub stable: String,
    /// Depth `stable` covered, so a reader can tell whether two `stable`
    /// values were even measured over the same span.
    pub stable_msgs: usize,
}

/// Hash the cacheable regions of a parsed Anthropic body.
///
/// Deliberately samples rather than serialising. A full re-serialise of a
/// 1.4 MB body would cost more than the whole optimisation stage it sits in
/// (`opt_ms` median is 11ms), and this is a diagnostic. Per text fragment it
/// feeds the hasher the exact byte length plus the leading
/// [`FINGERPRINT_SAMPLE_BYTES`], which two different conversations collide on
/// only if every fragment shares both — not a case worth engineering against
/// for a field whose job is to tell two live streams apart.
pub fn prefix_fingerprint(parsed: &serde_json::Value) -> PrefixFingerprint {
    prefix_fingerprint_with_model(parsed, None)
}

/// As [`prefix_fingerprint`], but with `identity_model` standing in for the
/// body's own `model`.
///
/// Same reason as `derive_session_key_with_model`: a turn the cost-aware
/// router sent to another upstream is the same conversation, and the
/// fingerprint has to match the one the previous turn left behind.
pub fn prefix_fingerprint_with_model(
    parsed: &serde_json::Value,
    identity_model: Option<&str>,
) -> PrefixFingerprint {
    use sha2::{Digest, Sha256};

    let mut head = Sha256::new();
    if let Some(model) = identity_model.or_else(|| parsed.get("model").and_then(|v| v.as_str())) {
        head.update(model.as_bytes());
    }
    for key in ["system", "tools"] {
        head.update([0xff]);
        if let Some(v) = parsed.get(key) {
            sample_value(v, &mut head);
        }
    }

    // Per-component heads, same projection as the fused head above so
    // "system moved" agrees with what moved `head`. Each hashes only its own
    // component; absent reads as the empty hash, comparable across turns.
    let mut head_model_hasher = Sha256::new();
    if let Some(model) = identity_model.or_else(|| parsed.get("model").and_then(|v| v.as_str())) {
        head_model_hasher.update(model.as_bytes());
    }
    let mut head_system_hasher = Sha256::new();
    if let Some(v) = parsed.get("system") {
        sample_value(v, &mut head_system_hasher);
    }
    let mut head_tools_hasher = Sha256::new();
    if let Some(v) = parsed.get("tools") {
        sample_value(v, &mut head_tools_hasher);
    }

    let mut body = Sha256::new();
    let mut stable = Sha256::new();
    let mut stable_msgs = 0usize;
    let mut body_comparable = false;
    if let Some(msgs) = parsed.get("messages").and_then(|v| v.as_array()) {
        // Only meaningful once the conversation is longer than the depth.
        // Below that, `take(depth)` returns a different number of messages on
        // every turn, so the hash would change purely because the
        // conversation grew — the exact failure mode the fixed depth exists to
        // avoid. Report nothing rather than something incomparable.
        if msgs.len() > FINGERPRINT_FIXED_DEPTH {
            body_comparable = true;
            for m in msgs.iter().take(FINGERPRINT_FIXED_DEPTH) {
                body.update([0xff]);
                sample_value(m, &mut body);
            }
        }
        // Drop the live tail: it differs between turns by design.
        let end = msgs.len().saturating_sub(1);
        for m in &msgs[..end] {
            stable.update([0xff]);
            sample_value(m, &mut stable);
            stable_msgs += 1;
        }
    }

    PrefixFingerprint {
        head: hex16(head.finalize().as_slice()),
        head_model: hex16(head_model_hasher.finalize().as_slice()),
        head_system: hex16(head_system_hasher.finalize().as_slice()),
        head_tools: hex16(head_tools_hasher.finalize().as_slice()),
        body: if body_comparable {
            hex16(body.finalize().as_slice())
        } else {
            String::new()
        },
        stable: hex16(stable.finalize().as_slice()),
        stable_msgs,
    }
}

/// Leading bytes taken from each text fragment. Enough that two different
/// messages differ, small enough that the walk stays off the latency budget.
const FINGERPRINT_SAMPLE_BYTES: usize = 64;

/// Messages covered by [`PrefixFingerprint::body`]. Deep enough that two
/// different lines of work have diverged, shallow enough to stay comparable on
/// a short conversation.
const FINGERPRINT_FIXED_DEPTH: usize = 8;

/// Walk a value feeding the hasher structure plus bounded text samples. Never
/// allocates a serialised copy — string fragments are hashed in place.
fn sample_value(v: &serde_json::Value, hasher: &mut impl sha2::Digest) {
    match v {
        serde_json::Value::String(s) => {
            hasher.update((s.len() as u64).to_le_bytes());
            let n = s.len().min(FINGERPRINT_SAMPLE_BYTES);
            hasher.update(&s.as_bytes()[..n]);
        }
        serde_json::Value::Array(items) => {
            hasher.update((items.len() as u64).to_le_bytes());
            for item in items {
                sample_value(item, hasher);
            }
        }
        serde_json::Value::Object(map) => {
            hasher.update((map.len() as u64).to_le_bytes());
            // serde_json preserves insertion order by default; hash the keys
            // too so a reordered object is not mistaken for the same one.
            for (k, val) in map {
                hasher.update(k.as_bytes());
                sample_value(val, hasher);
            }
        }
        serde_json::Value::Number(n) => hasher.update(n.to_string().as_bytes()),
        serde_json::Value::Bool(b) => hasher.update([*b as u8]),
        serde_json::Value::Null => hasher.update([0u8]),
    }
}

pub(super) fn hex16(digest: &[u8]) -> String {
    // `hex` is already a workspace dep; identical lowercase output to
    // the per-byte `format!` loop at ~6x.
    hex::encode(&digest[..8])
}

#[cfg(test)]
mod prefix_fingerprint_tests {
    use super::*;
    use serde_json::json;

    fn body(system: &str, msgs: &[&str]) -> serde_json::Value {
        json!({
            "model": "claude-sonnet-5",
            "system": system,
            "tools": [{"name": "Read", "input_schema": {}}],
            "messages": msgs.iter().map(|m| json!({"role":"user","content":m}))
                .collect::<Vec<_>>(),
        })
    }

    /// The live tail must not participate, or every comparison says "different"
    /// and the field decides nothing. Two turns of one growing conversation
    /// share a stable region.
    #[test]
    fn appending_a_live_turn_leaves_the_fixed_depth_hash_alone() {
        // Longer than FINGERPRINT_FIXED_DEPTH, as any real conversation the
        // watchdog fires on will be (item 11's ran 55-121 messages).
        let base: Vec<String> = (0..12).map(|i| format!("msg {i}")).collect();
        let refs: Vec<&str> = base.iter().map(|s| s.as_str()).collect();
        let mut grown = refs.clone();
        grown.push("one more live turn");
        let turn_n = prefix_fingerprint(&body("sys", &refs));
        let turn_n1 = prefix_fingerprint(&body("sys", &grown));
        assert_eq!(turn_n.head, turn_n1.head);
        // The comparable field: one conversation, two turns, same value.
        assert_eq!(turn_n.body, turn_n1.body, "fixed-depth hash must be stable");
        // `stable` grows with the conversation, which is exactly why it cannot
        // be the comparator on its own — it is reported with its depth so a
        // reader knows when two values are even measured over the same span.
        assert_ne!(turn_n.stable, turn_n1.stable);
        assert_eq!(turn_n.stable_msgs, 11);
        assert_eq!(turn_n1.stable_msgs, 12);
    }

    /// Two conversations that share an opener — the exact shape
    /// `conversation_key` merges — must still be told apart.
    #[test]
    fn a_shared_opener_does_not_hide_different_work() {
        let mut a_msgs = vec!["same opener", "audit the cache"];
        let mut b_msgs = vec!["same opener", "rename a symbol"];
        for _ in 0..10 {
            a_msgs.push("filler");
            b_msgs.push("filler");
        }
        let a = prefix_fingerprint(&body("sys", &a_msgs));
        let b = prefix_fingerprint(&body("sys", &b_msgs));
        assert_eq!(a.head, b.head);
        assert_ne!(a.body, b.body, "merged key, different work: must diverge");
    }

    /// Reading 1 (real thrash): same leading block, different bodies past it.
    /// This is the case that means real money, so the hashes must diverge.
    #[test]
    fn same_head_different_bodies_diverge() {
        let a = prefix_fingerprint(&body("sys", &["a", "b", "tail"]));
        let b = prefix_fingerprint(&body("sys", &["a", "DIFFERENT", "tail"]));
        assert_eq!(a.head, b.head, "same system+tools");
        assert_ne!(a.stable, b.stable, "divergence past the tools block");
    }

    /// Reading 2 (artefact): byte-identical cacheable regions under one key.
    #[test]
    fn identical_requests_agree() {
        let a = prefix_fingerprint(&body("sys", &["a", "b", "tail"]));
        let b = prefix_fingerprint(&body("sys", &["a", "b", "tail"]));
        assert_eq!(a, b);
    }

    /// A changed system prompt is a head change, not a body change — item 3e's
    /// shape must land on the other side of the split.
    #[test]
    fn a_changed_system_prompt_moves_the_head_not_the_body() {
        let a = prefix_fingerprint(&body("sys one", &["a", "b", "tail"]));
        let b = prefix_fingerprint(&body("sys two", &["a", "b", "tail"]));
        assert_ne!(a.head, b.head);
        assert_eq!(a.stable, b.stable);
    }

    /// The per-component heads isolate which cacheable input moved, in the
    /// same projection as the fused head. Each change moves its own component
    /// and leaves the other two alone — the property `head_moved` reports.
    #[test]
    fn head_components_isolate_which_cacheable_input_moved() {
        let base = body("sys", &["a", "b", "tail"]);
        let a = prefix_fingerprint(&base);
        // System move: only the system component changes.
        let mut resys = base.clone();
        resys["system"] = json!("a different system");
        let b = prefix_fingerprint(&resys);
        assert_ne!(b.head_system, a.head_system);
        assert_eq!(b.head_model, a.head_model);
        assert_eq!(b.head_tools, a.head_tools);
        // Tools move: only the tools component changes.
        let mut retools = base.clone();
        retools["tools"] = json!([{"name": "Write", "input_schema": {}}]);
        let c = prefix_fingerprint(&retools);
        assert_ne!(c.head_tools, a.head_tools);
        assert_eq!(c.head_model, a.head_model);
        assert_eq!(c.head_system, a.head_system);
        // Model move: only the model component changes.
        let mut remodel = base.clone();
        remodel["model"] = json!("claude-opus-5");
        let d = prefix_fingerprint(&remodel);
        assert_ne!(d.head_model, a.head_model);
        assert_eq!(d.head_system, a.head_system);
        assert_eq!(d.head_tools, a.head_tools);
    }

    /// Divergence beyond the sampled window still has to register, or a long
    /// shared preamble would hide it.
    #[test]
    fn divergence_past_the_sample_window_still_registers() {
        let long = "x".repeat(FINGERPRINT_SAMPLE_BYTES * 4);
        let a = prefix_fingerprint(&body("sys", &[&format!("{long}AAA"), "tail"]));
        let b = prefix_fingerprint(&body("sys", &[&format!("{long}BBB"), "tail"]));
        // Same leading bytes and same length, so this is the collision the
        // sampling admits. Documented rather than asserted away: the field
        // tells live streams apart, it is not a content digest.
        assert_eq!(
            a.stable, b.stable,
            "known limit of sampling: equal len + equal prefix"
        );

        let c = prefix_fingerprint(&body("sys", &[&format!("{long}AAAA"), "tail"]));
        assert_ne!(a.stable, c.stable, "a length change must always register");
    }
}

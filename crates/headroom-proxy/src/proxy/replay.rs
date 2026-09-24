//! Prefix replay and cache diagnostics: cache-key fingerprints, digest
//! ladders, tool roster notes, cold forks, and replay application.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// True if the upstream response is an SSE stream. Compares
/// `content-type` against `text/event-stream` (with optional
/// parameters). RFC 7231 §3.1.1.1: media types compare
/// case-insensitive on the type/subtype tokens.
/// One cheap digest per message, in order.
///
/// Used to check a single invariant inside one request: whatever the prefix
/// replay spliced in must still be there when the bytes go out. Everything
/// between those two points — breakpoint placement, memory injection, context
/// injection, PAYG rewrites — is supposed to leave the settled prefix alone,
/// and nothing verified that it did.
///
/// Returns `None` for a body without a `messages` array, which is not the
/// shape this checks.
/// Every property of the forwarded request that the provider's cache key
/// depends on, in one line, so the residue can be diffed turn-against-turn
/// offline instead of needing a separate instrument per hypothesis.
///
/// Returns `(model, marker_map, breakpoints)`. `marker_map` names each
/// `cache_control` position and its TTL — `sys:1h,m12:1h,m30:5m` — because a
/// breakpoint that moves behind the settled prefix, or a fifth one that pushes
/// an earlier one out, kills the read while every byte still matches.
pub(super) fn cache_key_fingerprint(body: &[u8]) -> Option<(String, String, usize)> {
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("?")
        .to_string();

    let ttl_of = |v: &serde_json::Value| {
        v.get("cache_control")
            .and_then(|c| c.get("ttl"))
            .and_then(|t| t.as_str())
            .unwrap_or("5m")
            .to_string()
    };
    let mut marks: Vec<String> = Vec::new();
    let mut scan = |label: String, container: Option<&serde_json::Value>| {
        let Some(blocks) = container.and_then(|c| c.as_array()) else {
            return;
        };
        for (i, b) in blocks.iter().enumerate() {
            if b.get("cache_control").is_some() {
                marks.push(format!("{label}[{i}]:{}", ttl_of(b)));
            }
        }
    };
    scan("sys".into(), parsed.get("system"));
    scan("tools".into(), parsed.get("tools"));

    if let Some(msgs) = parsed.get("messages").and_then(|m| m.as_array()) {
        for (i, m) in msgs.iter().enumerate() {
            if m.get("cache_control").is_some() {
                marks.push(format!("m{i}:{}", ttl_of(m)));
            }
            if let Some(blocks) = m.get("content").and_then(|c| c.as_array()) {
                for (j, b) in blocks.iter().enumerate() {
                    if b.get("cache_control").is_some() {
                        marks.push(format!("m{i}.{j}:{}", ttl_of(b)));
                    }
                }
            }
        }
    }
    let count = marks.len();
    Some((model, marks.join(","), count))
}

/// Cumulative digests of the forwarded message prefix at doubling depths —
/// `1:a1b2,2:c3d4,4:...,8:...`. Two turns of one conversation share every
/// checkpoint up to the point where their forwarded bytes first differ, so the
/// smallest depth whose digest moved localizes the divergence without logging
/// a digest per message.
///
/// This is the one property the whole cache rests on: the forwarded prefix is
/// byte-stable turn over turn, except for the tail this turn appends.
pub(super) fn prefix_digest_ladder(body: &[u8]) -> Option<String> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = parsed.get("messages")?.as_array()?;
    let mut hasher = DefaultHasher::new();
    let mut out = Vec::new();
    let mut depth = 1usize;
    for (i, m) in messages.iter().enumerate() {
        cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(m)
            .to_string()
            .hash(&mut hasher);
        if i + 1 == depth {
            out.push(format!("{depth}:{:04x}", hasher.finish() & 0xffff));
            depth *= 2;
        }
    }
    Some(out.join(","))
}

/// Windowed digests of the forwarded tail — `t1` covers the last message,
/// `t2` the last two, `t4` the last four — so churn in the tail can be placed
/// without logging a digest per message.
///
/// The head-anchored [`prefix_digest_ladder`] goes blind exactly where the
/// residual misses live: its checkpoints stop doubling at 32, while the
/// disputed region on a 50-message turn is messages 33+. Two turns sharing
/// every head checkpoint can still differ anywhere in the tail, and that is
/// the whole residue. Read this one back to front: the smallest window whose
/// digest moved bounds the churn to that many tail messages (`t1` moved: the
/// last message; `t1` held but `t2` moved: the second-to-last).
///
/// Same projection and hasher as the head ladder, so the two agree on what
/// counts as a difference. Windows clamp to the messages there are, keeping
/// the `t1,t2,t4` schema fixed for log queries. Emitted as its own
/// `tail_ladder` field rather than folded into `prefix_ladder`, so existing
/// parsers of that field keep working untouched.
pub(super) fn tail_digest_ladder(body: &[u8]) -> Option<String> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = parsed.get("messages")?.as_array()?;
    let mut out = Vec::new();
    for k in [1usize, 2, 4] {
        let start = messages.len().saturating_sub(k);
        let mut hasher = DefaultHasher::new();
        for m in &messages[start..] {
            cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(m)
                .to_string()
                .hash(&mut hasher);
        }
        out.push(format!("t{k}:{:04x}", hasher.finish() & 0xffff));
    }
    Some(out.join(","))
}

/// Digests of the two fields that precede every message in the provider's
/// cached prefix. A change to either kills the whole cache, and no message
/// digest can see it — `tools` and `system` are rewritten by four stages that
/// run after the replay stage (`maybe_prune_tools`, `maybe_compact_tool_
/// schemas`, the stable tool order pass, and `maybe_inject_context_management`).
pub(super) fn preamble_digests(body: &[u8]) -> Option<(u64, u64)> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let digest = |v: Option<&serde_json::Value>| {
        let mut hasher = DefaultHasher::new();
        match v {
            Some(v) => {
                cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(v)
                    .to_string()
                    .hash(&mut hasher);
            }
            None => 0u8.hash(&mut hasher),
        }
        hasher.finish()
    };
    Some((digest(parsed.get("system")), digest(parsed.get("tools"))))
}

pub(super) fn message_digests(body: &[u8]) -> Option<Vec<u64>> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = parsed.get("messages")?.as_array()?;
    Some(
        messages
            .iter()
            .map(|m| {
                let mut hasher = DefaultHasher::new();
                // The same projection prefix replay compares on, not the raw
                // bytes. `maybe_push_tail_breakpoint` moves the cache_control
                // marker after the replay stage on nearly every turn, and the
                // provider's prefix key ignores it — hashing it raw reports a
                // mutation on almost every request and hides real ones.
                cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(m)
                    .to_string()
                    .hash(&mut hasher);
                hasher.finish()
            })
            .collect(),
    )
}

/// First 12 hex chars of the SHA-256 of `value`. Enough to tell two prefixes
/// apart in a log without carrying their bytes.
pub(super) fn short_hash(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)[..12].to_string()
}

/// Log which parts of the cacheable prefix this request carries.
///
/// A fan-out of subagents shares a provider cache entry only where their
/// leading bytes are identical. Measured 2026-08-13: of 14 subagent
/// conversations, 5 shared a 43,603-token prefix and the other 9 each read a
/// slightly different floor, so eight cache entries were built where one would
/// have done. Sizes alone cannot say which component differs, so hash `system`
/// and `tools` separately — two requests whose `tools_fingerprint` matches but
/// whose `system_fingerprint` does not are diverging in the preamble, and vice
/// versa. `tool_names_fingerprint` isolates the common case further: the same
/// tools in a different ORDER hash differently there but identically by name
/// set, which names ordering as the culprit without a capture.
/// Last tool roster forwarded on each session.
///
/// The composition line fingerprints the tool names, which says *that* the
/// array moved but never *what* moved — and a tool arriving or leaving
/// invalidates the whole cached prefix behind it, since tools sit at the
/// front of the cache key. Measured over 09-02, five such turns cost 961k
/// tokens between them, every one of them a tool the client dropped. Naming
/// it is the difference between knowing a tool churns and being able to prune
/// it.
pub(super) fn tool_rosters() -> &'static Mutex<std::collections::HashMap<String, Vec<String>>> {
    static ROSTERS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Vec<String>>>> =
        std::sync::OnceLock::new();
    ROSTERS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Sessions tracked before the map starts forgetting. One entry is a session
/// key and a list of tool names; a few hundred of those is nothing, and the
/// cap only has to stop an unbounded process from growing one.
pub(super) const TOOL_ROSTER_CAPACITY: usize = 512;

/// Log which tools joined or left this session's array since the last turn.
///
/// Silent on the first turn of a session: there is nothing to compare against,
/// and "every tool appeared" is not news.
pub(super) fn note_tool_roster(session_key: &str, request_id: &str, names: &[&str]) {
    if session_key.is_empty() {
        return;
    }
    let current: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    let Ok(mut rosters) = tool_rosters().lock() else {
        return;
    };
    let previous = rosters.insert(session_key.to_string(), current.clone());
    if rosters.len() > TOOL_ROSTER_CAPACITY {
        // Whichever the map hands over first. Losing a baseline costs one
        // missed comparison, not a wrong one.
        if let Some(victim) = rosters
            .keys()
            .find(|key| key.as_str() != session_key)
            .cloned()
        {
            rosters.remove(&victim);
        }
    }
    drop(rosters);

    let Some(previous) = previous else {
        return;
    };
    if previous == current {
        return;
    }
    let added: Vec<&str> = current
        .iter()
        .filter(|name| !previous.contains(name))
        .map(String::as_str)
        .collect();
    let removed: Vec<&str> = previous
        .iter()
        .filter(|name| !current.contains(name))
        .map(String::as_str)
        .collect();
    if added.is_empty() && removed.is_empty() {
        // Same set, different order. Worth its own reading: order is part of
        // the cache key too, and `cache_stable_tool_order` exists to hold it.
        tracing::info!(
            target: "headroom.proxy",
            event = "tool_roster_reordered",
            request_id = %request_id,
            tool_count = current.len(),
            "the tools array kept its members and changed their order"
        );
        return;
    }
    tracing::warn!(
        target: "headroom.proxy",
        event = "tool_roster_changed",
        request_id = %request_id,
        added = %added.join(","),
        removed = %removed.join(","),
        count_before = previous.len(),
        count_after = current.len(),
        "the forwarded tools array changed; the cached prefix behind it is dead"
    );
}

pub(super) fn log_prefix_composition(request_id: &str, session_key: &str, body: &[u8]) {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    // Read the model off the body rather than the caller: the cache is keyed
    // per model, so an opus and a sonnet request with identical prefixes still
    // build separate entries, and the fingerprints only mean anything when
    // compared within one model.
    let model = parsed.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let part = |value: Option<&serde_json::Value>| -> (String, usize) {
        match value {
            Some(value) => {
                let text = value.to_string();
                (short_hash(&text), text.len())
            }
            None => ("absent".to_string(), 0),
        }
    };
    let (system_fingerprint, system_bytes) = part(parsed.get("system"));
    let (tools_fingerprint, tools_bytes) = part(parsed.get("tools"));
    let names: Vec<&str> = parsed
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    tracing::info!(
        target: "headroom.proxy",
        event = "prefix_composition",
        request_id = %request_id,
        model = %model,
        system_fingerprint = %system_fingerprint,
        system_bytes = system_bytes,
        tools_fingerprint = %tools_fingerprint,
        tools_bytes = tools_bytes,
        tool_names_fingerprint = %short_hash(&names.join(",")),
        tool_names_sorted_fingerprint = %short_hash(&sorted.join(",")),
        tool_count = names.len(),
        "cacheable prefix composition"
    );
    note_tool_roster(session_key, request_id, &names);
}

/// Anthropic's cache-write TTL split, as `(5m, 1h)`.
///
/// The flat `usage.cache_creation_input_tokens` the buffered path reads is a
/// total that says nothing about which TTL was billed, and the two differ:
/// a 5-minute write costs 1.25x input, a 1-hour write 2.0x. The breakdown sits
/// in a nested object, so pricing has to read it rather than assume the TTL the
/// proxy asked for. Mirrors the streaming parser in `sse::anthropic`.
///
/// Returns `(0, 0)` for any other provider — no one else publishes the field.
pub(super) fn anthropic_cache_ttl_split(usage: Option<&serde_json::Value>) -> (i64, i64) {
    let Some(cc) = usage.and_then(|u| u.get("cache_creation")) else {
        return (0, 0);
    };
    let get = |key: &str| cc.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    (
        get("ephemeral_5m_input_tokens"),
        get("ephemeral_1h_input_tokens"),
    )
}

/// Freeze-replay request stage (Anthropic `/v1/messages` buffered path).
///
/// Rust port of the Python handler's overlay call site
/// (`headroom/proxy/handlers/anthropic.py`, around the
/// `overlay_cached_prefix` / `normalize_message_cache_control` pair):
///
/// 1. Overlay the previously-forwarded prefix byte-identical onto this
///    turn's dispatcher output when this turn append-only-extends the
///    previous one (#1850). Idempotent and append-only-guarded, so it
///    is safe to run unconditionally on the flagged path.
/// 2. Own message-level `cache_control` placement (#1852): the client
///    moves its breakpoint every turn and the overlay replays past
///    markers, so without normalization they accumulate ~1/turn and
///    Anthropic hard-errors at >4. Applied last so the forwarded AND
///    recorded (next turn's replay source) messages stay bounded.
/// 3. Park `(original, forwarded)` under `request_id` so the SSE
///    completion side can feed the response's cache tokens back via
///    [`SessionReplayStore::complete`].
///
/// The body is re-serialized only when the replay actually changed the
/// messages; otherwise the dispatcher's bytes forward untouched.
/// (`serde_json` runs with `preserve_order`, so a re-serialization
/// keeps key order — the same property Python gets from `dict`.)
///
/// Visible crate-wide so the routed-model translate path replays its prefix
/// through this exact code rather than a parallel implementation — the whole
/// value of the stage is that the replayed bytes are byte-identical, which a
/// second implementation would be one refactor away from breaking.
/// Cold-prefix fork decision output.
pub(crate) struct ColdFork {
    pub messages: Vec<serde_json::Value>,
    pub transforms: Vec<String>,
    pub ttl_desc: String,
    pub idle_secs: f64,
}

/// Cold-prefix fork (port of upstream `HEADROOM_COLD_RECOMPACT`).
///
/// When the lane has been idle past the request's real cache TTL, the
/// provider cache is dead and the byte-identical splice preserves nothing.
/// Returns `Some` only when the operator opted in AND the turn is cold;
/// warm lanes (and the default-off flag) return `None` so the caller runs
/// the normal replay with byte-identical behavior.
///
/// The caller skips the overlay replay on `Some` (both modes — a dead
/// splice helps nothing); `messages` already carries the lossless
/// whole-prefix recompaction in cache mode, and the untouched originals in
/// token mode (whose recompression already ran with no frozen prefix to
/// preserve).
pub(crate) fn maybe_cold_fork(
    enabled: bool,
    cache_mode: bool,
    model: &str,
    messages: &[serde_json::Value],
    system: Option<&serde_json::Value>,
    idle_secs: Option<f64>,
    ccr_store: Option<std::sync::Arc<dyn headroom_core::ccr::CcrStore>>,
) -> Option<ColdFork> {
    use headroom_core::transforms::cold_prefix as cp;
    if !enabled {
        return None;
    }
    // Authoritative request-level tier (not a guess): the same value that
    // would drive net-cost pricing. `None` = caching disabled = nothing to
    // bust = every turn recompactable.
    let ttl = cp::anthropic_cache_ttl_seconds(model, messages, system);
    if !cp::should_cold_recompact(idle_secs, ttl) {
        return None;
    }
    let idle = idle_secs.unwrap_or(0.0);
    // Cache mode: lossless whole-prefix recompaction, then the Spark
    // reasoning-summary strip (unsigned advisory text only — signed
    // thinking and redacted blocks are untouchable). Token mode skips the
    // dead splice without rewriting: its recompression already ran with no
    // frozen prefix to preserve.
    let (messages, mut transforms) = if cache_mode {
        cp::cold_recompact_messages(messages, ccr_store)
    } else {
        (messages.to_vec(), Vec::new())
    };
    let (messages, transforms) = if cache_mode {
        let (stripped, n) = cp::strip_spark_reasoning_summaries(messages);
        if n > 0 {
            transforms.push(format!("cold:spark_summary:{n}blocks"));
        }
        (stripped, transforms)
    } else {
        (messages, transforms)
    };
    Some(ColdFork {
        messages,
        transforms,
        ttl_desc: ttl
            .map(|t| format!("{t}s"))
            .unwrap_or_else(|| "disabled".to_string()),
        idle_secs: idle,
    })
}

/// Parse the post-dispatch body for prefix replay: the JSON value plus its
/// `messages` array. Returns `None` (forwarding the body unchanged) when
/// the body is not JSON or has no messages array.
/// Extracted from `apply_prefix_replay` without behavior change.
pub(super) fn parse_replay_body(
    body: &bytes::Bytes,
    request_id: &str,
) -> Option<(serde_json::Value, Vec<serde_json::Value>)> {
    let parsed: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "prefix_replay_skipped",
                request_id = %request_id,
                error = %e,
                "prefix replay: post-dispatch body is not JSON; forwarding unchanged"
            );
            return None;
        }
    };
    let Some(optimized) = parsed.get("messages").and_then(|m| m.as_array()).cloned() else {
        tracing::debug!(
            event = "prefix_replay_skipped",
            request_id = %request_id,
            reason = "no_messages_array",
            "prefix replay: post-dispatch body has no messages array; forwarding unchanged"
        );
        return None;
    };
    Some((parsed, optimized))
}

/// Log a replay decline with the full divergence diagnosis: which reason,
/// where it first disagreed, the block shapes and text kinds on each side,
/// and how much was salvaged anyway. Also tells the observer, so a
/// re-cache event a turn later can name the cause instead of falling
/// through to "unattributable".
/// Extracted from `apply_prefix_replay` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(super) fn log_replay_decline(
    reason: crate::cache_stabilization::prefix_replay::ReplaySkip,
    prefix_miss: Option<crate::cache_stabilization::prefix_replay::PrefixMiss>,
    prev_orig: Option<&[serde_json::Value]>,
    original_messages: &[serde_json::Value],
    optimized: &[serde_json::Value],
    chain_id: u64,
    session_key: &str,
    request_id: &str,
    observer: Option<&crate::cache_stabilization::usage_observer::UsageObserver>,
    uptime_seconds: u64,
) {
    use crate::cache_stabilization::prefix_replay::ReplaySkip;

    // A turn that does not replay is where the money goes: measured over the
    // 2026-08-08/09 logs, non-replaying turns were 19% of traffic and carried
    // 97% of booked re-cache waste. `replayed_prefix` alone cannot say which of
    // five reasons applied, and they need opposite responses — a diverged
    // client prefix is not our doing, while a turn shorter than the stored
    // prefix means two streams are sharing one session slot.
    // The two heads below share one canonical comparison, so `first_diff_path`
    // and the text it names cannot disagree. Computed here rather than inline
    // in the event for that reason, and only on a decline — a replaying turn
    // never pays for it.
    let (diff_stored_text_head, diff_current_text_head) = match (Some(reason), prev_orig) {
        (
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index, ..
            }),
            Some(prev),
        ) => crate::cache_stabilization::prefix_replay::divergence_text_heads(
            prev,
            original_messages,
            first_diff_index,
        )
        .unwrap_or_default(),
        _ => (String::new(), String::new()),
    };
    if let Some(observer) = observer {
        observer.note_replay_skip(
            request_id,
            crate::cache_stabilization::usage_observer::ReplaySkipEvidence::from_inbound_original_histories(
                reason,
                prev_orig,
                original_messages,
            ),
        );
    }
    tracing::info!(
        event = "prefix_replay_not_replayed",
        request_id = %request_id,
        // Correlate replay declines with the drift and volatile-content
        // events emitted for the same logical session.  The raw session
        // key can contain an authorization credential or caller-supplied
        // identifier, so this is deliberately the existing 16-hex
        // SHA-256 log prefix rather than the key itself.
        session_key_hash = %crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key),
        reason = reason.as_str(),
        // Only set when `reason` is `no_previous_turn`, and the part that
        // makes it actionable: a first turn is free, an idle gap is already
        // lost, a missing tracker on a live session is a defect.
        miss_detail = prefix_miss.map(|m| m.as_str()).unwrap_or(""),
        proxy_uptime_seconds = uptime_seconds,
        // Which leading message first disagreed. A conversation that
        // declines every turn while growing normally is not being edited by
        // its client — something inside it churns per request and the
        // canonicalizer is not neutralising it. This names where.
        first_diff_index = match Some(reason) {
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index,
                    ..
            }) => first_diff_index as i64,
            _ => -1,
        },
        // The field that churned, named by structure alone — keys and
        // indices, never a value. One sample of `content[0].text` on the
        // opener says "an injected block changes per request, and we can
        // neutralise it"; `content[3].input` says a real edit. Without it
        // the index alone needs a distribution to say anything.
        first_diff_path = match (Some(reason), prev_orig) {
            (
                Some(ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                    ..
                }),
                Some(prev),
            ) => crate::cache_stabilization::prefix_replay::describe_divergence(
                prev,
                original_messages,
                first_diff_index,
            )
            .unwrap_or_default(),
            _ => String::new(),
        },
        // How much of the stored prefix was replayed anyway. A decline no
        // longer forwards this turn's own bytes for the whole prefix: the
        // run that still agrees comes from the stored copy, so the provider
        // keeps reading it instead of missing at message 0. Zero here means
        // the divergence was at the very first message and nothing could be
        // salvaged.
        replayed_prefix_msgs = match Some(reason) {
            Some(ReplaySkip::PrefixContentDiverged {
                replayed_prefix_msgs,
                ..
            }) => replayed_prefix_msgs as i64,
            _ => -1,
        },
        // Which block kinds sat on each side of that difference. A
        // `tool_result` that vanished points at something collapsing tool
        // output in front of the client; an ordinary text change points at
        // a real edit. Type names only, never block contents.
        diff_shape_stored = match (Some(reason), prev_orig) {
            (
                Some(ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                    ..
                }),
                Some(prev),
            ) => prev
                .get(first_diff_index)
                .map(crate::cache_stabilization::prefix_replay::block_type_shape)
                .unwrap_or_default(),
            _ => String::new(),
        },
        diff_shape_current = match Some(reason) {
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index,
                    ..
            }) => original_messages
                .get(first_diff_index)
                .map(crate::cache_stabilization::prefix_replay::block_type_shape)
                .unwrap_or_default(),
            _ => String::new(),
        },
        // What the text blocks on each side WERE. The shapes above say a
        // `text` block came or went; these say whether it was the client's
        // own ephemeral scaffolding or real content, which is the
        // difference between churn we could neutralise and an edit we must
        // respect. Closed vocabulary, never the text.
        diff_text_kinds_stored = match (Some(reason), prev_orig) {
            (
                Some(ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                    ..
                }),
                Some(prev),
            ) => prev
                .get(first_diff_index)
                .map(crate::cache_stabilization::prefix_replay::text_block_kinds)
                .unwrap_or_default(),
            _ => String::new(),
        },
        diff_text_kinds_current = match Some(reason) {
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index,
                    ..
            }) => original_messages
                .get(first_diff_index)
                .map(crate::cache_stabilization::prefix_replay::text_block_kinds)
                .unwrap_or_default(),
            _ => String::new(),
        },
        // What the differing text actually SAYS, on each side. The path
        // above names where a mismatch is and the kinds name what sort of
        // block held it, but neither shows the characters, so the
        // 2026-08-13 investigation had to infer trailing whitespace from
        // message shapes when a hundred characters of each string would
        // have shown it outright. The one deliberate exception to logging
        // values: head only, escaped, canonical form, and only for the
        // message the path already names.
        diff_stored_text_head = %diff_stored_text_head,
        diff_current_text_head = %diff_current_text_head,
        stored_prefix_msgs = prev_orig.map(|o| o.len()).unwrap_or(0),
        current_original_msgs = original_messages.len(),
        optimized_msgs = optimized.len(),
        // Which run of turns this one continues, `0` for none. Grouping by
        // conversation key or by message count cannot separate a branch, a
        // compaction and a second stream — three wrong conclusions came
        // from that on 2026-08-09. This can.
        chain_id = chain_id,
        "prefix replay declined: forwarding this turn's own bytes"
    );
}

/// Place tail + scaffold + system cache breakpoints within the provider
/// budget. Returns the normalized messages, breakpoints placed, and system
/// markers trimmed. Anthropic counts `cache_control` across `system`,
/// `tools` and `messages` together and refuses the whole request past 4.
/// `system` yields its second marker before the message tail yields either
/// of its two, and the scaffolding marker yields before the tail as well;
/// `tools` is PR-E3's and is never touched.
/// Extracted from `apply_prefix_replay` without behavior change.
pub(super) fn place_replay_breakpoints(
    parsed: &mut serde_json::Value,
    overlaid: Vec<serde_json::Value>,
    tail_breakpoints: usize,
    request_id: &str,
) -> (Vec<serde_json::Value>, usize, usize) {
    use crate::cache_stabilization::prefix_replay::{
        ANTHROPIC_CACHE_CONTROL_LIMIT, message_slots_within_budget, place_tail_cache_breakpoints,
        trim_system_breakpoints_to_budget,
    };

    // Anthropic counts `cache_control` across `system`, `tools` and `messages`
    // together and refuses the whole request past 4. `system` yields its second
    // marker before the message tail yields either of its two, and the
    // scaffolding marker yields before the tail as well; `tools` is PR-E3's and
    // is never touched.
    let slots = message_slots_within_budget(parsed, &overlaid, tail_breakpoints);
    if slots.tail < tail_breakpoints {
        tracing::warn!(
            event = "cache_marker_budget_clamped",
            request_id = %request_id,
            requested = tail_breakpoints,
            allowed = slots.tail,
            reserved_by_system_and_tools = slots.reserved,
            limit = ANTHROPIC_CACHE_CONTROL_LIMIT,
            "cache_control budget: placing fewer message breakpoints than asked \
             to keep the request under the provider's limit"
        );
    }
    let (normalized, breakpoints_placed) =
        place_tail_cache_breakpoints(overlaid, slots.tail, slots.scaffold);
    // Pay for the scaffolding marker out of `system`, and do it from what is
    // really on the body rather than from what was planned, so a plan that did
    // not come off cannot leave the request over the limit.
    let system_markers_trimmed = trim_system_breakpoints_to_budget(parsed, breakpoints_placed);
    if system_markers_trimmed > 0 {
        tracing::debug!(
            event = "system_marker_yielded",
            request_id = %request_id,
            dropped = system_markers_trimmed,
            "gave up a system breakpoint so the message tail keeps both of its own"
        );
    }
    (normalized, breakpoints_placed, system_markers_trimmed)
}

/// Report which messages this proxy altered before forwarding, singling
/// out altered ones carrying thinking blocks (a rejection at those indices
/// means the proxy caused it) plus signed blocks altered on the wire
/// (refused outright — always a defect). Indices and counts only.
/// Anthropic refuses a turn whose signed `thinking` blocks changed, naming a
/// message index but not who changed it. Both the client and this proxy
/// rewrite history, so a rejection is unattributable without knowing which
/// messages WE altered.
/// Extracted from `apply_prefix_replay` without behavior change.
pub(super) fn note_rewritten_messages(
    original_messages: &[serde_json::Value],
    normalized: &[serde_json::Value],
    request_id: &str,
) {
    // Anthropic refuses a turn whose signed `thinking` blocks changed, naming a
    // message index but not who changed it. Both the client and this proxy
    // rewrite history, so a rejection is unattributable without knowing which
    // messages WE altered. Report that, and single out the altered ones that
    // carry a thinking block — if a rejection's index appears here, the proxy
    // caused it. Indices and counts only.
    let rewritten = rewritten_message_report(original_messages, normalized);
    if !rewritten.indices.is_empty() || !rewritten.thinking_touched.is_empty() {
        tracing::info!(
            event = "messages_rewritten",
            request_id = %request_id,
            rewritten_count = rewritten.indices.len(),
            rewritten_indices = %join_indices(&rewritten.indices),
            // The ones that can be refused. Empty here means a thinking-block
            // rejection came from the client's own edits, not ours.
            rewritten_with_thinking_count = rewritten.with_thinking.len(),
            rewritten_with_thinking_indices = %join_indices(&rewritten.with_thinking),
            // Signed blocks altered on the wire, `cache_control` included. The
            // provider refuses these outright, so a non-empty list is a defect
            // regardless of how much it saves.
            thinking_touched_count = rewritten.thinking_touched.len(),
            thinking_touched_indices = %join_indices(&rewritten.thinking_touched),
            total_messages = original_messages.len(),
            // The bytes of the earliest messages exactly as forwarded, so two
            // consecutive turns can be diffed to name the first one that moved.
            // The drift detector cannot answer this: it filters ephemeral blocks
            // before comparing and the provider does not, so it calls a prefix
            // stable while the provider re-creates it.
            early_fingerprints = %crate::cache_stabilization::prefix_replay::early_message_fingerprints(normalized, 5),
            "messages this proxy altered before forwarding"
        );
    }
}

/// Serialize the replayed body back onto `parsed["messages"]`, booking the
/// applied replay with the observer and logging the placement. On
/// serialization failure forwards the pre-replay body (recording what was
/// ACTUALLY forwarded — the store must mirror the wire — never the
/// messages that failed to serialize).
/// Extracted from `apply_prefix_replay` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(super) fn serialize_replayed_body(
    changed: bool,
    parsed: &mut serde_json::Value,
    normalized: Vec<serde_json::Value>,
    optimized: Vec<serde_json::Value>,
    body: bytes::Bytes,
    replayed_prefix: bool,
    observer: Option<&crate::cache_stabilization::usage_observer::UsageObserver>,
    chain_id: u64,
    breakpoints_placed: usize,
    system_markers_dropped: usize,
    request_id: &str,
) -> (bytes::Bytes, Vec<serde_json::Value>) {
    if !changed {
        return (body, optimized);
    }
    parsed["messages"] = serde_json::Value::Array(normalized.clone());
    match serde_json::to_vec(parsed) {
        Ok(b) => {
            if replayed_prefix && let Some(observer) = observer {
                observer.note_replay_applied(
                    request_id,
                    crate::cache_stabilization::usage_observer::ReplayAppliedEvidence::new(
                        chain_id,
                        breakpoints_placed,
                        system_markers_dropped,
                    ),
                );
            }
            tracing::info!(
                event = "prefix_replay_applied",
                request_id = %request_id,
                replayed_prefix = replayed_prefix,
                chain_id = chain_id,
                // What went out on the wire, so a run can be attributed to
                // its placement rather than to the flag it was started with.
                breakpoints_placed = breakpoints_placed,
                system_markers_dropped = system_markers_dropped,
                "prefix replay: forwarded messages rewritten \
                 (prefix replay and/or cache_control normalization)"
            );
            (bytes::Bytes::from(b), normalized)
        }
        Err(e) => {
            // Record what we ACTUALLY forward (the pre-replay
            // bytes), never the messages we failed to serialize —
            // the store must mirror the wire.
            tracing::warn!(
                event = "prefix_replay_serialize_failed",
                request_id = %request_id,
                error = %e,
                "prefix replay: re-serialization failed; forwarding pre-replay body"
            );
            (body, optimized)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_prefix_replay(
    store: &SessionReplayStore,
    session_key: &str,
    request_id: &str,
    original_messages: Vec<serde_json::Value>,
    body: bytes::Bytes,
    // Told when the replay is declined, so a re-cache event a turn later can
    // name the cause instead of falling through to "unattributable".
    observer: Option<&cache_stabilization::usage_observer::UsageObserver>,
    // Seconds since this process started, so an empty store right after a
    // restart is not read as an unstable session key.
    uptime_seconds: u64,
    // How many tail breakpoints to place, and whether the client's `system`
    // markers may go. Both come from flags so the pair can be measured against
    // the single-marker placement it replaces.
    tail_breakpoints: usize,
    strip_system_breakpoints: bool,
) -> bytes::Bytes {
    use cache_stabilization::prefix_replay::{
        overlay_cached_prefix_reported, strip_system_cache_control,
    };

    let Some((mut parsed, optimized)) = parse_replay_body(&body, request_id) else {
        return body;
    };

    // Ask for the prefix THIS turn continues, not merely the session's last
    // one: several streams share a session key, and handing back another
    // stream's prefix guarantees the append-only guard rejects it and the turn
    // forwards fresh bytes over content the provider had cached. The system
    // digest gates the answer on the prefix lineage the provider actually
    // holds: replaying stored messages under a different system is a miss
    // that reports as a replay.
    let current_system_hash =
        cache_stabilization::prefix_replay::forwarded_system_digest(parsed.get("system"));
    let (prev_orig, prev_fwd, prefix_miss, chain_id) = match store.previous_turn_for(
        session_key,
        &original_messages,
        Some(&current_system_hash),
    ) {
        Ok((o, f, chain_id)) => (Some(o), Some(f), None, chain_id),
        Err(miss) => (None, None, Some(miss), 0),
    };
    let (overlaid, skip_reason) = overlay_cached_prefix_reported(
        optimized.clone(),
        &original_messages,
        prev_orig.as_deref(),
        prev_fwd.as_deref(),
        // Only splice a diverged prefix when this turn genuinely continues the
        // stored chain. A zero id means the store fell back to the session's
        // most recent prefix, which belongs to some other stream.
        chain_id != 0,
        // Provider-confirmed floor (port of upstream `aebe9895`): the leading
        // messages the provider confirmed cached replay unconditionally, so a
        // background recompression landing a smaller form of already-forwarded
        // history cannot bust the warm cache; beyond the floor the
        // non-inflation bound still lets improvements through, and a cold
        // cache (count 0) lets every accumulated improvement land at once.
        Some(store.confirmed_frozen_count(session_key)),
    );
    let replayed_prefix = overlaid != optimized;
    if let Some(reason) = skip_reason {
        log_replay_decline(
            reason,
            prefix_miss,
            prev_orig.as_deref(),
            &original_messages,
            &optimized,
            chain_id,
            session_key,
            request_id,
            observer,
            uptime_seconds,
        );
    }
    let (normalized, breakpoints_placed, system_markers_trimmed) =
        place_replay_breakpoints(&mut parsed, overlaid, tail_breakpoints, request_id);
    note_rewritten_messages(&original_messages, &normalized, request_id);
    // Only once a message breakpoint is in place. With none placed the client's
    // system markers are the only ones on the request, and dropping them would
    // turn caching off rather than move it.
    let system_markers_dropped = if strip_system_breakpoints && breakpoints_placed > 0 {
        strip_system_cache_control(&mut parsed)
    } else {
        0
    };
    let changed =
        normalized != optimized || system_markers_dropped > 0 || system_markers_trimmed > 0;

    let (final_body, forwarded_messages) = serialize_replayed_body(
        changed,
        &mut parsed,
        normalized,
        optimized,
        body,
        replayed_prefix,
        observer,
        chain_id,
        breakpoints_placed,
        system_markers_dropped,
        request_id,
    );

    // A side errand shares this conversation's session key but is not a step in
    // it. Parking it would make it the session's "previous turn", and the next
    // real turn would diverge at the final message and recache from there.
    if cache_stabilization::prefix_replay::is_side_errand(&original_messages) {
        tracing::info!(
            event = "prefix_replay_side_errand_not_parked",
            request_id = %request_id,
            messages = original_messages.len(),
            "prefix replay: side errand left out of the store"
        );
        return final_body;
    }

    store.begin_request(
        request_id,
        session_key,
        original_messages,
        forwarded_messages,
        cache_stabilization::prefix_replay::forwarded_system_digest(parsed.get("system")),
    );
    final_body
}

#[cfg(test)]
mod cold_fork_tests {
    use super::*;

    fn msgs() -> Vec<serde_json::Value> {
        vec![serde_json::json!({"role": "user", "content": "hi"})]
    }

    #[test]
    fn disabled_flag_never_forks() {
        assert!(
            maybe_cold_fork(
                false,
                true,
                "claude-opus-5",
                &msgs(),
                None,
                Some(99999.0),
                None
            )
            .is_none()
        );
    }

    #[test]
    fn unknown_session_is_warm() {
        // No idle reading (first turn): conservative warm, replay untouched.
        assert!(maybe_cold_fork(true, true, "claude-opus-5", &msgs(), None, None, None).is_none());
    }

    #[test]
    fn warm_lane_is_untouched() {
        assert!(
            maybe_cold_fork(true, true, "claude-opus-5", &msgs(), None, Some(10.0), None).is_none()
        );
    }

    #[test]
    fn cold_lane_recompacts_in_cache_mode() {
        let fork = maybe_cold_fork(
            true,
            true,
            "claude-opus-5",
            &msgs(),
            None,
            Some(99999.0),
            None,
        )
        .expect("idle-past-TTL lane must fork");
        assert_eq!(fork.ttl_desc, "300s");
        // Trivial messages: lossless passes fold nothing, input survives.
        assert_eq!(fork.messages, msgs());
    }
}

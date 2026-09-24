//! Freeze-replay / prefix-cache tracker — byte-identical replay of the
//! previously-forwarded (compressed) prefix so provider prompt caches stay warm.
//!
//! # Why this exists
//!
//! Clients like Claude Code already manage prompt caching (Anthropic: up to 4
//! `cache_control` breakpoints on a growing prefix). When Headroom compresses a
//! message that sits inside the cached prefix it changes the bytes the provider
//! hashed for its cache key — replacing a 90% read discount with a 25% write
//! penalty for the whole suffix. Worse, on the *next* turn the freeze path emits
//! the agent's ORIGINAL bytes for a message the provider had cached in its
//! COMPRESSED form last turn; forwarding original then mismatches the cached
//! prefix and busts it from that point (a real SWE-bench run attributed 100% of
//! cache misses to this `prefix_change`, ~56% of all cache-writes bust-induced).
//!
//! # What it does
//!
//! This is the Rust port of `headroom/cache/prefix_tracker.py`. It records, per
//! session, the exact messages we FORWARDED last turn (their compressed bytes)
//! and, on the next turn, replays them byte-for-byte in place of whatever the
//! compression pipeline just produced for the same leading positions. Only the
//! newly-appended suffix (the "delta") is left as the fresh compressor output.
//! The forwarded prefix therefore stays byte-identical turn-over-turn and the
//! prompt cache keeps hitting.
//!
//! # Design (mirrors the drift detector's per-session store)
//!
//! - [`SessionReplayStore`] holds one [`PrefixReplayTracker`] per session in a
//!   1000-entry LRU (same capacity + `Arc<Mutex<LruCache>>` shape as
//!   [`crate::cache_stabilization::drift_detector::DriftState`]), keyed by the
//!   shared [`derive_session_key`](crate::cache_stabilization::drift_detector::derive_session_key).
//!   It also holds a small LRU of *pending turns* keyed by `request_id` so the
//!   response-side usage observer can feed cache-token counts back into the
//!   tracker once the stream completes (the request→response correlation the
//!   Python handler did inline).
//! - [`overlay_cached_prefix`] is the request-side replay: given this turn's
//!   optimized (compressed) messages plus the previous turn's original and
//!   forwarded messages, it replays the previously-forwarded prefix byte-identical
//!   when this turn append-only-extends the previous one, else returns the
//!   optimized messages unchanged (accept a possible bust over forwarding wrong
//!   content).
//! - [`extract_cache_stable_delta`] is the cache-mode sibling: returns
//!   `(previously_forwarded_prefix, appended_delta)` so the caller can compress
//!   ONLY the delta.
//! - [`normalize_message_cache_control`] keeps message-level `cache_control`
//!   breakpoints bounded (Anthropic hard-errors at >4) and stable across the
//!   replay so the overlay itself never busts.
//!
//! # How the three spec commits are honored
//!
//! - **#1850** (freeze must forward the cached/compressed prefix byte-identical):
//!   [`overlay_cached_prefix`] replays `previous_forwarded` verbatim, append-only
//!   guarded and idempotent, in place of the compressor's per-position output.
//! - **#1852** (keep `cache_control` bounded + stable): the append-only guard
//!   runs on **content only** — [`canonicalize_for_prefix_compare`] strips
//!   `cache_control` and other transport noise before comparing — and
//!   [`normalize_message_cache_control`] strips every message-level marker and
//!   re-places exactly one ephemeral breakpoint on the last block so replayed
//!   markers cannot accumulate past Anthropic's limit.
//! - **#1868** (provider-agnostic delta + cc-agnostic prefix comparison): the
//!   comparison key is the shared [`canonicalize_for_prefix_compare`] projection
//!   — representation-agnostic (string↔block content sugar), transport/annotation
//!   agnostic (`_NON_SEMANTIC_KEYS`), and opaque over user tool payloads
//!   (`_OPAQUE_PAYLOAD_KEYS`) — never a source to rebuild forwarded bytes.
//!
//! # Interaction with the drift detector and the J4 offload gate
//!
//! A drift/rebuild boundary (the drift detector saw the cache hot zone change)
//! means the prefix the provider had cached is gone, so the stored
//! previously-forwarded prefix is stale. [`PrefixReplayTracker::invalidate`] is
//! called on a rebuild boundary to drop the stored prefix; the next turn then
//! starts a fresh replay chain. This is deliberately the same boundary the J4
//! offload gate uses to allow frozen-history conversion, so the two stay
//! consistent: on a boundary the prefix is rebuilt, on a steady-state turn it is
//! replayed byte-identical.
//!
//! Gated behind `config.prefix_replay` (default off) so it is a safe rollout.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ephemeral_spans::{
    SYSTEM_REMINDER_OPEN_TAG, block_carries_ephemeral_span, is_client_scaffolding_message,
    is_ephemeral_client_block, is_ephemeral_client_text, split_ephemeral_spans,
    split_trailing_ephemeral_spans, take_trailing_ephemeral_spans,
};
use lru::LruCache;
use serde_json::Value;

/// Production session capacity — matches the drift detector's 1000.
pub const REPLAY_STORE_CAPACITY: usize = 1000;

/// Pending-turn correlation capacity. In-flight requests awaiting their
/// response usage; bounded well above realistic concurrency.
pub const PENDING_CAPACITY: usize = 4096;

/// Minimum cached tokens before the tracker considers a prefix worth freezing,
/// mirroring `PrefixFreezeConfig.min_cached_tokens` in Python.
const MIN_CACHED_TOKENS: u64 = 1024;

/// Hard ceiling on prefixes kept per session beyond the most recent one.
///
/// Sized against real fan-out, not a guess: a `capture-beta` run puts 8 subagents
/// on one session, and each needs its own prefix or it busts the cache on every
/// turn it takes. The count alone is not the real bound — see
/// [`MAX_ALTERNATE_MESSAGES`].
///
/// Was 16, which made this the *only* bound that ever bit. Measured
/// 2026-08-23: a main conversation is dropped after exactly 17 subagent turns
/// while the store holds 480 messages against a 4,000 budget — 12% of the
/// bound that was supposed to govern. Eviction takes the least-recently-
/// displaced entry, and a main conversation waiting on a fan-out is precisely
/// that, so the largest and most expensive prefix in the store was the first
/// one thrown away. In the 2026-08-20/22 logs, 51 turns found no stream leading
/// them and re-cached 2.59M tokens, 5.6% of all cache creation, ~50k a turn.
///
/// This is a memory backstop, not a policy: it exists only to bound per-entry
/// `Vec` overhead across a 1000-session store, and it must never be the thing
/// that decides which stream survives. That job belongs to
/// [`MAX_ALTERNATE_MESSAGES`], which bounds the actual weight — the count does
/// not change how much is held.
///
/// Sized so it cannot bind in practice rather than picked round. Distinct
/// streams per session over the same logs: median 0, p90 4, p95 6, p99 11, max
/// 29; two sessions of 344 (0.58%) went past 16 and none past 32. 128 is 4.4x
/// the observed maximum. Above roughly 40 streams the message budget takes over
/// on any realistic prefix length (128 streams of 30 messages is 3,840, already
/// at the 4,000 budget), so the backstop stays out of the way by construction.
///
/// If a session ever does hit this ceiling, that is worth knowing rather than
/// absorbing: `prefix_replay_no_stream_leads_turn` prints both bounds so the
/// binding one can be read straight off the event.
const MAX_ALTERNATE_PREFIXES: usize = 128;

/// The bound that actually matters: total messages held across a session's
/// alternate prefixes.
///
/// A count-only cap prices a 20-message subagent the same as a 500-message
/// main conversation, though they differ by more than an order of magnitude in
/// what they cost to hold — and each entry keeps original *and* forwarded
/// arrays, across a 1000-session store. Budgeting messages lets the common
/// case (many short subagent streams) keep all of them while a few very long
/// conversations are held to a couple, which is the right trade in both
/// directions.
///
/// Sized so the count ceiling is what governs the fan-out case rather than
/// this: ten subagents holding 400 messages each fit, which is a long run for
/// a subagent. A budget that trimmed them would silently reintroduce the bust
/// this store exists to prevent, and it would do so on exactly the busiest
/// sessions.
///
/// Counted in messages rather than bytes deliberately: sizing the values would
/// mean serialising them, and this runs while the request is in flight.
const MAX_ALTERNATE_MESSAGES: usize = 4_000;

mod breakpoints;
mod compare;
mod divergence;
mod overlay;
mod store;
mod tracker;

// Globs cap each item at its own visibility: `pub` items stay public
// API, the rest stay in-crate. Modules with no `pub` item would
// otherwise warn that they re-export nothing.
#[allow(unused_imports)]
pub use self::{breakpoints::*, compare::*, divergence::*, overlay::*, store::*, tracker::*};

/// Why a turn declined to replay its cached prefix. Non-replaying turns were
/// 19% of measured traffic and carried 97% of booked re-cache waste, and the
/// five reasons need opposite responses — so each one has to be nameable.
#[cfg(test)]
mod skip_reason_tests;

/// `no_previous_turn` is the commonest replay decline and on its own says
/// nothing actionable. These pin the split that makes it useful.
#[cfg(test)]
mod prefix_miss_tests;

/// One session key carries several streams (item 11). Holding one prefix per
/// session makes every alternation forward fresh bytes over cached content —
/// a bust the proxy causes itself. These pin the multi-stream store.
#[cfg(test)]
mod interleaved_stream_tests;

/// A conversation that declines on every turn while growing normally is not
/// being edited by its client. These pin the index that says where the churn is.
#[cfg(test)]
mod divergence_index_tests;

/// The path locator must name the churning field without ever printing a value
/// — it runs on live traffic carrying user content.
#[cfg(test)]
mod divergence_path_tests;

/// The invariant the whole store rests on: `original` is what the CLIENT sent.
///
/// Capturing it after our own CTX stage rewrote the body made the append-only
/// guard compare our output against our output. Our offload decisions moved,
/// the guard saw a difference we had introduced, declined the replay, and bust
/// the cache it exists to protect — while the log named the client.
#[cfg(test)]
mod originals_are_the_clients_tests;

/// `content[len 2 vs 1]` says a block vanished but not which kind. These pin
/// the shape field that names it without logging any block's contents.
#[cfg(test)]
mod block_shape_tests;

#[cfg(test)]
mod tests;

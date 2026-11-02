//! CTX-3 — tool_result offload transform.
//!
//! Replaces oversized `tool_result` blocks in an Anthropic `/v1/messages`
//! body with a deterministic **structural digest** and stashes the original
//! bytes in the CCR store for later retrieval. Applied in the request path
//! **before** the live-zone compressors ([`crate::compression::compress_anthropic_request`]).
//!
//! # Why this is cache-safe (invariants I1–I6, `docs/ctx-mode-in-headroom-plan.md`)
//!
//! - **I1 (pure function).** The replacement is `digest(bytes)`: the
//!   content-type detector + compressor stack ([`compress_block_for_offload`],
//!   all pure) plus a **fixed** footer `<<ctx:HASH>> (N bytes offloaded; …)`
//!   where `HASH = blake3(bytes)[:24]` ([`compute_key`]) and `N = bytes.len()`.
//!   No timestamps, counters, RNG, or session state. Same input bytes → same
//!   output bytes on every call, across process restarts.
//! - **I2 (stable re-application).** Because the digest is recomputable from
//!   the block's own bytes, a block offloaded in turn N and resent raw by the
//!   client in turn N+1 is replaced with the *identical* digest again — no
//!   store lookup on the request path. That is why this transform is exempt
//!   from the frozen-count floor and may rewrite blocks in **all** messages.
//! - **I3 (append-only).** Thresholds are static config; a block only ever
//!   converts raw→digest, never back.
//! - **I6 (tokenizer gate).** If the digest is not smaller in tokens than the
//!   original, the original is kept. This decision is itself a pure function
//!   of the bytes (deterministic tokenizer over deterministic strings).
//!
//! ## Determinism audit (where it could leak, and what we did)
//!
//! - **Object key order / number formatting.** The body round-trips through
//!   `serde_json` with the `preserve_order` + `arbitrary_precision` features
//!   (see workspace `Cargo.toml`): object keys keep input order and numbers
//!   re-serialize byte-for-byte. Re-serialization is therefore deterministic
//!   turn-to-turn (the acceptance harness in `tests/ctx_cache_stability.rs`
//!   asserts prefix-stability directly).
//! - **HashMap iteration.** None: the walk is an ordered traversal of the
//!   `messages`/`content` arrays; nothing here iterates a hash map.
//! - **Float formatting.** The footer contains only integers; body floats are
//!   handled by `arbitrary_precision` above.
//! - **Compressor nondeterminism.** [`compress_block_for_offload`] reuses the
//!   live-zone compressors, all pure. When no compressor rewrites the block
//!   (e.g. `PlainText` without the kompress model on disk), the digest falls
//!   back to a deterministic preview cut ([`preview`]) — the context-mode
//!   behaviour — so plaintext tool output still offloads. The one
//!   environment-scoped caveat: whether the kompress arm or the preview arm
//!   runs depends on the HF model being in the on-disk cache, which is stable
//!   per machine. Documented on [`compress_block_for_offload`].
//!
//! # Composition with `context_editing`
//!
//! `maybe_inject_context_management` (`context_editing.rs`) injects Anthropic
//! server-side `context_management` (`clear_tool_uses`) directives into the
//! top-level request object. It never touches message `content`, so it
//! composes with this transform: after offload, `clear_tool_uses` simply fires
//! on already-small digests, which is harmless. Both-on is the default posture.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use headroom_core::ccr::compute_key;
use headroom_core::tokenizer::get_tokenizer;
use headroom_core::transforms::{DEFAULT_MODEL, compress_block_for_offload};
use lru::LruCache;
use serde_json::Value;

use crate::cache_stabilization::drift_detector::{
    ApiKind, model_free_lineage_key, session_key_log_prefix,
};

/// Static per-request offload settings (I3: never changes mid-session).
#[derive(Debug, Clone)]
pub struct CtxOffloadConfig {
    /// A block's serialized content must exceed this many bytes to qualify.
    pub min_bytes: usize,
    /// How close to the tail a block must be for a first conversion to skip the
    /// boundary wait: together with `stale_window` it sets the near-tail band
    /// (`distance < stale_margin + stale_window`). `0` with a zero window means
    /// every first conversion of a non-live block waits for a rebuild boundary.
    ///
    /// This field used to do a second job — it was the distance at which
    /// `--exclude-tools` stopped protecting a result. That gate is gone. The
    /// list exists so the model never acts on a summary of a file it is about
    /// to edit, which is a fair charge against the live-zone compressors: they
    /// rewrite a result and keep no original. Offload keeps the original; the
    /// block becomes a digest plus a preview and the full bytes come back
    /// through `headroom_retrieve`. Checked before lifting it: 10,415
    /// digest→content round trips against the client's own re-sent bytes were
    /// byte-identical, with no missing content and no dangling digests across
    /// 691 bodies, `Read` results among them.
    ///
    /// What lifting it buys, measured on the blindguard capture (7,839 turns)
    /// with the offline cache simulator, 2,000-byte floor and the boundary gate
    /// on: -6.6% against Claude Code with the list honoured versus -11.4% with
    /// it lifted on API weights, and -1.5% versus -13.3% on subscription
    /// weights.
    ///
    /// [`headroom_core::tool_exclusion::is_verbatim_excluded`] still applies at
    /// any distance: those results break when their bytes change at all.
    ///
    /// # This band is inert while prefix replay is on
    ///
    /// Measured 2026-08-24 over a 2,649-request capture. Offload runs at
    /// `proxy.rs:3144` and `apply_prefix_replay` at `proxy.rs:3995`, and
    /// `overlay_cached_prefix` takes its prefix from
    /// `previous_forwarded_messages`, so any conversion this band makes inside
    /// the replayed prefix is thrown away before the body goes upstream.
    ///
    /// The two never overlap. 96% of turns add two or three messages, so the
    /// fresh tail is two or three deep, while the band starts at
    /// `stale_margin` = 4. Across consecutive forwarded turns, 10,235 eligible
    /// blocks stayed un-offloaded and exactly **one** converted late.
    ///
    /// Lowering the margin does not rescue it. `tool_result` blocks land on
    /// even distances, distance 0 is the live zone that must stay verbatim, and
    /// distance 2 is unreplayed only on the 27% of turns adding three messages.
    /// 253 of 10,485 eligible sightings (2.3MB of 101.5MB) ever sit where a
    /// conversion would survive.
    ///
    /// Converting the rest means paying a rebuild, and on the median it does
    /// not pay back: re-creating a median 108,911 tokens to free 5,257 breaks
    /// even after 125 turns against a median conversation of 58. But four of
    /// 18 conversations with a backlog would have repaid it, and a gate that
    /// is off for all of them leaves that 22% on the table. The median is the
    /// wrong statistic to gate on when the decision can be made per
    /// conversation: `tokens_after`, what the backlog frees, and how many
    /// turns the session has already run are all knowable at request time.
    /// `ctx_offload_accounting` now carries all three
    /// (`bytes_after_deepest_deferral`, `bytes_deferred`, `turns_seen`) so the
    /// rule can be fitted to measurement instead of guessed at.
    ///
    /// `bench/cachesim.py` cannot price this — it applies a strategy uniformly
    /// to the client body and models no replay, so `stale_margin` 1 and 4 score
    /// byte-identical there. Its 5.6pp "headroom" for offloading the backlog is
    /// a counterfactual where the block was never forwarded verbatim, which no
    /// setting reaches.
    pub stale_margin: usize,
    /// How many messages past `stale_margin` a first conversion may happen on an
    /// ordinary turn instead of waiting for a rebuild boundary. `0` waits
    /// always.
    ///
    /// Converting a block that is already inside the cached prefix costs one
    /// rewrite of everything after it, and buys a smaller prompt on every later
    /// turn. Cache creation bills at 1.45 and reads at 0.09, so the trade takes
    /// `16.1 * tokens_after / tokens_saved` turns to come out ahead. Deep in the
    /// history that is hundreds of turns and never pays — hence the boundary
    /// gate, which waits for a turn that is rewriting anyway.
    ///
    /// Just past the margin it is a different trade, because `tokens_after` is
    /// tiny there. Measured over 3,545 bodies at depth ≥ 20 on 2026-08-17: the
    /// last 4 messages are 1,460 tokens (median), and a qualifying block in the
    /// 4-to-8-back window saves 2,280. That is a **10-turn** payback against a
    /// median conversation of 15 turns, and conversations of 51-150 turns hold
    /// 58.7% of all cache reads.
    ///
    /// Those rates are the 5-minute tier. Under `--force-1h-cache-ttl` a write
    /// bills at 2.0 against a read at 0.1 (`headroom_core::pricing`), so the
    /// multiplier is 19, not 16.1, and every payback above is a fifth longer
    /// than it reads — the 10-turn window is 12 turns, still inside a median
    /// conversation, and the 125-turn backlog is 148.
    ///
    /// So this window is a deliberate, bounded cache cost — the only one in this
    /// module. Widening it moves `tokens_after` up fast (the last 8 messages are
    /// already 3,699 tokens) and the payback with it.
    pub stale_window: usize,
    /// Seed a newborn session's gate from the same conversation's prior
    /// session (model switch, resume), so its frozen history converts on
    /// first sight instead of stalling Deferred until a boundary. Off by
    /// default; see `--ctx-offload-cross-session-seed`. Has no effect on
    /// live sessions (seeding refuses anything the gate already knows) and
    /// none on wire bytes when no donor exists.
    pub cross_session_seed: bool,
}

/// One offloaded block. Its CCR original is stored inline by
/// [`crate::ctx::offload_store::OffloadStore::persist`], because a retrieval
/// can land in the same turn as the offload; only the FTS indexing goes to the
/// background worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffloadRecord {
    /// `blake3(original)[:24]` — the retrieval key embedded in the digest.
    pub hash: String,
    /// The original block content bytes (what the client sent).
    pub original: String,
    /// Paired `tool_use` command/tool name, retained with the durable index
    /// job for diagnostics. Empty when no pairing was found.
    pub title: String,
    /// Gate entry added by this request. If the durable index outbox refuses
    /// the request, callers remove this new entry before forwarding raw bytes.
    pub gate_rollback: Option<(String, String)>,
}

/// Result of running the transform over one request body.
#[derive(Debug, Default)]
pub struct OffloadOutcome {
    /// Number of blocks replaced with a digest.
    pub blocks_offloaded: usize,
    /// PR-J4: qualifying frozen blocks left raw because the turn is not a
    /// rebuild boundary (they will convert at the next boundary).
    pub blocks_deferred: usize,
    /// Serialized bytes of those blocks, as they will be re-sent verbatim on
    /// every turn until a boundary opens.
    ///
    /// The count alone cannot price the deferral. Converting a deferred block
    /// rewrites the cached prefix from that block forward, so whether the
    /// conversion pays for itself is the ratio of what it frees to what it
    /// forces rewritten — and the numerator was never recorded. Bytes rather
    /// than tokens because the tokenizer pass a token count needs is exactly
    /// the work the deferral skipped.
    pub bytes_deferred: usize,
    /// Distance from the tail of the shallowest and deepest deferred block.
    ///
    /// [`CtxOffloadConfig::stale_window`] already converts inside a fixed band
    /// because `tokens_after` is small there; past it every first conversion
    /// waits. Whether the band is set right, and whether the backlog behind it
    /// is shallow or deep, is a question about this distribution, and it was
    /// never recorded — only the count of what was held back.
    pub deferred_min_distance: Option<usize>,
    pub deferred_max_distance: Option<usize>,
    /// Serialized bytes of the messages after the deepest deferred block: what
    /// converting the whole backlog at once would force rewritten.
    ///
    /// The denominator of the payback the module documents as
    /// `tokens_after / tokens_saved`. Measured rather than assumed, because
    /// the assumption — that a deferred block sits deep, so the rewrite is
    /// nearly the whole body — is the one thing the decision turns on.
    pub bytes_after_deepest_deferral: usize,
    /// PR-J5 thrash guard: conversions of frozen blocks not previously in the
    /// session's offload set. Non-zero on a non-boundary turn means the I4
    /// invariant was violated (a cache-thrash bug) — the caller warns loudly.
    ///
    /// Window conversions are counted in [`Self::window_offloads`] instead. They
    /// are frozen conversions on a non-boundary turn by design, so counting them
    /// here made the guard fire on correct behaviour — twice within 15 turns of
    /// shipping `stale_window`. A guard that cries wolf is worse than no guard,
    /// because the next real thrash bug hides inside the noise.
    pub frozen_new_offloads: usize,
    /// Conversions that rode the near-tail window rather than a boundary. These
    /// each spend a small, deliberate cache rewrite; see
    /// [`CtxOffloadConfig::stale_window`] for the payback arithmetic. Worth
    /// watching, not warning about.
    pub window_offloads: usize,
    /// Tokens removed from the body, summed over the offloaded blocks.
    ///
    /// Free to collect: the per-block tokenizer gate already counts both the
    /// original and the digest to decide whether the swap is worth making, so
    /// this only keeps a difference it was throwing away. Lets callers report
    /// a real `tokens_saved` instead of estimating one from byte counts.
    pub tokens_saved: i64,
    /// Originals to persist off the request path (may be empty).
    pub records: Vec<OffloadRecord>,
}

impl OffloadOutcome {
    /// Widen the recorded deferral depth to include `distance`.
    pub fn note_deferred_distance(&mut self, distance: usize) {
        self.deferred_min_distance = Some(match self.deferred_min_distance {
            Some(prev) => std::cmp::min(prev, distance),
            None => distance,
        });
        self.deferred_max_distance = Some(match self.deferred_max_distance {
            Some(prev) => std::cmp::max(prev, distance),
            None => distance,
        });
    }

    /// Whether any block was rewritten (i.e. the body bytes changed).
    pub fn changed(&self) -> bool {
        self.blocks_offloaded > 0
    }
}

/// Fixed marker prefix. A block whose text already contains this is a digest
/// (idempotency fast path).
///
/// Shared with the live-zone pass, which skips blocks carrying it — one
/// spelling, so the writer here and the reader there cannot drift.
use headroom_core::transforms::live_zone::CTX_OFFLOAD_MARKER_PREFIX as MARKER_PREFIX;

/// PR-J4 — boundary-gated offload policy (invariant I4 of
/// `docs/notes/realignment/13-phase-J-history-offload.md`).
///
/// The digest itself is a pure function of the block bytes, so *re-applying*
/// an offload is always cache-stable. The one remaining cache-bust risk is
/// the **first** conversion of a block that already sits inside the client's
/// cached frozen prefix (e.g. the proxy joins a session mid-flight): rewriting
/// it on a steady-state turn pays a fresh cache write for no reason. The gate
/// therefore permits a first conversion only when:
///
/// - the block is in the **live tail** (the last message) — it has never been
///   cached, so converting it before its first cache write is free; or
/// - the drift detector reported a **rebuild boundary** this turn — the client
///   is re-writing the cache anyway, so the conversion rides that write.
///
/// Once converted, the block's hash enters a per-session **monotonic set**
/// (invariant I3): subsequent turns re-apply the offload unconditionally, so
/// the digest never flip-flops back to raw bytes on a steady-state turn.
///
/// # Why the set is on disk
///
/// It used to live only in memory, and the comment on the field below claimed
/// that losing it "merely defers" a conversion, which is safe. That was wrong in
/// the direction that costs money. A block already forwarded as a digest, whose
/// hash the gate has forgotten, is a FIRST conversion again: too deep for the
/// window and with no boundary in sight, it forwards RAW where the provider
/// cached a digest, so the prefix diverges mid-history — and it stays raw,
/// losing the saving for the rest of the conversation.
///
/// Measured on 2026-08-17: 296 blocks sat deferred across the restarts of one
/// afternoon, all of them previously converted. Persisting the set is the same
/// move `--replay-store-dir` made for forwarded prefixes, for the same reason.
/// Cap on session keys remembered per cross-model lineage for gate seeding.
pub const SEED_LINEAGE_CAP: usize = 4;

/// Cap on hashes installed by one seed. Per-session sets are unbounded, so a
/// pathological donor must not clone unbounded into every newborn session.
/// Live traffic shows ~26 conversions per 51 turns; this is headroom, not a
/// working-set estimate.
pub const SEED_MAX_HASHES: usize = 4096;

/// Outcome of [`OffloadGate::seed_if_absent`]. Returned (not logged) so the
/// wiring layer can count `Seeded` vs `RefusedLive` vs `DonorEmpty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedOutcome {
    Seeded { count: usize },
    RefusedLive,
    DonorEmpty,
}

pub struct OffloadGate {
    /// session key → hashes already offloaded in that session. Bounded LRU;
    /// eviction re-reads from disk when the session is next seen.
    sessions: Mutex<LruCache<String, HashSet<String>>>,
    /// Cross-session lineage index for gate seeding: (identity branch,
    /// first-message hash) → session keys, most-recent-birth-first. Lets a
    /// newborn session (model switch, resumed conversation) find the donor
    /// session whose conversions it should inherit. Same-credential only by
    /// construction of the key; bounded per entry AND bounded map (an entry
    /// is a few dozen bytes, but distinct conversations are unbounded over a
    /// proxy's lifetime — eviction just means no donor, i.e. today's
    /// behavior). Updated on the rare first-request path only.
    lineages: Mutex<LruCache<(String, String), VecDeque<String>>>,
    /// Where the sets are kept so they survive a restart. `None` keeps them in
    /// memory only, which is the pre-2026-08-17 behaviour.
    persist_dir: Option<Arc<std::path::PathBuf>>,
    /// Held from snapshot to rename, so two turns on one session cannot write
    /// their snapshots out of order or into the same temporary file. Either
    /// way the older, smaller set would win on disk, and after a restart the
    /// hashes it lacked would revert their digests to raw — a cache bust.
    persist_lock: Mutex<()>,
}

/// Distinguishes temporary files written by this process; combined with the
/// pid it keeps a second proxy on the same directory from sharing one.
static PERSIST_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One session's converted-hash set, as written to disk.
///
/// Holds hashes and a timestamp — never the session key, which can carry a
/// credential. The key only appears as the SHA-256 in the filename, exactly as
/// [`crate::cache_stabilization::prefix_replay`] does it. A test asserts it.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedGate {
    saved_at_unix: u64,
    hashes: Vec<String>,
}

/// The cross-session lineage index, as written to disk. Without this, a
/// restart keeps every session's converted set (gate files) but forgets
/// which sessions share a lineage — and a post-restart model switch would
/// cold-start despite a warm donor on disk. Entries mirror the in-memory
/// index 1:1; a lineage whose sessions all miss is simply never chosen.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedLineages {
    saved_at_unix: u64,
    entries: Vec<PersistedLineage>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedLineage {
    branch: String,
    first_message_hash: String,
    sessions: Vec<String>,
}

/// Where the lineage index lives. Deliberately NOT a session hash: the sweep
/// keys staleness off mtime, and every birth rewrites this file, so it ages
/// exactly like the gate files it points at.
fn lineages_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("lineages.json")
}

/// How stale a persisted set may be and still be honoured.
///
/// Two bounds, and this sits between them. Below: an hour, after which the
/// provider has dropped the cache entry a revert would have busted — but the set
/// carries the SAVING as well as the safety, and a conversation resumed the next
/// morning should keep its digests rather than re-inflate its prompt. Above: the
/// offload store keeps originals for 7 days (`--ctx-offload-ttl-seconds`), and a
/// digest whose original is gone is a pointer to nothing.
///
/// A day is comfortably inside the upper bound and covers an overnight resume.
const GATE_PERSIST_MAX_AGE: Duration = Duration::from_secs(86_400);

/// Where one session's set lives. See [`PersistedGate`] for why it is hashed.
fn gate_path(dir: &std::path::Path, session_key: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(session_key.as_bytes());
    dir.join(format!("{}.json", hex::encode(digest)))
}

/// Delete sets past [`GATE_PERSIST_MAX_AGE`]. One `read_dir` per process start.
fn sweep_stale_gates(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        // Age from the file's own mtime: a truncated or foreign file should be
        // swept too, and parsing every one defeats a cheap sweep.
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                t.elapsed()
                    .map(|e| e > GATE_PERSIST_MAX_AGE)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn gate_unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl OffloadGate {
    /// # Panics
    /// Panics if `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("OffloadGate capacity must be > 0");
        Self {
            sessions: Mutex::new(LruCache::new(cap)),
            lineages: Mutex::new(LruCache::new(cap)),
            persist_dir: None,
            persist_lock: Mutex::new(()),
        }
    }

    /// [`Self::new`], with the converted-hash sets kept under `dir` so a restart
    /// does not turn every digest back into raw bytes.
    ///
    /// Creates the directory and sweeps anything past
    /// [`GATE_PERSIST_MAX_AGE`]. Failing either turns persistence off for this
    /// process rather than failing the proxy — the fallback is the in-memory
    /// behaviour that shipped for months. Deleting the directory is the off
    /// switch; the cost is one restart's worth of reverts.
    ///
    /// # Panics
    /// Panics if `capacity == 0`.
    pub fn with_persistence(capacity: usize, dir: std::path::PathBuf) -> Self {
        let mut gate = Self::new(capacity);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(
                event = "offload_gate_persist_unavailable",
                dir = %dir.display(),
                %error,
                "cannot create the offload-gate directory; keeping the set in memory only"
            );
            return gate;
        }
        let swept = sweep_stale_gates(&dir);
        tracing::info!(
            event = "offload_gate_persist_enabled",
            dir = %dir.display(),
            stale_files_removed = swept,
            "persisting offload conversions across restarts"
        );
        gate.persist_dir = Some(Arc::new(dir.clone()));
        gate.rehydrate_lineages(&dir);
        gate
    }

    /// Load the lineage index written by [`Self::note_session_birth`]. Same
    /// freshness rule as gate files; a stale or unreadable index is an empty
    /// one (safe fallback: sessions warm as they do today).
    fn rehydrate_lineages(&self, dir: &std::path::Path) {
        let restored: usize = std::fs::read(lineages_path(dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistedLineages>(&bytes).ok())
            .filter(|g| {
                gate_unix_now().saturating_sub(g.saved_at_unix) <= GATE_PERSIST_MAX_AGE.as_secs()
            })
            .map(|g| {
                let mut lineages = self.lineages.lock().unwrap_or_else(|p| p.into_inner());
                let mut n = 0;
                for e in g.entries {
                    let mut sessions: VecDeque<String> = e.sessions.into_iter().collect();
                    sessions.truncate(SEED_LINEAGE_CAP);
                    if !sessions.is_empty() {
                        lineages.put((e.branch, e.first_message_hash), sessions);
                        n += 1;
                    }
                }
                n
            })
            .unwrap_or(0);
        if restored > 0 {
            tracing::info!(
                event = "offload_gate_lineages_rehydrated",
                lineages = restored,
                "restored cross-session lineage index recorded before this process started"
            );
        }
    }

    /// Snapshot the lineage index for persistence. Cloned under the lock,
    /// written outside it; the file is tiny (bounded entries × bounded
    /// sessions) and births are rare.
    fn persist_lineages(&self, dir: &std::path::Path) {
        let snapshot: Vec<PersistedLineage> = self
            .lineages
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(
                |((branch, first_message_hash), sessions)| PersistedLineage {
                    branch: branch.clone(),
                    first_message_hash: first_message_hash.clone(),
                    sessions: sessions.iter().cloned().collect(),
                },
            )
            .collect();
        let snapshot = PersistedLineages {
            saved_at_unix: gate_unix_now(),
            entries: snapshot,
        };
        let _ = std::fs::write(
            lineages_path(dir),
            serde_json::to_vec(&snapshot).unwrap_or_default(),
        );
    }

    /// Read this session's set into memory if it is not already there.
    ///
    /// Installs an empty set on a miss, so a session with nothing on disk costs
    /// one failed read per process rather than one per block. The read happens
    /// with no lock held.
    fn hydrate(&self, session: &str) {
        let Some(dir) = self.persist_dir.as_deref() else {
            return;
        };
        match self.sessions.lock() {
            Ok(guard) if guard.contains(session) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let loaded = std::fs::read(gate_path(dir, session))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistedGate>(&bytes).ok())
            .filter(|g| {
                gate_unix_now().saturating_sub(g.saved_at_unix) <= GATE_PERSIST_MAX_AGE.as_secs()
            });
        let restored = loaded.as_ref().map(|g| g.hashes.len()).unwrap_or(0);
        let set: HashSet<String> = loaded
            .map(|g| g.hashes.into_iter().collect())
            .unwrap_or_default();
        if let Ok(mut guard) = self.sessions.lock()
            && !guard.contains(session)
        {
            guard.put(session.to_string(), set);
            if restored > 0 {
                tracing::info!(
                    event = "offload_gate_rehydrated",
                    conversions = restored,
                    "restored offload conversions recorded before this process started"
                );
            }
        }
    }

    fn contains(&self, session: &str, hash: &str) -> bool {
        self.hydrate(session);
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions
            .get(session)
            .map(|set| set.contains(hash))
            .unwrap_or(false)
    }

    fn record(&self, session: &str, hash: &str) {
        let _serialised = self.persist_lock.lock().unwrap_or_else(|p| p.into_inner());
        let snapshot = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            match sessions.get_mut(session) {
                Some(set) => {
                    set.insert(hash.to_string());
                }
                None => {
                    let mut set = HashSet::new();
                    set.insert(hash.to_string());
                    sessions.put(session.to_string(), set);
                }
            }
            // Clone under the lock, write outside it. Conversions are rare — 26
            // in 51 turns on live traffic — so a small write per conversion is
            // cheaper than any scheme for batching it.
            sessions.get(session).cloned()
        };
        if let (Some(dir), Some(set)) = (self.persist_dir.as_deref(), snapshot) {
            self.persist(dir, session, set);
        }
    }

    /// Undo new conversion entries when the caller had to forward the raw
    /// request because persistence was unavailable. Existing entries are not
    /// included in `gate_rollback` and remain untouched.
    pub fn rollback_unstored_records(&self, records: &[OffloadRecord]) {
        for record in records {
            let Some((session, key)) = record.gate_rollback.as_ref() else {
                continue;
            };
            let _serialized = self.persist_lock.lock().unwrap_or_else(|p| p.into_inner());
            let snapshot = {
                let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(set) = sessions.get_mut(session) {
                    set.remove(key);
                }
                sessions.get(session).cloned()
            };
            if let (Some(dir), Some(set)) = (self.persist_dir.as_deref(), snapshot) {
                self.persist(dir, session, set);
            }
        }
    }

    /// Cross-session seeding for newborn sessions (model switch, resumed
    /// conversation). Unlike [`Self::adopt_from`], the recipient does NOT
    /// continue the donor's bytes — it starts a fresh lineage whose history
    /// happens to hold identical content. Safe only at birth: the recipient
    /// must never have forwarded bytes on this session, or converting
    /// previously-raw blocks mid-history would shift its prefix. The caller
    /// guarantees birth (drift first-sight); this method additionally refuses
    /// any session the gate already knows, including hydrate-installed empty
    /// sets — a live session that never converted anything holds `{}` and
    /// must NOT be seeded.
    ///
    /// Outcome:
    /// - `Seeded` — donor set cloned (verbatim, `p512:` namespace intact),
    ///   truncated to [`SEED_MAX_HASHES`], installed and persisted.
    /// - `RefusedLive` — gate already knows the session (live session,
    ///   drift-eviction rebirth with surviving gate state, or concurrent
    ///   double birth — all benign refusals).
    /// - `DonorEmpty` — donor yielded nothing; nothing installed.
    pub fn seed_if_absent(&self, donor_session: &str, new_session: &str) -> SeedOutcome {
        {
            let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if sessions.contains(new_session) {
                return SeedOutcome::RefusedLive;
            }
        }
        // Same two donor sources as `adopt_from`: in-memory set (rehydrating
        // from disk first so an evicted-but-persisted donor still seeds),
        // with the same freshness filter. No locks held across the read.
        self.hydrate(donor_session);
        let donor: HashSet<String> = self
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .peek(donor_session)
            .cloned()
            .unwrap_or_default();
        if donor.is_empty() {
            return SeedOutcome::DonorEmpty;
        }
        let _serialised = self.persist_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        // Re-check under the same hold that installs: a concurrent birth may
        // have installed (or the session recorded) since the first check.
        // Refusal here is benign — both writers carry same-conversation sets.
        if sessions.contains(new_session) {
            return SeedOutcome::RefusedLive;
        }
        let set: HashSet<String> = donor.into_iter().take(SEED_MAX_HASHES).collect();
        let count = set.len();
        sessions.put(new_session.to_string(), set);
        let snapshot = sessions.get(new_session).cloned();
        drop(sessions);
        if let (Some(dir), Some(set)) = (self.persist_dir.as_deref(), snapshot) {
            self.persist(dir, new_session, set);
        }
        SeedOutcome::Seeded { count }
    }

    /// Record a newborn session under its cross-model lineage so a later
    /// session on the same lineage (model switch, resume) can seed from it.
    /// Most-recent-birth-first, bounded per lineage; call on the rare
    /// drift-first-sight path only, never per request.
    pub fn note_session_birth(&self, lineage: &(String, String), session: &str) {
        let mut lineages = self.lineages.lock().unwrap_or_else(|p| p.into_inner());
        // LruCache has no entry API: pop + re-put keeps most-recent-first
        // with dedup and refreshes recency in one move.
        let mut entry: VecDeque<String> = lineages.pop(lineage).unwrap_or_default();
        entry.retain(|s| s != session);
        entry.push_front(session.to_string());
        entry.truncate(SEED_LINEAGE_CAP);
        lineages.put(lineage.clone(), entry);
        drop(lineages);
        // No lock nesting (unlike `record`, which serializes writers): the
        // snapshot is taken above and written below, so lock order cannot
        // invert no matter who else holds what.
        if let Some(dir) = self.persist_dir.as_deref() {
            self.persist_lineages(dir);
        }
    }

    /// Birth-ordered candidate donors for `lineage`, excluding `exclude`.
    /// The caller tries them via [`Self::seed_if_absent`], first non-empty
    /// wins; "most recent" is most-recently-born (birth-only updates cannot
    /// track activity — and need not: hashes do not expire semantically).
    pub fn seed_candidates(&self, lineage: &(String, String), exclude: &str) -> Vec<String> {
        self.lineages
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(lineage)
            .map(|v| {
                v.iter()
                    .filter(|s| s.as_str() != exclude)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Shared wiring for cross-session gate seeding (P2): call on the drift
/// first-sight path for a newborn session, before that session's first gate
/// lookup in the same request (S1a ordering — the drift call sites run before
/// offload on both the Claude and routed paths).
///
/// Records the birth under the session's cross-model lineage, then tries
/// that lineage's donor sessions in birth order until one seeds (first
/// non-empty wins) or the gate refuses (already known — benign). No donor,
/// or no lineage (explicit session id, messageless body), is a silent no-op:
/// the session warms exactly as it does today.
///
/// The caller gates this on `CtxOffloadConfig.cross_session_seed`; the
/// helper itself is unconditional so unit tests can drive the mechanism
/// without config plumbing.
pub fn seed_newborn_session(
    gate: &OffloadGate,
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    body: &Value,
    kind: ApiKind,
    session_key: &str,
    request_id: &str,
) {
    let Some(lineage) = model_free_lineage_key(headers, client_addr, body, kind) else {
        return;
    };
    gate.note_session_birth(&lineage, session_key);
    for donor in gate.seed_candidates(&lineage, session_key) {
        match gate.seed_if_absent(&donor, session_key) {
            SeedOutcome::Seeded { count } => {
                tracing::info!(
                    event = "offload_gate_session_seeded",
                    request_id = %request_id,
                    session_key_hash = %session_key_log_prefix(session_key),
                    donor_session_key_hash = %session_key_log_prefix(&donor),
                    conversions = count,
                    "newborn session inherited its lineage's offload conversions; \
                     known blocks convert on first sight"
                );
                crate::observability::ctx_metrics::observe_gate_seeded();
                return;
            }
            SeedOutcome::RefusedLive => {
                // The gate learned this session between birth detection and
                // seeding (concurrent double birth, or surviving gate state
                // after a drift-eviction rebirth). Either way there is nothing
                // left to do — and nothing safe to merge.
                crate::observability::ctx_metrics::observe_gate_seed_refused();
                return;
            }
            SeedOutcome::DonorEmpty => continue,
        }
    }
}

impl OffloadGate {
    /// Copy the donor's conversions onto `session`, which has just adopted the
    /// donor's forwarded prefix (see `SessionReplayStore`).
    ///
    /// The prefix carries the donor's digests, so the adopter's later turns
    /// must re-apply them through the `prior` path. Without this the gate has
    /// never seen those blocks under the adopter's key, the digest reverts to
    /// the raw block, and the provider re-caches from there. The donor's own
    /// set is left as it was.
    pub fn adopt_from(
        &self,
        donor: &crate::cache_stabilization::prefix_replay::AdoptionDonor,
        session: &str,
    ) {
        use crate::cache_stabilization::prefix_replay::AdoptionDonor;
        let hashes: HashSet<String> = match donor {
            AdoptionDonor::Session(key) => {
                self.hydrate(key);
                let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                sessions.peek(key.as_str()).cloned().unwrap_or_default()
            }
            // Same file naming as the replay store: the SHA-256 hex of the key.
            AdoptionDonor::PersistedDigest(digest) => self
                .persist_dir
                .as_deref()
                .and_then(|dir| std::fs::read(dir.join(format!("{digest}.json"))).ok())
                .and_then(|bytes| serde_json::from_slice::<PersistedGate>(&bytes).ok())
                .filter(|g| {
                    gate_unix_now().saturating_sub(g.saved_at_unix)
                        <= GATE_PERSIST_MAX_AGE.as_secs()
                })
                .map(|g| g.hashes.into_iter().collect())
                .unwrap_or_default(),
        };
        if hashes.is_empty() {
            return;
        }
        let adopted = hashes.len();
        let snapshot = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            match sessions.get_mut(session) {
                Some(set) => set.extend(hashes),
                None => {
                    sessions.put(session.to_string(), hashes);
                }
            }
            sessions.get(session).cloned()
        };
        tracing::info!(
            event = "offload_gate_adopted",
            conversions = adopted,
            "seeded a session's offload conversions from the session whose prefix it adopted"
        );
        if let (Some(dir), Some(set)) = (self.persist_dir.as_deref(), snapshot) {
            self.persist(dir, session, set);
        }
    }

    /// Best-effort and quiet on failure: a lost set costs what the in-memory
    /// gate cost before it existed.
    fn persist(&self, dir: &std::path::Path, session: &str, set: HashSet<String>) {
        let mut hashes: Vec<String> = set.into_iter().collect();
        // Sorted so the file is stable for the same content — nothing depends on
        // it, but a diffable file is worth the one sort per conversion.
        hashes.sort();
        let snapshot = PersistedGate {
            saved_at_unix: gate_unix_now(),
            hashes,
        };
        let Ok(bytes) = serde_json::to_vec(&snapshot) else {
            return;
        };
        let path = gate_path(dir, session);
        // Write-then-rename, so a restart mid-write cannot leave a truncated file
        // that parses as a valid but short set.
        let temporary = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            PERSIST_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if std::fs::write(&temporary, &bytes).is_ok() {
            let _ = std::fs::rename(&temporary, &path);
        }
    }
}

/// Per-turn inputs to the [`OffloadGate`] decision. `None` policy = ungated
/// (pre-J4 behavior; used by tests and as the explicit kill path).
pub struct OffloadPolicy<'a> {
    pub gate: &'a OffloadGate,
    /// Opaque per-session key (same derivation as the drift detector).
    pub session_key: &'a str,
    /// Whether the drift detector observed a hot-zone rebuild on this turn.
    pub rebuild_boundary: bool,
}

/// Ceiling on the preview kept by the no-compressor fallback, in bytes.
/// Matches context-mode's `FETCH_PREVIEW_LIMIT` (3072): enough to orient the
/// model, small enough that offload always shrinks a large block.
const PREVIEW_BYTES: usize = 3072;

/// Floor on the same preview. A fixed 3,072-byte cut cannot shrink a 4,000-byte
/// block, so blocks under roughly 3.2KB were unreachable whatever `min_bytes`
/// said — and blocks that size are where the bytes now are: dropping the floor
/// from 15,000 to 4,000 reaches 45% of all `tool_result` bytes against 27%.
/// Below this the preview stops being worth reading.
const PREVIEW_FLOOR_BYTES: usize = 600;

/// Preview budget for one block: a quarter of it, clamped to the two constants
/// above. Pure function of the block's own length (I1) — no session state, so
/// the same bytes yield the same digest on every turn and after a restart.
fn preview_budget(len: usize) -> usize {
    (len / 4).clamp(PREVIEW_FLOOR_BYTES, PREVIEW_BYTES)
}

/// Ceiling on the preview of a block first converted after 2026-09-02. The
/// legacy budget above yielded a 974-byte median preview; a digest exists to
/// point at the store, and half a kilobyte orients the model as well.
const PREVIEW_CAP_BYTES: usize = 512;

/// Gate namespace for conversions made under [`PREVIEW_CAP_BYTES`]. A digest
/// is a pure function of the block bytes AND the budget it was cut with, so a
/// block converted under the legacy budget must keep regenerating under it:
/// its digest is already in the provider's cache, and a shorter preview would
/// be a new block. The un-namespaced entry therefore keeps meaning "legacy
/// budget", and only never-converted blocks take the cap. Persisted gate files
/// from before the cap carry only legacy entries and keep working unchanged.
const PREVIEW_CAP_GATE_NS: &str = "p512:";

/// Look `key` up under both gate entry shapes. Returns whether the block was
/// converted before in this session, the entry to record it under, and the
/// preview budget its digest must be cut with.
fn gate_lookup(policy: Option<&OffloadPolicy>, key: &str, len: usize) -> (bool, String, usize) {
    let capped_key = format!("{PREVIEW_CAP_GATE_NS}{key}");
    let Some(p) = policy else {
        return (false, capped_key, PREVIEW_CAP_BYTES);
    };
    if p.gate.contains(p.session_key, key) {
        return (true, key.to_string(), preview_budget(len));
    }
    let prior = p.gate.contains(p.session_key, &capped_key);
    (prior, capped_key, PREVIEW_CAP_BYTES)
}

/// Longest prefix of `text` whose UTF-8 encoding is at most `max_bytes`,
/// cut on a char boundary, with a truncation notice. Pure function of
/// `text` (I1) — the context-mode preview cut, ported.
fn preview(text: &str, max_bytes: usize) -> String {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated — retrieval pointer below]", &text[..end])
}

/// Build the deterministic footer appended to a digest. Pure function of
/// `hash` and `orig_len` — no volatile fields (I1).
///
/// The pointer names the `headroom_retrieve` tool, which is injected into the
/// same request (`proxy.rs`, gated on `ccr_inject_tool`) and resolves against
/// the same CCR store this record is persisted to. It used to read
/// `headroom ctx get <hash>` — a shell command, which the model cannot run and
/// which no agent has a tool for. Offload therefore looked healthy while
/// nothing was ever retrieved: the content was reachable, but the only
/// instruction the model ever saw pointed at a surface it could not use.
///
/// Naming the tool was not enough on its own. Written as a call signature —
/// `headroom_retrieve(hash="…")` — the pointer reads as a named callable
/// rather than a tool, and `headroom_retrieve` is a deferred tool, absent
/// from the schema until something surfaces it. Sessions that met the
/// pointer before that reached for the Skill tool and got "Unknown skill".
/// So the footer says "tool" in words, which no other surface is called.
fn footer(hash: &str, orig_len: usize) -> String {
    format!(
        "\n{MARKER_PREFIX}{hash}>> ({orig_len} bytes offloaded; \
         use the headroom_retrieve tool with hash=\"{hash}\")"
    )
}

/// Map each offloaded CCR hash back to the file whose Read produced it, for
/// the Reads that have since gone stale.
///
/// The digest a model retrieves is the file as it was *before* later edits, and
/// [`footer`] cannot say so: it is a pure function of the block bytes by design,
/// and staleness flips false→true partway through a conversation. Writing it
/// there would rewrite an already-cached block and break the prefix at that
/// point — the most expensive thing this proxy can do. So the warning is
/// attached at retrieval instead, where it costs no forwarded bytes and is
/// evaluated against the newest history.
pub(crate) fn stale_offloaded_reads(messages: &[Value]) -> HashMap<String, String> {
    use headroom_core::transforms::read_lifecycle::{
        ReadLifecycleConfig, ReadLifecycleManager, ReadState,
    };

    // tool_use_id -> hash, for the blocks we have already turned into digests.
    let mut hash_by_tool_use: HashMap<&str, String> = HashMap::new();
    for msg in messages {
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let (Some(id), Some(text)) = (
                block.get("tool_use_id").and_then(Value::as_str),
                block.get("content").and_then(tool_result_text),
            ) else {
                continue;
            };
            if let Some(hash) = marker_hash(&text) {
                hash_by_tool_use.insert(id, hash);
            }
        }
    }
    if hash_by_tool_use.is_empty() {
        return HashMap::new();
    }

    let manager = ReadLifecycleManager::new(ReadLifecycleConfig::default(), None);
    manager
        .classify(messages)
        .into_iter()
        .filter(|c| c.state == ReadState::Stale)
        .filter_map(|c| {
            hash_by_tool_use
                .get(c.tool_call_id.as_str())
                .map(|hash| (hash.clone(), c.file_path))
        })
        .collect()
}

/// The banner prepended to retrieved content whose file has since been edited.
pub(crate) fn stale_read_warning(path: &str) -> String {
    format!(
        "[headroom] {path} was edited after this Read. What follows is the file \
         as it stood BEFORE those edits — use it as history, and Read {path} \
         again for its current contents."
    )
}

/// Pull the hash out of `<<ctx:HASH>>`, if this text is one of our digests.
pub(crate) fn marker_hash(text: &str) -> Option<String> {
    let start = text.find(MARKER_PREFIX)? + MARKER_PREFIX.len();
    let rest = &text[start..];
    let end = rest.find(">>")?;
    Some(rest[..end].to_string())
}

/// Walk every message's `tool_result` blocks and replace qualifying ones with
/// a digest. Mutates `parsed` in place; returns what changed + records to
/// persist. Pure function of `parsed` + `config` (I1/I2).
pub fn offload_anthropic_request(
    parsed: &mut Value,
    config: &CtxOffloadConfig,
    policy: Option<&OffloadPolicy>,
) -> OffloadOutcome {
    let mut outcome = OffloadOutcome::default();

    // First pass (immutable borrow): map tool_use_id → command/tool title so
    // the FTS chunk title is deterministic. Built from a clone so the second
    // pass can take a mutable borrow without aliasing.
    let tool_titles = collect_tool_titles(parsed);

    let Some(messages) = parsed.get_mut("messages").and_then(Value::as_array_mut) else {
        return outcome;
    };

    let last_idx = messages.len().saturating_sub(1);
    // Lowest index, i.e. deepest in the history: converting the backlog costs
    // one rewrite of everything after the earliest block in it.
    let mut deepest_deferred_msg: Option<usize> = None;
    for (msg_idx, message) in messages.iter_mut().enumerate() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        // PR-J4: the last message is the live tail — never yet cached, so a
        // first conversion there is free. Everything earlier is (potentially)
        // inside the cached prefix and gated on a rebuild boundary.
        let is_live = msg_idx == last_idx;
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let (title, tool_name) = tool_titles.get(&tool_use_id).cloned().unwrap_or_default();
            // These results break when their bytes change at all, at any
            // distance from the tail. No margin reaches them.
            if headroom_core::tool_exclusion::is_verbatim_excluded(&tool_name) {
                continue;
            }
            // `--exclude-tools` deliberately does NOT gate here. It stops the
            // live-zone compressors, which rewrite a result and keep no
            // original; offload leaves a digest and a preview and hands the
            // full bytes back through `headroom_retrieve`, so nothing is
            // destroyed and the argument for the list does not carry over. The
            // list cost -6.6% instead of -11.4% against Claude Code on the
            // blindguard capture (API weights) for protection the store already
            // provides. See `CtxOffloadConfig::stale_margin`.
            //
            // Distance from the tail GROWS as the conversation does, so any
            // position test flips under a block that is already inside the
            // cached prefix. Converting it there would rewrite the prefix
            // mid-history, which is the most expensive thing this proxy can do.
            // Two things stop that, and both are needed:
            //
            //   - the PR-J4 boundary gate below defers every first conversion
            //     of a non-live block to a turn that is rebuilding anyway, so
            //     the transition is never paid for on a steady-state turn;
            //   - `prior` is checked BEFORE anything positional, so a block
            //     already converted stays converted. Claude Code edits its own
            //     history and the message count can fall, which would otherwise
            //     let a deep block read as near-tail again and revert
            //     digest→raw. That is the same flip in the other direction and
            //     busts the cache just as hard.
            let distance = last_idx - msg_idx;
            // Close enough to the tail that the rewrite it costs is small — see
            // `stale_window`. Deeper than this, a first conversion still waits.
            let near_tail =
                config.stale_window > 0 && distance < config.stale_margin + config.stale_window;
            match offload_tool_result(block, config, &title, policy, is_live, near_tail) {
                BlockOutcome::Offloaded {
                    record,
                    prior,
                    tokens_saved,
                } => {
                    outcome.blocks_offloaded += 1;
                    // Labelled by tool, because a flat total cannot show
                    // whether lifting `--exclude-tools` here actually reached
                    // the file and search results it was lifted for.
                    crate::observability::ctx_offload_by_tool::observe_offloaded_tool(&tool_name);
                    if !prior && !is_live {
                        // Intended (window) and unintended (I4 violation) frozen
                        // conversions are counted apart, so the guard downstream
                        // keeps meaning "this should never happen".
                        if near_tail {
                            outcome.window_offloads += 1;
                        } else {
                            outcome.frozen_new_offloads += 1;
                        }
                    }
                    outcome.tokens_saved += tokens_saved;
                    outcome.records.push(record);
                }
                BlockOutcome::Deferred { bytes } => {
                    outcome.blocks_deferred += 1;
                    outcome.bytes_deferred += bytes;
                    outcome.note_deferred_distance(distance);
                    deepest_deferred_msg = Some(match deepest_deferred_msg {
                        Some(prev) => std::cmp::min(prev, msg_idx),
                        None => msg_idx,
                    });
                }
                BlockOutcome::Skipped => {}
            }
        }
    }

    // Only walked when something was actually held back, so an ordinary turn
    // pays nothing for the measurement.
    if let Some(first) = deepest_deferred_msg {
        outcome.bytes_after_deepest_deferral = messages
            .iter()
            .skip(first + 1)
            .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
            .sum();
    }

    outcome
}

/// Per-block result of [`offload_tool_result`].
enum BlockOutcome {
    /// Block was rewritten to a digest. `prior` = its hash was already in the
    /// session's offload set (a re-application, not a first conversion).
    Offloaded {
        record: OffloadRecord,
        prior: bool,
        /// Original minus digest, from the counts the gate already took.
        tokens_saved: i64,
    },
    /// Block qualified but the PR-J4 gate deferred it to the next boundary.
    Deferred { bytes: usize },
    /// Block did not qualify (too small / already a digest / no text).
    Skipped,
}

/// Extract, digest, and replace one `tool_result` block's content. Returns the
/// record to persist, or `None` if the block did not qualify / did not shrink /
/// was already a digest.
fn offload_tool_result(
    block: &mut Value,
    config: &CtxOffloadConfig,
    title: &str,
    policy: Option<&OffloadPolicy>,
    is_live: bool,
    near_tail: bool,
) -> BlockOutcome {
    let Some(original) = block.get("content").and_then(tool_result_text) else {
        return BlockOutcome::Skipped;
    };

    // Idempotency fast path (I2): a block that already carries our marker is a
    // digest — pass through untouched. (Re-processing the raw block would yield
    // identical bytes anyway; this just avoids re-hashing.)
    if original.contains(MARKER_PREFIX) {
        return BlockOutcome::Skipped;
    }

    // Qualify on serialized byte length (I3: static threshold).
    if original.len() <= config.min_bytes {
        return BlockOutcome::Skipped;
    }

    let hash = compute_key(original.as_bytes());

    // Whether this block has been converted before, in this session. Read
    // before anything positional so that "already a digest" outranks every
    // other test: monotonicity (I3) is what keeps the prefix stable, and a
    // block that reverts to raw costs the same as one that converts late.
    let (prior, gate_key, budget) = gate_lookup(policy, &hash, original.len());

    // PR-J4 boundary gate: a frozen block's *first* conversion only rides a
    // rebuild boundary; re-applications (hash already in the session set) and
    // live-tail blocks always pass. See [`OffloadGate`].
    if let Some(p) = policy
        && !prior
        && !is_live
        && !near_tail
        && !p.rebuild_boundary
    {
        return BlockOutcome::Deferred {
            bytes: original.len(),
        };
    }
    // Structural compressor when one applies; otherwise a plain preview cut,
    // mirroring context-mode's behaviour (charSafePrefix + pointer). The
    // original is always retrievable by hash, so the digest only needs to
    // orient the model, not preserve information. Both arms are pure
    // functions of the block bytes (I1/I2).
    let (strategy, compressed) = compress_block_for_offload(&original);
    let body = if strategy.is_some() {
        compressed
    } else {
        preview(&original, budget)
    };
    let digest = format!("{body}{}", footer(&hash, original.len()));

    // Tokenizer gate (I6): keep the original unless the digest is strictly
    // smaller in tokens. Deterministic — pure function of the two strings.
    let tokenizer = get_tokenizer(DEFAULT_MODEL);
    let digest_tokens = tokenizer.count_text(&digest);
    let original_tokens = tokenizer.count_text(&original);
    if digest_tokens >= original_tokens {
        return BlockOutcome::Skipped;
    }
    // Strictly positive: the gate above returned on `>=`.
    let tokens_saved = (original_tokens - digest_tokens) as i64;

    // Replace content, preserving the client's shape: string stays string;
    // in an array the digest takes the first text block's place and the other
    // text blocks go. Image and document blocks stay where they were: the
    // store holds only the text, so a dropped one could never be retrieved.
    // Both are deterministic.
    match block.get("content") {
        Some(Value::String(_)) => {
            block["content"] = Value::String(digest);
        }
        Some(Value::Array(blocks)) => {
            let mut digest = Some(digest);
            let kept = blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(Value::as_str) != Some("text") {
                        return Some(b.clone());
                    }
                    digest
                        .take()
                        .map(|text| serde_json::json!({"type": "text", "text": text}))
                })
                .collect();
            block["content"] = Value::Array(kept);
        }
        // `tool_result_text` returned text, so content is a string or array.
        _ => return BlockOutcome::Skipped,
    }

    // PR-J4 (I3, monotonicity): once converted, the hash stays in the session
    // set so every later turn re-applies the offload without re-gating.
    if let Some(p) = policy {
        p.gate.record(p.session_key, &gate_key);
    }

    BlockOutcome::Offloaded {
        record: OffloadRecord {
            hash,
            original,
            title: title.to_string(),
            gate_rollback: policy
                .filter(|_| !prior)
                .map(|policy| (policy.session_key.to_string(), gate_key.clone())),
        },
        prior,
        tokens_saved,
    }
}

/// Gate-set namespace for `tool_use` conversions. A Write's `content` can be
/// byte-identical to an earlier Read's result, and the two must not share a
/// "converted before" verdict: the result's digest says nothing about whether
/// this `tool_use` block was ever forwarded as one.
const TOOL_USE_GATE_NS: &str = "tool_use:";

/// Tools whose inputs are instructions the model writes again later. A digest
/// in place of an old `Agent` prompt teaches the model to write its next
/// prompt as a cut preview plus a pointer, with a hash it invents (seen live on
/// Spark: nine truncated prompts in a row, every pointer absent from the store).
/// Their inputs stay verbatim.
const TOOL_USE_KEEP_VERBATIM: &[&str] = &["Agent", "Task", "SendMessage"];

/// Replace large string values in prior-turn `tool_use` inputs (a Write's
/// `content`, an Edit's `new_string`) with the digest [`offload_tool_result`]
/// writes: a preview plus [`footer`], keyed by the value's own hash, so
/// `headroom_retrieve` hands the original back. Every key stays; only string
/// values of at least `min_bytes` change.
///
/// Stricter than the tool_result pass, because a cached `tool_use` rewritten
/// mid-history is a recache the user pays for. A FIRST conversion touches a
/// block only where it provably sits outside every provider cache entry:
///
/// - the newest assistant message, at an index the replay store has never
///   forwarded (`forwarded_before` is the store's count; `None` means unknown,
///   which is treated as "already forwarded"); or
/// - any message, on a rebuild boundary.
///
/// There is no near-tail window. Re-applications (`prior`) run everywhere but
/// the last message, so a converted block never reverts. `put` stores the
/// original before a first conversion is committed; when it fails the block
/// stays raw, because a digest with no record behind it is a loss on its own
/// and one that cannot be re-applied next turn is a recache.
pub fn offload_tool_use_inputs(
    parsed: &mut Value,
    config: &CtxOffloadConfig,
    policy: &OffloadPolicy,
    forwarded_before: Option<usize>,
    put: &dyn Fn(&OffloadRecord) -> bool,
) -> OffloadOutcome {
    let mut outcome = OffloadOutcome::default();
    let Some(messages) = parsed.get_mut("messages").and_then(Value::as_array_mut) else {
        return outcome;
    };
    let last_idx = messages.len().saturating_sub(1);
    let newest_assistant = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
    let tokenizer = get_tokenizer(DEFAULT_MODEL);

    for (msg_idx, message) in messages.iter_mut().enumerate() {
        // The last message is the client's live tail; left alone by contract.
        if msg_idx == last_idx {
            continue;
        }
        let first_allowed = policy.rebuild_boundary
            || (Some(msg_idx) == newest_assistant
                && forwarded_before.is_some_and(|sent| msg_idx >= sent));
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let tool_name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if TOOL_USE_KEEP_VERBATIM.contains(&tool_name.as_str()) {
                continue;
            }
            let Some(input) = block.get_mut("input").and_then(Value::as_object_mut) else {
                continue;
            };
            // `preserve_order` keeps this walk in the client's key order.
            for (_, value) in input.iter_mut() {
                let Value::String(original) = value else {
                    continue;
                };
                if original.contains(MARKER_PREFIX) || original.len() < config.min_bytes {
                    continue;
                }
                let hash = compute_key(original.as_bytes());
                let (prior, gate_key, budget) = gate_lookup(
                    Some(policy),
                    &format!("{TOOL_USE_GATE_NS}{hash}"),
                    original.len(),
                );
                if !prior && !first_allowed {
                    outcome.blocks_deferred += 1;
                    outcome.bytes_deferred += original.len();
                    outcome.note_deferred_distance(last_idx - msg_idx);
                    continue;
                }
                let digest = format!(
                    "{}{}",
                    preview(original, budget),
                    footer(&hash, original.len())
                );
                let digest_tokens = tokenizer.count_text(&digest);
                let original_tokens = tokenizer.count_text(original);
                if digest_tokens >= original_tokens {
                    continue;
                }
                let record = OffloadRecord {
                    hash,
                    original: original.clone(),
                    title: tool_name.clone(),
                    gate_rollback: (!prior)
                        .then(|| (policy.session_key.to_string(), gate_key.clone())),
                };
                if !prior && !put(&record) {
                    tracing::warn!(
                        event = "ctx_offload_tool_use_put_failed",
                        tool = %tool_name,
                        "offload store rejected the original; leaving the tool_use input raw"
                    );
                    continue;
                }
                *value = Value::String(digest);
                if !prior {
                    policy.gate.record(policy.session_key, &gate_key);
                }
                outcome.blocks_offloaded += 1;
                outcome.tokens_saved += (original_tokens - digest_tokens) as i64;
                outcome.records.push(record);
            }
        }
    }
    outcome
}

/// Concatenated text of a `tool_result` `content` field. `content` is either a
/// string or an array of blocks; only `text` blocks contribute (matching how
/// the client renders tool output). `None` when there is no text at all.
pub(crate) fn tool_result_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let mut out = String::new();
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = b.get("text").and_then(Value::as_str)
                {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        _ => None,
    }
}

/// Map every `tool_use` block's id → a deterministic title (the Bash command
/// string when present, else the tool name). Used as the FTS chunk title so it
/// is byte-derived from the same request.
fn collect_tool_titles(parsed: &Value) -> std::collections::HashMap<String, (String, String)> {
    let mut map = std::collections::HashMap::new();
    let Some(messages) = parsed.get("messages").and_then(Value::as_array) else {
        return map;
    };
    for message in messages {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
            let title = block
                .get("input")
                .and_then(|i| i.get("command"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| name.to_string());
            // The title is the shell command when there is one, so it cannot
            // answer "which tool produced this"; the exclusion check needs the
            // tool name itself.
            map.insert(id.to_string(), (title, name.to_string()));
        }
    }
    map
}

#[cfg(test)]
mod tests;

// Globs cap each item at its own visibility: `pub` items stay public
// API, the rest stay in-crate. Modules with no `pub` item would
// otherwise warn that they re-export nothing.
#[allow(unused_imports)]
pub use self::{};

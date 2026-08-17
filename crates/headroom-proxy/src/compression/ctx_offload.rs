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

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use headroom_core::ccr::compute_key;
use headroom_core::tokenizer::get_tokenizer;
use headroom_core::transforms::{compress_block_for_offload, DEFAULT_MODEL};
use lru::LruCache;
use serde_json::Value;

/// Static per-request offload settings (I3: never changes mid-session).
#[derive(Debug, Clone)]
pub struct CtxOffloadConfig {
    /// A block's serialized content must exceed this many bytes to qualify.
    pub min_bytes: usize,
    /// Tools whose results the operator excluded from lossy rewriting
    /// (`--exclude-tools`).
    ///
    /// Offload is lossy from the model's point of view — the block is replaced
    /// by a preview and the rest has to be asked for — so an exclusion that
    /// only bound the live-zone compressors left the same content being
    /// swapped out here, one stage earlier. Honouring the list in both places
    /// is what makes `--exclude-tools` mean what it says. The default list
    /// covers the file and search tools for exactly this reason.
    pub exclude_tools: Vec<String>,
    /// How many messages back from the tail a block must be before an
    /// `exclude_tools` entry stops protecting it. `0` protects the whole
    /// history, which is the behaviour this field was added to relax.
    ///
    /// The exclusion above exists so the model never acts on a summary of a
    /// file it is about to edit. That argument holds for the results it is
    /// working with and weakens with every turn that passes: a file read
    /// twenty messages ago is history, the digest keeps its head, and the rest
    /// is one `headroom_retrieve` call away. Offload differs from the live-zone
    /// compressors here — it is lossy to look at but nothing is destroyed.
    ///
    /// Measured over 4,046 forwarded bodies on 2026-08-17: raw `tool_result`
    /// blocks were 22% of a 135,814-token mean prompt, and `Read` alone was
    /// 9.9% of it with not one block ever digested. Offloading only blocks this
    /// far back, at a 4,000-byte floor, recovers 11.4% of the prompt.
    ///
    /// [`headroom_core::tool_exclusion::is_verbatim_excluded`] still applies at
    /// any distance: those results break when their bytes change at all.
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
    /// So this window is a deliberate, bounded cache cost — the only one in this
    /// module. Widening it moves `tokens_after` up fast (the last 8 messages are
    /// already 3,699 tokens) and the payback with it.
    pub stale_window: usize,
}

/// One offloaded block, handed to the background worker for storage. Never
/// touched on the request path beyond construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffloadRecord {
    /// `blake3(original)[:24]` — the retrieval key embedded in the digest.
    pub hash: String,
    /// The original block content bytes (what the client sent).
    pub original: String,
    /// Deterministic chunk title for the FTS index: the paired `tool_use`'s
    /// command/tool name. Empty when no pairing was found.
    pub title: String,
}

/// Result of running the transform over one request body.
#[derive(Debug, Default)]
pub struct OffloadOutcome {
    /// Number of blocks replaced with a digest.
    pub blocks_offloaded: usize,
    /// PR-J4: qualifying frozen blocks left raw because the turn is not a
    /// rebuild boundary (they will convert at the next boundary).
    pub blocks_deferred: usize,
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
/// `REALIGNMENT/13-phase-J-history-offload.md`).
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
pub struct OffloadGate {
    /// session key → hashes already offloaded in that session. Bounded LRU so
    /// abandoned sessions age out; evicting a live session merely defers its
    /// frozen-history offloads to the next rebuild boundary (safe).
    sessions: Mutex<LruCache<String, HashSet<String>>>,
}

impl OffloadGate {
    /// # Panics
    /// Panics if `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("OffloadGate capacity must be > 0");
        Self {
            sessions: Mutex::new(LruCache::new(cap)),
        }
    }

    fn contains(&self, session: &str, hash: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions
            .get(session)
            .map(|set| set.contains(hash))
            .unwrap_or(false)
    }

    fn record(&self, session: &str, hash: &str) {
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
fn footer(hash: &str, orig_len: usize) -> String {
    format!(
        "\n{MARKER_PREFIX}{hash}>> ({orig_len} bytes offloaded; \
         retrieve: headroom_retrieve(hash=\"{hash}\"))"
    )
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
            // An operator who excluded a tool from lossy rewriting meant this
            // stage too: offload replaces the result with a preview, which is
            // exactly what the exclusion exists to prevent — while the model is
            // still working with the result. `stale_margin` messages later that
            // reasoning has expired; see the field's docs.
            let excluded = headroom_core::tool_exclusion::is_tool_excluded(
                &tool_name,
                config.exclude_tools.iter().map(String::as_str),
            );
            // Distance from the tail GROWS as the conversation does, so this
            // predicate flips from false to true under a block that is already
            // inside the cached prefix. Converting it there would rewrite the
            // prefix mid-history, which is the most expensive thing this proxy
            // can do. Two things stop that, and both are needed:
            //
            //   - the PR-J4 boundary gate below defers every first conversion
            //     of a non-live block to a turn that is rebuilding anyway, so
            //     the transition is never paid for on a steady-state turn;
            //   - `prior` is checked BEFORE this exclusion, so a block already
            //     converted stays converted. Claude Code edits its own history
            //     and the message count can fall, which would otherwise let a
            //     stale block read as fresh again and revert digest→raw. That
            //     is the same flip in the other direction and busts the cache
            //     just as hard.
            let distance = last_idx - msg_idx;
            let stale = config.stale_margin > 0 && distance >= config.stale_margin;
            let excluded_unless_prior = excluded && !stale;
            // Close enough to the tail that the rewrite it costs is small — see
            // `stale_window`. Deeper than this, a first conversion still waits.
            let near_tail = config.stale_window > 0
                && distance < config.stale_margin + config.stale_window;
            match offload_tool_result(
                block,
                config,
                &title,
                policy,
                is_live,
                excluded_unless_prior,
                near_tail,
            ) {
                BlockOutcome::Offloaded {
                    record,
                    prior,
                    tokens_saved,
                } => {
                    outcome.blocks_offloaded += 1;
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
                BlockOutcome::Deferred => outcome.blocks_deferred += 1,
                BlockOutcome::Skipped => {}
            }
        }
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
    Deferred,
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
    excluded_unless_prior: bool,
    near_tail: bool,
) -> BlockOutcome {
    let Some(original) = block.get("content").and_then(|c| tool_result_text(c)) else {
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
    let prior = policy.is_some_and(|p| p.gate.contains(p.session_key, &hash));

    // A tool the operator excluded, on a block the model may still be working
    // with. First conversions wait; conversions already made stand.
    if excluded_unless_prior && !prior {
        return BlockOutcome::Skipped;
    }

    // PR-J4 boundary gate: a frozen block's *first* conversion only rides a
    // rebuild boundary; re-applications (hash already in the session set) and
    // live-tail blocks always pass. See [`OffloadGate`].
    if let Some(p) = policy {
        if !prior && !is_live && !near_tail && !p.rebuild_boundary {
            return BlockOutcome::Deferred;
        }
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
        preview(&original, preview_budget(original.len()))
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

    // Replace content, preserving the client's shape: string stays string,
    // array becomes a single text block. Both are deterministic.
    match block.get("content") {
        Some(Value::String(_)) => {
            block["content"] = Value::String(digest);
        }
        _ => {
            block["content"] = Value::Array(vec![serde_json::json!({
                "type": "text",
                "text": digest,
            })]);
        }
    }

    // PR-J4 (I3, monotonicity): once converted, the hash stays in the session
    // set so every later turn re-applies the offload without re-gating.
    if let Some(p) = policy {
        p.gate.record(p.session_key, &hash);
    }

    BlockOutcome::Offloaded {
        record: OffloadRecord {
            hash,
            original,
            title: title.to_string(),
        },
        prior,
        tokens_saved,
    }
}

/// Concatenated text of a `tool_result` `content` field. `content` is either a
/// string or an array of blocks; only `text` blocks contribute (matching how
/// the client renders tool output). `None` when there is no text at all.
fn tool_result_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let mut out = String::new();
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
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
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(min: usize) -> CtxOffloadConfig {
        CtxOffloadConfig {
            min_bytes: min,
            exclude_tools: Vec::new(),
            stale_margin: 0,
            stale_window: 0,
        }
    }

    /// A tool_result body whose content is `body`, paired to a bash tool_use.
    fn req(body: &str) -> Value {
        json!({
            "model": "claude-3-5-sonnet-20241022",
            "messages": [
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"tu_1","name":"Bash",
                     "input":{"command":"cat big.log"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"tu_1","content": body}
                ]}
            ]
        })
    }

    fn first_tool_result_text(parsed: &Value) -> String {
        let content = &parsed["messages"][1]["content"][0]["content"];
        tool_result_text(content).unwrap_or_default()
    }

    /// `--exclude-tools` has to bind here too. Offload is lossy from the
    /// model's side — the block becomes a preview it must ask to expand — so
    /// an exclusion honoured only by the live-zone compressors still let the
    /// same content be swapped out one stage earlier.
    #[test]
    fn an_excluded_tool_is_not_offloaded() {
        let body = "ERROR: disk full\n".repeat(50);
        let mut parsed = req(&body);
        let config = CtxOffloadConfig {
            min_bytes: 200,
            exclude_tools: vec!["Bash".to_string()],
            stale_margin: 0,
            stale_window: 0,
        };
        let out = offload_anthropic_request(&mut parsed, &config, None);
        assert_eq!(out.blocks_offloaded, 0, "excluded tool must not offload");
        assert_eq!(first_tool_result_text(&parsed), body, "content untouched");
    }

    /// A conversation whose `tool_result` sits at message 1 with `tail` filler
    /// messages after it, so its distance from the tail is what the test varies.
    fn req_with_tail(body: &str, tool: &str, tail: usize) -> Value {
        let mut messages = vec![
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"tu_1","name":tool,"input":{"file_path":"/big.rs"}}
            ]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_1","content": body}
            ]}),
        ];
        for i in 0..tail {
            messages.push(json!({"role":"assistant","content":[
                {"type":"text","text":format!("step {i}")}
            ]}));
        }
        json!({"model":"claude-3-5-sonnet-20241022","messages": messages})
    }

    fn excluded_cfg(stale_margin: usize) -> CtxOffloadConfig {
        CtxOffloadConfig {
            min_bytes: 200,
            exclude_tools: vec!["Read".to_string()],
            stale_margin,
            stale_window: 0,
        }
    }

    fn big_body() -> String {
        "ERROR: disk full on volume /dev/sda1\n".repeat(200)
    }

    /// The point of `stale_margin`: a file read the model has long since moved
    /// past is history, and the bytes are retrievable, so the exclusion that
    /// protects results in play stops applying.
    #[test]
    fn an_excluded_tool_offloads_once_it_is_far_enough_back() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 6);
        let out = offload_anthropic_request(&mut parsed, &excluded_cfg(4), None);
        assert_eq!(out.blocks_offloaded, 1, "message 1 of 8 is past the margin");
    }

    /// ...and while it is still in play, nothing changes.
    #[test]
    fn an_excluded_tool_inside_the_margin_is_still_protected() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 1);
        let out = offload_anthropic_request(&mut parsed, &excluded_cfg(4), None);
        assert_eq!(out.blocks_offloaded, 0, "the model may still need this one");
        assert_eq!(first_tool_result_text(&parsed), body, "content untouched");
    }

    /// A margin of zero is the behaviour from before the field existed.
    #[test]
    fn a_zero_margin_protects_the_whole_history() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 40);
        let out = offload_anthropic_request(&mut parsed, &excluded_cfg(0), None);
        assert_eq!(out.blocks_offloaded, 0, "zero must mean off, not immediate");
    }

    /// Verbatim exclusions are not distance-sensitive: those results break when
    /// their bytes change at all, so no margin reaches them.
    #[test]
    fn a_verbatim_excluded_tool_is_never_offloaded_however_stale() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "WebFetch", 40);
        let config = CtxOffloadConfig {
            min_bytes: 200,
            exclude_tools: Vec::new(),
            stale_margin: 4,
            stale_window: 0,
        };
        let out = offload_anthropic_request(&mut parsed, &config, None);
        assert_eq!(out.blocks_offloaded, 0, "WebFetch must stay byte-faithful");
    }

    /// The cache-safety half of `stale_margin`. Distance from the tail grows,
    /// so a first conversion of a block inside the cached prefix has to wait for
    /// a turn that is rewriting the prefix anyway.
    #[test]
    fn a_newly_stale_block_waits_for_a_rebuild_boundary() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 6);
        let gate = OffloadGate::new(16);
        let policy = OffloadPolicy {
            gate: &gate,
            session_key: "sess",
            rebuild_boundary: false,
        };
        let out = offload_anthropic_request(&mut parsed, &excluded_cfg(4), Some(&policy));
        assert_eq!(out.blocks_offloaded, 0, "not on a steady-state turn");
        assert_eq!(out.blocks_deferred, 1, "deferred, not abandoned");
        assert_eq!(first_tool_result_text(&parsed), body, "prefix bytes unchanged");
    }

    /// The other half, and the one that is easy to miss: Claude Code edits its
    /// own history, so the message count can FALL and a stale block can read as
    /// fresh again. Reverting the digest would bust the prefix exactly as hard
    /// as converting it late, so a conversion already made has to stand.
    #[test]
    fn a_converted_block_stays_converted_when_the_history_shrinks() {
        let body = big_body();
        let gate = OffloadGate::new(16);
        let boundary = OffloadPolicy {
            gate: &gate,
            session_key: "sess",
            rebuild_boundary: true,
        };
        let mut deep = req_with_tail(&body, "Read", 6);
        let first = offload_anthropic_request(&mut deep, &excluded_cfg(4), Some(&boundary));
        assert_eq!(first.blocks_offloaded, 1, "converts on the boundary turn");
        let digest = first_tool_result_text(&deep);

        // Same block, now one message from the tail: inside the margin, so the
        // exclusion would apply again if `prior` were not checked first.
        let mut shallow = req_with_tail(&body, "Read", 1);
        let steady = OffloadPolicy {
            gate: &gate,
            session_key: "sess",
            rebuild_boundary: false,
        };
        let second = offload_anthropic_request(&mut shallow, &excluded_cfg(4), Some(&steady));
        assert_eq!(second.blocks_offloaded, 1, "must not revert to raw");
        assert_eq!(
            first_tool_result_text(&shallow),
            digest,
            "the same bytes must yield the same digest at any depth"
        );
    }

    fn window_cfg(stale_margin: usize, stale_window: usize) -> CtxOffloadConfig {
        CtxOffloadConfig {
            min_bytes: 200,
            exclude_tools: vec!["Read".to_string()],
            stale_margin,
            stale_window,
        }
    }

    fn gate_policy<'a>(gate: &'a OffloadGate, boundary: bool) -> OffloadPolicy<'a> {
        OffloadPolicy {
            gate,
            session_key: "sess",
            rebuild_boundary: boundary,
        }
    }

    /// Just past the margin the rewrite a conversion costs is small — the last
    /// four messages are 1,460 tokens where a qualifying block saves 2,280 — so
    /// it pays for itself in about ten turns and does not need to wait for a
    /// boundary that may never come.
    #[test]
    fn a_block_inside_the_window_converts_without_a_boundary() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 4);
        let gate = OffloadGate::new(16);
        let out = offload_anthropic_request(
            &mut parsed,
            &window_cfg(4, 4),
            Some(&gate_policy(&gate, false)),
        );
        assert_eq!(out.blocks_offloaded, 1, "distance 4 is inside the window");
        assert_eq!(out.blocks_deferred, 0);
    }

    /// Deeper than the window the same trade needs hundreds of turns, because
    /// everything after the block is rewritten. Those still wait for a turn that
    /// is rewriting anyway.
    #[test]
    fn a_block_past_the_window_still_waits_for_a_boundary() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 20);
        let gate = OffloadGate::new(16);
        let out = offload_anthropic_request(
            &mut parsed,
            &window_cfg(4, 4),
            Some(&gate_policy(&gate, false)),
        );
        assert_eq!(out.blocks_offloaded, 0, "distance 20 is far too deep");
        assert_eq!(out.blocks_deferred, 1);
    }

    /// The PR-J5 guard exists to shout when frozen history converts on a quiet
    /// turn, which the window now does on purpose. Counted together, the guard
    /// fired twice in the first 15 turns after `stale_window` shipped — and a
    /// guard that fires on correct behaviour hides the next real bug.
    #[test]
    fn a_window_conversion_does_not_trip_the_thrash_guard() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 4);
        let gate = OffloadGate::new(16);
        let out = offload_anthropic_request(
            &mut parsed,
            &window_cfg(4, 4),
            Some(&gate_policy(&gate, false)),
        );
        assert_eq!(out.blocks_offloaded, 1);
        assert_eq!(out.window_offloads, 1, "counted as a window conversion");
        assert_eq!(
            out.frozen_new_offloads, 0,
            "the guard must stay quiet for a conversion the window authorised"
        );
    }

    /// The same conversion riding a boundary instead still counts as a frozen
    /// one, so the guard keeps its teeth where the window does not reach.
    #[test]
    fn a_boundary_conversion_beyond_the_window_still_counts_as_frozen() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 20);
        let gate = OffloadGate::new(16);
        let out = offload_anthropic_request(
            &mut parsed,
            &window_cfg(4, 4),
            Some(&gate_policy(&gate, true)),
        );
        assert_eq!(out.blocks_offloaded, 1, "a boundary lets the deep one through");
        assert_eq!(out.window_offloads, 0, "too deep to be a window conversion");
        assert_eq!(out.frozen_new_offloads, 1);
    }

    /// A zero window is the boundary-only behaviour, which is the default.
    #[test]
    fn a_zero_window_waits_even_next_to_the_tail() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 4);
        let gate = OffloadGate::new(16);
        let out = offload_anthropic_request(
            &mut parsed,
            &window_cfg(4, 0),
            Some(&gate_policy(&gate, false)),
        );
        assert_eq!(out.blocks_offloaded, 0);
        assert_eq!(out.blocks_deferred, 1);
    }

    /// The window must not reopen the exclusion for a block the model is still
    /// working with: inside the margin it stays raw however near the tail it is.
    #[test]
    fn the_window_does_not_override_the_margin() {
        let body = big_body();
        let mut parsed = req_with_tail(&body, "Read", 1);
        let gate = OffloadGate::new(16);
        let out = offload_anthropic_request(
            &mut parsed,
            &window_cfg(4, 4),
            Some(&gate_policy(&gate, false)),
        );
        assert_eq!(out.blocks_offloaded, 0, "distance 1 is still in play");
        assert_eq!(first_tool_result_text(&parsed), body);
    }

    /// I1 restated for this field: position decides *whether* a block converts,
    /// never *what* it converts to. If the digest moved with depth, every turn
    /// would rewrite the prefix.
    #[test]
    fn the_digest_does_not_depend_on_position() {
        let body = big_body();
        let mut near = req_with_tail(&body, "Bash", 4);
        let mut far = req_with_tail(&body, "Bash", 40);
        offload_anthropic_request(&mut near, &cfg(200), None);
        offload_anthropic_request(&mut far, &cfg(200), None);
        assert_eq!(first_tool_result_text(&near), first_tool_result_text(&far));
    }

    /// The preview has to scale, or `min_bytes` below ~3.2KB is a no-op: a fixed
    /// 3,072-byte cut cannot shrink a 4,000-byte block.
    #[test]
    fn the_preview_budget_scales_with_the_block() {
        assert_eq!(preview_budget(100), PREVIEW_FLOOR_BYTES, "floor holds");
        assert_eq!(preview_budget(8_000), 2_000, "a quarter in the middle");
        assert_eq!(preview_budget(10_000_000), PREVIEW_BYTES, "ceiling holds");
        for len in [4_000usize, 20_000, 200_000] {
            assert!(
                preview_budget(len) < len,
                "a {len}-byte block must be able to shrink"
            );
        }
    }

    #[test]
    fn offloads_oversized_block_with_marker() {
        // Repetitive log content compresses well and is comfortably > 200 B.
        let body = "ERROR: disk full\n".repeat(50);
        let mut parsed = req(&body);
        let out = offload_anthropic_request(&mut parsed, &cfg(200), None);
        assert_eq!(out.blocks_offloaded, 1);
        assert_eq!(out.records.len(), 1);
        let text = first_tool_result_text(&parsed);
        assert!(text.contains("<<ctx:"));
        assert!(text.contains("bytes offloaded"));
        // Title is the paired bash command (deterministic).
        assert_eq!(out.records[0].title, "cat big.log");
        assert_eq!(out.records[0].original, body);
        assert_eq!(out.records[0].hash.len(), 24);
    }

    #[test]
    fn plaintext_read_output_offloads_via_preview_fallback() {
        // Simulates a Claude Code `Read` result: line-number prefixes make the
        // content classify as PlainText, where no structural compressor
        // applies (kompress is off by default). The preview fallback must
        // still offload it — this was silently a no-op before.
        let mut body = String::new();
        for i in 1..=1900 {
            body.push_str(&format!(
                "{i}\t[[package]]\nname = \"crate-{i}\"\nchecksum = \"abc{i}\"\n"
            ));
        }
        assert!(body.len() > 50_000);
        let mut parsed = req(&body);
        let out = offload_anthropic_request(&mut parsed, &cfg(50_000), None);
        assert_eq!(out.blocks_offloaded, 1);
        let text = first_tool_result_text(&parsed);
        assert!(
            text.len() < 4096,
            "digest must be small, got {}",
            text.len()
        );
        assert!(text.starts_with("1\t[[package]]"), "preview keeps the head");
        assert!(text.contains("truncated"));
        assert!(text.contains(&format!(
            "retrieve: headroom_retrieve(hash=\"{}\")",
            out.records[0].hash
        )));
        assert_eq!(out.records[0].original, body);
    }

    #[test]
    fn preview_cuts_on_char_boundary() {
        // A multi-byte char straddling the budget must not split.
        let body = format!("{}é{}", "x".repeat(3071), "y".repeat(60_000));
        let cut = preview(&body, PREVIEW_BYTES);
        assert!(cut.starts_with(&"x".repeat(3071)));
        assert!(!cut.contains('\u{FFFD}'));
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    #[test]
    fn below_threshold_is_untouched() {
        let body = "small output";
        let mut parsed = req(body);
        let out = offload_anthropic_request(&mut parsed, &cfg(50_000), None);
        assert_eq!(out.blocks_offloaded, 0);
        assert_eq!(first_tool_result_text(&parsed), body);
    }

    #[test]
    fn digest_is_deterministic_same_bytes_twice() {
        let body = "ERROR: disk full\n".repeat(50);
        let mut a = req(&body);
        let mut b = req(&body);
        offload_anthropic_request(&mut a, &cfg(200), None);
        offload_anthropic_request(&mut b, &cfg(200), None);
        assert_eq!(a, b, "same input bytes must produce identical output bytes");
    }

    #[test]
    fn already_offloaded_block_passes_through() {
        let body = "ERROR: disk full\n".repeat(50);
        let mut parsed = req(&body);
        offload_anthropic_request(&mut parsed, &cfg(200), None);
        let after_first = parsed.clone();
        // Re-running must be a no-op (idempotent): the digest already carries
        // the marker.
        let out = offload_anthropic_request(&mut parsed, &cfg(200), None);
        assert_eq!(out.blocks_offloaded, 0);
        assert_eq!(parsed, after_first);
    }

    /// The pointer has to name a tool the model can actually call. Pinned
    /// because pointing it at a shell command is what kept retrieval at zero:
    /// the content was stored and reachable, and the model was told to run
    /// something it has no way to run.
    #[test]
    fn marker_format_is_pinned() {
        let f = footer("abc123", 1234);
        assert_eq!(
            f,
            "\n<<ctx:abc123>> (1234 bytes offloaded; \
             retrieve: headroom_retrieve(hash=\"abc123\"))"
        );
    }

    #[test]
    fn array_content_shape_preserved_as_text_block() {
        let body = "ERROR: disk full\n".repeat(50);
        let mut parsed = json!({
            "model": "claude-3-5-sonnet-20241022",
            "messages": [
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"x"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"tu_1",
                     "content":[{"type":"text","text": body}]}
                ]}
            ]
        });
        let out = offload_anthropic_request(&mut parsed, &cfg(200), None);
        assert_eq!(out.blocks_offloaded, 1);
        let content = &parsed["messages"][1]["content"][0]["content"];
        assert!(content.is_array());
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"].as_str().unwrap().contains("<<ctx:"));
    }

    // ── PR-J4: boundary gate ──

    /// Request with a large tool_result in FROZEN history (not the last
    /// message) plus a trailing small live message.
    fn req_frozen(body: &str) -> Value {
        json!({
            "model": "claude-3-5-sonnet-20241022",
            "messages": [
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"tu_1","name":"Bash",
                     "input":{"command":"cat big.log"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"tu_1","content": body}
                ]},
                {"role":"assistant","content":[{"type":"text","text":"ok"}]},
                {"role":"user","content":[{"type":"text","text":"next question"}]}
            ]
        })
    }

    #[test]
    fn gate_defers_frozen_block_on_steady_state_turn() {
        let gate = OffloadGate::new(8);
        let body = "ERROR: disk full\n".repeat(50);
        let mut parsed = req_frozen(&body);
        let policy = OffloadPolicy {
            gate: &gate,
            session_key: "sess",
            rebuild_boundary: false,
        };
        let out = offload_anthropic_request(&mut parsed, &cfg(200), Some(&policy));
        assert_eq!(out.blocks_offloaded, 0);
        assert_eq!(out.blocks_deferred, 1);
        assert_eq!(out.frozen_new_offloads, 0);
        // Bytes untouched — the cached prefix is preserved (I4).
        assert_eq!(parsed["messages"][1]["content"][0]["content"], json!(body));
    }

    #[test]
    fn gate_permits_frozen_block_on_rebuild_boundary_then_stays_offloaded() {
        let gate = OffloadGate::new(8);
        let body = "ERROR: disk full\n".repeat(50);

        // Boundary turn: frozen block converts (riding the client's rebuild).
        let mut parsed = req_frozen(&body);
        let boundary = OffloadPolicy {
            gate: &gate,
            session_key: "sess",
            rebuild_boundary: true,
        };
        let out = offload_anthropic_request(&mut parsed, &cfg(200), Some(&boundary));
        assert_eq!(out.blocks_offloaded, 1);
        assert_eq!(out.frozen_new_offloads, 1);

        // Next steady-state turn: client resends raw history; the monotonic
        // set re-applies the identical offload (I3 — no flip-flop).
        let mut resent = req_frozen(&body);
        let steady = OffloadPolicy {
            gate: &gate,
            session_key: "sess",
            rebuild_boundary: false,
        };
        let out2 = offload_anthropic_request(&mut resent, &cfg(200), Some(&steady));
        assert_eq!(out2.blocks_offloaded, 1);
        assert_eq!(out2.blocks_deferred, 0);
        assert_eq!(
            out2.frozen_new_offloads, 0,
            "re-application is not a new frozen conversion"
        );
        assert_eq!(
            parsed, resent,
            "boundary turn and re-application produce identical bytes"
        );
    }

    #[test]
    fn gate_permits_live_tail_block_without_boundary() {
        let gate = OffloadGate::new(8);
        let body = "ERROR: disk full\n".repeat(50);
        // `req` puts the tool_result in the LAST message (live tail).
        let mut parsed = req(&body);
        let policy = OffloadPolicy {
            gate: &gate,
            session_key: "sess",
            rebuild_boundary: false,
        };
        let out = offload_anthropic_request(&mut parsed, &cfg(200), Some(&policy));
        assert_eq!(out.blocks_offloaded, 1);
        assert_eq!(out.frozen_new_offloads, 0);
        assert!(first_tool_result_text(&parsed).contains("<<ctx:"));
    }

    #[test]
    fn gate_offload_set_is_session_scoped() {
        let gate = OffloadGate::new(8);
        let body = "ERROR: disk full\n".repeat(50);

        // Session A converts on a boundary.
        let mut a = req_frozen(&body);
        let pa = OffloadPolicy {
            gate: &gate,
            session_key: "sess-A",
            rebuild_boundary: true,
        };
        assert_eq!(
            offload_anthropic_request(&mut a, &cfg(200), Some(&pa)).blocks_offloaded,
            1
        );

        // Session B on a steady-state turn must NOT inherit A's set.
        let mut b = req_frozen(&body);
        let pb = OffloadPolicy {
            gate: &gate,
            session_key: "sess-B",
            rebuild_boundary: false,
        };
        let out = offload_anthropic_request(&mut b, &cfg(200), Some(&pb));
        assert_eq!(out.blocks_offloaded, 0);
        assert_eq!(out.blocks_deferred, 1);
    }
}

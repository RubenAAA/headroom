# Phase J — Frozen-History Offload (CCR re-cache)

**Status:** IN PROGRESS — implementation started 2026-07-03 (`7037c705`, "CTX-3
deterministic tool_result offload transform") and has been tuned since; latest
offload commit `4f223054` (2026-08-19). The threat model below was accepted: the
prefix mutation is recoverable and gated behind ctx mode.
Shipped so far: `crates/headroom-proxy/src/compression/ctx_offload.rs` (the
transform), `crates/headroom-proxy/src/ctx/offload_store.rs` (the store),
`crates/headroom-proxy/src/observability/ctx_offload_by_tool.rs` (per-tool
counters), covered by `crates/headroom-proxy/tests/ctx_cache_stability.rs`.
In flight, uncommitted: `crates/headroom-proxy/src/bin/offload_replay.rs`
(offline corpus replay through the real transform) and
`crates/headroom-proxy/src/memory/deferred.rs` (holding memory answers for the
next request), plus the gap analysis in `bench/HANDOFF-offload-gap.md`.
Note: the shipped marker is `<<ctx:HASH>>`, not the `<<ccr:HASH>>` this design
sketched — read the marker syntax below as a design draft.
**Owner:** RubenAAA (fork initiative; not part of the original A–I realignment audit).
**Depends on:** Phase B (live-zone engine), Phase E (cache-stabilization detectors —
specifically `drift_detector`), the CCR store (`headroom_core::ccr`).
**Goal:** Reclaim ~15–25% input-token cost on *long* agentic sessions by moving
stale, already-consumed `tool_result` blocks out of the frozen cache prefix into
retrievable `<<ccr:HASH>>` markers — **without cache thrashing and without
information loss.**

---

## 1. Problem

Today (Phase B) headroom compresses **only the latest user message above the
frozen floor** (`live_zone.rs:646`). The frozen prefix — system, tools, and the
entire `tool_result` history of file reads / command output — is **byte-preserved
forever**. That is the correct default: re-compressing cached bytes busts the
prompt cache for a net loss.

But on a long coding session the frozen prefix is where ~90% of the tokens live,
and it grows unbounded. Prompt caching makes cached reads cheap (0.1×), yet:

- The prefix keeps **growing** every turn (each new file read is appended, then
  frozen). Cached or not, a 60 k-token prefix read at 0.1× still costs more than
  a 25 k-token one.
- The prefix is **periodically rebuilt anyway** — the Phase-E diagnostic (this
  session) measured `system,tools,early_messages` drift every ~3–15 min, i.e.
  Claude Code re-emits and re-writes the whole prefix on a regular cadence.

Phase J shrinks that prefix by replacing stale `tool_result` bodies with compact
retrieval markers the model can expand on demand.

---

## 2. The cache-write math (why naïve offload loses)

Anthropic prompt-cache pricing (relative to base input = 1.0×):

| Operation | Multiplier |
|---|---:|
| Uncached input token | 1.00× |
| Cache **write** (5-min TTL) | 1.25× |
| Cache **read** | 0.10× |

Let the frozen prefix be `P` tokens, shrinking to `P'` after offload (`P' < P`,
`Δ = P − P'`).

**Naïve "offload every turn":** each turn the prefix bytes change → cache miss →
pay a fresh **write** on `P'` (1.25·P') instead of a **read** on `P` (0.10·P).
For any realistic `P' > 0.08·P` this is **strictly worse**. Offloading on a cadence
that re-writes the cache every turn means you *never* collect a cache hit — it
converts cheap reads into expensive writes. This is the trap.

**Break-even for a one-time offload:** an offload that holds for `n` subsequent
cached turns is worth it iff

```
   one-time extra write cost      <      ongoing savings
   1.25·P'  −  0.10·P             <      n · 0.10·Δ
```

`n` (turns before the *next* natural prefix rebuild) is the lever — and we
measured `n` is **small** (a rebuild every few minutes). A small amortization
window is what makes a free-standing offload marginal.

---

## 3. Core insight: offload **only at a natural cache-rebuild boundary**

The amortization problem disappears if we **stop paying for the write separately.**

The Phase-E `drift_detector` already fires `cache_drift_observed` the moment the
client rebuilds `system`/`tools` — i.e. the moment the cache **is already being
re-written for free, on the client's dime.** If headroom performs its offload
*on exactly that request*, the write of the shrunken prefix replaces a write that
was going to happen anyway. Marginal extra write cost ≈ `1.25·(P'−P) < 0` (we are
writing *fewer* tokens than the client's own rebuild would have). Every turn after
that, until the next rebuild, reads the smaller prefix at 0.10×.

**Policy in one line:** *offload stale history only on a turn where
`drift_detector` reports a `system`/`tools` rebuild; never on a steady-state
cached turn.* This makes Phase J nearly-free by construction and uses
infrastructure that already exists.

(Steady-state turns still get Phase B live-zone compression as today — Phase J is
purely additive and only engages at rebuild boundaries.)

---

## 4. Architecture

```
inbound request
   │
   ├─ drift_detector.observe()  ──► rebuild_boundary? ──┐  (Phase E, existing)
   │                                                    │
   ├─ live_zone compress (latest msg)                   │  (Phase B, existing)
   │                                                    ▼
   ├─ IF rebuild_boundary AND policy.eligible():   offload_frozen_history()   ← NEW (J2)
   │       • select eligible stale tool_result blocks
   │       • replace body with <<ccr:HASH>> marker (deterministic)
   │       • store original under HASH (session-pinned)            ← J1 store changes
   │
   ├─ inject headroom_retrieve tool def (if any marker present)    ← NEW (J3)
   │
   └─ forward upstream
          ▲
response ──┤
          └─ intercept headroom_retrieve tool_use → serve original from store  ← NEW (J3)
```

Reuses: `ccr::CcrStore`, `ccr::compute_key`/`marker_for`, `drift_detector`,
`compute_frozen_count`. New code is the offload selector, the retrieve-tool
subsystem, and the store-lifetime changes.

---

## 5. Hard dependency: the retrieve-tool subsystem (does not exist in Rust)

Confirmed by audit: `grep headroom_retrieve crates/headroom-proxy/src` returns
**nothing**. The Python proxy (`headroom/ccr/tool_injection.py`) owns tool
injection + serving; the Rust live-zone port only *emits* markers and *tracks*
retrieve call-ids to avoid re-compressing their output. **Offloading content the
model cannot retrieve = silent information loss = wrong answers.** Therefore J3 is
a *blocking* prerequisite, not an enhancement. It must:

1. **Inject** a `headroom_retrieve` tool definition into `tools[]` whenever the
   outgoing request contains ≥1 `<<ccr:HASH>>` marker. Schema: `{ hash: string }`.
2. **Intercept** the model's `tool_use` for `headroom_retrieve` in the response
   stream, look up `HASH` in the store, and return the original bytes as the
   `tool_result` — *without* forwarding that call upstream to the user's tools.
3. **Strip** the injected tool from any path that would surprise the client, and
   never let the injected tool collide with a user tool of the same name
   (namespace as `mcp__headroom__headroom_retrieve`, matching the live-zone
   call-id convention at `live_zone.rs:2478`).

Note: injecting a tool **changes `tools[]`** → a one-time cache write. By §3 we
only do this on a rebuild boundary, so it rides the free write. Once injected it
must remain **byte-stable** every subsequent turn (deterministic schema) so it
does not itself become a drift source.

---

## 6. Eligibility — what may be offloaded, and what NEVER

**Eligible** (all must hold):
- Block is a `tool_result` (file read, command output, search dump).
- Block index `< frozen_message_count` (already frozen; not the live zone).
- Block is older than `K_RECENT` turns (default **3**) — never touch the model's
  short-term working set.
- Block body ≥ `MIN_OFFLOAD_TOKENS` (default **512**) — small blocks aren't worth
  a retrieval round-trip.
- Block has not been referenced by a later assistant turn since it was produced
  (best-effort: no later `tool_use`/text mentions its `tool_use_id` or a stable
  substring). Conservative: when unsure, **do not** offload.

**NEVER eligible:**
- `system`, `tools` (the model reads these directly; no retrieval path).
- Anything in the live zone (≥ frozen floor).
- The most recent `K_RECENT` turns.
- `thinking` / `tool_use` blocks (only `tool_result` *bodies*).
- Any block whose offload would leave the conversation structurally invalid
  (orphaned `tool_use` without its `tool_result`, etc.).

---

## 7. Determinism & monotonicity (the anti-thrash invariant)

The client (Claude Code) does **not** know headroom offloaded anything — it
re-sends the *full* original history every turn. So headroom must **re-apply the
same offload deterministically on every subsequent turn** to keep the prefix
small. Requirements:

- **Deterministic marker bytes:** `HASH = compute_key(original_bytes)` → identical
  input always yields the identical marker. Same history ⇒ same offloaded bytes
  ⇒ cache-stable *after* the boundary write. (No timestamps/counters in the
  marker — `<<ccr:HASH N_rows_offloaded>>` style is fine; `N` is derived, stable.)
- **Monotonic set:** once a block is offloaded in a session it stays offloaded;
  the eligible set only grows. Prevents flip-flop (offload → restore → offload)
  which would re-thrash the cache.
- **Idempotent re-application:** offloading an already-offloaded request (marker
  already present) is a no-op.

---

## 8. Store lifetime (information-loss surface)

The CCR store today: `DEFAULT_TTL = 1800s` (30 min), `DEFAULT_CAPACITY = 1000`,
LRU eviction (`ccr/mod.rs:60-66`). **Both are information-loss bugs for offload:**

- A 30-min TTL can expire an offloaded block mid-session → model retrieves →
  `None` → lost context. **Fix:** offloaded entries must be **session-pinned**
  (no TTL, or TTL ≥ session lifetime) and exempt from LRU eviction, OR the store
  must be sized/scoped per session. Required design decision (J1).
- LRU capacity 1000 can evict an offloaded block under pressure. Same fix.

Failure mode if a retrieval misses: **fail *open*, not closed** — if the store
returns `None`, headroom must **not** have offloaded that block, or must restore
the original on the next turn. Never serve an empty/placeholder `tool_result` for
a real retrieval; that silently corrupts the model's context.

---

## 9. Threat model

| # | Threat | Vector | Mitigation |
|---|---|---|---|
| T1 | **Information loss** | Offloaded block expires/evicts before retrieval | §8 session-pinned, eviction-exempt store; fail-open restore |
| T2 | **Retrieval failure** | Model retrieves a `HASH` headroom never stored | Only inject the tool when markers present; store-before-marker ordering; `None` ⇒ restore original next turn |
| T3 | **Cache thrash (self-DoS)** | Non-deterministic markers / non-monotonic set re-write cache every turn | §7 determinism + monotonicity invariants; offload only at rebuild boundary (§3) |
| T4 | **Secret leakage into store** | Tool_result contains a Tier-1 secret; stored in CCR | Store inherits the same redaction/never-log rules; store is in-proc, not logged; markers carry no payload |
| T5 | **Marker spoofing** | Client/model emits a fake `<<ccr:HASH>>` to read another session's data | Store is **session-scoped** (keyed by `derive_session_key`); a HASH from session A is not resolvable in session B |
| T6 | **Session/tenant bleed** | Shared store returns another session's payload for a colliding HASH | Per-session namespace (T5); HASH includes session salt if store is shared |
| T7 | **Structural corruption** | Offloading orphans a `tool_use`/`tool_result` pair | §6 structural-validity gate; re-parse + validate before forwarding (mirror CodeCompressor's syntax-guard pattern) |
| T8 | **Replay / non-idempotent retrieve** | Same retrieve served twice diverges | Retrieval is a pure store read; idempotent by construction |
| T9 | **Injected-tool collision** | `headroom_retrieve` shadows a user tool | Namespaced name `mcp__headroom__headroom_retrieve` (§5) |

**Fail-closed posture for offload, fail-open for retrieval:** when *eligibility*
is uncertain, do not offload (lose savings, keep correctness). When *retrieval*
fails, restore the original (lose savings, keep correctness). Correctness always
beats the token win.

---

## 10. Invariants (must hold or the feature is off)

- **I1 — Recoverability:** every offloaded byte is retrievable by the model for the
  session lifetime. No lossy offload, ever.
- **I2 — Determinism:** identical input history ⇒ identical outgoing bytes.
- **I3 — Monotonicity:** the offloaded set never shrinks within a session.
- **I4 — Boundary-only writes:** Phase J never mutates the prefix on a steady-state
  cached turn — only on a `drift_detector` rebuild boundary.
- **I5 — Sacred sub-zones untouched:** `system` and `tools` content is never
  offloaded (only the injected retrieve *tool def* is added, deterministically).
- **I6 — Structural validity:** the post-offload request re-parses to a valid
  provider envelope with no orphaned tool pairs.

A property test per invariant; a kill-switch flag (`--enable-history-offload`,
default **off**) so the feature can be disabled instantly in prod.

---

## 11. PR breakdown

| PR | Branch | Risk | LOC | Summary |
|---|---|---|---:|---|
| **J0** | `feat/J0-offload-simulator` | LOW | +400 | **Run first.** Offline simulator + parameter sweep (below). Decides the winning config *before* a line of production code. No proxy changes; pure analysis over recorded sessions. |
| **J1** | `feat/J1-ccr-store-session-pinned` | MED | +250 | Session-scoped, eviction-exempt, no-TTL store mode for offload entries (§8). Property tests for no-evict + session isolation (T5/T6). |
| **J2** | `feat/J2-frozen-offload-selector` | **HIGH** | +500 | Eligibility selector (§6) + deterministic marker rewrite (§7) + structural-validity guard (§6/I6). Pure function over a parsed body; no I/O. Heavy property/fixture tests. |
| **J3** | `feat/J3-retrieve-tool-subsystem` | **HIGH** | +600 | Inject `headroom_retrieve` tool when markers present; intercept its `tool_use` in the response stream; serve from store; namespacing (T9). The blocking prerequisite (§5). |
| **J4** | `feat/J4-boundary-gated-policy` | MED | +200 | Wire J2 to fire **only** on a `drift_detector` rebuild boundary (§3); `--enable-history-offload` kill-switch; monotonic per-session offload-set state (I3). |
| **J5** | `feat/J5-offload-observability` | LOW | +150 | `history_offload_applied{blocks, tokens_freed, prefix_before, prefix_after}` event; retrieval hit/miss counters; thrash guard (warn if offload fires on a non-boundary turn). |
| **J6** | `feat/J6-offload-e2e` | MED | +300 | End-to-end: simulated long session, assert prefix shrinks, cache-write count does NOT increase vs baseline, retrieval round-trips byte-exact, zero information loss across TTL boundary. |

**Order:** **J0 (eval) →** J1 → J3 → J2 → J4 → J5 → J6. (J3 before J2 so nothing is
ever offloaded without a working retrieval path — I1 by construction.) ~2,400 LOC,
~2–3 weeks after J0 picks the design.

### Phase J0 — empirical evaluation: sweep before build

Per "try everything and measure," J0 builds a cheap **offline simulator** so we
compare design variants on *real recorded sessions* before committing to the
build — then build only the winning combination.

**Corpus (what it needs):** a handful of recorded sessions captured via an
env-gated request-body dump (`HEADROOM_CAPTURE_DIR`, default off, **never logs
Tier-1** — see T4). One short, one long, one code-heavy. ~30 lines + a rebuild;
the dump is a **pure observer** (no body mutation — preserves the sacred
invariant, same posture as the Phase-E detectors).

**Simulator:** a Rust test-bin that replays each captured session turn-by-turn
and, for a given config, computes **without touching the network**: prefix token
count over time; cache-write vs cache-read tokens (using the measured
`drift_detector` cadence as the rebuild signal); blocks offloaded; retrieval
events; **net token cost vs the Phase-B baseline.**

**Benchmarkable knobs — SWEEP these, the simulator ranks them:**

| Knob | Range |
|---|---|
| `MIN_OFFLOAD_TOKENS` | 256 / 512 / 1024 / 2048 |
| `K_RECENT` (protected recent turns) | 2 / 3 / 5 / 8 |
| offload **trigger** | boundary-only (§3) / every-N-turns / size-threshold / hybrid |
| offload **depth** | largest-1 / top-k-by-size / all-eligible |

Output: a ranked table `(config → net savings, cache-write delta, retrieval count)`
per session **and** aggregated, so we pick the winning **combination** on evidence.

**NOT benchmarkable — design calls, not token metrics (DECIDE, don't sweep):**
- **Store scope** (per-session vs shared+salt) — identical token math; a
  correctness/complexity choice settled by T5/T6.
- **Provider/streaming v1 scope** — a *coverage* choice (what fraction of sessions
  are eligible), measured as coverage %, not "better savings."

The distinction is the point: "try all and measure" applies to the four knobs
above; the architecture choices are settled by the threat model, not a sweep.

---

## 12. Acceptance / evidence (Definition of Done)

- All six invariants (§10) covered by green property tests.
- J6 e2e proves, on a recorded long session: (a) prefix token count strictly
  decreases after a boundary offload; (b) total **cache-write tokens do not exceed
  baseline** (proves §3 — we ride the free write); (c) every offloaded block
  retrieves byte-exact; (d) a forced store-expiry triggers fail-open restore, not
  a corrupt `tool_result`.
- Threat-model table (§9) each mapped to a test or an explicit accepted-risk note.
- Kill-switch verified: `--enable-history-offload=false` ⇒ byte-identical to
  today's Phase B output.

---

## 13. Failure modes & rollback

- **Rollback:** flip `--enable-history-offload` off → instant revert to Phase B
  behavior (I4/kill-switch). No migration, no persisted state to unwind (store is
  in-proc).
- **Degradation:** store pressure / OOM → stop offloading (eligibility fails
  closed), keep serving full prefix. Worst case = today's cost, never worse.
- **Observability trip:** J5 warns if any offload write lands on a non-boundary
  turn (an I4 violation = a thrash bug); treat as a page-worthy regression.

---

## 14. Open questions (need decision before J1)

1. `[DECIDE — T5/T6]` **Store scope:** per-session in-proc map vs the existing
   shared LRU with a session salt? Identical token math, so not a J0 sweep —
   settled by the threat model (per-session is simpler for isolation but needs
   session GC).
2. `[BUILD — J4]` **Boundary detection coupling:** `drift_detector` currently only
   *observes* (returns nothing actionable). J4 needs it to *return* a
   `rebuild_boundary` signal — a small, safe extension (still no body mutation in
   the detector).
3. `[SWEEP — J0]` **`K_RECENT` / `MIN_OFFLOAD_TOKENS` / trigger / depth** — the four
   benchmarkable knobs (§11 J0 table). Picked by the simulator on the recorded
   corpus, not by reasoning.
4. `[DECIDE — coverage]` **Streaming retrieval:** serving `headroom_retrieve`
   mid-stream requires injecting a synthetic `tool_result` turn — confirm this
   composes with the existing SSE tee paths (`vertex/raw_predict.rs`, bedrock
   streaming) or restrict v1 to non-streaming / Anthropic-only. A coverage choice
   (J0 can *measure* the % of sessions each scope would cover, but it's not a
   savings sweep).
5. `[DECIDE — process]` **Upstream-ability:** is this a fork-only feature or a
   candidate for the upstream REALIGNMENT sequence? Affects branch/commit
   conventions.

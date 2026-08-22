# TODO: Recache Classification & Warning Behavior

**Created:** 2026-07-07 | **Updated:** 2026-07-09
**Status:** Implemented 2026-07-09 — `RecacheEventKind` (Drift/Expected) in `usage_observer.rs`, `event_kind` on `/cache-health`, two-window ⚠/ℹ statusline, Prometheus `unknown` reason relabelled `expected`. Not done: `compression_applied` plumbing (open question, not needed for the classification rule).

---

## Problem

The statusline recache warning (`⚠ recache Xm ago: ...`) fires for ALL cache invalidation events with equal severity and 10-minute persistence. Many of these are **false alarms** — not actually wasted tokens — and should not be warnings.

**Root cause:** Subagent closing or `/clear` resets conversation context, causing the usage observer to see a cache bust. But the structural hashes are identical (no drift), so the "wasted tokens" are not actually wasted — the cache was legitimately invalidated by the session ending. The cause is known and should be explained to the user, not shown as "unknown cause".

## Findings

### Log Analysis (2026-07-07, 129 total recache events)

| Category | Count | drift_dims | Cause |
|----------|-------|------------|-------|
| Drift (genuine structural change) | ~100+ | non-empty | system, tools, early_messages changed |
| Unknown (volatile content) | 20+ | empty | UUIDs, timestamps in messages[3+] |
| Compression-triggered | varies | varies | Compression modifies prefix → cache bust |
| Subagent closing | varies | empty | Conversation context reset when subagent finishes |
| `/clear` command | varies | empty | Conversation context reset by user |

**Key discovery:** 20+ "unknown" events have empty `drift_dims` — the drift detector only hashes system, tools, and first 3 messages. Root causes are **subagent closing** and **`/clear` command**: when a subagent finishes or the user runs `/clear`, the conversation context resets, causing the usage observer to see a cache bust — but the structural hashes are identical (no drift). The "wasted tokens" are not actually wasted; the cache was legitimately invalidated by the session ending. The cause is known and should be explained to the user, not shown as "unknown cause".

### Live Events (2026-07-08)

**Event 1 (~10:04 UTC) — Large waste**
```
wasted_tokens: 70,985
expected_cache_read: 90,939
actual_cache_read: 19,954
drift_dims: null
conversation_key: 32b2ea8742d2a21f
```

**Event 2 (~10:25 UTC) — Small waste**
```
wasted_tokens: 241
expected_cache_read: 58,457
actual_cache_read: 58,216
drift_dims: null
conversation_key: 2e7b2826f3f6d3a1
```

**Event 3 (~10:31 UTC) — Large waste, same conversation as Event 2**
```
wasted_tokens: 115,391
expected_cache_read: 133,816
actual_cache_read: 18,361
drift_dims: null
conversation_key: 2e7b2826f3f6d3a1
```

**Root cause:** Subagent closing. When a subagent finishes, the conversation context resets, causing the usage observer to see a cache bust — but the structural hashes are identical (no drift). The "wasted tokens" are not actually wasted; the cache was legitimately invalidated by the session ending.

**Event 4 (~10:45 UTC) — `/clear` command**
```
wasted_tokens: 97,000 (approx)
conversation_key: unknown (conversation reset)
```

**Root cause:** `/clear` command resets conversation context. Same mechanism as subagent closing — the cache is legitimately invalidated by the session ending, not by structural drift. The "unknown cause" label is misleading; the cause is known and should be explained to the user.

### Anthropic overloaded_error (2026-07-07 ~21:53-21:55)

Transient capacity errors from upstream, not proxy bugs. Request IDs: `d656d8fc`, `534fa2ab`, `2b2d0ba4`.

---

## Proposed Classification

| Condition | Level | Icon | Persistence |
|-----------|-------|------|-------------|
| `drift_dims` is not empty | Warning | ⚠ | 180s (3 min) |
| `drift_dims` is empty | Info | ℹ | 60s (1 min) |

**Rationale:**
- Non-empty `drift_dims` = genuine structural change detected (system, tools, early_messages)
- Empty `drift_dims` = subagent closing, `/clear` command, volatile content in later messages, terminal close — expected behavior, **not actually wasted tokens**
- **The cause is known** for subagent closing and `/clear` — should be explained to the user, not shown as "unknown cause"

---

## Implementation Plan (from session 2026-07-07)

### Files to modify

1. **`crates/headroom-proxy/src/cache_stabilization/usage_observer.rs`**
   - Add `RecacheEventKind` enum: `Drift` | `Expected`
   - Add `compression_applied: bool` to `PendingRequest` and `begin_request()`
   - Classify events in `complete()` based on drift_dims presence
   - Add `event_kind` field to `RecacheEvent` struct

2. **`crates/headroom-proxy/src/proxy.rs`**
   - Pass compression outcome to `begin_request()` (currently not passed)
   - Gate: drift detection runs BEFORE compression (line 1728), compression runs after

3. **`scripts/statusline-cache-health.sh`**
   - Two persistence windows: `RECACHE_DRIFT_WINDOW=180`, `RECACHE_EXPECTED_WINDOW=60`
   - Conditional icon: ⚠ for Drift, ℹ for Expected

4. **`crates/headroom-proxy/src/observability/recache.rs`**
   - Update Prometheus labels if needed (currently `reason` label: system/tools/early_messages/multi/unknown)

5. **Tests**
   - Update existing tests in `usage_observer.rs`
   - Add new test cases for Expected vs Drift classification

### Open Questions

- [ ] **Subagent hypothesis:** Do recache events with `drift_dims=null` correlate with subagent closings? Need to check logs for `subagent` or `actor` events near recache timestamps.
- [ ] **`/clear` explanation:** How should the statusline explain that `/clear` caused the cache drop? e.g. `ℹ cache cleared by /clear command`
- [ ] Does the proxy already know whether compression was applied on a given request? If so, that signal can tag the recache event. If not, need to add it.
- [ ] Should the drift detector window be extended (e.g. first 10 messages) to catch more volatile content cases?

---

## Current State (2026-07-08 ~10:45 UTC)

- **Total recache events:** 7+ (since restart)
- **Total wasted tokens:** ~392K+
- **Hit rate:** 94.2% (50 samples)
- **Last event:** ~97K tokens wasted from `/clear` command
- **Previous events:** 115K (subagent closing), 241 tokens (same conversation), 70K (different conversation)
- **TTL expiries:** 0
- **Known false alarm causes:** subagent closing, `/clear` command

---

## How to Verify the Fix

1. **Restart proxy** after implementing changes
2. **Trigger compression** (send large JSON) → should show `ℹ cache drop` (not `⚠ recache`)
3. **Wait 1 minute** → info message disappears
4. **Trigger drift** (change tools/system mid-conversation) → should show `⚠ recache`
5. **Wait 3 minutes** → warning disappears
6. **Check `/cache-health`** endpoint — `last_event.event_kind` should be `Drift` or `Expected`

## How to Reproduce the Current Bug

- **Subagent closing:** Spawn a subagent, let it finish, observe `⚠ recache` in statusline
- **`/clear` command:** Run `/clear` in Claude Code, observe `⚠ recache` in statusline
- **Volatile content:** Any conversation with UUIDs, timestamps in messages beyond index 3
- The proxy currently shows `⚠ recache` for these — should be `ℹ cache drop` (not actually wasted tokens)
- The cause is known and should be explained to the user, not shown as "unknown cause"

---

## Related Issues

- P3-35 in `REALIGNMENT/01-bug-list.md` — No cache-bust drift detector telemetry
- P3-35a (added 2026-07-08) — Drift detector blind spot: `drift_dims=null` recache events (likely false alarms from subagent closing)
- **`/clear` command false alarm:** User-triggered `/clear` resets conversation context, causing recache warning with "unknown cause" — should be info level with explanation

---

# Hypothesis ledger — 2026-08-22

The July analysis above named three causes without measuring how much each
accounts for. This section is a running ledger instead: one entry per
hypothesis, each carrying its status and the evidence that put it there. A
refuted entry stays in the file. The point is that nobody tests it twice.

**Measurement window.** Proxy PID 56134, started 13:21 local on 2026-08-22
(09:21 UTC — the log timestamps in UTC and the process start prints local,
which is worth knowing before writing any filter over it). 502 requests, 27
recache events, two conversations involved. `restart-headroom.sh` does not
roll `~/headroom-proxy.log`, so anything read from that file without a
timestamp filter mixes in older builds.

**The whole loss, this window: 14,207 tokens across 20 turns.** Small. Worth
sizing before anyone spends a week on it.

| Attribution | Events |
|---|---|
| `unexplained_after_replay` | 20 |
| `prefix_content_diverged` | 4 |
| `aftershock_of_diverged_prefix` | 2 |
| `early_messages` | 1 |

## H1 — The proxy moves the cache hot zone. REFUTED

The reason `outbound_drift_state` and `observe_outbound_drift` were built:
the inbound hash is taken before any stage runs, so proxy-caused movement in
`system`, `tools` or the first three messages could not appear in
`drift_dims`.

It is not happening. Across 502 requests the outbound detector logged 8
first-request events and 2 drift events, and both drift events paired 1:1
with an inbound drift on the same session about 60ms earlier — the client
moved, and we carried it. The `origin: "proxy"` branch has never fired.

The instrument works; the answer is no. Keep it — it is what lets the next
person skip this hypothesis in one query.

## H2 — Volatile content (UUIDs, timestamps) deep in history. REFUTED as the main cause

The July table blamed "UUIDs, timestamps in messages[3+]" for 20+ unknown
events. Measured: 3 of 27 recached turns carried any volatile finding, against
a base rate of 15/502 (3%) across all requests. Enriched roughly fourfold, so
the effect is real, but it is six findings and cannot account for twenty
events.

Locations on recached turns ran `messages[16]` to `messages[107]` — all past
the hot zone, so widening the drift hash to cover them would explain three
events and no more.

## H3 — Subagent close or `/clear` resets the conversation. REFUTED for these events

The July hypothesis. It does not fit this window. All 20 events land
mid-conversation with message counts growing monotonically (3, 15, 19, 40,
48, 59, 67, 73, 81, 91, 97, 111, 113, 121, 123) and an active prefix-replay
chain reaching `chain_id` 25. A context reset would restart that chain, not
deepen it.

It may still explain events in other windows. It does not explain these.

## H4 — Something writes a cache block that is never read. OPEN, and the strongest lead

`classify_turn` (`usage_observer.rs:385-404`) computes
`wasted = min(prev.read + prev.write − read, write)`.

In 18 of 20 events `wasted_tokens` equals a `cache_creation` figure exactly:

- **6 events** match the *previous* turn's write (243, 239, 240, 239, 209,
  248). Exact equality here means `cache_read` came back precisely equal to
  the previous turn's read — the block written last turn was not read this
  turn, at all. That is a block paid for and never used.
- **12 events** equal *this* turn's write, which is the clamp in the formula
  binding, so they only tell us the shortfall was at least that large.

The six exact matches are the sharp signal, and the repeated ~240-token
figure suggests one fixed block rather than a drifting prefix.

## Measured and uniform, so not discriminating

Prefix replay's stable window ends exactly one message short of the end
(`total − stable == 1`) on all 20 events. That is by design — the newest
message is new — and it holds on clean turns too, so it separates nothing on
its own. Recorded here so it is not mistaken for a finding.

Unexplained turns are *shorter* conversations than average (median 67
messages against 308) with higher output (691 tokens against 273). Neither
has an explanation yet.

## Open, untested

- **H5 — proxy injection into the latest user message breaks the next turn's
  prefix.** We mutate the newest message; the client resends it unmutated
  next turn. Prefix replay exists to paper over exactly this, and these
  events are all `unexplained_*after_replay*`, so if it is the cause then
  replay is not restoring what it should. Needs captured request bodies to
  test; log fields cannot settle it.
- **H6 — the second tail breakpoint writes a block nothing reads.** The proxy
  runs `--cache-tail-breakpoints 2`. A marker at the very tail creates a block
  holding the newest content, and if the next turn cannot reuse it the cost
  repeats every turn. Fits the constant ~240-token figure. Also needs
  captured bodies.

`HEADROOM_CAPTURE_DIR` is unset on the running proxy, so no request bodies
exist for this window. Testing H5 or H6 means enabling capture and waiting
for a recurrence.

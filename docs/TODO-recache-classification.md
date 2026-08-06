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

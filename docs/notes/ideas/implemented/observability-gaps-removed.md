# Implemented: August observability gaps removed

- **Status:** closed 2026-08-12 (item 12 family)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §12 + `proxy-experiments-closures.md`


## Fix 12.1/13

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **12.1/13:** added warnings for malformed CCR hashes and CCR tool calls with
  missing identifiers in `headroom-core`. Retrieval behavior is unchanged, but
  failed validation and unmatchable results now leave an operator-visible trace.


## Recache session key

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **Recache events now carry a session key (2026-08-09).** `begin_request` takes
  it and `PendingRequest` parks it, on both the Anthropic and routed paths, so
  `cache_recache_observed` joins to the drift and volatile events directly
  instead of through time. The value is the drift detector's own
  `session_key_log_prefix(session_key)` — not a re-derivation, and emphatically
  not the earlier attempt's `hash(conversation_key)`, which was the same field
  name holding a different number and joined to nothing. A test asserts the
  field reaches the emitted event, and a second asserts a request that never
  reached the drift gate prints it empty rather than inventing one.


## Fix 12.3/12.4/12.7

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **12.3/12.4/12.7:** semantic-cache hits, misses, TTL expiry, and capacity
  eviction are logged; memory FTS cleanup failures and in-memory CCR capacity
  evictions are now operator-visible.


## Fix 12.2

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **12.2:** CCR context tracker capacity evictions now emit the evicted hash,
  configured capacity, and resulting size.


## Fix 12.3 ctx

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **12.3:** CTX offload persistence now logs CCR and FTS outcomes separately,
  including partial persistence.


## Fix 12.6

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **12.6:** `ctx purge` now propagates chunk-delete errors instead of reporting a
  false zero count.


## Fix 12.7

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **12.7:** Codex rate-limit shape misses now emit a warning rather than silently
  returning `None`.


## Item 12 audit

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 12. Code paths with no instrumentation at all

An audit of `headroom-proxy` and `headroom-core` looking for silent paths
rather than reading metrics. Ranked by risk. None of these are confirmed
faults — they are places where a fault would leave no trace.

1. **`headroom-core/src/ccr/response_handler.rs` — zero tracing calls in
   ~700 lines.** `parse_ccr_tool_calls` (:199), `extract_tool_calls` (:108)
   and `create_tool_result_message` (:244) drive the CCR retrieval round-trip
   shipped in `35b75455`. A hash mismatch, an unresolvable tool-call id
   (`unwrap_or("")` at :214-228) or a retrieval returning empty content
   produces no log line. Newest code, least tested, completely dark.
2. **`headroom-core/src/ccr/context_tracker.rs` — zero tracing calls.** LRU
   eviction at :174-179 silently drops tracked contexts past
   `max_tracked_contexts`. If proactive expansion stops matching because
   entries were evicted early, the only symptom is a slow decline in savings
   numbers that are themselves estimates.
3. **`headroom-proxy/src/ctx/offload_store.rs:120-149` `persist_one`** writes
   to the CCR store and the FTS index independently, then returns `ccr_ok` as
   the overall result. A half-failure is not distinguished anywhere. Since
   `/ctx/get` reads only CCR and `/ctx/search` only the index, a CCR-side
   failure leaves a record that is searchable but not retrievable, with no
   line saying which half failed.
4. **`headroom-proxy/src/semantic_cache.rs`** is invisible except at two call
   sites in `proxy.rs`. No signal on eviction (:80-88) or TTL expiry
   (:148-154). An undersized `max_entries` that evicts on every insert would
   look exactly like a cache that never helps.
5. **`headroom-proxy/src/memory/ctx_backend.rs:211-232`** — `delete_memory`
   and `clear_user` re-index with `let _ = self.index.index_content(...)`.
   An index failure after a successful record delete leaves an orphaned FTS
   entry, swallowed with no log. The orphan-skip path at :172 does log
   `memory_index_orphan`, so the inconsistency is visible in the file itself.
6. **`headroom-core/src/ctx/store.rs:232`** — `purge_all` does
   `conn.execute("DELETE FROM chunks", []).unwrap_or(0)`, turning a SQL error
   into a fake "0 rows deleted", then runs three more deletes with `?` that
   would propagate. Same function, two error policies.
7. **`headroom-core/src/ccr/backends/in_memory.rs:90-98`** — capacity-forced
   eviction of unexpired offloaded content has no signal. Offloaded output
   becomes unretrievable and nothing distinguishes eviction pressure from
   TTL expiry.
8. **`codex_rate_limits.rs:148` `extract_rate_limits`** returns `None`
   silently when the response shape moves outside the
   `["response","info","item"]` checklist. The statusline segment would go
   stale with no warning.
9. **`prefix_replay.rs:317-388`** computes a `changed` bool and discards it
   (`let _ = changed;` at :388); the caller at `proxy.rs:4409` re-derives the
   same thing. Harmless today, dead signal tomorrow.

Checked and found adequately instrumented:
`cache_stabilization/anthropic_cache_control.rs` (logs every skip and apply),
`sse/anthropic.rs` usage merging, `usage_observer.rs`. Both
`cache_hit_rate.rs` and `usage_observer.rs` correctly key off upstream
`usage` rather than proxy estimates, so the item 2 and item 10 estimate
problems do not extend to them.


## Item 8 notes

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 8. Smaller notes

- Two `injection row missing for a known conversation; injecting nothing
  (fail-safe)` events (10:33:11, 11:49:07). The fail-safe held, but those
  turns silently got no injection and nothing downstream records it. Neither
  log line carries a request_id, so they can't be tied to a turn.
- Startup warns AWS creds absent for Bedrock. Harmless unless routing there.
- Every request logs `non-PAYG auth mode`, skipping cache_control
  auto-placement and both sort passes (328/328 requests). Correct for a
  subscription, but those three optimisations are inert — worth knowing
  before attributing savings to them.
- 68 unparseable lines in the log file overall.
- `tools[]` indices for the same static sample vary across requests (25, 34,
  39, 41, 48, 55). Probably differing MCP sets per session rather than
  reordering within a session, but unconfirmed.


## Closure 12

*moved from `docs/notes/proxy-experiments-closures.md`*

**12 — closed 2026-08-12: the remaining observability gaps are removed.** The
headline was already stale: `ccr/response_handler.rs` warns on an unmatchable
tool-call ID, and its proxy caller logs parse failures, missing hashes, mixed
tools, round limits, continuation errors and residual unresolved calls with a
request ID. Current traffic contains 18 CCR-related records across six message
classes, including three mixed-tool decisions each paired with an explicit
client-resolution classification and stream-splice record. The original
context-tracker, half-persist, semantic-cache eviction/TTL, memory re-index,
purge-error and in-memory CCR eviction gaps likewise have tracing or propagate
their errors; the source audit confirms seven of the nine entries were already
addressed.

Quota observation now emits exactly one joinable
`codex_rate_limits_missing` warning when a routed Codex stream ends without
quota in either response headers or any SSE frame. The controlled missing case
emits one event even when finalization is invoked twice, and carries the request
ID; the positive control with a stream quota object emits zero. This is latched
at stream end rather than warned once per ordinary frame. The discarded
`prefix_replay` `changed` local and its dead assignments were also removed; the
caller-visible decision remains unchanged.

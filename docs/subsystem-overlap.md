# Where the subsystems overlap

Headroom grew compression and cache stabilisation on its own; the ctx subsystem
arrived later, carrying capture, offload, recall injection and retrieval. Both
shrink tool results, both store content for later recovery, both track
conversation identity. This page says which pairs compose and which were doing
the same job twice.

## Content reduction

Two stages shrink `tool_result` blocks, in this order:

1. **ctx offload** (`compression/ctx_offload.rs`) — blocks over
   `--ctx-offload-min-bytes` (50 KB by default) are replaced by a ~3 KB preview
   plus a retrieval pointer, with the original stored under a hash.
2. **live-zone compression** (`transforms/live_zone.rs`) — blocks over 512 B
   are rewritten by a content-type-specific compressor.

**Additive, now that the seam is closed.** Offload handles the very large
blocks worth storing out of band; live-zone handles everything above 512 B that
offload left alone. They ran on the same blocks in sequence, and a 3 KB digest
clears live-zone's 512 B floor, so live-zone re-compressed offload's digest and
appended a `<<ccr:hash>>` marker of its own — redeeming to the digest rather
than the original. A model following the inner pointer got a lossy copy back
and the true bytes became unreachable.

Live-zone now excludes any block carrying `<<ctx:` (`ExclusionReason::
CtxOffloadDigest`). The marker constant lives in `live_zone.rs` and is imported
by `ctx_offload.rs`, so the writer and the reader cannot drift.

`--exclude-tools` binds both stages. It used to bind only live-zone, which made
the exclusion mean less than it said: an excluded tool's output was still
swapped for a digest one stage earlier. Since the default list covers `Read`,
`Grep`, `Edit` and the other file and search tools, expect offload volume to
fall once both honour it — that is the exclusion working, not offload breaking.

## Retrieval and storage

One `CcrStore` instance, owned by `OffloadStore` and reached through
`AppState::ccr_store` (`proxy.rs:213`). Offload writes each original twice, to
two stores, under the same hash:

| store | serves |
|---|---|
| CCR sqlite KV (`ccr_store.db`) | `headroom_retrieve(hash)` and `GET /ctx/get/:hash` |
| FTS content index | `GET /ctx/search` |

**Additive.** Same bytes, two access patterns: exact-hash lookup for the model,
full-text search for the operator. Deliberate, and documented in
`ctx/offload_store.rs`.

## Injection

Three stages append to the latest user turn, in this order (`proxy.rs`):

1. CCR proactive expansion — appends previously-offloaded content back
2. ctx recall / resume injection — a recall block or resume snapshot
3. memory injection — memory entries, when `--memory` is on

**Additive, and now bounded together.** Each stage still has its own cap —
recall by result count, memory by entry count, expansion by
`--ccr-max-proactive-expansions` — but nothing summed them, so three
individually-small appenders could inflate one turn while every per-stage
counter reported success.

`--max-injection-bytes` (32 KB default, `injection_budget.rs`) is one ceiling
per request, drawn down in the order the stages run. `0` turns all three off.

Recall is the exception: it **reserves against the budget but is never
clipped**. Its block is decided once per conversation and replayed
byte-for-byte into the cached prefix (I4), so cutting it on a later turn would
rewrite bytes the provider had already cached and bust the prefix — costing far
more than the injection saves. Expansion and memory both append to the live
tail, which is re-sent every turn, so clipping them is cache-safe.

When a stage is clipped, `ctx_injection_clipped_bytes_total{stage}` rises and
an `injection_budget_clipped` line names the stage. If `wire_verdict.bytes_out`
climbs while `bytes_in` holds steady, read that metric first.

## Conversation identity

Three hashing passes run over overlapping parts of the same message array:

| site | key | why it differs |
|---|---|---|
| `ctx/identity.rs` | `session_key` + `system` + first message | conversation identity for recall |
| `cache_stabilization/usage_observer.rs` | `session_key` + first message only | **deliberately** excludes `system` — including it would hash a mutated system prompt to a "new" conversation and hide the cache bust it caused |
| `ctx/identity.rs::prefix_hash` | per-message, length-prefixed | prefix-drift detection |

**Additive by design, duplicated in compute.** The divergence between the first
two is intentional and documented at the `usage_observer` definition; merging
them would reintroduce a bug fixed in July 2026. What is genuinely duplicated
is the work: several SHA-256 passes per request with no shared cache. Worth
addressing only if it shows up in `opt_ms`.

## Recall — consolidated

Memory used to carry its own search: `memory/local_backend.rs` kept entries in
a `Vec`, scored them by counting overlapping words, and lost everything on
restart. Beside it sat `ctx/store.rs`, a persistent FTS5 BM25 index with porter
stemming and trigram matching, already used for recall injection and
`/ctx/search`. Two answers to "what text is relevant to this query", and the
weaker one was the default.

`memory/ctx_backend.rs` now implements the same `MemoryBackend` trait over the
ctx store, so the swap was one call site. What changes in practice:

- **Stemming.** A memory saying "the scripts are cached" now matches a query
  for "caching". Word overlap returned nothing.
- **Persistence.** Memories survive a restart.

`CtxStore` indexes labelled sources and knows nothing about memory ids, users,
or validity windows, so the record itself lives in
`headroom_core::ctx::MemoryRecordStore` (`memories.db`) keyed by the same id
the index uses as its label. Search ranks ids; the record store says what they
are. Writes go record-first: a crash between the two leaves a memory that is
retrievable but not yet searchable, which is a weaker failure than an index
entry pointing at nothing. Search skips an orphaned hit rather than failing.

`LocalMemoryBackend` remains as the fallback for when no store directory
resolves — a degraded memory beats none — and logs `memory_backend_fallback`
when it is used, so the weaker path is never silent.

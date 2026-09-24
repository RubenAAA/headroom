# Implemented: durable CTX-3 index outbox

- **Status:** implemented in the working tree; not built, released, or measured
  against post-change live traffic.
- **Source:** active `~/headroom-proxy.log` process from 2026-09-23T17:11Z;
  `crates/headroom-proxy/src/ctx/offload_store.rs` and
  `crates/headroom-core/src/ctx/store.rs`.
- **Value:** keep offloaded blocks available to text search and recall when
  indexing falls behind, without keeping the full backlog in RAM.
- **Next:** build and measure enqueue rate, per-project drain rate, and oldest
  pending-job age before changing the queue limits or writer concurrency.

## Evidence

The active process filled the 128 MiB in-memory index queue at 21:03Z. The
queue logger reported `shed=1`, `10`, and `100`; it logs powers of ten, so at
least 100 batches were shed. No `shed=1000` warning appeared in this process
window. `ctx_offload_accounting` then showed bursts of 168–639 offloaded blocks
per minute between 21:06Z and 21:10Z.

Two earlier `ctx_offload_persist_partial` records reported
`database is locked`, `index_ok=false`, `ccr_ok=true`. These were separate from
the later queue saturation: the original remained available by hash, but the
FTS write failed. The current code returns on that error and does not retry.

`OffloadStore::persist` writes originals to the CCR store inline, then puts
full records on one process-local worker queue. One background thread handles
the queue serially; for every record it runs `CtxStore::index_content`, which
opens its own transaction and inserts into the Porter FTS table, trigram FTS
table, and vocabulary. At the queue limit, the job is permanently shed. The
original stays retrievable by exact hash, but text search and recall cannot
find it through the project index.

The 128 MiB cap is intentional: the code records that an unbounded offload
backlog previously contributed to a 26 GB RSS/OOM incident. Raising this cap
would postpone shedding while increasing memory risk; adding workers without
measuring per-project load could also add SQLite writer contention.

## Implementation

`OffloadStore` stores complete records in a SQLite outbox under the CTX store
directory. The outbox is capped at 1 GiB / 100,000 jobs and retains each
original until both CCR and project FTS writes succeed. Startup resumes pending
rows. Batches stay within one project and up to 32 jobs / 16 MiB (a single
larger job drains alone); FTS writes share one transaction. SQLite busy/locked
and other write failures get bounded exponential retry delays.

The Anthropic request path falls back to its original body if the outbox cannot
accept the index work, and removes new offload-gate entries. The routed path
keeps CCR retrieval available and emits a distinct index-degraded warning.
Source labels use the content hash so repeated tool commands do not overwrite
one another's indexed output. Metrics report pending jobs, bytes, oldest age,
batch duration, retries, and capacity refusals.

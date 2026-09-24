# CTX-3 offload index backlog

- **Finding:** the old offload index queue permanently shed FTS work at its
  128 MiB RAM cap, and the single worker used one SQLite transaction per
  record. CCR retrieval could still work while text search and recall missed
  those blocks.
- **Evidence window:** active `~/headroom-proxy.log` process starting
  2026-09-23T17:11:21Z; queue saturation around 21:03Z. The queue logged
  `shed=1`, `10`, and `100`, so at least 100 batches were shed. Logs showed
  168–639 offloaded blocks/minute during the 21:06–21:10Z burst. Two
  `database is locked` index failures occurred at startup and were separate
  from the later queue saturation; their `ccr_ok=true` records remained
  retrievable but were not retried.
- **Root cause:** `OffloadStore` kept full originals in a process-local queue,
  drained by one thread that called `CtxStore::index_content` separately for
  every record. Once the 128 MiB budget filled, the index jobs were discarded.
- **Resolution in the working tree:** CTX-3 now writes records to a bounded
  SQLite outbox, drains up to 32 records / 16 MiB per project transaction,
  retains originals until CCR and FTS both succeed, and retries failed writes
  with bounded exponential backoff. Pending jobs, bytes, oldest age, batch
  duration, retries, and capacity refusals are exposed as metrics. The
  Anthropic path passes through raw requests at capacity; the routed path
  reports an index-degraded event while retaining the CCR digest path.
- **Validation scope:** these source changes have not been built or exercised
  against live traffic; the live-proxy measurements above describe only the
  pre-change implementation.

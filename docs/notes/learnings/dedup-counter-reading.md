# Learning: deduped-counter reads zero; the high-water mark is the mechanism

- **Source:** `docs/ctx-sessions-dedup.md` ("reading the result")
- **Claim:** `proxy_ctx_events_deduped_total` steady-state means the incremental
  capture mark is slipping again, not that the backstop works. Zero is healthy:
  the mark is the mechanism, the index is the net.


## Detail

*moved from `docs/ctx-sessions-dedup.md`*

## Reading the result

Most projects need nothing: the store indexes them itself. What the cleanup
buys on a large file is the 9.7 GB, and a build that finishes in a fraction of
73 seconds next time.

`proxy_ctx_events_deduped_total` counts the inserts the store refuses. A steady
rate on a running proxy means the incremental capture high-water mark is
slipping again, not that the backstop is working. Zero is the healthy reading;
the mark is the mechanism, the index is the net.


## Snapshot-cap burn

*moved from `docs/ctx-sessions-dedup.md`*

## What the duplicates were costing beyond disk

`ctx::inject` builds a resume snapshot from `get_events(prior, 200)`, ordered by
id. With the whole conversation re-inserted per turn, those 200 rows covered
only the first few turns, repeated. `render_section` in
`headroom_core::ctx::snapshot` does dedup its lines, but only after taking the
first `MAX_PER_SECTION` items, so the repeats burned the cap before the dedup
saw them. Neither reader ranks by frequency, so no ranking depended on the
duplicates and none of that behaviour changed with the fix.

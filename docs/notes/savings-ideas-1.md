# Savings ideas not yet captured (2026-09-03)

Source: `~/headroom-proxy.log` and rotations `.1`, `.3`, `.4`, covering
2026-08-31 07:27 to 2026-09-03 17:42 (first and last days partial). Prices from
`crates/headroom-core/src/pricing.rs`; `claude-fable-5-1` priced at the
`claude-` fallback (sonnet-4 rates). Every turn logs `non-PAYG auth mode`, so
dollars below are list-price equivalents, not a bill.

Scripts: `/tmp/join.py` (builds `/tmp/req.json` from the log), `/tmp/an1.py`
(spend by day and model), `/tmp/an2.py` (output, TTL, duplicates),
`/tmp/an3.py` (sidecar, recache, CCR), `/tmp/comp.py` (transcript composition).

> Note (2026-09-10): `proxy.rs`/`config.rs`/`sidecar.rs` line numbers are
> 2026-09-03-era (`proxy.rs` is now ~14k lines). Measurements stand;
> re-resolve cites before acting (current anchors: `ccr_continuation_usage`
> at `proxy.rs:6692/9346`, cache gate `semantic_cache_hit` at `:4014`).

## Where the money goes

Per day, averaged over the four logged days (measured):

| bucket | tokens/day | $/day | share |
|---|---|---|---|
| cache read | 803M | 335 | 62% |
| cache write (1h) | 15.0M | 111 | 21% |
| output | 4.08M | 83 | 15% |
| uncached input | ~1.8M | 10 | 2% |
| hidden CCR rounds (not in ledger) | — | 11 | 2% |

By model: opus 16,733 turns, 134k average context, about $440/day; sonnet-5
8,915 turns, 80k average, about $70/day; fable 1,627 turns on one day; haiku
192 turns.

> **Moved to [`docs/notes/learnings/output-bucket-composition.md`](learnings/output-bucket-composition.md)** — client-composition paragraph lives in the learning file.

## Ideas, ranked by $/day

> **Moved to [`docs/notes/ideas/implemented/sidecar-strip-long-context-beta.md`](ideas/implemented/sidecar-strip-long-context-beta.md)** — sidecar beta-strip proposal + DONE record.

> **Moved to [`docs/notes/ideas/implemented/output-block-type-instrumentation.md`](ideas/implemented/output-block-type-instrumentation.md)** — instrumentation proposal + 09-11 CLOSED verdict (trial declined).

> **Moved to [`docs/notes/ideas/implemented/ccr-continuation-cache-read-logging.md`](ideas/implemented/ccr-continuation-cache-read-logging.md)** — accounting proposal + 09-11 DONE record ($0.026/retrieve).

> **Moved to [`docs/notes/ideas/rejected/tools-block-cuts.md`](ideas/rejected/tools-block-cuts.md)** — measured-block verdicts moved out (tools cut declined here, thinking question in its file).

## Ruled out, with numbers

> **Moved to [`docs/notes/ideas/rejected/proxy-response-cache.md`](ideas/rejected/proxy-response-cache.md)** — ruled-out verdicts moved out (recache bullet stays — it already points at the recache doc).

- **Recache waste:** $9.27/day across all attributions, owned by
  `recache-classification.md`.

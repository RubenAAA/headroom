# Cache writes: what is workload, what is loss, what to fix

Measured on the live log (`~/headroom-proxy.log`, 2026-09-03 14:50Z–17:45Z)
and the rotations `.log.1` (09-03 08:01Z–13:22Z), `.log.3` (09-01/02) and
`.log.4` (08-31). Event `turn_cost_ledger` gives per-turn
`cache_read_input_tokens` / `cache_creation_input_tokens` / `input_tokens`
by `conversation_key`; `prefix_composition`, `prefix_replay_not_replayed`,
`cache_recache_observed`, `messages_rewritten`, `ctx_offload_accounting`,
`prior_thinking_dropped`, `sidecar_*` are joined by `request_id`. Scripts:
`/tmp/wr_analysis.py`, `/tmp/floor.py`, `/tmp/chase.py`..`chase6.py`,
`/tmp/stall.py`, `/tmp/evrate.py`.

> **Moved to [`docs/notes/learnings/cache-write-growth-vs-excess.md`](learnings/cache-write-growth-vs-excess.md)** —  doubling analysis (theory, measurement, tail) lives in the learning file.

## 4. The causes, one by one

> **Moved to [`docs/notes/ideas/implemented/msg1-collapse-attribution-fix.md`](ideas/implemented/msg1-collapse-attribution-fix.md)** — msg1-collapse analysis + 09-11 fixed verdict.

> **Moved to [`docs/notes/ideas/implemented/gate-prior-thinking-drop.md`](ideas/implemented/gate-prior-thinking-drop.md)** — drop-gate analysis + 09-11 implemented verdict (note §0b tension in file).

> **Moved to [`docs/notes/ideas/implemented/recache-attribution-order-fix.md`](ideas/implemented/recache-attribution-order-fix.md)** — attribution-order analysis + 09-11 fixed verdict.

> **Moved to [`docs/notes/learnings/write-tail-scatter.md`](learnings/write-tail-scatter.md)** — small tail observations (tool flaps, restarts) live in the learning file.

## 5. Not proxy faults, for completeness

- Upstream 529/overloaded retries 124 per 1000 turns live (2 baseline);
  "error inside a 200 stream" 88; `stream_incomplete` 21.
- Forwarded TTFB median 2.2 s live vs 1.3–1.4 s baseline once the morning
  stall ended (see `speed-ideas.md`).
- CCR continuation (`sending continuation` → `retrieval handled`) median
  62 s at 16h live (n=12) vs 4–11 s baseline; 13 requests over 40 s. The
  log has nothing between the two lines; cause open.
- Outbound body inflation (p90 +76–109 kB today, negative before): checked
  and closed. Inflated turns carry `prefix_replay_applied` and have w:r
  0.024 against 0.044 for shrunk turns; the proxy re-expands a prefix the
  client trimmed so the cache still hits.

## 6. How to know it worked

Re-run `/tmp/floor.py` after a day of similar work. Targets:

- `growth < 0` turns' writes under 8% of later-turn writes (08-31/09-01
  level), from 23% PM today.
- `prior_thinking_dropped` on later turns with `first_diff_index > 1`: zero.
- `sidecar_fallback` with status 400 on the beta message: zero;
  `sidecar_detected` turns absent from the ledger.
- `cache_recache_observed` with `origin=proxy` on a turn whose
  `prefix_replay_not_replayed` has `first_diff_index=1` and path `role`:
  zero.
- Excess share stays within 7–15%; if it rises with the tail fixed, that is
  a new problem.

## 7. Re-audit — 2026-09-11 (tree at `2914e9ac`, log 09-10 13:06–22:55 UTC)

Verdict first: **4.2 is the only live item — open and bleeding. 4.1 and 4.4
are fixed in code and holding in the window. 4.3 is done and confirmed. §6
needs a rebuild, not a re-read** (`/tmp/floor.py` and siblings are gone with
`/tmp`; the 09-03 numbers below are history, not baselines).

- **§6 targets, current-window scoreboard:** `prior_thinking_dropped`
  with `first_diff_index > 1` — 12, target zero: FAIL (this is 4.2 above).
  `sidecar_fallback` 400s — 0: PASS. `origin=proxy` on msg1-collapse — 0:
  PASS (trigger absent). The `growth < 0` share and excess-share lines
  cannot be re-scored without rebuilding `floor.py` — that rebuild is the
  actual §6 work, not re-reading these numbers.

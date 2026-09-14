# Closures: the proxy experiments log

What each item was measured against, and what changed to close it.

Source: `proxy-experiments-2026-08.md` — 2,601 lines, 28 numbered
findings recorded between 2026-07-07 and 2026-08-09. All 26 items tracked here
are closed, all on 2026-08-12: items 16, 1, 2, 10, 15, 9 and 3 first, then 6,
8, 12, 14 and 20.

One warning about that document: its code references have drifted. Item 1 names
`proxy.rs:3144-3147`, which is now unrelated code. Check any line number there
against the current source before trusting it.

> **Moved to [`learnings/zero-event-sample-sizes.md`](learnings/zero-event-sample-sizes.md)** — investigate-first, two-numbers, one-item-at-a-time, don't-quote-3 rules.

> **Moved to [`learnings/recache-counting-rules.md`](learnings/recache-counting-rules.md)** — all six traps verbatim (they overlap the counting rules — one home now).

## The items

> **Moved to [`ideas/implemented/search-verbatim-fix.md`](ideas/implemented/search-verbatim-fix.md)** — closure evidence (fix + deployment/exercise numbers).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — closure evidence for all four (windows, joins, tests).

### Blind spots

> **Moved to [`ideas/implemented/sse-blind-spot-closed.md`](ideas/implemented/sse-blind-spot-closed.md)** — 469-dispatch reconciliation (zero silent).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — post-fix reversal (1.22M saved vs 13.9k drift).

> **Moved to [`ideas/implemented/failed-work-bucket.md`](ideas/implemented/failed-work-bucket.md)** — failed-work aggregate + controlled before/after.

### Lower stakes

> **Moved to [`learnings/auth-gated-passes-deliberate.md`](learnings/auth-gated-passes-deliberate.md)** — joinable-injection + non-fault notes (469 turns, zero live failures).

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — gap-by-gap source audit + quota latch.

> **Moved to [`ideas/implemented/retry-after-cap-fix.md`](ideas/implemented/retry-after-cap-fix.md)** — controlled over-cap proof on both paths.

> **Moved to [`ideas/rejected/client-withdrawals-wont-fix.md`](ideas/rejected/client-withdrawals-wont-fix.md)** — won't-fix verdict (owner chose correctness; no patch).

## How to measure a change

Build, then put it live: `cargo build --release -p headroom-proxy`, then
`~/restart-headroom.sh` run detached. `/metrics` gives process-scoped counters
that reset with the restart, so the comparison is clean by construction.

Read the result with `headroom savings`, `/stats` (the `savings_verdict` field
subtracts the proxy's own cache busts from its savings), and `/metrics` for
`headroom_cache_read_tokens_total` and `headroom_cache_write_tokens_total`.

**Wait for enough traffic before believing a zero.** For an event class running
at 4% of requests, ~75 requests are needed before an observed zero drops below
5% probability by chance, and ~115 before it drops below 1%. Fourteen requests
proves nothing. Count requests, state the count, and do the arithmetic in your
report.

> **Moved to [`ideas/implemented/anthropic-shape-fixes.md`](ideas/implemented/anthropic-shape-fixes.md)** — thinking-block cache_control refusal + bare-string wrapping (367 requests, zero events).

## Ground rules

Never run `git commit` or `git push`. Report what you changed and let the owner
commit.

`observability::ccr_splice::tests::the_summary_ignores_the_routine_reason` fails
about half the time under parallel scheduling — it races another test over the
global Prometheus registry. It is not yours, it predates this work, and it is
worth fixing separately.

# TODO: Recache Classification & Warning Behavior

**Created:** 2026-07-07 | **Updated:** 2026-07-09
**Status:** Implemented 2026-07-09 — `RecacheEventKind` (Drift/Expected) in `usage_observer.rs`, `event_kind` on `/cache-health`, two-window ⚠/ℹ statusline, Prometheus `unknown` reason relabelled `expected`. Not done: `compression_applied` plumbing (open question, not needed for the classification rule).

> Note (2026-09-10): bare `prefix_replay.rs:*` cites below mean
> `cache_stabilization/prefix_replay.rs`; `proxy.rs` numbers are July-era
> (file is now ~14k lines). Entries are a living ledger — refuted entries
> stay by policy.

---

> **Moved to [`ideas/implemented/recache-drift-expected-split.md`](ideas/implemented/recache-drift-expected-split.md)** — July classification work (problem through related issues) that shipped the Drift/Expected split.

> **Moved to [`learnings/recache-h1-h3-refuted.md`](learnings/recache-h1-h3-refuted.md)** — ledger framing; the refutations below share its window.

> **Moved to [`learnings/recache-h4-unread-blocks.md`](learnings/recache-h4-unread-blocks.md)** — strongest lead at the time (exact-match unread blocks); later superseded by stream-matching work — kept for the record.

> **Moved to [`ideas/implemented/stream-matching-fix.md`](ideas/implemented/stream-matching-fix.md)** — prefix-window design note (stable window ends one short by design).

> **Moved to [`ideas/implemented/recache-instruments.md`](ideas/implemented/recache-instruments.md)** — H5/H6 open hypotheses (injection-vs-replay, tail breakpoint) that the instruments were built to settle.

> **Moved to [`learnings/recache-hiding-places-checked.md`](learnings/recache-hiding-places-checked.md)** — bias statement + TTL/slack/clamp/booking checks.

> **Moved to [`ideas/implemented/recache-instruments.md`](ideas/implemented/recache-instruments.md)** — under-reporting question + the stream-identity instrument it asked for.

> **Moved to [`learnings/recache-aug23-token-flow.md`](learnings/recache-aug23-token-flow.md)** — 2026-08-20/22 token flow: attribution table, refuted causes, divergence shapes, open question.

> **Moved to [`ideas/implemented/recache-alternate-cap-128.md`](ideas/implemented/recache-alternate-cap-128.md)** — unit-test reproduction + sizing.

> **Moved to [`ideas/implemented/recache-evict-by-size.md`](ideas/implemented/recache-evict-by-size.md)** — size-predictor table + skip-not-stop eviction.

> **Moved to [`ideas/implemented/recache-instruments.md`](ideas/implemented/recache-instruments.md)** — 58-turn residue audit that motivated the forwarded-prefix instrument.

> **Moved to [`learnings/recache-hiding-places-checked.md`](learnings/recache-hiding-places-checked.md)** — TTL/front-rewrite/offload-volume refutations.

## Older threads, closed — 2026-08-23

> **Moved to [`ideas/dead-crush-flags.md`](ideas/dead-crush-flags.md)** — dead CLI flags (`--min-tokens-to-crush`, `--max-items-after-crush` never read; SmartCrusher built from `::default()`).

> **Moved to [`learnings/recache-counting-rules.md`](learnings/recache-counting-rules.md)** — denominator-mismatch lesson (subset vs corpus).

> **Moved to [`ideas/implemented/recache-workingdir-preview.md`](ideas/implemented/recache-workingdir-preview.md)** — invalidate-has-no-caller claim + its same-day correction (it is called — too early).

> **Moved to [`ideas/implemented/recache-workingdir-preview.md`](ideas/implemented/recache-workingdir-preview.md)** — capture method + the three divergences the page resolves.

> **Moved to [`ideas/implemented/recache-workingdir-preview.md`](ideas/implemented/recache-workingdir-preview.md)** — working-directory preview-pin fix in full.

> **Moved to [`ideas/implemented/recache-subagent-hook-removal.md`](ideas/implemented/recache-subagent-hook-removal.md)** — SubagentStart hook analysis in full.

> **Moved to [`ideas/implemented/recache-instruments.md`](ideas/implemented/recache-instruments.md)** — BlockTag instrument + attribution table that motivated it.

> **Moved to [`ideas/implemented/recache-role-predicate.md`](ideas/implemented/recache-role-predicate.md)** — withdrawal analysis (signature, guard miss, measurement).

> **Moved to [`ideas/implemented/recache-accumulation-watch-reread.md`](ideas/implemented/recache-accumulation-watch-reread.md)** — 29,321-turn baseline readings.

> **Moved to [`ideas/implemented/recache-provider-reasons.md`](ideas/implemented/recache-provider-reasons.md)** — closure analysis + 09-02 rename update.

> **Moved to [`ideas/implemented/recache-concurrent-naming.md`](ideas/implemented/recache-concurrent-naming.md)** — timing proof that the flag earns its name.

> **Moved to [`ideas/implemented/recache-role-predicate.md`](ideas/implemented/recache-role-predicate.md)** — pre-restart baseline + predictions (other buckets ride along as context).

> **Moved to [`ideas/implemented/recache-provider-reasons.md`](ideas/implemented/recache-provider-reasons.md)** — window framing for the 09-02 audit.

> **Moved to [`ideas/implemented/recache-role-predicate.md`](ideas/implemented/recache-role-predicate.md)** — both predictions met (diverged 70x down).

> **Moved to [`ideas/implemented/sidecar-replay-exclusion.md`](ideas/implemented/sidecar-replay-exclusion.md)** — spinner-sidecar replay pollution analysis (fix confirmed in tail audit).

> **Moved to [`ideas/implemented/recache-first-turn-reasons.md`](ideas/implemented/recache-first-turn-reasons.md)** — first-turn write analysis (recall placement rationale).

> **Moved to [`learnings/offload-vs-retrieval-3x.md`](learnings/offload-vs-retrieval-3x.md)** — TTL fine, offload-vs-retrieval 3x, dead block, sampler restart.

> **Moved to [`ideas/implemented/recache-first-turn-reasons.md`](ideas/implemented/recache-first-turn-reasons.md)** — reason set + metric.

## Tail audit — 2026-09-11

Window: `~/headroom-proxy.log` 2026-09-10 13:06–22:55 UTC (~10 h; the file
may span binaries, so read counts as window totals). Every still-open thread
from 09-02 checked against the tree at `2914e9ac` + worktree:


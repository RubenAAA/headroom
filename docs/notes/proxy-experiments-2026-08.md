# Proxy experiments and findings (2026-08)

> Note (2026-09-10): code references have drifted since August —
> `proxy.rs` is now ~14k lines, so every `proxy.rs:NNNN` cite below is
> stale (e.g. `:3144-3147` is now `InflightGuard` boilerplate, outcome
> code moved to `~5063-5090`). Check any line number against current
> source before quoting it. Substance (measurements, closures) stands.

**Closed.** Every item below was resolved between 2026-08-09 and 2026-08-12.
The closure evidence — what was measured, and what changed — is in
[proxy-experiments-closures.md](proxy-experiments-closures.md). Nothing here is outstanding work.

It started as notes from watching one live proxy run, and it is kept now for
what it refutes rather than what it proposes. Item 19 is a fix that was built,
measured and reverted. Item 25 lists three premises that each looked
well-supported when acted on, and the cheap check that would have killed each
one. Item 27 is the rule that `prefix_replay_applied` means the forwarded bytes
changed, not that the replay worked — it invalidated item 21's headline. Read
the relevant item before rebuilding any of this.

Every figure is scoped to the window its own item names, and some are
superseded: item 3's totals are marked do-not-quote after item 11's fix. Item
numbering follows the order things were found, not the order they were worked,
and there is no item 17.

**Run observed:** pid 15975, started 14:33:08 +04 (10:33:08Z), binary
`~/.local/bin/headroom-proxy`, log `~/headroom-proxy.log` (unrotated — scope
every query by process start, see memory note).

**Window analysed:** 10:33:08Z–11:40:27Z, then live tail past 11:46Z.
5844 log lines, 340 forwarded requests, 311 booked turns.

Already covered elsewhere, not repeated here: empty `drift_dims` recache
events are classified as expected (subagent close, `/clear`) in
[recache-classification.md](recache-classification.md). The 534,485
"expected" tokens below fall in that bucket and are excluded from waste totals.

---

> **Moved to [`proxy-experiments-closures.md`](proxy-experiments-closures.md)** — superseded triage guide (all items closed; outcomes in closures + learnings).

> **Moved to [`ideas/implemented/stream-matching-fix.md`](ideas/implemented/stream-matching-fix.md)** — settled finding (merged streams proved by message counts).

> **Moved to [`learnings/auth-gated-passes-deliberate.md`](learnings/auth-gated-passes-deliberate.md)** — inert-passes decision + coarse-flag warning.

### Settled by the telemetry (2026-08-09, new binary live at 04:02Z)

> **Moved to [`ideas/implemented/search-verbatim-fix.md`](ideas/implemented/search-verbatim-fix.md)** — digitbug settlement (verbatim renderer confirmed live).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — pinning-is-correct settlement (per-request saving re-applied).

> **Moved to [`ideas/implemented/sse-blind-spot-closed.md`](ideas/implemented/sse-blind-spot-closed.md)** — no-repro window (35 turns, nothing to fix yet).

### Fixes started (2026-08-09)

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — compression-accounts-own-figure fix + ctx_offload_accounting diagnostic.

> **Moved to [`learnings/absent-log-field-vs-behaviour.md`](learnings/absent-log-field-vs-behaviour.md)** — retry log fields (header/source/clamp) that closed the log-vs-behaviour gap.

> **Moved to [`ideas/implemented/sse-blind-spot-closed.md`](ideas/implemented/sse-blind-spot-closed.md)** — detached-parser waiter (panics/cancellations visible).

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — CCR validation warnings now operator-visible.

> **Moved to [`ideas/implemented/volatile-warning-dedup.md`](ideas/implemented/volatile-warning-dedup.md)** — volatile identifiers joinable to drift/recache events.

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — session key on recache events (joins drift/volatile directly).

> **Moved to [`ideas/implemented/sse-blind-spot-closed.md`](ideas/implemented/sse-blind-spot-closed.md)** — parser sent/dropped chunk counts on completion + failure.

> **Moved to [`ideas/implemented/retry-after-cap-fix.md`](ideas/implemented/retry-after-cap-fix.md)** — routed retries carry IDs + header/source/clamp fields.

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — routed accounting split + CTX-understatement note.

> **Moved to [`ideas/implemented/volatile-warning-dedup.md`](ideas/implemented/volatile-warning-dedup.md)** — warn-on-move with in-request-set semantics + LRU bounds.

> **Moved to [`ideas/implemented/failed-work-bucket.md`](ideas/implemented/failed-work-bucket.md)** — `request_failed_accounting` on the shared funnel (ledger untouched).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — placement counterfactuals on outcomes; ledger price unchanged pending decision.

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — semantic-cache, memory-FTS, CCR-eviction visibility.

> **Moved to [`ideas/implemented/search-verbatim-fix.md`](ideas/implemented/search-verbatim-fix.md)** — `transform_byte_integrity` debug event (hash both sides; off at info).

> **Moved to [`ideas/implemented/retry-after-cap-fix.md`](ideas/implemented/retry-after-cap-fix.md)** — session_key_hash on retry warnings (join retries to recaches).

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — CCR tracker eviction events.

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — CTX persistence split outcomes (CCR vs FTS).

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — `ctx purge` propagates chunk-delete errors.

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — Codex rate-limit shape-miss warnings.

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — 3d/3e ruled out (read for what was excluded).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — negative-tok_after investigation in full (two mechanisms, log_compressor focus, root cause, re-emission).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — baseline scope finding (live zone, not request).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — superseded spend-vs-save numbers (do not quote; artefact per item 11).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — double-counting suspicion (concurrent pairs).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — cold-start premise refuted in source (first requests classify Expected).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — tools-drift hypothesis dropped (cold-start explains both).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — large system drift, client-origin (injection ruled out).

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — concurrency clustering note (superseded by item 11).

> **Moved to [`ideas/implemented/volatile-warning-dedup.md`](ideas/implemented/volatile-warning-dedup.md)** — static-value evidence + caveats incl. 09-09 fix decision.

> **Moved to [`learnings/conversation-key-merges-streams.md`](learnings/conversation-key-merges-streams.md)** — flip-flop evidence (two prefixes, never a third) as found.

> **Moved to [`ideas/implemented/failed-work-bucket.md`](ideas/implemented/failed-work-bucket.md)** — failed-turn invisibility analysis + partial/failure accounting question.

> **Moved to [`learnings/absent-log-field-vs-behaviour.md`](learnings/absent-log-field-vs-behaviour.md)** — Retry-After correction in full (jitter arithmetic, clamp question, field test).

> **Moved to [`ideas/implemented/retry-after-cap-fix.md`](ideas/implemented/retry-after-cap-fix.md)** — second retry path gaps (fields, date fallback, jitter, request_id).

> **Moved to [`ideas/implemented/sse-blind-spot-closed.md`](ideas/implemented/sse-blind-spot-closed.md)** — blind-spot reconciliation in full.

> **Moved to [`ideas/implemented/sse-blind-spot-closed.md`](ideas/implemented/sse-blind-spot-closed.md)** — discarded-handle hypothesis + confirmation protocol.

> **Moved to [`learnings/unbooked-share-of-wire.md`](learnings/unbooked-share-of-wire.md)** — unbooked turns are representative-sized (17% of wire bytes).

> **Moved to [`learnings/input-tokens-uncached-tail.md`](learnings/input-tokens-uncached-tail.md)** — `input_tokens` measures the uncached tail on warm turns — never a size/cost proxy.

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — fresh-rate ledger pricing + repeat-booking evidence.

> **Moved to [`ideas/implemented/stream-matching-fix.md`](ideas/implemented/stream-matching-fix.md)** — alternation evidence + the two readings (before settlement).

> **Moved to [`ideas/implemented/stream-matching-fix.md`](ideas/implemented/stream-matching-fix.md)** — pinned-value-crosses-keys counter-evidence + source reading.

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — nine dark paths as found (pre-fix audit).

> **Moved to [`ideas/ccr-retrieval-readback-check.md`](ideas/ccr-retrieval-readback-check.md)** — store present-and-plausible; 532/538 never read back — the open read-path question.

> **Moved to [`ideas/implemented/savings-accounting-corrections.md`](ideas/implemented/savings-accounting-corrections.md)** — routed re-emission evidence (zero compression events behind 178k tokens).

> **Moved to [`ideas/implemented/retry-after-cap-fix.md`](ideas/implemented/retry-after-cap-fix.md)** — post-limit cold starts (326k rewrite) + clamp-stakes argument.

> **Moved to [`ideas/implemented/observability-gaps-removed.md`](ideas/implemented/observability-gaps-removed.md)** — small notes (row-miss joinability, Bedrock creds, inert passes, unparseables, tools indices).

> **Moved to [`learnings/aug-run-health-and-hunch.md`](learnings/aug-run-health-and-hunch.md)** — 2026-08-08 health snapshot (opt_ms, ttfb, hit rate, idle-gap colds, tok_inflated=0).

> **Moved to [`ideas/implemented/search-verbatim-fix.md`](ideas/implemented/search-verbatim-fix.md)** — digit-mutation detective work in full (zero-padded-minute signature).

## How to reproduce every figure here

`scripts/proxy_log_audit.py` re-derives them from a proxy log. It prints
derived counts only, never raw log lines.

```
python scripts/proxy_log_audit.py all --log ~/headroom-proxy.log \
    --since 2026-08-08T10:33:08
```

| subcommand | item |
| --- | --- |
| `negtokens` | 1, 1a |
| `cumulative` | 1c |
| `recache` | 3, 3a, 11 |
| `coldstart` | 3c |
| `volatile` | 4 |
| `retries` | 6, 7 |
| `unbooked` | 9 |
| `ledger` | 10 |
| `ccr` | 13 |

**`--since` is not optional in practice.** The log is never rotated, so
without it every count spans months and many restarts. Use the process start
timestamp. Figures in this document use `2026-08-08T10:33:08`.

Numbers will not match this document exactly if you run against a longer
window — the run continued after the document was written. What should hold
is the *shape*: the ratios between transforms in item 1a, and the all-upward
disagreement in item 1c. If one of those flips, the finding is wrong and should
be struck.

`retry_after seen anywhere: 0` is **not** a shape-invariant — an earlier
revision listed it as one. It reports a field the proxy never logs, so it reads
0 whatever the retry code does. It will keep reading 0 after item 7 is fixed in
behaviour, and only change when the log field is added.

Live watcher used during the observation: `/tmp/hr_watch.py`, a throwaway
that tails the log and prints errors, exhausted retries, recache waste over
5K tokens, negative token counts and cold cache on large conversations. Not
preserved; the audit script covers the same ground after the fact.

> **Moved to [`ideas/implemented/sse-blind-spot-closed.md`](ideas/implemented/sse-blind-spot-closed.md)** — 400-rate finding, invisibility cause, thinking-block attribution, zero-cost note, watch item.

> **Moved to [`ideas/rejected/partial-prefix-replay.md`](ideas/rejected/partial-prefix-replay.md)** — built-measured-reverted record (splice matched neither turn; all-or-nothing pinned instead).

> **Moved to [`learnings/diverged-bust-origins.md`](learnings/diverged-bust-origins.md)** — early-not-boundary finding, shape census, two client behaviours, real-token cost.

> **Moved to [`learnings/replay-hit-95-unattributed.md`](learnings/replay-hit-95-unattributed.md)** — 328/344 clean hits; alternates cleared; residual unattributable without per-stream ids.

> **Moved to [`learnings/cost-saves-measured.md`](learnings/cost-saves-measured.md)** — compression ~1.5% at best, writes 55% of bill, 38% re-cache + scoping trap.

> **Moved to [`ideas/implemented/reminder-seal.md`](ideas/implemented/reminder-seal.md)** — churn confirmation, three ways out, seal design + live correction.

> **Moved to [`ideas/implemented/reminder-seal.md`](ideas/implemented/reminder-seal.md)** — 19% conflated with merged streams → defensible 1.5%; seal stays (correct on single streams).

> **Moved to [`learnings/failed-premises-cheap-checks.md`](learnings/failed-premises-cheap-checks.md)** — three failed premises + the measurement discipline they earned.

> **Moved to [`ideas/implemented/chain-id-grouping.md`](ideas/implemented/chain-id-grouping.md)** — chain-id design (continuity runs, id 0 semantics, pinned tests).

> **Moved to [`learnings/recache-counting-rules.md`](learnings/recache-counting-rules.md)** — applied-means-changed correction (0.6% vs 32%) + the rule.

> **Moved to [`ideas/implemented/reminder-relocation.md`](ideas/implemented/reminder-relocation.md)** — compare-blind + forward-without design, halves argument, invariant, pins.

> **Moved to [`ideas/implemented/reminder-relocation.md`](ideas/implemented/reminder-relocation.md)** — whole-message churn case + front-of-pipeline move + guards.

> **Moved to [`ideas/implemented/offload-gap-shipped.md`](ideas/implemented/offload-gap-shipped.md)** — six proved-and-shipped findings (exclude-tools, CCR cap, tail breakpoint, retrieve holes, overload budget, stream hold).

> **Moved to [`ideas/rejected/offload-gap-disproved.md`](ideas/rejected/offload-gap-disproved.md)** — five disproved levers incl. markercheck close (do not re-open without explaining windowgap).

Root causes found:

> **Moved to [`learnings/ttl-policy-gap-66pp.md`](learnings/ttl-policy-gap-66pp.md)** — 1h-vs-5m policy gap root cause (keep 1h on subscription, off on API).

> **Moved to [`learnings/ranking-tests-miss-thresholds.md`](learnings/ranking-tests-miss-thresholds.md)** — 0.032-cap vs 0.3-floor mechanism + FTS-rank testing trap.

> **Moved to [`ideas/implemented/memory-frozen-boundary-fix.md`](ideas/implemented/memory-frozen-boundary-fix.md)** — system-length used as messages frozen count; pass 0, real boundary from replay tracker.

> **Moved to [`learnings/saved-dollars-need-placement.md`](learnings/saved-dollars-need-placement.md)** — Opus 5 mispricing inflated the ledger but can't touch cachesim (no dollar arithmetic).

> **Moved to [`learnings/offload-gap-loose-ends.md`](learnings/offload-gap-loose-ends.md)** — J4 gate, thinking-strip, byte composition, allocator, tokenize/fsync, replay-boundary, near-tail findings.

> **Moved to [`ideas/parse-body-once-measurement.md`](ideas/parse-body-once-measurement.md)** — 61 ms + 0.155 ms/KB breakdown incl. the 54.3 ms tracker attribution.

> **Moved to [`learnings/offload-gap-loose-ends.md`](learnings/offload-gap-loose-ends.md)** — replay-boundary rate gap + near-tail correction (kept open, don't quote absolute deferrals).

> **Moved to [`learnings/offload-gap-loose-ends.md`](learnings/offload-gap-loose-ends.md)** — symlink layout, double-carried user_id, empty workspace partition.


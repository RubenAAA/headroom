# Idea: prove or kill the commit-latency hypothesis

- **Status:** ANSWERED 2026-09-21 — confirmed on 558 witnessed events; the
  5s `COMMIT_LATENCY_WINDOW` stays as it is. See Findings at the bottom.
  (Shipped 2026-09-17 as a witness-only flag, deliberately unvalidated.)
- **Source:** the `MissedNewestWrite` "typical at gaps under 3s" comment, and
  the 72%-of-overlapping-pairs figure, which was mined offline with a
  different method than the live pending-map detector. Neither establishes how
  long Anthropic takes to commit a cache write, which is the number the window
  pretends to know.
- **Value:** sets the window honestly — or removes the flag. A confirmed race
  is avoidable client-side (turn serialization, fan-out discipline, the
  concurrency shed cap); a phantom flag is noise on every future triage.
- **Next:** two steps. (a) One-line change: log `idle_seconds` (already
  computed as `idle_gap`) on the recache lines — today it only rides TTL
  expiries, so gap-vs-landing needs cross-line mining. (b) Table
  `commit_race_suspect` × gap × `landing` over a week. Confirm =
  `missed_newest_write` concentrates in suspect ∧ small-gap; kill = no
  concentration, in which case delete the flag rather than widening it.
- **Update 2026-09-17:** (a) shipped — `idle_seconds` on all four
  `cache_recache_observed` arms (`usage_observer.rs`) as float
  (`idle_gap.as_secs_f64()`; the TTL line keeps its truncated int — same
  name, finer precision here, since the 5s boundary is unqueryable at
  1s resolution). (b) measured once on 186 events: suspect=true on 96%
  missed / 100% partial BUT also 93% of commit-impossible non-monotonic
  `between_entries` at a 91.4% sub-5s base rate — specificity ≈ 0.
  Verdict REFINE, not confirm/kill: detector mechanics validate 61/61,
  traffic has no slow-gap contrast class (4 all day). Still needs ~a week
  of mixed traffic; the deciding number is the suspect-rate on slow-gap
  (>60s) missed events. Frame as provider staleness with race inside it
  (monotonic subset only); non-monotonic serves now 47, was 29.

- **Independent check 2026-09-17 (213 events):** suspect rates reproduce —
  missed 78/82 (95%), partial 72/72 (100%), between 48/53 (91%); base rate
  95%. Slow-gap (>5s) contrast class still tiny: 10 events / 37.6k of 861k
  (4%), including missed_newest at 24s, 42s, 431s gaps that no race explains.
  Note: `idle_seconds` is deployed but has fired zero times so far (no
  recache since the redeploy) — first post-deploy recache line proves it
  end to end. File stays open; nothing further to do until slow-gap
  contrast accumulates.

## Cross-note (2026-09-17, from `recache-landing-natural-kinds.md`)

The landing histogram found 29 older-snapshot serves (`between_entries`
landing at `pp` + sliver) that are *non-monotonic* — the provider served
less than the stream previously read. Commit latency cannot produce those;
replica lag or eviction must. So even a confirmed race covers at most the
monotonic subset (missed/partial). Frame the hypothesis as provider
staleness with commit race inside it, not race alone.

## Findings 2026-09-21 (6-day window, Anthropic models only) — CONFIRMED

Ran step (b): `commit_race_suspect` × `idle_seconds` × `landing` over 558
witnessed `cache_recache_observed` events, 2026-09-17T21:18 to 2026-09-21,
scoped to `claude-*` requests by joining `turn_cache_fingerprint.model` on
`request_id` (23,914 Anthropic requests in the window).

The deciding number — suspect rate among `MissedNewestWrite` landings, by
inter-turn gap:

| gap | MISSED n | suspect rate |
|---|---|---|
| 1-3s | 35 | 100% |
| 3-5s | 107 | 100% |
| 5-10s | 75 | 100% |
| 10-60s | 25 | 76% |
| >60s | 5 | 20% |

Base suspect rate over all 558: **94%**. The slow-gap contrast class the
2026-09-17 reading waited for now exists, and it breaks the right way: the
rate collapses to 20% where a commit race cannot explain the miss. The flag
is not a phantom. **Keep it.**

Landing shape, same window: 73% of suspect events at a 3-5s gap land MISSED,
against 15% at 10-60s and 8% past 60s. Non-suspect short gaps produced
**0 MISSED in 6 events**. The long-gap inversion — non-suspect events run
32-40% MISSED at 10-60s and beyond — is the eviction lane the section above
predicts, not the race.

**No change to `COMMIT_LATENCY_WINDOW`.** The 5-10s band already reads 100%
suspect, so 5s leaves no race event unflagged. An earlier draft of this
reading argued for widening it; that conflated two clocks — `idle_seconds`
is time since the previous turn, the constant ages a *sibling completion*.
The doc comment at `usage_observer.rs:141` now carries the measured band in
place of the unvalidated "cluster at gaps under 3s".


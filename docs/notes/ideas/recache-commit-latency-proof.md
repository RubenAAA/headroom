# Idea: prove or kill the commit-latency hypothesis

- **Status:** open (the 5s `COMMIT_LATENCY_WINDOW` shipped 2026-09-17 as a
  witness-only flag — deliberately unvalidated)
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


# Idea: test whether the five landings are natural kinds

- **Status:** open (pure log query; no code)
- **Source:** `CacheLanding::classify` in
  `crates/headroom-proxy/src/cache_stabilization/usage_observer.rs`.
  `MissedNewestWrite` (gaps under 3s) and `PartialOfPreviousWrite` (Fable's
  fixed 69–114 tokens) have shape evidence. `BetweenEntries` is defined as
  "anything else below `prev_read`" — a leftover bucket wearing a name. The
  landing is now the primary observable for the whole residual bucket (it
  rides `RecacheEvent.landing` since 2026-09-17), so its ontology matters.
- **Value:** if `between_entries` is noise-slicing, every query built on it
  misleads; if the positions cluster, the five names earned their keep.
- **Next:** histogram the fractional position
  `(actual − prev_read) / (prev_boundary − prev_read)` over unexplained
  events. Every input is already on the line (`previous_cache_read`,
  `previous_boundary`, `previous_previous_boundary`, `actual_cache_read`).
  Clustered ⇒ keep the five; uniform ⇒ collapse `between_entries` into an
  explicitly unpositioned remainder instead of a faux diagnosis.

## Findings 2026-09-17 (116 residual events, same window as triage)

- **`missed_newest_write`: real kind.** 46/46 with `actual == prev_read`
  exactly. Keep.
- **`partial_of_previous_write`: real shape, wrong constant.** Offsets into
  the previous write run 209–4575 tokens here — zero in the commented
  69–114 band, so the Fable constant does not generalize. But the fractional
  position clusters hard near zero (30/39 below 0.3, 17/39 below 0.1): reads
  stop just inside the previous write. The kind holds; the constant should be
  struck from the comment.
- **`between_entries`: real cluster, wrong name.** 28/29 with known older
  boundary land at `pp` + a few-hundred-token sliver (median fractional
  position in the older segment 0.01, max 0.06) — while `prev_read` sits near
  2× `pp`. These are not "between" entries; they are serves of a
  full-generation-stale snapshot plus a sliver. Do NOT collapse: 25% of
  residual events, and the most informative kind (see below).
- **`dropped_older_entry`: inconclusive.** 2 events under a mixed rule (one
  genuinely below `pp`, one `pp`-unknown fallback). Keep, too rare to judge.
- **`free_read_not_persisted`: no data.** 0 events in window.

Unifying story: the residual bucket reads as **stale-snapshot serves under
rapid turns**, with kinds measuring staleness depth (missed ⊂ partial ⊂
older-snapshot). Consequence for `recache-commit-latency-proof.md`: the 29
older-snapshot serves are *non-monotonic* (served less than the stream
previously read), which commit latency alone cannot produce — replica lag or
eviction must cover them. The 5s suspect flag stays useful, but "race" is the
monotonic subset, not the whole story.

Recommendation: keep all five positions (nothing here is uniform noise);
rename nothing (metric-label churn for zero query gain) — but fix the
`between_entries` doc comment to "served snapshot a full generation stale"
and strike the 69–114 constant from the partial comment. Names stay,
meanings corrected.


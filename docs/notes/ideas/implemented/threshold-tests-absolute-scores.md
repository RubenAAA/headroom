# Idea: threshold tests must assert absolute scores, not rankings

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/proxy-followups.md` §6 (RRF scoring bug, `3cee05d1`)
- **Summary:** the memory backend's tests asserted presence and ordering, never
  an absolute score against `min_similarity` — so a fused score capped at 0.032
  against a floor of 0.3 passed everything while stopping all injection for two
  days. See learning `learnings/ranking-tests-miss-thresholds.md`.
> **09-11 outcome:** pins exist (ranker, ctx_backend, injection floor); a capped-scores regression fails today.
- **Next (superseded):** any test guarding a threshold asserts the number, not the ranking.


## Detail

*moved from `docs/notes/proxy-followups.md`*

The memory backend's own tests never caught the scoring bug, which had stopped
every injection for two days. They assert on presence and ordering and never on
an absolute score against `min_similarity`, so a fused score capped at 0.032
against a floor of 0.3 passed all of them. Any test guarding a threshold has to
assert the number, not the ranking.

**Closed 2026-09-11 — the demand is met.** Absolute pins now exist:
`ranker.rs:159-161` (exact 0.9/0.7/0.5), `:301` (eps pin),
`ctx_backend.rs:1187-1188` (`rank_to_score` bounds), `injection.rs:189`
(default floor is 0.3), and the incident itself is documented in code
(`ctx_backend.rs:216-217`: the 0.032 cap vs 0.3 floor, 7,265 consecutive
`all_below_min_similarity` events). A capped-scores regression fails these
today.

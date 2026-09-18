# Idea: triage the residual bucket with the new witness split

- **Status:** open (instruments shipped 2026-09-17; needs a 2–3 day window)
- **Source:** 2026-09-17 recache investigation. The 13-event window (56,423
  wasted tokens) held 9 `unexplained_after_replay` events whose forwarded
  fingerprints looked steady by eyeball — beta stable, tools/system stable,
  `history_rewritten=false` — but eyeball over 9 events is not a measurement.
- **Value:** decides whether "mostly unavoidable" stands or hides a systematic
  cause, and routes to the right fix: timing races (client serialization, shed
  cap), key rotations (beta/marker handling), or eviction-or-deeper (nothing
  to do locally).
- **Next:** on the build carrying `landing` + witnesses, split unexplained
  events by `commit_race_suspect` × (`beta_changed` OR `markers_changed`) ×
  `landing`. All fields ride the `cache_recache_observed` line; no new code,
  just the query. Suspect-correlated + `provider_missed_newest_write` ⇒ race;
  rotation flags ⇒ key handling; all-false across landings ⇒ eviction or a
  cause nobody has guessed (see `recache-blind-spot-watch.md` before inventing
  one).

## Deployment + automatic collection (2026-09-17)

- Witness build deployed 2026-09-17T13:27Z (`make build-proxy` +
  `restart-headroom.sh`; live bits verified, rollback at
  `~/.local/bin/headroom-proxy.prev`). Restart resets in-memory counters —
  post-restart totals start at zero by design.
- Daily collector (cron `5 9 * * *`, tag `headroom-recache-triage`):
  `~/.headroom-triage/triage.py --collect ~/.headroom-triage/results.log`.
  Handles old and new log formats; embedded new-build flags override the
  fingerprint/timing join. Baseline 2026-09-17 seeded (115 events, all old
  build).
- Review on/after **2026-09-20**: `tail
  ~/.headroom-triage/results.log` — the script appends `=== WINDOW COMPLETE
  ===` itself once the date passes. Then run the full table (`python3
  ~/.headroom-triage/triage.py`), compare against the retrospective findings
  below, and close or extend this file. Nothing to remember before then; the
  evidence collects itself.

Closes the "what were the recent nine" question prospectively: the past nine
cannot be re-attributed, but the next nine arrive pre-instrumented.

## Findings 2026-09-17 (retrospective, old build)

Ran the split offline over 08:04–13:16Z (115 residual events, 481,985
tokens) by joining `turn_cache_fingerprint` (beta/markers) and
`vs_stock_turn` completion times on `request_id`/conversation key. The
running proxy predates the witness build, so `landing` = old
`attribution_reason`, and commit-gap is fingerprint-time minus previous
completion — an overestimate of the true begin-gap, meaning suspect=true is
certain and borderline cases under-read.

| bucket (suspect, rotated, sibling) | events | wasted tokens |
|---|---|---|
| suspect, markers-advanced, no sibling | 110 | 465,070 |
| not suspect | 5 | 16,915 (one at 5.0s gap is borderline-suspect by the overestimate) |

Gap distribution previous-completion → this fingerprint: p50 0.3s, 109/115
under 3s. By landing: missed_newest 46, partial 39, between 28,
dropped_older 2.

- **Commit-race dominates the residual circumstantially**: ~98% of residual
  tokens sit in sub-5s gaps on sequential (non-overlapping) turns. Correlation,
  not causation — the live suspect flag exists to confirm this prospectively.
- **Beta exonerated for this window**: 0/115 changed; whole-log scan found 6
  beta flips in 2,900 consecutive pairs (all one `a068→c660` rotation), none on
  a residual turn.
- **TTL exonerated**: 0 flips in 2,900 pairs.
- **`markers_changed` as shipped is useless**: markers advance every turn by
  design (`m22.3→m24.0` as the breakpoint follows growth), so the flag fires
  on normal advancement. Needs normalizing (TTL profile + marker count, not
  raw string) before the watch means anything — see follow-up below.
- **Sibling witness caught nothing** (0/115; only 3 multi-key sessions in the
  window). Weak by construction for fan-out, which shares keys; it only sees
  re-keys.
- **4 events (~9.4k tokens) remain genuinely unexplained**: missed_newest at
  24s and 42s gaps, between_entries at 83s — eviction-shaped, too few to
  generalize.

Follow-ups: normalize `markers_changed` (or drop it, keep `forward_markers`
for offline diff); the remaining triage continues automatically once the
witness build deploys — this file stays open until the prospective split
confirms or contradicts the retrospective one.


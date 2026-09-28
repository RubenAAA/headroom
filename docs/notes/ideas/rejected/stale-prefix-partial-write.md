# Idea: stale-prefix partial write (provider never honors a first-turn write)

- **Status:** open, awaiting recurrence data
- **Source:** `scripts/scan-log.sh` 2026-09-22 — conversation `ba0d6fcc187e56c4`
  (sonnet-5, 11:45–11:47Z): 11 `unexplained_after_replay` recaches, ~20k
  tokens each, ~200k total, inside 2 minutes.
- **Shape (distinct from newest-write-timing):** landing
  `provider_partial_of_previous_write` every time. The provider kept serving
  a fixed ~31–36k prefix while the conversation grew 51k→56k — a ~20k block
  written at first turn was never honored, and every turn re-paid it for 2
  minutes (plus 10 `unearned_cache_write_observed`). Bytes matched replay,
  so no normalization can fix it.
- **Value:** if this shape recurs, the fix is write verification, not
  replay: re-read after a large first-turn write, or split the initial
  write so one unhonored block cannot pin the whole prefix stale.
- **Next:** grep the log for `landing = "provider_partial_of_previous_write"`
  with waste > 10k per event; collect 3+ instances with conversation keys,
  models, first-turn write sizes, and whether the prefix ever recovered.
  If all instances are first-turn-anchored, prototype a post-write
  read-back on writes over a threshold.

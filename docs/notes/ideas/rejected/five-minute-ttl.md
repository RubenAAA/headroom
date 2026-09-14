# Rejected: 5-minute cache TTL

- **Status:** rejected 2026-09-03 (simulation loses $25/day)
- **Source:** `docs/notes/savings-ideas-1.md` (ruled out)
- **Summary:** 26,354 inter-turn gaps under 5 min vs 422 between 5–60. A
  simulated 5m TTL pays $25/day more in re-writes than it saves on the 1.25×
  rate. 1h TTL is right.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

- **5m instead of 1h TTL:** 26,354 inter-turn gaps under 5 minutes, 422
  between 5 and 60, 21 over 60. Simulated 5m TTL on opus pays $25/day more in
  re-writes than it saves on the 1.25x rate. 1h is right.

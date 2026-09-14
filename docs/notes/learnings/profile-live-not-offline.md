# Learning: profile live, not offline — porter beat trigram

- **Source:** `docs/speed-ideas.md` §0.1 (first `memory_search_timings` samples)
- **Claim:** the offline Python harness said trigram is 85% of query time;
  live says porter 70 ms of 98 ms, 16-term queries (not 150), wide pass 79% of
  search despite 40 narrow hits. Three plan sub-items were aimed at the wrong
  target each. Caveat recorded too: 3 quiet-box samples vs p50 1,539 ms —
  join on `inflight` before touching the search.
- **Rule:** the harness ranks candidates; the live event picks the lever.

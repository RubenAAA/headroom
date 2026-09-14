# Rejected: cutting the tools[] block further

- **Status:** rejected 2026-09-03 (quality trade, low return)
- **Source:** `docs/notes/savings-ideas-1.md` (measured but not recommended)
- **Summary:** tools[] is $32/day (opus $24) at 0.1× cache-read pricing; pruning
  already removes 3 of 23. Any further cut shrinks what the model sees for
  little money.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

## Measured but not recommended

**tools[] as cache reads: $32/day** (opus $24, sonnet-5 $6.5). 45-58KB and
about 27 tools per turn after `pruned tools[] per policy` removes 3 of 23.
Part of this is the known memory-tool item. Any further cut shrinks what the
model sees, and at 0.1x cache-read pricing it returns little.

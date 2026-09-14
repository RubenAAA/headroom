# Learning: CCR fragments across workers (and what fixes which piece)

- **Source:** `docs/notes/rust-dev.md` (multi-worker deployment)
- **Claim:** per-process state (compression caches, prefix tracker, TOIN,
  CostTracker) fragments under round-robin `--workers N` — a turn-2 on worker
  B can't resolve worker A's marker. Sqlite backend fixes CCR (+ restarts);
  Redis removes stickiness needs; only a sticky LB fixes all of it. Startup
  warns loudly on `N > 1`, scoped to what's actually still per-worker.
  (Note: some `server.py` log lines still assume in-memory-by-default — stale.)

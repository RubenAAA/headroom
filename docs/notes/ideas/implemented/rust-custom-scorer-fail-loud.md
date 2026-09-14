# Implemented: custom scorer closed fail-loud (not silently dropped)

- **Status:** dispositioned 2026-04-29 (full plumbing waits on Stage-3c.2 —
  see `../rust-relevance-bridge.md`)
- **Source:** `docs/notes/rust-dev.md` (SmartCrusher table)
- **Summary:** `relevance_config`/`scorer` args kept for source compat but raise
  `NotImplementedError` when non-None — dropping a user scorer silently is the
  textbook silent-fallback bug.

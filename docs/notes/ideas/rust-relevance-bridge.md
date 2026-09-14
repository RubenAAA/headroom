# Idea: relevance-crate Python bridge (custom scorer plumbing)

- **Status:** open, blocked on Stage-3c.2
- **Source:** `docs/notes/rust-dev.md` (SmartCrusher table: custom scorer closed
  fail-loud)
- **Summary:** custom `relevance_config`/`scorer` args raise
  `NotImplementedError` rather than silently dropping — correct posture, but
  full plumbing waits on Stage-3c.2's relevance-crate Python bridge.
- **Next:** unblock with 3c.2; until then the fail-loud stands (see
  `implemented/rust-custom-scorer-fail-loud.md`).

# Rejected: semantic hold for thinking models

- **Status:** deliberately not built (layering cost + first-paint delay on
  every thinking turn)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (proactive cycling)
- **Summary:** SSE parsing in the retry layer to protect thinking-heavy turns
  (which fill the 8 KB hold during thinking, leaving the answer unprotected)
  costs paint delay everywhere. Deferred; see known non-goal in the source.

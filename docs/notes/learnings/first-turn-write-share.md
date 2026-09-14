# Learning: first turns dominate cache writes

- **Source:** `docs/notes/recache-classification.md` (2026-09-02 audit)
- **Claim:** 215 new sessions' first turns = 7.5M tokens, 42% of all writes;
  system+tools read back at ~17k while the first user message (~24k, ~95k for
  mid-conversation arrivals) is all write. The shared 47 KB CLAUDE.md reminder
  should be read, not written (see open
  `ideas/recache-recall-after-scaffolding.md`).
- **Rule:** any write-share analysis starts by separating first turns.

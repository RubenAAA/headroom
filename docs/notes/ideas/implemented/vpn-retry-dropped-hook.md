# Implemented: Stop hook auto-continues dropped turns

- **Status:** shipped (`retry-dropped-turn.sh`)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md`
- **Summary:** post-commit drops end marked-but-well-formed via the finisher;
  the hook auto-continues tool-discarded AND plain-text truncations (max
  3/session, never fails closed).

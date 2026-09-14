# Implemented: pool idle timeout flag (corpse shrink)

- **Status:** shipped (`--pool-idle-timeout`, default unchanged 90 s; run 25 s)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md`
- **Summary:** rotations strand pooled sockets as corpses; 25 s ages them out
  (was hardcoded 90) at the price of more handshakes. Corpse turns fail fast
  and self-heal through pre-commit retry.

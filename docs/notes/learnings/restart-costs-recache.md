# Learning: never restart the proxy on rotation

- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (+ same-day revert)
- **Claim:** restart wipes replay store, usage observer, CCR tracker → fleet
  recaches everything at once plus an error burst — far worse than ~25 s of
  corpse-RST turns. A restart was briefly wired into the watcher and reverted
  same-day for exactly this. Corpses age out (`--pool-idle-timeout`), turns
  fail fast and self-heal pre-commit; the finisher + retry layers make them
  survivable instead. Accepted cost, documented.

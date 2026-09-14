# Rejected: proxy restart on rotation

- **Status:** rejected and reverted same-day
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md`
- **Summary:** briefly wired into the watcher, then reverted: a restart wipes
  in-memory session state (replay store, usage observer, CCR tracker) and the
  fleet recaches everything at once plus an error burst — far worse than ~25 s
  of corpse-RST turns. Restarts stay forbidden; see learning
  `../learnings/restart-costs-recache.md`.


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

Deliberately NOT done:

- No proxy restart on rotation, ever — restarting wipes in-memory session
   state and forces fleet-wide recaches plus an error burst, far worse than
   the ~25 s of corpse-RST turns while the old pool ages out. (A restart was
  briefly wired into the watcher and reverted same-day for exactly this
  reason.) The corpse window is accepted cost; the finisher + retry layers
  above are what make those turns survivable instead.

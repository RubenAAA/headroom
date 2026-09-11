# Idea: one-time upgrade restart for the rotation-drain machinery

- **Status:** done 2026-09-11 (verified live, no restart needed)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` ("making rotations safe")
- **Close-out:** `GET /debug/inflight` returns 200 `{"in_flight":0}` on the
  running proxy (PID 943, started after the 04:42 binary build) — the upgrade
  restart already happened and the drain machinery is live. No further restart;
  per-rotation restarts stay forbidden.
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` ("making rotations safe")
- **Summary:** `/debug/inflight`, the routed guard, and the pool-idle flag ship
  in the tree but need a restart to go live. One upgrade restart at idle —
  not per rotation (recache cost applies every time; see learning
  `learnings/restart-costs-recache.md`). Until then the watcher rotates
  undrained (endpoint 404s).
- **Next:** restart once at idle, confirm the watcher drains (no 404s).


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

Both proxy pieces (`/debug/inflight`, routed guard, pool flag) ship in the
tree but need a proxy restart to go live — a single one-time upgrade
restart at an idle moment, not per-rotation restarts (which stay
forbidden: the recache cost applies every time). The watcher runs the new
drain code now and simply rotates undrained (endpoint 404s) until then.

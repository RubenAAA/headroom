# Idea: verify the shutdown drain fix on the next deploy

- **Status:** verified 2026-09-14 — 7+ restarts since the fix, every one
  begin→listening in 1–3 s, zero "port still held / SIGKILL" lines; drains
  bounded (8.5 s, 21.6 s clean, one 30 s timeout expiry, no hang).
- **Source:** `docs/speed-ideas.md` §0.3 (2026-09-04)
- **Summary:** `main.rs` used to `sleep(grace)` inside the shutdown future, so
  the timeout delayed the drain instead of bounding it, then waited on
  in-flight connections forever (three 09-03 deploys needed SIGKILL). Now
  returns on signal and bounds the drain with `select!`.
- **Next:** watch the next deploy: pass is `shutdown_drained` with small
  `drain_ms` and no "still up after 40s, forcing"; `shutdown_drain_timed_out`
  + `inflight` tells whether anything was actually streaming.


## Detail

*moved from `docs/notes/speed-ideas.md`*

**3. The next deploy tests the shutdown fix.** `main.rs` used to
`sleep(grace)` inside the shutdown future, so the timeout delayed the drain
instead of bounding it and then waited on in-flight connections forever.
Three deploys on 09-03 needed SIGKILL. It now returns on the signal and
bounds the drain with a `select!`. Watch whether the deploy script still
prints "still up after 40s, forcing"; `shutdown_drained` with a small
`drain_ms` is the pass, `shutdown_drain_timed_out` says it overran and
`inflight` says whether anything was actually still streaming.


## §0b status

*moved from `docs/notes/speed-ideas.md`*

**Shutdown: fix live, verification mixed.** 2 `shutdown_drained` (1.5–2 s
vs 30 s grace — pass) + 1 `shutdown_drain_timed_out` that bounded
correctly at 30 s. Update 2026-09-11: `headroom-proxy.log.1` now shows 3×
`shutdown_drained` (drain_ms 2031, 1458, 609 — all clean passes) + 1×
`shutdown_drain_timed_out` bounded at 30001 ms (no hang, no SIGKILL).
"Still up after 40s" is deploy-script stdout, not a
proxy-log line — that half is still unchecked; look at deploy output,
not the log.

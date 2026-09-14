# Idea: read the running process env, not the flags file, for memory mode

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/proxy-followups.md` §6 (2026-08-21)
- **Summary:** the live proxy had no `HEADROOM_MEMORY_*` in its environment, so
  it ran `auto_tail` instead of the configured `tool` mode — `claude-launcher`
  sources the flags file only in the branch that starts the proxy, so a reused
  live proxy exports nothing. Any memory-mode behavior read off the flags file
  while the process runs different env is misdiagnosed.
> **09-11 outcome:** mitigated via launcher warning; keep the /proc habit, no code worth writing.
- **Next (superseded):** before treating any memory-mode behavior as configured, read
  `/proc/<pid>/environ` (or the process record), not the flags file. Consider a
  startup log line echoing the effective memory mode.


## Detail

*moved from `docs/notes/proxy-followups.md`*

## 6. Two loose ends left by the memory retrieval bug

**Status: open, both found 2026-08-21 while closing the RRF scoring bug in
`3cee05d1`.**

The live proxy had no `HEADROOM_MEMORY_*` in its environment, so it ran
`auto_tail` rather than the configured `tool` mode. `claude-launcher` sources the
flags file only in the branch that starts the proxy, so a run that reuses a live
proxy exports nothing. Read the environment of the running process, not the
flags file, before treating any memory-mode behaviour as configured.

**Half-closed 2026-09-11: trap mitigated, habit retained, no code worth writing.**
Live process (started 02:48, `/proc` env) carries `HEADROOM_MEMORY_MODE=tool`
+ `HEADROOM_MEMORY_INJECT_TOOLS=1` — the failure is not present. The launcher
now warns loudly on exactly the trap (`contrib/claude-launcher:135-139`:
reuse keeps started-with flags; pending flags listed; kill-and-rerun
prescribed). Comparing flag sets across processes to auto-enforce this would
be machinery around a case the warning already covers. Keep the diagnostic
habit above; change nothing.

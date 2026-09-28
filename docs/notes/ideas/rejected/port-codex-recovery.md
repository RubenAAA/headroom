# Idea: port Codex response-recovery

- **Status:** rejected 2026-09-28 — it repairs state the fork never
  creates.
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `providers/codex/recovery.py` (+748, new) — Codex
  response-recovery logic, entirely Python. Distinct from the local
  NPU/Codex translate work (`0fed13b8`).
- **Next:** read the module; decide build vs skip (only matters on Codex
  recovery paths).

## Decision 2026-09-28

`recovery.py` is not response recovery. Its docstring: "Transactional
recovery of Codex state left in a temporary Headroom home." Python
`headroom wrap codex` points `CODEX_HOME` at a temp dir
(`cli/wrap.py:6406`), and this module rescues history and config from
temp homes a crash left behind. Its only callers are `cli/wrap.py:2745` and
`cli/recover.py`. The fork has no wrap CLI (see `rejected/port-wrap-cli.md`) and
nothing in `crates/` or `contrib/` sets `CODEX_HOME`: Codex models are
routed inside a Claude Code session. No temp homes, nothing to recover.

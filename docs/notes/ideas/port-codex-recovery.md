# Idea: port Codex response-recovery

- **Status:** open (large; scoping question first)
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `providers/codex/recovery.py` (+748, new) — Codex
  response-recovery logic, entirely Python. Distinct from the local
  NPU/Codex translate work (`0fed13b8`).
- **Next:** read the module; decide build vs skip (only matters on Codex
  recovery paths).

# Idea: port the OpenAI handler + cold-prefix hooks delta

- **Status:** open (very large)
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `proxy/handlers/openai.py` (+2451/-687, 7 commits) —
  model-aware cold-prefix hooks (Kimi/GLM reasoning compaction), streaming
  fixes. Rust has `handlers/` but not the hook logic.
- **Next:** re-diff; port hook-by-hook with parity fixtures.

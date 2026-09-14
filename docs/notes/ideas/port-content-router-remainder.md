# Idea: close the content-router remainder gap

- **Status:** open, partly done (re-diff, don't port wholesale)
- **Source:** `docs/notes/upstream-port-backlog.md` group A
- **Summary:** Python rewrote router dispatch (`content_router.py` +1518/-313);
  local `7dd551ac` + `e539a3b0` closed part (router/gemini fixes, PHP, tool
  exclusion). What remains is the decision-logic delta vs Rust
  `content_router.rs`.
- **Next:** re-diff and port only what's left.

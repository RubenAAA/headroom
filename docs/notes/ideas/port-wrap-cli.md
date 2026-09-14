# Idea: port the CLI wrapper rewrite (biggest gap)

- **Status:** open (very large; consciously-scoped decision first)
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `cli/wrap.py` (+2760/-562, 20 commits: quiet-CLI env defaults,
  Serena guards, per-agent scaffolding) has no Rust counterpart — the single
  biggest port gap. Related: `cli/install.py` defaults (+493/-47) — see
  `port-install-defaults.md`.
- **Next:** decide port vs skip (Rust CLI wrapper is a product decision, not a
  parity obligation); if porting, start from the scaffolding refactor, not the
  whole file.

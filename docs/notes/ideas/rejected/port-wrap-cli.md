# Idea: port the CLI wrapper rewrite (biggest gap)

- **Status:** rejected 2026-09-28 — the fork launches agents with
  `cclaude` (`contrib/claude-launcher`), not a wrap CLI.
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `cli/wrap.py` (+2760/-562, 20 commits: quiet-CLI env defaults,
  Serena guards, per-agent scaffolding) has no Rust counterpart — the single
  biggest port gap. Related: `cli/install.py` defaults (+493/-47) — see
  `port-install-defaults.md`.
- **Next:** decide port vs skip (Rust CLI wrapper is a product decision, not a
  parity obligation); if porting, start from the scaffolding refactor, not the
  whole file.

## Decision 2026-09-28

Skip. `docs/notes/upstream-triage-0.39.md` already marks every in-range
`wrap` commit N/A on the same ground ("Python wrap CLI; fork uses cclaude"),
and the upstream-port session agreed. `cclaude` starts the proxy, sets
`ANTHROPIC_BASE_URL` and execs `claude`; the Codex and Spark routes run
inside that session through `--extra-model-route`, so there is no second
agent CLI to wrap. Re-open only if the fork decides to ship a Rust `wrap`.

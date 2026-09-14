# Learning: maturin smoke-test outside the repo root

- **Source:** `docs/notes/rust-dev.md` (maturin wiring)
- **Claim:** repo root contains the `headroom/` package, so importing from
  there resolves the full SDK (heavy deps) instead of the maturin namespace
  package. `cd /tmp` first (or install `headroom` into the same venv so
  `_core.so` lands alongside).

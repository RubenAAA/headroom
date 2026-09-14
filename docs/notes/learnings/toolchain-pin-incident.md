# Learning: pin the toolchain (the 2026-04-27 drift incident)

- **Source:** `docs/notes/rust-dev.md` (Phase 0 blockers)
- **Claim:** tracking `stable` let CI drift ahead of dev boxes;
  `rust-toolchain.toml` pins 1.95.0 so a new-stable clippy lint can't break CI
  without firing locally. Don't bump casually.

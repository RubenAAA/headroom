# Implemented: shutdown drain bounded by select!

- **Status:** shipped 2026-09-04 — verify on next deploy (see
  `../verify-shutdown-drain-fix.md`)
- **Source:** `docs/speed-ideas.md` §0.3
- **Summary:** `main.rs` slept `grace` *inside* the shutdown future (delaying
  the drain, then waiting forever; three 09-03 deploys needed SIGKILL). Now
  returns on signal, drain bounded by `select!`.

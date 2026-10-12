# Idea: one cargo target/ per machine, collected on its own

- **Status:** done 2026-10-12. `6a6845f77`: dev builds keep line tables only,
  dependencies no debug info; a full gate build now writes 14.4GB.
  `163721f6c`: `cclaude` started in a checkout or worktree points
  `CARGO_TARGET_DIR` at the main checkout's `target/`, and it and the
  pre-push hook run `scripts/cargo-gc.sh --auto` (at most once a day).
  `2d0073f44`: the GC also removes `target/*/incremental` dirs idle for
  `GC_INCREMENTAL_DAYS` (default 1).
- **Source:** disk-full cleanup, 2026-10-12.
- **Summary:** agents each built into their own target dir (40-80 GB each)
  and nothing removed old output. cargo-sweep skips incremental caches,
  which held 43 of 46 GB in the main `target/`. Free disk went from 49G to
  394G after the cleanup, which also removed 77 idle worktrees.
- **Rule:** in AGENTS.md, "One `target/` per machine".

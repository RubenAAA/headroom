# Shared-worktree protocol (parallel sessions)

Date: 2026-09-11. Context: several sessions share this checkout and an
uncommitted implementation was lost to a worktree-wide revert tonight.
Binding on every session working here, including the author of this note.

1. NEVER run worktree-wide destructive commands: `git checkout -- .`,
   `git restore .`, `git clean` (any flags), `git reset --hard`,
   `git stash -u`. No exceptions.
2. Stage and commit ONLY files you authored: `git add <your paths>`,
   never `git add -A` / `git add .`. Never stage, unstage, or commit
   another session's files.
3. Commit your own work promptly so it survives other sessions' mistakes.
4. `cargo fmt` formats the whole workspace even with file args — check
   `git status`/`git diff --stat` after running it and revert hunks in
   files you don't own (read them first; never revert unread files).

## Current file claims (do not touch others' files)

- cursor bridge mismatch diagnostics + breaker:
  `crates/headroom-proxy/src/cursor/bridge.rs`,
  `crates/headroom-proxy/src/cursor/handler.rs`
  (uncommitted; hands off — will be committed by its author)
- P1 gate seeding + follow-ups:
  `crates/headroom-proxy/src/cache_stabilization/drift_detector.rs`,
  `crates/headroom-proxy/src/compression/ctx_offload.rs`,
  `docs/ctx-cross-session-gate-seeding-plan.md`

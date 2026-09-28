#!/usr/bin/env bash
# Install the local-only pre-push hook: symlink .git/hooks/pre-push to
# scripts/hooks/pre-push, which lists what it runs. A symlink rather than a
# copy, so a `git pull` that changes the gate changes it for the next push; a
# copy went stale once and skipped `--all-targets` and cargo deny for days.
#
# Idempotent; replaces an older copied hook. Bypass per-push with
# `git push --no-verify`.
# (Replaces upstream-python/scripts/install-git-hooks.sh for local use;
# that script pulls npx + venv + full ci-precheck and is left untouched.)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOKS_DIR="$(git -C "$ROOT" rev-parse --path-format=absolute --git-path hooks)"
HOOK="$HOOKS_DIR/pre-push"

mkdir -p "$HOOKS_DIR"
ln -sfn "$ROOT/scripts/hooks/pre-push" "$HOOK"
echo "✅ installed: $HOOK -> $ROOT/scripts/hooks/pre-push"
echo "   Runs fmt + clippy + what-to-run --run + check-drift + check-log-events + check-complexity + check-file-size + check-hygiene + cargo deny."
echo "   Hygiene needs cargo-machete, taplo-cli, cargo-sort for its optional legs:"
echo "     cargo install cargo-machete taplo-cli cargo-sort --locked"
echo "   Each leg skips gracefully when its tool is absent."
echo "   Bypass: git push --no-verify"

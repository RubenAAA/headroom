#!/usr/bin/env bash
# Install the local-only pre-push hook. No CI, no network, no npx/venv.
#
# The hook runs, on every `git push`:
#   1. cargo fmt --check (fast fail)
#   2. cargo clippy --workspace (fast fail)
#   3. scripts/what-to-run.sh --run (touched-area suites only, not the
#      full workspace — full `make test-nextest` stays a manual call)
#   4. scripts/check-drift.sh (flags.md freshness, shellcheck, var coverage;
#      runs inside what-to-run when config/scripts changed, plus once here
#      unconditionally because it is seconds-cheap)
#
# Idempotent. Bypass per-push with `git push --no-verify`.
# (Replaces upstream-python/scripts/install-git-hooks.sh for local use;
# that script pulls npx + venv + full ci-precheck and is left untouched.)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK="$ROOT/.git/hooks/pre-push"

if [[ ! -d "$ROOT/.git/hooks" ]]; then
    echo "error: .git/hooks/ not found — run from a git checkout root" >&2
    exit 1
fi

cat > "$HOOK" <<'HOOK_EOF'
#!/usr/bin/env bash
# Local-only pre-push: fmt + clippy + touched-area tests + drift.
# Bypass: git push --no-verify
set -euo pipefail
ROOT="$(git rev-parse --show-toplevel)"

while IFS=' ' read -r local_ref local_sha remote_ref remote_sha; do
    if [[ "$local_sha" == "0000000000000000000000000000000000000000" ]]; then
        continue
    fi
    echo "── pre-push (local): verifying $local_ref → $remote_ref"
    # Pushed range, not the working tree: what-to-run maps --base...HEAD,
    # and for the current branch local_sha == HEAD at push time. New
    # branches (remote zero) keep the working-tree default below.
    if [[ "$remote_sha" != "0000000000000000000000000000000000000000" && -z "${PUSH_BASE:-}" ]]; then
        PUSH_BASE="$remote_sha"
    fi
done

cd "$ROOT"
echo "── pre-push (local): cargo fmt --check"
cargo fmt --all -- --check || {
    echo "❌ pre-push: fmt failed. Run 'cargo fmt --all'." >&2
    echo "   Bypass: git push --no-verify" >&2
    exit 1
}
echo "── pre-push (local): cargo clippy"
cargo clippy --workspace -- -D warnings || {
    echo "❌ pre-push: clippy failed." >&2
    echo "   Bypass: git push --no-verify" >&2
    exit 1
}
echo "── pre-push (local): touched-area suites"
if [[ -n "${PUSH_BASE:-}" ]]; then
    bash scripts/what-to-run.sh --base "$PUSH_BASE" --run || {
        echo "❌ pre-push: touched-area tests failed." >&2
        echo "   Bypass: git push --no-verify" >&2
        exit 1
    }
else
    bash scripts/what-to-run.sh --run || {
        echo "❌ pre-push: touched-area tests failed." >&2
        echo "   Bypass: git push --no-verify" >&2
        exit 1
    }
fi
echo "── pre-push (local): drift checks"
bash scripts/check-drift.sh || {
    echo "❌ pre-push: drift checks failed." >&2
    echo "   Bypass: git push --no-verify" >&2
    exit 1
}
echo "── pre-push (local): log-event ratchet"
bash scripts/check-log-events.sh || {
    echo "❌ pre-push: new warn!/error! without event field." >&2
    echo "   Bypass: git push --no-verify" >&2
    exit 1
}
echo "✅ pre-push (local): PASSED"
HOOK_EOF

chmod +x "$HOOK"
echo "✅ installed: $HOOK"
echo "   Runs fmt + clippy + what-to-run --run + check-drift + check-log-events."
echo "   Bypass: git push --no-verify"

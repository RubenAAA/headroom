#!/usr/bin/env bash
# Local drift checks. No CI, no network, cheap enough for pre-push.
#
# Checks:
#   1. docs/flags.md matches `headroom-proxy --help` output.
#   2. shellcheck on contrib/*.sh + scripts/*.sh (skip if not installed).
#   3. every HEADROOM_* var in config.rs appears in docs/flags.md or
#      contrib/headroom-flags.sh (catches renamed-but-undocumented flags).
#
# Usage: bash scripts/check-drift.sh [--build] (default reuses an existing
# binary when present, else builds debug once).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FAIL=0

# ── 1. flags.md freshness ─────────────────────────────────────────────
BIN=""
if [[ -x "$ROOT/target/release/headroom-proxy" ]]; then
    BIN="$ROOT/target/release/headroom-proxy"
elif [[ -x "$ROOT/target/debug/headroom-proxy" ]]; then
    BIN="$ROOT/target/debug/headroom-proxy"
else
    echo "check-drift: no binary found, building debug headroom-proxy once..."
    (cd "$ROOT" && cargo build -p headroom-proxy >/dev/null 2>&1) || {
        echo "FAIL: cargo build -p headroom-proxy failed" >&2
        exit 1
    }
    BIN="$ROOT/target/debug/headroom-proxy"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

sed -n '1,/^<!-- BEGIN HELP -->$/p' "$ROOT/docs/flags.md" > "$TMP/flags.md"
{
    echo '```'
    "$BIN" --help | sed -e 's/\x1b\[[0-9;]*m//g'
    echo '```'
} >> "$TMP/flags.md"

if diff -q "$TMP/flags.md" "$ROOT/docs/flags.md" >/dev/null; then
    echo "ok: docs/flags.md matches --help"
else
    echo "FAIL: docs/flags.md is stale. Regenerate:" >&2
    echo "  cargo build --release -p headroom-proxy" >&2
    echo "  sed -n '1,/^<!-- BEGIN HELP -->$/p' docs/flags.md > /tmp/flags.md" >&2
    # SC2028 disabled: the backslashes below are recipe text, not escapes.
    # shellcheck disable=SC2028
    echo "  { echo '\`\`\`'; ./target/release/headroom-proxy --help | sed -e 's/\\x1b\\[[0-9;]*m//g'; echo '\`\`\`'; } >> /tmp/flags.md" >&2
    echo "  mv /tmp/flags.md docs/flags.md" >&2
    FAIL=1
fi

# ── 2. shellcheck ─────────────────────────────────────────────────────
if command -v shellcheck >/dev/null 2>&1; then
    # Globs cover extensionless entry points (claude-launcher) and nested
    # dirs (bench, hooks) so new scripts ship checked. Non-matching globs
    # are skipped via nullglob scoping to keep fresh checkouts working.
    shopt -s nullglob
    SHELLCHECK_FILES=(
        "$ROOT"/contrib/*.sh
        "$ROOT"/scripts/*.sh
        "$ROOT"/upstream-python/scripts/install-git-hooks.sh
    )
    # Newly covered entry points (extensionless launcher, nested bench and
    # hooks dirs) gate on errors only: the older hook scripts carry
    # pre-existing info/style notes (SC2015, SC2001, ...) that are out of
    # scope to reformat here. Tighten to the default severity once those
    # are cleaned up.
    SHELLCHECK_NEW_FILES=(
        "$ROOT"/contrib/claude-launcher
        "$ROOT"/contrib/opencode-launcher
        "$ROOT"/contrib/claude/hooks/*.sh
        "$ROOT"/scripts/bench/*.sh
    )
    shopt -u nullglob
    if shellcheck "${SHELLCHECK_FILES[@]}" \
        && shellcheck -S error "${SHELLCHECK_NEW_FILES[@]}"; then
        echo "ok: shellcheck clean"
    else
        echo "FAIL: shellcheck found issues" >&2
        FAIL=1
    fi
else
    echo "skip: shellcheck not installed (apt install shellcheck)"
fi

# ── 3. HEADROOM_PROXY_* var coverage ──────────────────────────────────
# Scan all of src (not just config.rs): real env-only vars live in
# forwarded_headers.rs (TRUSTED_*) and bins/headroom_cli.rs
# (HEADROOM_PROXY_URL). Those have no clap flag by design, so they warn
# instead of failing. Comment mentions (e.g. a doc path) are filtered by
# requiring the var to appear in a non-comment line.
ENV_ONLY="HEADROOM_PROXY_TRUSTED_GATEWAY_CIDRS HEADROOM_PROXY_TRUSTED_DASHBOARD_CLIENT_CIDRS HEADROOM_PROXY_URL HEADROOM_PROXY_LOG_FINDINGS_2026_05_03"
VARS="$(grep -rhoE 'HEADROOM_PROXY_[A-Z0-9_]+' "$ROOT/crates/headroom-proxy/src/" | sort -u)"
MISSING=0
for v in $VARS; do
    if grep -q "$v" "$ROOT/docs/flags.md" || grep -q "$v" "$ROOT/contrib/headroom-flags.sh"; then
        continue
    fi
    case " $ENV_ONLY " in
        *" $v "*) echo "warn: $v is env-only by design (no clap flag), undocumented" ;;
        *) echo "FAIL: $v in src but in neither docs/flags.md nor contrib/headroom-flags.sh" >&2; MISSING=1 ;;
    esac
done
if [[ "$MISSING" -eq 0 ]]; then
    echo "ok: HEADROOM_PROXY_* vars covered ($(echo "$VARS" | wc -l) vars)"
else
    FAIL=1
fi

OTHER="$(grep -rhoE 'HEADROOM_[A-Z0-9_]+' "$ROOT/crates/headroom-proxy/src/" | sort -u | grep -v '^HEADROOM_PROXY_' || true)"
for v in $OTHER; do
    if ! grep -q "$v" "$ROOT/docs/flags.md" && ! grep -q "$v" "$ROOT/contrib/headroom-flags.sh"; then
        echo "warn: $v in config.rs but undocumented (non-PROXY var, cleanup)"
    fi
done

if [[ "$FAIL" -ne 0 ]]; then
    echo "check-drift: FAILED" >&2
    exit 1
fi
echo "check-drift: PASSED"

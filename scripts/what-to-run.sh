#!/usr/bin/env bash
# Change -> test map for local blast-radius checks. No CI, no network.
#
# Usage:
#   scripts/what-to-run.sh [--base <ref>] [--run] [--list]
#
# Default maps the working tree (staged + unstaged + untracked vs HEAD) —
# the changes you are about to push. Pass --base origin/main to map the
# whole branch range instead.
#
# Mapping is conservative: unknown files fall back to test-unit; cache and
# routing changes pull in the integration suites that actually guard them.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE=""
RUN=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --base) BASE="$2"; shift 2 ;;
        --run) RUN=1; shift ;;
        --list) RUN=0; shift ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$BASE" ]]; then
    BASE="HEAD"
fi

if [[ "$BASE" == "HEAD" ]]; then
    mapfile -t FILES < <(git -C "$ROOT" diff --name-only HEAD)
    mapfile -t UNTRACKED < <(git -C "$ROOT" ls-files --others --exclude-standard)
    FILES=("${FILES[@]}" "${UNTRACKED[@]}")
else
    mapfile -t FILES < <(git -C "$ROOT" diff --name-only "$BASE"...HEAD 2>/dev/null || git -C "$ROOT" diff --name-only HEAD)
    mapfile -t UNTRACKED < <(git -C "$ROOT" ls-files --others --exclude-standard)
    FILES=("${FILES[@]}" "${UNTRACKED[@]}")
fi

declare -A SEEN=()
FILES_UNIQ=()
for f in "${FILES[@]}"; do
    [[ -z "${f:-}" ]] && continue
    if [[ -z "${SEEN[$f]:-}" ]]; then
        SEEN[$f]=1
        FILES_UNIQ+=("$f")
    fi
done

hit() {
    # `case` glob-matches by design (patterns like
    # "crates/headroom-proxy/src/cache_stabilization/*"); [[ == ]] would
    # need an unquoted RHS for the same, which shellcheck flags.
    local pat="$1" f
    # shellcheck disable=SC2254
    for f in "${FILES_UNIQ[@]}"; do case "$f" in $pat) return 0 ;; esac; done
    return 1
}

# Prefer nextest (what CI shards run); fall back to plain cargo test so
# fresh checkouts without cargo-nextest still work (mirrors Makefile).
if command -v cargo-nextest >/dev/null 2>&1; then
    UNIT_PROXY_CACHE="cargo nextest run -p headroom-proxy --profile ci -E 'kind(lib) and test(cache_stabilization)'"
    INT_CACHE="cargo nextest run -p headroom-proxy --profile ci --test cache --test capture --test ccr"
    INT_ROUTED="cargo nextest run -p headroom-proxy --profile ci --test routing --test ccr --test integration_local_model --test sse"
    UNIT_PROXY_CONFIG="cargo nextest run -p headroom-proxy --profile ci -E 'kind(lib) and test(config)'"
    UNIT_CORE_COST="cargo nextest run -p headroom-core --profile ci -E 'kind(lib) and (test(cost_tracker) or test(pricing) or test(savings))'"
    UNIT_CORE_XFORM="cargo nextest run -p headroom-core --profile ci -E 'kind(lib) and (test(transforms) or test(compression) or test(crusher))'"
else
    UNIT_PROXY_CACHE="cargo test -p headroom-proxy --lib cache_stabilization"
    INT_CACHE="cargo test -p headroom-proxy --test cache --test capture --test ccr"
    INT_ROUTED="cargo test -p headroom-proxy --test routing --test ccr --test integration_local_model --test sse"
    UNIT_PROXY_CONFIG="cargo test -p headroom-proxy --lib config"
    UNIT_CORE_COST="cargo test -p headroom-core --lib cost_tracker"
    UNIT_CORE_XFORM="cargo test -p headroom-core --lib transforms"
fi

CMDS=()
NOTES=()
add() { CMDS+=("$1"); NOTES+=("$2"); }

if [[ ${#FILES_UNIQ[@]} -eq 0 ]]; then
    add "make test-unit" "no changes detected; sanity: test-unit"
else
    if hit "crates/headroom-proxy/src/cache_stabilization/*" || hit "crates/headroom-proxy/tests/cache_key_contract.rs"; then
        add "$UNIT_PROXY_CACHE" "cache unit"
        add "$INT_CACHE" "contract + prefix/roster/order suites"
    fi
    if hit "crates/headroom-proxy/src/routed/*" || hit "crates/headroom-proxy/src/handlers/*" || hit "crates/headroom-proxy/src/sidecar.rs" || hit "crates/headroom-proxy/src/sse/*" || hit "crates/headroom-proxy/src/output_shaper.rs" || hit "crates/headroom-proxy/src/model_router.rs"; then
        add "$INT_ROUTED" "routed + sidecar + sse suites"
    fi
    if hit "crates/headroom-proxy/src/config.rs" || hit "docs/flags.md" || hit "contrib/headroom-flags.sh"; then
        add "bash scripts/check-drift.sh" "flags/docs drift"
        add "$UNIT_PROXY_CONFIG" "config unit"
    fi
    if hit "crates/headroom-core/src/pricing.rs" || hit "crates/headroom-core/src/cost_tracker.rs" || hit "crates/headroom-core/src/savings*" ; then
        add "$UNIT_CORE_COST" "core cost tests"
        add "make test-parity" "parity fixtures"
    fi
    if hit "crates/headroom-core/src/transforms/*" || hit "crates/headroom-core/src/compression*" || hit "crates/headroom-core/src/smart_crusher/*"; then
        add "$UNIT_CORE_XFORM" "core transform tests"
        add "make test-parity" "parity fixtures"
    fi
    if hit "contrib/*" || hit "scripts/*"; then
        add "bash scripts/check-drift.sh" "shellcheck + drift"
    fi
    if [[ ${#CMDS[@]} -eq 0 ]]; then
        add "make test-unit" "no mapped area; fallback: fast unit loop"
    fi
fi

echo "what-to-run: base=${BASE} files=${#FILES_UNIQ[@]}"
for f in "${FILES_UNIQ[@]}"; do echo "  changed: $f"; done
echo "recommended:"
i=0
for c in "${CMDS[@]}"; do
    n="${NOTES[$i]:-}"
    echo "  - $c${n:+  # $n}"
    i=$((i + 1))
done

if [[ "$RUN" -eq 1 ]]; then
    for c in "${CMDS[@]}"; do
        echo "── running: $c"
        (cd "$ROOT" && bash -c "$c")
    done
fi

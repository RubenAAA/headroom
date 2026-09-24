#!/usr/bin/env bash
# Ratchet: no Rust source file may grow past 3000 lines, and files already
# past it may not grow further.
#
# Why: proxy.rs was split from 17,192 to 13,303 lines on 2026-09-21 and was
# back to 14,964 three days later. New work lands in the file everyone
# already has open. On 2026-09-25 it went to about 1,100 lines, with the
# rest in proxy/<area>.rs. Put new code in the module for its area, or a
# new one.
#
# scripts/file-size-baseline.txt pins `path max_lines` for each file over
# the limit. `*` in place of a number exempts a file that grows by design.
# Re-pin after shrinking a file with --update-baseline.
#
# Usage: bash scripts/check-file-size.sh [--update-baseline]

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASELINE="$ROOT/scripts/file-size-baseline.txt"
LIMIT=3000

cd "$ROOT"
sizes() {
    find crates -name '*.rs' -not -path '*/target/*' -print0 | xargs -0 wc -l |
        awk -v limit="$LIMIT" '$2 != "total" && $1 > limit { print $2, $1 }' | sort
}

if [[ "${1:-}" == "--update-baseline" ]]; then
    exempt="$(awk '$2 == "*"' "$BASELINE" 2>/dev/null || true)"
    {
        grep '^#' "$BASELINE" 2>/dev/null || true
        sizes | while read -r path lines; do
            if grep -qx "$path \*" <<<"$exempt"; then
                echo "$path *"
            else
                echo "$path $lines"
            fi
        done
    } >"$BASELINE.tmp"
    mv "$BASELINE.tmp" "$BASELINE"
    echo "re-pinned: $BASELINE"
    exit 0
fi

fail=0
while read -r path lines; do
    pinned="$(awk -v p="$path" '$1 == p { print $2 }' "$BASELINE")"
    if [[ "$pinned" == "*" ]]; then
        continue
    elif [[ -z "$pinned" ]]; then
        echo "FAIL: $path is $lines lines, over the $LIMIT limit. Split it by area." >&2
        fail=1
    elif ((lines > pinned)); then
        echo "FAIL: $path grew $pinned -> $lines lines. Put new code in a module for its area." >&2
        fail=1
    elif ((lines < pinned)); then
        echo "note: $path shrank $pinned -> $lines; re-pin with --update-baseline"
    fi
done < <(sizes)
if ((fail)); then
    exit 1
fi
echo "ok: no file over $LIMIT lines grew"

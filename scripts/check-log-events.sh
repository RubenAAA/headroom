#!/usr/bin/env bash
# Ratchet: every tracing::warn!/error! must carry `event = "..."`.
#
# Why: on 2026-09-22, 15 memory/CCR continuation 403s logged with no `event`
# field and collapsed into an ungroupable bucket during a log sweep — the
# cluster was in the data and invisible. New warn/error sites without an
# event re-create that blind spot.
#
# Legacy sites (155 at pinning) are grandfathered via the baseline count;
# the check fails only when the count GROWS, and prints every currently
# missing site so the new ones are identifiable. Drive the count down by
# adding events to old sites and re-pinning with --update-baseline.
#
# Usage: bash scripts/check-log-events.sh [--update-baseline]

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASELINE="$ROOT/scripts/log-events-baseline.txt"

count_missing() {
    python3 - "$ROOT" <<'EOF'
import re, sys, pathlib
root = pathlib.Path(sys.argv[1])
macro_re = re.compile(r'tracing::(warn|error)!\s*\(')
total = 0
per_crate = {}
for p in sorted((root / 'crates').rglob('*.rs')):
    if '/target/' in str(p) or '/tests/' in str(p):
        continue
    try:
        lines = p.read_text().splitlines()
    except OSError:
        continue
    i = 0
    while i < len(lines):
        m = macro_re.search(lines[i])
        if m:
            depth = 0
            j = i
            body = []
            started = False
            while j < len(lines):
                for ch in lines[j]:
                    if ch == '(':
                        depth += 1
                        started = True
                    elif ch == ')':
                        depth -= 1
                body.append(lines[j])
                if started and depth <= 0:
                    break
                j += 1
                if j - i > 60:
                    break
            if 'event =' not in '\n'.join(body) and 'event=' not in '\n'.join(body):
                total += 1
                crate = p.relative_to(root / 'crates').parts[0]
                per_crate[crate] = per_crate.get(crate, 0) + 1
            i = j
        i += 1
print(f'total={total}')
for crate in sorted(per_crate):
    print(f'{crate}={per_crate[crate]}')
EOF
}

list_missing() {
    python3 - "$ROOT" <<'EOF'
import re, sys, pathlib
root = pathlib.Path(sys.argv[1])
macro_re = re.compile(r'tracing::(warn|error)!\s*\(')
for p in sorted((root / 'crates').rglob('*.rs')):
    if '/target/' in str(p) or '/tests/' in str(p):
        continue
    try:
        lines = p.read_text().splitlines()
    except OSError:
        continue
    i = 0
    while i < len(lines):
        m = macro_re.search(lines[i])
        if m:
            macro = m.group(1)
            depth = 0
            j = i
            body = []
            started = False
            while j < len(lines):
                for ch in lines[j]:
                    if ch == '(':
                        depth += 1
                        started = True
                    elif ch == ')':
                        depth -= 1
                body.append(lines[j])
                if started and depth <= 0:
                    break
                j += 1
                if j - i > 60:
                    break
            if 'event =' not in '\n'.join(body) and 'event=' not in '\n'.join(body):
                print(f'{macro:5s} {p.relative_to(root)}:{i + 1} :: {lines[i].strip()[:100]}')
            i = j
        i += 1
EOF
}

if [[ "${1:-}" == "--update-baseline" ]]; then
    count_missing > "$BASELINE"
    echo "re-pinned: $BASELINE"
    cat "$BASELINE"
    exit 0
fi

if [[ ! -f "$BASELINE" ]]; then
    echo "error: baseline $BASELINE missing (run with --update-baseline)" >&2
    exit 2
fi

CURRENT="$(count_missing)"
cur_total="$(echo "$CURRENT" | sed -n 's/^total=//p')"
base_total="$(sed -n 's/^total=//p' "$BASELINE")"

if [[ "$cur_total" -gt "$base_total" ]]; then
    echo "FAIL: warn!/error! sites without event grew ${base_total} -> ${cur_total}." >&2
    echo "Add event = \"...\" to the new sites below (or fix old ones and re-pin):" >&2
    list_missing
    exit 1
fi
echo "ok: event-less warn/error sites ${cur_total} <= baseline ${base_total}"

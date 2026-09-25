#!/usr/bin/env bash
# Ratchet: no function may exceed clippy's cognitive complexity threshold
# (clippy.toml, 25) unless the baseline names it.
#
# Why: the 2026-09 cleanup brought every production function under 25; by
# 2026-09-24 eleven had crept back over. The lint is allow-by-default, so
# nothing flagged them.
#
# Test tables that are long but flat carry #[allow(clippy::cognitive_complexity)]
# at the function. The baseline holds functions still waiting on a refactor,
# one `path::fn_name` per line; names, not line numbers, so edits elsewhere in
# the file do not break it. Remove an entry once its function is under 25.
#
# Usage: bash scripts/check-complexity.sh

set -euo pipefail

if ! command -v python3 >/dev/null 2>&1; then
    echo "── complexity: python3 not installed; skipping"
    exit 0
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASELINE="$ROOT/scripts/complexity-baseline.txt"

MESSAGES="$(mktemp)"
trap 'rm -f "$MESSAGES"' EXIT

cd "$ROOT"
status=0
cargo clippy -p headroom-core -p headroom-proxy -p headroom-simulators -p headroom-parity \
    --all-targets --message-format=json --quiet -- -W clippy::cognitive_complexity \
    >"$MESSAGES" 2>/dev/null || status=$?

python3 - "$ROOT" "$BASELINE" "$MESSAGES" "$status" <<'EOF'
import json, re, sys, pathlib
root, baseline = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
allowed = {l.strip() for l in baseline.read_text().splitlines()
           if l.strip() and not l.startswith('#')}
over = {}
for line in open(sys.argv[3]):
    try:
        msg = json.loads(line).get('message') or {}
    except ValueError:
        continue
    if msg.get('level') == 'error':
        print(msg.get('rendered', ''), file=sys.stderr, end='')
        continue
    if (msg.get('code') or {}).get('code') != 'clippy::cognitive_complexity':
        continue
    span = next(s for s in msg['spans'] if s['is_primary'])
    src = (root / span['file_name']).read_text().splitlines()[span['line_start'] - 1]
    m = re.search(r'\bfn\s+(\w+)', src)
    key = f"{span['file_name']}::{m.group(1) if m else span['line_start']}"
    score = re.search(r'\((\d+/\d+)\)', msg['message'])
    over[key] = f"{span['file_name']}:{span['line_start']} ({score.group(1) if score else '?'})"
if sys.argv[4] != '0':
    print(f'FAIL: cargo clippy exited {sys.argv[4]}', file=sys.stderr)
    sys.exit(1)
new = sorted(k for k in over if k not in allowed)
for k in sorted(allowed - over.keys()):
    print(f'note: {k} is under the threshold now; drop it from {baseline.name}')
if new:
    print('FAIL: functions over the cognitive complexity threshold:', file=sys.stderr)
    for k in new:
        print(f'  {over[k]}  {k.rsplit("::", 1)[1]}', file=sys.stderr)
    print('Split them, or for a flat test table add #[allow(clippy::cognitive_complexity)].',
          file=sys.stderr)
    sys.exit(1)
print(f'ok: {len(over)} over threshold, all in {baseline.name}')
EOF

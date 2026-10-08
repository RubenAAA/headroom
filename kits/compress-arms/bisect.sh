#!/usr/bin/env bash
# Bisect: restart baseline proxy with EXTRA_FLAGS, POST p02 once, report capture size.
# Usage: bisect.sh "extra flags..." <tag>
set -u
BASE=/tmp/compress-arms
PORT=8789
# kill existing side proxy on 8789 (never touch 8787)
PID=$(ss -tlnp 2>/dev/null | grep -oP ':8789.*users:\(\("headroom-proxy",pid=\K[0-9]+' || true)
if [ -n "${PID:-}" ]; then kill "$PID"; fi
for i in $(seq 1 20); do ss -tln 2>/dev/null | grep -q ':8789' || break; sleep 0.5; done
rm -f "$BASE"/capture/baseline/*
python3 -I "$BASE/stub.py" baseline > "$BASE/logs/stub-baseline.log" 2>&1 &
STUBPID=$!
sleep 1
# shellcheck disable=SC2086
env -u HEADROOM_MODEL_ROUTER_ENABLED -u HEADROOM_MODEL_ROUTES \
  XDG_STATE_HOME=$BASE/xdg/state XDG_DATA_HOME=$BASE/xdg/data \
  $HOME/.local/bin/headroom-proxy \
  --listen "127.0.0.1:$PORT" \
  --upstream http://127.0.0.1:18881 \
  --compression \
  --compression-mode all_messages \
  --enable-cross-turn-dedup \
  --code-aware true \
  --cache false \
  --memory false \
  --memory-project-root "$BASE/memproj" \
  --replay-store-dir "$BASE/replay-bisect" \
  --ctx-store-dir "$BASE/ctx-baseline" \
  --lossless false \
  --enable-kompress false \
  --disable-kompress true \
  $1 \
  > "$BASE/logs/bisect-$2.log" 2>&1 &
PROXYPID=$!
sleep 5
SIZE=$(python3 -I -c "
import urllib.request
body = open('$BASE/payloads/p02-log-medium.json','rb').read()
req = urllib.request.Request('http://127.0.0.1:$PORT/v1/messages', data=body, headers={'Content-Type':'application/json','x-api-key':'dummy','anthropic-version':'2023-06-01'})
r = urllib.request.urlopen(req, timeout=60); r.read()
import os
print(os.path.getsize('$BASE/capture/baseline/001.bin'))
")
echo "tag=$2 extra=[$1] p02_orig=10141 fwd=$SIZE"
kill "$PROXYPID" "$STUBPID" 2>/dev/null
wait 2>/dev/null

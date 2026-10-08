#!/usr/bin/env bash
# Launch a side proxy for one arm. Usage: side_proxy.sh <arm> <port>
# arm: baseline | lossless | kompress
# Isolated state: logs, replay store, ctx store under /tmp/compress-arms/.
# Live proxy on 8787 untouched; upstream is the local stub (no paid APIs).
set -u
ARM=$1
PORT=$2
BASE=/tmp/compress-arms
BIN=$HOME/.local/bin/headroom-proxy

LOSSLESS=false
EN_KOMPRESS=false
DIS_KOMPRESS=true
EXCLUDE_TOOLS=
case $ARM in
  baseline) ;;
  lossless) LOSSLESS=true ;;
  losslessfold) EXCLUDE_TOOLS='Bash,BashOutput,Grep,Read' ;;
  kompress) EN_KOMPRESS=true; DIS_KOMPRESS=false ;;
  *) echo "unknown arm: $ARM" >&2; exit 1 ;;
esac

# Clean env: no model router, no memory injection, no launcher leftovers.
unset HEADROOM_MODEL_ROUTER_ENABLED HEADROOM_MODEL_ROUTES HEADROOM_MEMORY_INJECT_TOOLS HEADROOM_MEMORY_MODE
export HEADROOM_MODEL_ROUTER_ENABLED=0
export XDG_STATE_HOME=$BASE/xdg/state
export XDG_DATA_HOME=$BASE/xdg/data
# NOTE: HOME and XDG_CACHE_HOME intentionally untouched so kompress can
# resolve ~/.cache/huggingface cache-only (proxy never downloads).

exec $BIN \
  --listen "127.0.0.1:$PORT" \
  --upstream http://127.0.0.1:18881 \
  --compression \
  --compression-mode all_messages \
  --prefix-replay true \
  --enable-cross-turn-dedup \
  --code-aware true \
  --smart-crusher-compaction true \
  --max-items-after-crush 15 \
  --cache false \
  --cache-tail-breakpoints 2 \
  --cache-pin-tool-roster true \
  --memory false \
  --memory-project-root $BASE/memproj \
  --ctx-store-dir "$BASE/ctx-$ARM" \
  --replay-store-dir "$BASE/replay-$ARM" \
  --lossless $LOSSLESS \
  --enable-kompress $EN_KOMPRESS \
  --disable-kompress $DIS_KOMPRESS \
  --disable-kompress-fallback true \
  --disable-kompress-anthropic false \
  --disable-kompress-openai false \
  --force-kompress-all false \
  ${EXCLUDE_TOOLS:+--exclude-tools "$EXCLUDE_TOOLS"} \
  >>"$BASE/logs/$ARM.log" 2>&1

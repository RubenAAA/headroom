#!/usr/bin/env bash
# msg0 side proxies. Installed binary only; state+logs under /tmp/msg0.
# Ports: stub-st 18881, stub-mt 18880, test-st 8788, control-st 18882, test-mt 8789.
set -u
BIN=~/.local/bin/headroom-proxy
mkproxy() { # name listen upstream extra-enforcement-flag
  local name=$1 listen=$2 upstream=$3 enf=$4
  $BIN \
    --listen "$listen" \
    --upstream "$upstream" \
    --compression \
    --compression-mode all_messages \
    --auth-mode-policy-enforcement "$enf" \
    --cache false \
    --replay-store-dir /tmp/msg0/state-"$name"/replay-prefixes \
    --ctx-store-dir /tmp/msg0/state-"$name"/ctx \
    --log-level info \
    >>/tmp/msg0/"$name".log 2>&1 &
  echo "$name pid=$!"
}
case "${1:-}" in
  stubs)
    MSG0_CAPTURE_DIR=/tmp/msg0/captures-st MSG0_STUB_PORT=18881 \
      MSG0_STUB_QUEUE=2 MSG0_STUB_DELAY_MS=100 \
      python3 -I /tmp/msg0/stub_st.py >>/tmp/msg0/stub-st.log 2>&1 &
    echo "stub-st pid=$!"
    MSG0_CAPTURE_DIR=/tmp/msg0/captures-mt MSG0_STUB_PORT=18880 \
      python3 -I /tmp/msg0/stub_mt.py >>/tmp/msg0/stub-mt.log 2>&1 &
    echo "stub-mt pid=$!"
    ;;
  proxies)
    mkproxy test-st 127.0.0.1:8788 http://127.0.0.1:18881 disabled
    mkproxy control-st 127.0.0.1:18882 http://127.0.0.1:18881 enabled
    mkproxy test-mt 127.0.0.1:8789 http://127.0.0.1:18880 disabled
    ;;
  *) echo "usage: $0 {stubs|proxies}" >&2; exit 1 ;;
esac

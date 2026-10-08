#!/usr/bin/env bash
# Response-side truncation experiment (repeatable driver).
# One stub + side-proxy pair per batch; per-batch capture dirs / logs / state.
# Ports: stub 18883, proxy 127.0.0.1:8791. Live :8787 untouched.
# Batches: b1 = Content-Length truncation (HTTP/1.0),
#          b2 = TE-chunked on HTTP/1.0 (invalid framing -> 503+retry path),
#          b3 = valid chunked truncation (HTTP/1.1).
set -u
D=/tmp/msg0
BIN=~/.local/bin/headroom-proxy
STUBPID=""
PROXYPID=""

stop_one() { # exact-pattern single pid
  local p
  p=$(pgrep -f "$1" | head -1)
  [ -n "$p" ] && kill "$p" 2>/dev/null
  return 0
}
stop_all() {
  stop_one 'stub_trun[c]\.py'
  stop_one 'headroom-proxy --listen 127\.0\.0\.1:8791'
  sleep 1
  return 0
}

run_batch() { # name stub_mode stub_framing http11 n
  local name=$1 mode=$2 framing=$3 http11=$4 n=$5
  stop_all
  rm -rf "$D/captures-$name" "$D/state-$name"
  mkdir -p "$D/captures-$name" "$D/state-$name"
  : > "$D/stub-$name.log"; : > "$D/test-$name.log"; rm -f "$D/sent-$name.jsonl"
  MSG0_CAPTURE_DIR="$D/captures-$name" MSG0_STUB_PORT=18883 \
    MSG0_TRUNC_MODE="$mode" MSG0_TRUNC_FRAMING="$framing" MSG0_HTTP11="$http11" \
    nohup python3 -I "$D/stub_trunc.py" >>"$D/stub-$name.log" 2>&1 &
  STUBPID=$!
  "$BIN" --listen 127.0.0.1:8791 --upstream http://127.0.0.1:18883 \
    --compression --compression-mode all_messages \
    --auth-mode-policy-enforcement disabled --cache false \
    --replay-store-dir "$D/state-$name/replay-prefixes" \
    --ctx-store-dir "$D/state-$name/ctx" \
    --log-level info >>"$D/test-$name.log" 2>&1 &
  PROXYPID=$!
  for i in $(seq 1 15); do
    if ss -tln | grep -q 18883 && ss -tln | grep -q 8791; then break; fi
    sleep 1
  done
  ss -tln | grep -q 18883 || { echo "stub did not listen for $name"; return 1; }
  ss -tln | grep -q 8791 || { echo "proxy did not listen for $name"; return 1; }
  python3 -I "$D/send_trunc.py" http://127.0.0.1:8791 "$D/sent-$name.jsonl" "$n"
  python3 -I "$D/verify_trunc.py" "$D/sent-$name.jsonl" "$D/captures-$name" \
    "$D/test-$name.log" "$D/report-$name.json"
}

case "${1:-all}" in
  b1) run_batch trunc-b1 alternate alternate 0 60 ;;
  b2) run_batch trunc-b2 alternate chunked 0 12 ;;
  b3) run_batch trunc-b3 alternate chunked 1 12 ;;
  b4) run_batch trunc-b4 all chunked 1 6 ;;
  b5) run_batch trunc-b5 all clen 0 6 ;;
  stop) stop_all ;;
  all)
    run_batch trunc-b1 alternate alternate 0 60
    run_batch trunc-b2 alternate chunked 0 12
    run_batch trunc-b3 alternate chunked 1 12
    stop_all
    ;;
esac

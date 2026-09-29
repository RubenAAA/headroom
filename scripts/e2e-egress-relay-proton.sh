#!/usr/bin/env bash
# End-to-end check of egress-relay's Proton lane against real Proton servers.
#
# Runs a second relay beside the live one: its own state directory and ports,
# Nord switched off (a second set of Nord lanes would eat the account's device
# sessions), and a stub in place of the Headroom proxy so rotation can gate and
# drain. Needs the Proton WireGuard configs and wireproxy that install.sh sets
# up. Writes the report to $OUT (default /tmp/egress-relay-proton-e2e.txt);
# exits nonzero on the first failed check.
#
#   scripts/e2e-egress-relay-proton.sh [path/to/egress-relay]
set -euo pipefail

RELAY="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/release/egress-relay}"
OUT="${OUT:-/tmp/egress-relay-proton-e2e.txt}"
# The free plan allows one connection: a live relay's Proton lane and the
# tunnel this test starts would share it. Refuse to run beside one.
if "$RELAY" status 2>/dev/null | python3 -c 'import json,sys; sys.exit(0 if any(l.get("provider")=="proton" for l in json.load(sys.stdin).get("lanes",[])) else 1)' 2>/dev/null; then
    echo "the running egress relay has a Proton lane; run this once it is stopped" >&2
    exit 1
fi
WORK=$(mktemp -d)
STUB_PORT=18799
export HEADROOM_EGRESS_RELAY_STATE_DIR="$WORK/state"
export HEADROOM_EGRESS_RELAY_BASE_PORT=18700
export HEADROOM_NORD_SOCKS_CREDENTIALS_FILE="$WORK/no-nord.json"
export HEADROOM_PROXY_URL="http://127.0.0.1:$STUB_PORT"

: >"$OUT"
report() { printf '%s\n' "$*" | tee -a "$OUT"; }
fail() { report "FAIL: $*"; exit 1; }

cleanup() {
    "$RELAY" stop >/dev/null 2>&1 || true
    if [ -n "${STUB_PID:-}" ]; then kill "$STUB_PID" 2>/dev/null || true; fi
    rm -rf "$WORK"
}
trap cleanup EXIT

# Answers the two proxy endpoints rotation uses: the per-egress gate and the
# in-flight count, which is always zero here.
python3 - "$STUB_PORT" <<'PY' &
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def reply(self, body):
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        rotating = json.loads(self.rfile.read(length) or b"{}").get("rotating")
        self.reply({"ok": True, "rotating": rotating})
    def do_GET(self):
        self.reply({"in_flight": 0, "zen_held": 0})
    def log_message(self, *args):
        pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
STUB_PID=$!

lane_field() { python3 -c "import json,sys; print(json.load(sys.stdin)['lanes'][0]['$1'])"; }

report "relay: $RELAY"
"$RELAY" env >"$WORK/env" || fail "env did not start the relay"
grep -q "socks5h://127.0.0.1:18700'" "$WORK/env" || fail "pool is not the one Proton lane on 18700"
report "ok: env started the relay with one lane on 18700"

status=$("$RELAY" status)
[ "$(lane_field provider <<<"$status")" = proton ] || fail "lane 0 is not the Proton lane"
first_host=$(lane_field host <<<"$status")
first_exit=$(lane_field exit_fingerprint <<<"$status")
id=$(lane_field egress_id <<<"$status")
report "ok: lane 0 provider=proton host=$first_host exit=$first_exit"

pgrep -f "wireproxy -s -c $HEADROOM_EGRESS_RELAY_STATE_DIR/wireproxy.conf" >/dev/null ||
    fail "no wireproxy child running"
zen=$(curl -s -m 20 --socks5-hostname 127.0.0.1:18700 -o /dev/null -w '%{http_code}' \
    https://opencode.ai/zen/v1/models) || true
[ "$zen" = 200 ] || fail "Zen through the lane answered '$zen'"
report "ok: Zen models endpoint through the lane: HTTP 200"

"$RELAY" test >"$WORK/test" || fail "test failed: $(cat "$WORK/test")"
report "ok: test: $(tail -1 "$WORK/test")"

rotated=$("$RELAY" rotate "$id" manual) || fail "rotation failed: $rotated"
status=$("$RELAY" status)
second_host=$(lane_field host <<<"$status")
second_exit=$(lane_field exit_fingerprint <<<"$status")
[ "$second_host" != "$first_host" ] || fail "rotation kept $first_host"
[ "$second_exit" != "$first_exit" ] || fail "rotation kept the same exit"
[ "$(pgrep -fc "wireproxy -s -c $HEADROOM_EGRESS_RELAY_STATE_DIR/wireproxy.conf")" = 1 ] ||
    fail "more than one wireproxy after rotation: the account allows one connection"
report "ok: rotate moved $first_host -> $second_host, exit $first_exit -> $second_exit, one wireproxy"

zen=$(curl -s -m 20 --socks5-hostname 127.0.0.1:18700 -o /dev/null -w '%{http_code}' \
    https://opencode.ai/zen/v1/models) || true
[ "$zen" = 200 ] || fail "Zen after rotation answered '$zen'"
report "ok: Zen through the rotated lane: HTTP 200"

"$RELAY" stop >/dev/null || fail "stop failed"
for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -f "wireproxy -s -c $HEADROOM_EGRESS_RELAY_STATE_DIR/wireproxy.conf" >/dev/null || break
    sleep 0.5
done
pgrep -f "wireproxy -s -c $HEADROOM_EGRESS_RELAY_STATE_DIR/wireproxy.conf" >/dev/null &&
    fail "wireproxy outlived the relay"
report "ok: stop took wireproxy down with the relay"
report "PASS"

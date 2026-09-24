#!/usr/bin/env bash
# Focused contract test for the watcher's per-egress callback path.
set -euo pipefail

# When this file is invoked as the configured rotator, record its exact args
# without creating another fixture executable.
if [[ -n "${ZEN_ROTATOR_TEST_CAPTURE:-}" ]]; then
  if [[ -v HEADROOM_HTTP_PROXY || -v HEADROOM_ZEN_HTTP_PROXY_POOL ]]; then
    echo "provider proxy URLs leaked into the rotator environment" >&2
    exit 3
  fi
  [[ "${HEADROOM_PROXY_URL:-}" == "http://127.0.0.1:8787" ]] || exit 4
  printf '%s %s\n' "${1:-}" "${2:-}" >>"$ZEN_ROTATOR_TEST_CAPTURE"
  exit 0
fi

REPO_DIR=$(cd "$(dirname "$0")/.." && pwd)
TEST_HOME=$(mktemp -d "${TMPDIR:-/tmp}/headroom-zen-rotate-test.XXXXXX")
trap 'rm -rf "$TEST_HOME"' EXIT
export HOME="$TEST_HOME"

source "$REPO_DIR/contrib/zen-rotate-watch.sh"

WATCHLOG="$TEST_HOME/watch.log"
ZEN_EGRESS_MODE=0
ZEN_EGRESS_ROTATE_COMMAND="$REPO_DIR/scripts/test-zen-rotate-watch.sh"
ZEN_EGRESS_ROTATE_TIMEOUT=2
ZEN_EGRESS_STAMP_DIR="$TEST_HOME/stamps"
ZEN_ROTATOR_TEST_CAPTURE="$TEST_HOME/calls.log"
export ZEN_ROTATOR_TEST_CAPTURE
COOLDOWN_SECS=0
HEADROOM_HTTP_PROXY='http://user:password@proxy.invalid'
HEADROOM_ZEN_HTTP_PROXY_POOL=$'socks5h://user:password@127.0.0.1:18600\nsocks5h://user:password@127.0.0.1:18601'
export HEADROOM_HTTP_PROXY HEADROOM_ZEN_HTTP_PROXY_POOL

# A reactive per-egress event must not move the pool-wide timed rotation.
next_proactive_at=123
schedule_next() { echo 456; }
ZEN_EGRESS_MODE=1
reset_proactive_schedule_after_reactive
[[ "$next_proactive_at" == 123 ]]
ZEN_EGRESS_MODE=0
reset_proactive_schedule_after_reactive
[[ "$next_proactive_at" == 456 ]]
ZEN_EGRESS_MODE=1

zen_egress_ids() {
  printf '%s\n' proxy-aaaaaaaaaaaa proxy-bbbbbbbbbbbb proxy-cccccccccccc
}

# The drain reads only the rotating egress's count; a proxy without
# `egress_in_flight` falls back to the global count less held turns.
inflight_body='{"in_flight":5,"zen_held":1,"egress_in_flight":{"proxy-aaaaaaaaaaaa":0,"proxy-bbbbbbbbbbbb":3}}'
curl() { printf '%s' "$inflight_body"; }
[[ "$(inflight proxy-aaaaaaaaaaaa)" == 0 ]]
[[ "$(inflight proxy-bbbbbbbbbbbb)" == 3 ]]
[[ "$(inflight)" == 4 ]]
inflight_body='{"in_flight":5,"zen_held":1}'
[[ "$(inflight proxy-aaaaaaaaaaaa)" == 4 ]]
inflight_body='not json'
[[ "$(inflight proxy-aaaaaaaaaaaa)" == -1 ]]

# A busy egress defers its own rotation; it never "rotates anyway".
inflight_body='{"in_flight":1,"zen_held":0,"egress_in_flight":{"proxy-aaaaaaaaaaaa":1}}'
DRAIN_SECS=0
if drain proxy-aaaaaaaaaaaa; then
  echo "busy egress reported drained" >&2
  exit 1
fi
grep -qF 'drain: egress=proxy-aaaaaaaaaaaa timed out with in_flight=1' "$WATCHLOG"
if grep -qF 'rotating anyway' "$WATCHLOG"; then
  echo "per-egress drain timeout claimed it would rotate anyway" >&2
  exit 1
fi
unset -f curl

inflight() { echo 0; }
drain() { return 0; }

refresh_zen_egress_mode
[[ "$ZEN_EGRESS_MODE" == 1 ]]

event_id=$(zen_egress_id_from_line \
  '{"event":"zen_egress_rate_limited","egress_id":"proxy-bbbbbbbbbbbb","status":429}')
[[ "$event_id" == proxy-bbbbbbbbbbbb ]]

# A reactive rotation targets exactly the egress identified by the 429.
rotate_zen_egress "$event_id" rate-limit
[[ "$(grep -cF 'proxy-bbbbbbbbbbbb rate-limit' "$ZEN_ROTATOR_TEST_CAPTURE")" -eq 1 ]]
if grep -qF 'proxy-aaaaaaaaaaaa rate-limit' "$ZEN_ROTATOR_TEST_CAPTURE"; then
  echo "unrelated egress was rotated for this 429" >&2
  exit 1
fi
if grep -qF 'proxy-cccccccccccc rate-limit' "$ZEN_ROTATOR_TEST_CAPTURE"; then
  echo "unrelated egress was rotated for this 429" >&2
  exit 1
fi

# A scheduled rotation enumerates every configured egress, once each.
rotate_all_zen_egresses proactive
for id in proxy-aaaaaaaaaaaa proxy-bbbbbbbbbbbb proxy-cccccccccccc; do
  [[ "$(grep -cF "$id proactive" "$ZEN_ROTATOR_TEST_CAPTURE")" -eq 1 ]]
done

# An invalid ID fails closed without invoking the rotator.
if rotate_zen_egress 'not-an-egress' rate-limit; then
  echo "invalid egress ID unexpectedly accepted" >&2
  exit 1
fi
grep -qF 'refusing invalid egress id' "$WATCHLOG"

# Missing control configuration never falls back to a global VPN command.
ZEN_EGRESS_ROTATE_COMMAND=""
if rotate_zen_egress proxy-aaaaaaaaaaaa rate-limit; then
  echo "missing rotator unexpectedly reported success" >&2
  exit 1
fi
grep -qF 'no shared VPN route was changed' "$WATCHLOG"

echo "per-egress watcher contract passed"

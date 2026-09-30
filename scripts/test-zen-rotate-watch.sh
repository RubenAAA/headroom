#!/usr/bin/env bash
# Focused contract test for the watcher's per-egress callback path.
set -euo pipefail

# When this file is invoked as the configured rotator, record its exact args
# without creating another fixture executable.
if [[ -n "${ZEN_ROTATOR_TEST_CAPTURE:-}" ]]; then
  if [[ "${1:-}" == status ]]; then
    echo '{"base_port":18620,"lanes":[{"slot":3,"egress_id":"proxy-bbbbbbbbbbbb"}]}'
    exit 0
  fi
  if [[ -v HEADROOM_HTTP_PROXY || -v HEADROOM_ZEN_HTTP_PROXY_POOL ]]; then
    echo "provider proxy URLs leaked into the rotator environment" >&2
    exit 3
  fi
  [[ "${HEADROOM_PROXY_URL:-}" == "http://127.0.0.1:8787" ]] || exit 4
  printf '%s %s\n' "${1:-}" "${2:-}" >>"$ZEN_ROTATOR_TEST_CAPTURE"
  if [[ -n "${ZEN_ROTATOR_TEST_FAIL:-}" ]]; then
    echo '{"ok":false,"error":"no verified distinct Nord exit available"}'
    exit 1
  fi
  exit 0
fi

REPO_DIR=$(cd "$(dirname "$0")/.." && pwd)
TEST_HOME=$(mktemp -d "${TMPDIR:-/tmp}/headroom-zen-rotate-test.XXXXXX")
trap 'rm -rf "$TEST_HOME"' EXIT
export HOME="$TEST_HOME"

source "$REPO_DIR/contrib/zen-rotate-watch.sh"

# The Zen probe is stubbed (it names no lane); its own test uses the real one.
eval "real_$(declare -f zen_probe_status)"
zen_probe_status() { :; }
forget_rotation_history() {
  mkdir -p "$ZEN_EGRESS_STAMP_DIR"
  rm -f "$ZEN_EGRESS_STAMP_DIR"/*.attempts "$ZEN_EGRESS_STAMP_DIR"/*.backoff
}

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

# A rotator that answers no is logged with what it said, and is not retried
# at once: the whole-cycle retry drained every healthy lane for nothing.
ZEN_ROTATOR_TEST_FAIL=1
export ZEN_ROTATOR_TEST_FAIL
rc=0
rotate_zen_egress proxy-aaaaaaaaaaaa proactive || rc=$?
[[ "$rc" -eq 3 ]]
grep -qF 'rotation FAILED (proactive)' "$WATCHLOG"
grep -qF 'no verified distinct Nord exit available' "$WATCHLOG"
forget_rotation_history
rotate_all_zen_egresses proactive || {
  echo "refused rotations asked for an immediate whole-cycle retry" >&2
  exit 1
}
# A drain that ran out of time still does.
forget_rotation_history
# shellcheck disable=SC2317
drain() { return 1; }
if rotate_all_zen_egresses proactive; then
  echo "deferred rotation did not ask for a retry" >&2
  exit 1
fi
drain() { return 0; }
unset ZEN_ROTATOR_TEST_FAIL

# A failed rotation backs the egress off, so the next one waits; a manual
# rotation does not wait.
forget_rotation_history
ZEN_ROTATOR_TEST_FAIL=1
export ZEN_ROTATOR_TEST_FAIL
rotate_zen_egress proxy-aaaaaaaaaaaa proactive || true
calls=$(wc -l <"$ZEN_ROTATOR_TEST_CAPTURE")
rotate_zen_egress proxy-aaaaaaaaaaaa proactive
[[ "$(wc -l <"$ZEN_ROTATOR_TEST_CAPTURE")" -eq "$calls" ]]
grep -qF 'egress=proxy-aaaaaaaaaaaa backing off' "$WATCHLOG"
rotate_zen_egress proxy-aaaaaaaaaaaa manual || true
[[ "$(wc -l <"$ZEN_ROTATOR_TEST_CAPTURE")" -eq $((calls + 1)) ]]
unset ZEN_ROTATOR_TEST_FAIL

# The wait doubles with each failure in a row, up to the cap.
forget_rotation_history
for expected in 240 480 960 1920 3600 3600; do
  rotation_failed proxy-aaaaaaaaaaaa
  read -r until _ <"$ZEN_EGRESS_STAMP_DIR/proxy-aaaaaaaaaaaa.backoff"
  wait_secs=$((until - $(date +%s)))
  if ! ((wait_secs > expected - 5 && wait_secs <= expected)); then
    echo "backoff after this failure was ${wait_secs}s, wanted ${expected}s" >&2
    exit 1
  fi
done

# No egress rotates more than ROTATE_MAX_PER_HOUR times an hour.
forget_rotation_history
ROTATE_MAX_PER_HOUR=2
: >"$ZEN_ROTATOR_TEST_CAPTURE"
for _ in 1 2 3; do rotate_zen_egress proxy-cccccccccccc proactive; done
[[ "$(wc -l <"$ZEN_ROTATOR_TEST_CAPTURE")" -eq 2 ]]
grep -qF 'rotation budget spent (2 in the last hour)' "$WATCHLOG"
ROTATE_MAX_PER_HOUR=6

# A rotation that leaves the new exit limited by Zen is a failed one.
forget_rotation_history
zen_probe_status() { echo 429; }
rc=0
rotate_zen_egress proxy-aaaaaaaaaaaa proactive || rc=$?
[[ "$rc" -eq 3 ]]
grep -qF 'Zen answers 429 on the new exit; counted as failed' "$WATCHLOG"
[[ -f "$ZEN_EGRESS_STAMP_DIR/proxy-aaaaaaaaaaaa.backoff" ]]
zen_probe_status() { echo 000; }
forget_rotation_history
rc=0
rotate_zen_egress proxy-aaaaaaaaaaaa proactive || rc=$?
[[ "$rc" -eq 3 ]]
# One that Zen lets through clears the backoff.
zen_probe_status() { echo 403; }
rotation_failed proxy-aaaaaaaaaaaa
rotate_zen_egress proxy-aaaaaaaaaaaa manual
[[ ! -e "$ZEN_EGRESS_STAMP_DIR/proxy-aaaaaaaaaaaa.backoff" ]]
zen_probe_status() { :; }

# The probe goes through the lane's own local port, and is skipped for a
# lane the rotator does not list.
curl() { printf '%s' "$*" >"$TEST_HOME/curl.args"; printf 429; }
[[ "$(real_zen_probe_status proxy-bbbbbbbbbbbb)" == 429 ]]
grep -qF 'socks5h://127.0.0.1:18623' "$TEST_HOME/curl.args"
rm -f "$TEST_HOME/curl.args"
[[ -z "$(real_zen_probe_status proxy-aaaaaaaaaaaa)" ]]
[[ ! -e "$TEST_HOME/curl.args" ]]
unset -f curl

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

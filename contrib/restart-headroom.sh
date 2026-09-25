#!/usr/bin/env bash
# Restart the headroom proxy onto a freshly built binary.
#
# Runs fully detached (setsid + nohup + stdio redirected), so it completes even
# though killing the proxy also kills whatever launched this script — including
# a Claude Code session routed through it. That is the whole point: the kill and
# the restart must not share a fate.
#
# Rolls back to the previous binary if the new one fails to come up, so a bad
# build cannot leave every session stranded.
#
# SAFETY — what a restart costs and how this script limits it:
#
# 1. In-flight turns. SIGTERM starts the proxy's graceful drain
#    (--graceful-shutdown-timeout, 30s); turns still generating past that die
#    mid-SSE, truncated, with upstream tokens already paid for. So this script
#    polls /debug/inflight and waits for zero (bounded by --wait-secs, default
#    120s) BEFORE signalling. --force skips the wait.
#
# 2. Connection-refused window. Between the old listener closing and the new
#    one binding, connects get ECONNREFUSED. Sub-second; Claude Code retries.
#    The new binary is staged before SIGTERM so the gap is bind-time only.
#
# 3. Per-process pins re-latch. In-memory holds (working-dir, role sentence,
#    billing pin) do not survive a restart; prefix replay is disk-backed and
#    does. Restarting re-latches pins and can re-cache a prefix — so restart
#    less, and never to fix a pin artifact.
#
# Usage: restart-headroom.sh [--force] [--wait-secs=N]

set -uo pipefail

FORCE=0
WAIT_SECS=120
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    --wait-secs=*) WAIT_SECS="${arg#--wait-secs=}" ;;
    *) echo "restart-headroom: unknown arg '$arg' (usage: restart-headroom.sh [--force] [--wait-secs=N])" >&2; exit 2 ;;
  esac
done

LIVE_BIN="$HOME/.local/bin/headroom-proxy"
BACKUP="$HOME/.local/bin/headroom-proxy.prev"
WORKDIR="$HOME/meta"
LOG="$HOME/headroom-proxy.log"
PORT=8787
FLAGS_FILE="$HOME/.headroom-flags.sh"

# On macOS the GNU tools this script uses — setsid, stat -c, grep -oP — install
# outside the default PATH. install.sh writes this file to put them in front.
# It does not exist on Linux, where they are already the system versions.
# shellcheck source=/dev/null
[ -r "$HOME/.headroom-paths.sh" ] && source "$HOME/.headroom-paths.sh"

# Resolve the candidate only after loading the installed checkout path. The
# installer writes HEADROOM_REPO to this file, and a worktree install must not
# silently restart from the main checkout's target directory.
NEW_BIN="${HEADROOM_REPO:-$HOME/headroom}/target/release/headroom-proxy"

# Abort rather than start a proxy on defaults. A bare proxy serves traffic
# perfectly well and costs more, so the failure would be silent.
if [[ ! -r "$FLAGS_FILE" ]]; then
  echo "restart-headroom: $FLAGS_FILE missing; refusing to start on defaults" >&2
  exit 1
fi
# shellcheck source=$HOME/.headroom-flags.sh
# shellcheck disable=SC1091
source "$FLAGS_FILE"

log() { printf '%s  %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

# The Zen egress pool comes from a private file, not from whatever the calling
# shell exports. On 2026-09-25 the proxy ran pool-less because the shell that
# restarted it never ran `nord-socks-egress env`. Zen then shared the
# device-wide route, each Zen 429 made the watcher rotate the VPN, and every
# rotation reset the Codex and Spark streams in flight. Nothing reported it.
# Write the file with:
#   (umask 077; nord-socks-egress env > ~/.headroom-zen-pool.env)
ZEN_POOL_ENV="${HEADROOM_ZEN_POOL_ENV:-$HOME/.headroom-zen-pool.env}"
if [ -e "$ZEN_POOL_ENV" ]; then
  zen_pool_perm=$(stat -c '%u %a' "$ZEN_POOL_ENV" 2>/dev/null || stat -f '%u %Lp' "$ZEN_POOL_ENV" 2>/dev/null)
  if [[ "$zen_pool_perm" =~ ^$(id -u)\ [0-7]*00$ ]]; then
    unset HEADROOM_ZEN_HTTP_PROXY_POOL HEADROOM_ZEN_EGRESS_ROTATE_COMMAND
    # `set -a`: the proxy only sees the pool if it is exported, and the file
    # may hold bare assignments.
    set -a
    # shellcheck source=/dev/null
    source "$ZEN_POOL_ENV"
    set +a
  else
    echo "restart-headroom: WARNING: ignoring $ZEN_POOL_ENV — it must be yours and mode 0600 (is: ${zen_pool_perm:-unreadable})" >&2
    log "WARNING: ignoring $ZEN_POOL_ENV — it must be yours and mode 0600 (is: ${zen_pool_perm:-unreadable})"
  fi
fi

# Starting without a pool is legitimate on a machine with no relay. With a
# healthy relay up it means Zen goes back on the shared route, so say so on
# the terminal and in the log.
if [ -z "${HEADROOM_ZEN_HTTP_PROXY_POOL:-}" ] && [ -x "$HOME/.local/bin/nord-socks-egress" ]; then
  zen_lanes=$("$HOME/.local/bin/nord-socks-egress" status 2>/dev/null |
    python3 -c 'import json,sys; j=json.load(sys.stdin); print(j.get("lane_count", 0) if j.get("ok") else 0)' 2>/dev/null) || zen_lanes=0
  if [[ "${zen_lanes:-0}" =~ ^[1-9][0-9]*$ ]]; then
    zen_msg="WARNING: starting WITHOUT the Zen egress pool while nord-socks-egress has $zen_lanes lanes up. Zen will share the device-wide route and every Zen 429 will rotate the VPN, resetting Codex and Spark streams. Fix: (umask 077; nord-socks-egress env > $ZEN_POOL_ENV)"
    printf '\n!!! restart-headroom: %s\n\n' "$zen_msg" >&2
    log "$zen_msg"
  fi
fi

# `lsof` first: it reports the owning pid on both Linux and macOS. The macOS
# `ss` comes from iproute2mac, which wraps netstat and emits neither the
# `sport = :N` filter nor the `pid=N` column the Linux one does — installing it
# is not enough to make those work.
listener_pid() {
  local pid=""
  if command -v lsof >/dev/null 2>&1; then
    pid=$(lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -1)
  fi
  if [ -z "$pid" ]; then
    pid=$(ss -ltnp 2>/dev/null | grep -E "[:.]${PORT}[[:space:]]" |
          grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
  fi
  printf '%s' "$pid"
}

listening() { [ -n "$(listener_pid)" ]; }

# The VPN-exit watcher survives proxy restarts on its own — it tails the log
# path with `tail -F` (never an fd that goes stale) and keeps its cooldown
# stamp in a file — so restarts must NOT kill it. But a reboot or a stray
# pkill leaves nothing watching, and on 2026-09-10 six stacked instances were
# found tripping over each other's flock. Hence ensure-one, called wherever
# this script leaves a proxy listening.
ensure_watcher() {
  local watch="$HOME/.local/bin/zen-rotate-watch.sh"
  # Same guard as the launchers, logged since this script runs detached.
  if [ -n "${HEADROOM_ZEN_HTTP_PROXY_POOL:-}" ]; then
    if [ -z "${HEADROOM_ZEN_EGRESS_ROTATE_COMMAND:-}" ] || [ ! -x "$HEADROOM_ZEN_EGRESS_ROTATE_COMMAND" ]; then
      log "Zen egress pool has no executable HEADROOM_ZEN_EGRESS_ROTATE_COMMAND; per-egress 429 and scheduled rotations are disabled (no shared VPN route will be changed)"
      if pgrep -f "[z]en-rotate-watch\.sh" >/dev/null 2>&1; then
        log "an existing watcher may still rotate the device-wide VPN route; stop it before relying on isolated egresses"
      fi
      return 0
    fi
  fi
  [ -x "$watch" ] || return 0
  # Bracket trick: pgrep -f would otherwise match this script's own command
  # line, which quotes the pattern.
  local pids pid stale=0
  pids=$(pgrep -f "[z]en-rotate-watch\.sh" 2>/dev/null)
  if [ -n "${HEADROOM_ZEN_HTTP_PROXY_POOL:-}" ]; then
    # A device-wide watcher left over from before the pool rotates the
    # shared route on every Zen 429, which is the failure the pool exists
    # to end. Swap it for a per-egress one.
    for pid in $pids; do
      # No /proc (macOS): cannot tell the mode, so leave it be.
      [ -r "/proc/$pid/environ" ] || continue
      tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -qx 'HEADROOM_ZEN_EGRESS_MODE=1' || stale=1
    done
    if [ "$stale" = "1" ]; then
      log "replacing the device-wide watcher (pids: ${pids//$'\n'/ }) with a per-egress one"
      pkill -f "[z]en-rotate-watch\.sh" 2>/dev/null
      sleep 1
      pids=""
    fi
  fi
  [ -z "$pids" ] || return 0
  if [ -n "${HEADROOM_ZEN_HTTP_PROXY_POOL:-}" ]; then
    setsid nohup env -u HEADROOM_HTTP_PROXY -u HEADROOM_ZEN_HTTP_PROXY_POOL \
      HEADROOM_PROXY_URL="http://127.0.0.1:$PORT" HEADROOM_PROXY_LOG="$LOG" \
      HEADROOM_ZEN_EGRESS_MODE=1 \
      HEADROOM_ZEN_EGRESS_ROTATE_COMMAND="$HEADROOM_ZEN_EGRESS_ROTATE_COMMAND" \
      "$watch" >>"$HOME/zen-rotate-watch.log" 2>&1 </dev/null &
    disown
    log "per-egress watcher (re)started"
    return 0
  fi
  setsid nohup env -u HEADROOM_HTTP_PROXY -u HEADROOM_ZEN_HTTP_PROXY_POOL \
    "$watch" >>"$HOME/zen-rotate-watch.log" 2>&1 </dev/null &
  disown
  log "watcher (re)started"
}

# Keep this flag set in step with the `cclaude` command line. A proxy already
# listening on 8787 is REUSED, and flags only apply to the process that starts
# one — so whatever is set here is what actually runs, and anything missing is
# silently not in effect.
start_proxy() {
  cd "$WORKDIR" || { log "start_proxy: cd $WORKDIR failed; not starting"; exit 1; }
  # Capture writes whole request bodies to disk — a debugging tool, armed by
  # HEADROOM_CAPTURE_DIR. The proxy inherits that variable from whoever ran this
  # script, so start it with the var set to keep a live capture going, e.g.
  #   env HEADROOM_CAPTURE_DIR=$HOME/headroom-capture-<question> restart-headroom.sh
  #
  # DISARMED 2026-08-11 after two runs that earned their keep: it named the drift
  # source (Claude Code deletes a <system-reminder> block from an old user
  # message, discarding every cached token after it), proved the --prefix-replay
  # fix, and then refuted the offset-matching replay idea. It costs roughly 1 MB
  # per request with no rotation — the msg0 run passed 99 MB.
  #
  # Re-arm for a specific question, into a fresh directory so the diff sees only
  # that run:  env HEADROOM_CAPTURE_DIR=$HOME/headroom-capture-<question>
  #
  # Know its limit before reaching for it: `maybe_capture` runs BEFORE
  # compression, so it records what the CLIENT sent. For what WE forward, use the
  # `early_fingerprints` field on `messages_rewritten` — a hash of each of the
  # first 5 messages exactly as forwarded, diffed across consecutive turns of one
  # conversation_key.
  #
  # The measured settings live in ~/.headroom-flags.sh, which `claude-launcher`
  # sources too. Change them there, not here — a reboot leaves nothing on 8787
  # and the launcher, not this script, is what starts the proxy back up.
  #
  # The control is the run started 12:02:29 (fixed replay, one message marker,
  # system markers forwarded): 51.99 billed-fresh-equivalents per client KB over
  # 178 turns, hit 90.4%, drift 71% of cache creation. Compare with
  # ~/headroom-savings.py, which buckets by proxy start.
  setsid nohup "$LIVE_BIN" \
    --listen "127.0.0.1:$PORT" \
    --upstream https://api.anthropic.com \
    --ctx-capture=true \
    --ctx-offload=true \
    --ctx-inject=true \
    --local-model qwen3.6-uncensored \
    --local-upstream http://localhost:8080 \
    "${HEADROOM_FLAGS[@]}" \
    >>"$LOG" 2>&1 </dev/null &
  disown
}

# Give the caller's own in-flight reply a moment to land before the port
# drops: killing the proxy kills the session that launched this script, and
# that reply has to get out first.
sleep 5

# Turns currently in flight, or -1 when the endpoint is unreachable
# (old proxy binary, proxy down, or transient curl failure).
inflight() {
  local n
  n=$(curl -s --max-time 5 "http://127.0.0.1:$PORT/debug/inflight" 2>/dev/null \
    | python3 -c 'import json,sys; j=json.load(sys.stdin); print(max(0, j.get("in_flight", -1) - j.get("zen_held", 0)) if "in_flight" in j else -1)' 2>/dev/null) \
    || n="-1"
  printf '%s' "$n"
}

# Wait for in-flight turns to finish BEFORE signalling the old proxy, so the
# graceful drain (30s) is a backstop, not the plan. Bounded: after WAIT_SECS
# the restart goes ahead anyway and stragglers die truncated. Returns 0 on a
# clean drain (or drain-blind), 1 when stragglers remain.
wait_for_drain() {
  if [[ "$FORCE" == "1" ]]; then
    log "drain: --force given, skipping inflight wait"
    return 0
  fi
  local deadline=$(( $(date +%s) + WAIT_SECS )) n
  n=$(inflight)
  if [[ "$n" == "-1" ]]; then
    log "drain: no inflight endpoint (old proxy); proceeding without drain"
    return 0
  fi
  while (( $(date +%s) < deadline )); do
    if [[ "$n" == "0" ]]; then
      log "drain: no turns in flight, proceeding"
      return 0
    fi
    sleep 2
    n=$(inflight)
  done
  log "drain: timed out with in_flight=${n:-unknown}; restarting anyway"
  return 1
}

log "=== restart begin ==="

if [[ ! -x "$NEW_BIN" ]]; then
  log "FATAL: $NEW_BIN missing or not executable; leaving the running proxy alone"
  exit 1
fi

OLD_PID=$(listener_pid)
log "current listener pid=${OLD_PID:-none}"

if [[ -n "${OLD_PID:-}" ]]; then
  wait_for_drain || log "restarting with turns still in flight; stragglers will truncate"
fi

# Keep the outgoing binary so a failed start can be undone.
if [[ -f "$LIVE_BIN" ]]; then
  cp -f "$LIVE_BIN" "$BACKUP" && log "backed up previous binary to $BACKUP"
fi

if [[ -n "${OLD_PID:-}" ]]; then
  kill "$OLD_PID" 2>/dev/null
  for _ in $(seq 1 50); do
    listening || break
    sleep 0.2
  done
  if listening; then
    log "port still held after SIGTERM; sending SIGKILL"
    kill -9 "$OLD_PID" 2>/dev/null
    sleep 1
  fi
fi

cp -f "$NEW_BIN" "$LIVE_BIN" || { log "FATAL: copy failed"; exit 1; }
log "installed new binary ($(stat -c %s "$LIVE_BIN") bytes)"

start_proxy

# Health check: the port must come back, or we put the old binary back.
for _ in $(seq 1 60); do
  listening && break
  sleep 0.5
done

if listening; then
  NEW_PID=$(listener_pid)
  log "OK: proxy listening on $PORT (pid=${NEW_PID:-unknown})"
  ensure_watcher
  log "=== restart done ==="
  exit 0
fi

log "NEW BINARY FAILED TO LISTEN — rolling back"
if [[ -f "$BACKUP" ]]; then
  cp -f "$BACKUP" "$LIVE_BIN" && start_proxy
  for _ in $(seq 1 60); do
    listening && break
    sleep 0.5
  done
  if listening; then
    log "rollback OK: previous binary is serving again"
  else
    log "ROLLBACK FAILED: proxy is DOWN, start it by hand"
  fi
  listening && ensure_watcher
else
  log "no backup available; proxy is DOWN"
fi
log "=== restart done (rolled back) ==="
exit 1

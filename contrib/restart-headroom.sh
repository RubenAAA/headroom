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

set -uo pipefail

NEW_BIN="${HEADROOM_REPO:-$HOME/headroom}/target/release/headroom-proxy"
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

# Abort rather than start a proxy on defaults. A bare proxy serves traffic
# perfectly well and costs more, so the failure would be silent.
if [[ ! -r "$FLAGS_FILE" ]]; then
  echo "restart-headroom: $FLAGS_FILE missing; refusing to start on defaults" >&2
  exit 1
fi
# shellcheck source=$HOME/.headroom-flags.sh
source "$FLAGS_FILE"

log() { printf '%s  %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

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
    pid=$(ss -ltnp 2>/dev/null | grep -E "[:.]$PORT[[:space:]]" |
          grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
  fi
  printf '%s' "$pid"
}

listening() { [ -n "$(listener_pid)" ]; }

# Keep this flag set in step with the `cclaude` command line. A proxy already
# listening on 8787 is REUSED, and flags only apply to the process that starts
# one — so whatever is set here is what actually runs, and anything missing is
# silently not in effect.
start_proxy() {
  cd "$WORKDIR" || exit 1
  # Capture writes whole request bodies to disk — a debugging tool, armed by
  # HEADROOM_CAPTURE_DIR. The proxy inherits that variable from whoever ran this
  # script, so removing an assignment here would not turn it off; `env -u` does.
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
  setsid nohup env -u HEADROOM_CAPTURE_DIR "$LIVE_BIN" \
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

# Give the caller's in-flight reply a moment to finish before the port drops.
sleep 5

log "=== restart begin ==="

if [[ ! -x "$NEW_BIN" ]]; then
  log "FATAL: $NEW_BIN missing or not executable; leaving the running proxy alone"
  exit 1
fi

OLD_PID=$(listener_pid)
log "current listener pid=${OLD_PID:-none}"

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
  listening && log "rollback OK: previous binary is serving again" \
            || log "ROLLBACK FAILED: proxy is DOWN, start it by hand"
else
  log "no backup available; proxy is DOWN"
fi
log "=== restart done (rolled back) ==="
exit 1

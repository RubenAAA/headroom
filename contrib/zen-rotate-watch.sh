#!/usr/bin/env bash
# Watch headroom-proxy.log for Zen/Spark rate limits; rotate the NordVPN exit
# and restart the proxy onto the new egress, then keep watching.
#
# Start once from a shell in the `nordvpn` group (it runs forever):
#   setsid nohup "$HOME/.local/bin/zen-rotate-watch.sh" >>"$HOME/zen-rotate-watch.log" 2>&1 &
# Stop:  pkill -f zen-rotate-watch
# Follow: tail -f ~/zen-rotate-watch.log
#
# Trigger: `local_model_upstream_error` with `"status":429` — the exact line
# the proxy emits when Zen refuses a routed turn (12h wait included).
# Rotation cycles the country list until api.ipify.org reports a different
# egress, then restarts via the vendored script so pooled sockets and the
# egress cache pick up the new path.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESTART_SCRIPT="$SCRIPT_DIR/restart-headroom.sh"
LOG="$HOME/headroom-proxy.log"
WATCHLOG="$HOME/zen-rotate-watch.log"
STAMP="$HOME/.zen-rotate.last"
COUNTRIES=(Netherlands Germany France Sweden Switzerland Poland)
COOLDOWN_SECS=120

log() { printf '%s  %s\n' "$(date '+%F %T')" "$*" >>"$WATCHLOG"; }

egress() { curl -s --max-time 10 https://api.ipify.org 2>/dev/null || echo unknown; }

rotate() {
  (
    flock -n 9 || {
      log "rotation already in progress, skipping"
      exit 0
    }
    last=$(cat "$STAMP" 2>/dev/null || echo 0)
    now=$(date +%s)
    if ((now - last < COOLDOWN_SECS)); then
      log "cooldown active, skipping"
      exit 0
    fi
    date +%s >"$STAMP"
    before=$(egress)
    log "rate limit seen (egress=$before); rotating..."
    for c in $(shuf -e "${COUNTRIES[@]}"); do
      nordvpn connect "$c" >>"$WATCHLOG" 2>&1 || continue
      sleep 8
      after=$(egress)
      if [[ -n "$after" && "$after" != unknown && "$after" != "$before" ]]; then
        log "rotated via $c: $before -> $after; restarting proxy"
        bash "$RESTART_SCRIPT" >>"$WATCHLOG" 2>&1
        log "done; retry the failed turn"
        return 0
      fi
      log "egress still $after after $c, trying next country"
    done
    log "rotation FAILED: egress still $before"
  ) 9>"$STAMP.flock"
}

log "watcher started (pid $$)"
tail -n0 -F "$LOG" 2>/dev/null | while IFS= read -r line; do
  if printf '%s' "$line" | grep -q 'local_model_upstream_error'; then
    if printf '%s' "$line" | grep -q '"status":429'; then
      rotate
    fi
  fi
done

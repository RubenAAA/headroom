#!/usr/bin/env bash
# Watch headroom-proxy.log for Zen/Spark rate limits; rotate the NordVPN exit
# and leave a notice for the sleeping sessions, then keep watching.
#
# Start once from a shell in the `nordvpn` group (it runs forever):
#   setsid nohup "$HOME/.local/bin/zen-rotate-watch.sh" >>"$HOME/zen-rotate-watch.log" 2>&1 &
# Stop:  pkill -f zen-rotate-watch
# Follow: tail -f ~/zen-rotate-watch.log
#
# Manual rotation (same drain + notices as the automatic path — never run bare
# `nordvpn connect` while turns are in flight; it RSTs every active stream):
#   zen-rotate-watch.sh --rotate-now [country]
#
# Trigger 1: `local_model_upstream_error` with `"status":429` — the exact line
# the proxy emits when Zen refuses a routed turn (12h wait included).
# Trigger 2: the session-visible form of the same refusal, `temporarily
# limiting requests`, polled in recent spark transcripts. The proxy log only
# shows what arrived while the tail was running; the transcript poll catches
# a refusal the log trigger missed.
# Rotation cycles the country list — reshuffled and recycled until api.ipify.org
# reports a different egress, bounded by ROTATE_DEADLINE_SECS rather than by
# list exhaustion (countries are reusable; giving up after one pass through
# the list would strand us on a congested exit). No proxy restart, ever:
# restarting wipes in-memory session state
# and forces fleet-wide recaches plus a burst of errors — far worse than a
# few corpse-RST turns served from the old pool until it ages out (~25 s,
# `--pool-idle-timeout`).
# Rotations happen two ways. Reactive: a routed 429 (log tail or session
# transcript) rotates at once and leaves a notice for every recently-active
# spark session, since turns already died. Proactive: every PROACTIVE_INTERVAL_SECS
# (± jitter, so no cron-shaped pattern for abuse detection to key on) the watcher
# rotates into a quiet moment — in-flight turns are drained first, and when the box is
# busy the rotation defers rather than killing live turns. A drained rotation
# leaves no notices; only stragglers that died truncated get one.
# A 429 hits every session behind the
# proxy, so there is no victim lookup: any hit rotates, and every
# recently-active spark session gets a notice file that `rotation-notice.sh`
# relays on its next prompt (one-shot). The old billed `claude --resume`
# wake is gone: resuming loads each session's full context as a billed turn,
# six figures of tokens per session, for news the session reads free on its
# next prompt anyway.

set -uo pipefail

LOG="$HOME/headroom-proxy.log"
WATCHLOG="$HOME/zen-rotate-watch.log"
STAMP="$HOME/.zen-rotate.last"
COUNTRIES=(
# Europe (NordVPN names; Turkey not Turkiye).
Albania Andorra Armenia Austria Azerbaijan Belgium Bosnia_And_Herzegovina
Bulgaria Cayman_Islands Cyprus Croatia Czech_Republic Denmark Estonia Finland
France Georgia Germany Greece Hungary Iceland Ireland Isle_Of_Man Italy Jersey
Latvia Liechtenstein Lithuania Luxembourg Malta Moldova Monaco Montenegro
Netherlands North_Macedonia Norway Poland Portugal Romania Serbia Slovakia
Slovenia Spain Sweden Switzerland Ukraine United_Kingdom
# Middle East only.
Bahrain Iraq Israel Jordan Kuwait Lebanon Qatar Turkey United_Arab_Emirates
Yemen
)
COOLDOWN_SECS=120
# Proactive cycling: rotate on a schedule, not just on rate limits, so the
# exit moves before a bucket fills — at a moment we choose, drained, instead
# of mid-turn after a refusal. Interval ± jitter (no exact hourly pattern).
PROACTIVE_INTERVAL_SECS=3600
PROACTIVE_JITTER_SECS=600
# Busy-box deferral: a due proactive rotation with turns in flight waits
# this long and rechecks, rather than burning the drain timeout.
PROACTIVE_DEFER_SECS=300
# Rotation-attempt bound: recycle reshuffled countries until egress moves or
# this elapses (a fully dead VPN path must fail loudly, not spin forever).
ROTATE_DEADLINE_SECS=600
# Recent spark sessions are notice candidates (see below).
WAKE_WINDOW_SECS=900
# Model string as recorded in transcript assistant messages.
SPARK_MODEL="claude-muse-spark"
# Transcript poll: how often to look, and how fresh a transcript must be to
# count. The marker stays in the file forever, so only recently-written files
# can fire — a stale error never retriggers on its own.
POLL_SECS=30
HIT_WINDOW_SECS=180
HIT_MARKER="temporarily limiting requests"
# Where rotation notices land: per-session files next to the review state the
# hooks already use. `rotation-notice.sh` (UserPromptSubmit) relays an
# unreported one on the session's next prompt, then marks it reported.
NOTICE_DIR="$HOME/.local/state/spark-review"

log() { printf '%s  %s\n' "$(date '+%F %T')" "$*" >>"$WATCHLOG"; }

egress() { curl -s --max-time 10 https://api.ipify.org 2>/dev/null || echo unknown; }

# Recently-active spark sessions, by transcript scan. Same selection the old
# billed `claude --resume` wake used — but waking loads each session's FULL
# context as a billed turn (six figures of tokens per session), so notices
# replaced it: a tiny file per session, relayed free on the next prompt.
notice_sessions() {
  python3 - "$WAKE_WINDOW_SECS" "$SPARK_MODEL" <<'PY' 2>/dev/null
import glob, json, os, sys, time
window, model = int(sys.argv[1]), sys.argv[2]
now = time.time()
base = os.path.expanduser('~/.claude/projects')
for path in glob.glob(os.path.join(base, '*', '*.jsonl')):
    try:
        age = now - os.path.getmtime(path)
    except OSError:
        continue
    if age > window:
        continue
    try:
        with open(path, errors='replace') as f:
            tail = f.readlines()[-40:]
    except OSError:
        continue
    if any('"model"' in line and model in line for line in tail):
        print(os.path.basename(path)[:-len('.jsonl')])
PY
}

# Record a rotation where sessions will find it: one small JSON file per
# recently-active spark session. $1 = reason (rate-limit|proactive|manual),
# $2/$3 = egress before/after. Never billed, never loads a context.
write_notices() {
  local reason="$1" before="$2" after="$3" ts sid f
  ts=$(date +%s)
  mkdir -p "$NOTICE_DIR" 2>/dev/null || return 0
  for sid in $(notice_sessions); do
    f="$NOTICE_DIR/$sid.rotation.json"
    # Refresh unconditionally: a newer rotation supersedes whatever the
    # session hasn't seen yet. The hook marks .rotation.reported on relay.
    rm -f "$NOTICE_DIR/$sid.rotation.reported"
    cat >"$f" <<EOF
{"ts": $ts, "reason": "$reason", "egress_before": "$before", "egress_after": "$after",
 "message": "VPN exit rotated ($before -> $after): upstream rate limits clear. Retry your last failed request. If the connection dropped mid-response and a tool call was discarded, re-issue it -- nothing ran."}
EOF
  done
  log "rotation notices written (reason=$reason, egress=$before->$after)"
}

# Next proactive rotation: now + interval ± jitter. Recomputed after every
# rotation (reactive rotations reset the clock — a fresh exit needs no
# proactive cycle on top of it).
schedule_next() {
  echo $(( $(date +%s) + PROACTIVE_INTERVAL_SECS + RANDOM % (2 * PROACTIVE_JITTER_SECS + 1) - PROACTIVE_JITTER_SECS ))
}

# Turns currently in flight, or -1 when the endpoint is unreachable
# (old proxy binary, proxy down, or transient curl/python failure).
inflight() {
  local n
  n=$(curl -s --max-time 5 "http://127.0.0.1:8787/debug/inflight" 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("in_flight", -1))' 2>/dev/null) || n=-1
  printf '%s' "$n"
}

# Wait for in-flight turns to land before killing the tunnel: a rotation
# RSTs every active stream by design. Bounded — after DRAIN_SECS the
# rotation goes ahead anyway and the stragglers die truncated (the proxy
# closes those turns marked, and the resume prompt covers re-issue).
# Returns 0 on a clean drain (or drain-blind), 1 when stragglers remain.
DRAIN_SECS=90
drain() {
  local deadline=$(( $(date +%s) + DRAIN_SECS ))
  # First read decides: no endpoint (old proxy binary, pre-drain support)
  # means drain-blind — rotate at once rather than burning the timeout.
  local n
  n=$(inflight)
  if [[ "$n" == "-1" ]]; then
    log "drain: no inflight endpoint (old proxy); rotating without drain"
    return 0
  fi
  while (( $(date +%s) < deadline )); do
    if [[ "$n" == "0" ]]; then
      log "drain: no turns in flight, rotating"
      return 0
    fi
    sleep 2
    n=$(inflight)
  done
  log "drain: timed out with in_flight=${n:-unknown}; rotating anyway"
  return 1
}

# Cycle reshuffled countries — recycled, not one pass — until the egress
# moves or ROTATE_DEADLINE_SECS elapse. Prints the winning country.
# Returns 1 when nothing moved (dead VPN path): loud failure, not a spin.
rotate_until_moved() {
  local before="$1" deadline=$(( $(date +%s) + ROTATE_DEADLINE_SECS ))
  local c after
  while (( $(date +%s) < deadline )); do
    for c in $(shuf -e "${COUNTRIES[@]}"); do
      (( $(date +%s) < deadline )) || break
      timeout 120 nordvpn connect "$c" >>"$WATCHLOG" 2>&1 || continue
      sleep 8
      after=$(egress)
      if [[ -n "$after" && "$after" != unknown && "$after" != "$before" ]]; then
        log "rotated via $c: $before -> $after"
        printf '%s' "$c"
        return 0
      fi
      log "egress still $after after $c, trying next country"
    done
  done
  log "rotation FAILED: egress still $before after ${ROTATE_DEADLINE_SECS}s of trying"
  return 1
}

# Rotate the exit. Mode selects throttle + notice behavior:
#   auto      — reactive (rate limit seen): turns already died, always notify.
#   scheduled — proactive (hourly tick): notify only stragglers the drain
#               could not save; a clean drain notifies nobody.
#   manual    — explicit operator request: bypasses the cooldown, always
#               notifies. Optional $2 pins the country (falls back to cycling
#               when the pin misses).
rotate() {
  local mode="${1:-auto}" pin="${2:-}"
  (
    flock -n 9 || {
      log "rotation already in progress, skipping ($mode request)"
      exit 0
    }
    if [[ "$mode" != "manual" ]]; then
      last=$(cat "$STAMP" 2>/dev/null || echo 0)
      now=$(date +%s)
      if ((now - last < COOLDOWN_SECS)); then
        log "cooldown active, skipping ($mode request)"
        exit 0
      fi
    fi
    date +%s >"$STAMP"
    before=$(egress)
    log "$mode rotation requested (egress=$before); draining..."
    drained_clean=1
    drain || drained_clean=0
    if [[ -n "$pin" ]]; then
      timeout 120 nordvpn connect "$pin" >>"$WATCHLOG" 2>&1
      sleep 8
      after=$(egress)
      if [[ -n "$after" && "$after" != unknown && "$after" != "$before" ]]; then
        log "rotated via $pin: $before -> $after"
      else
        log "pinned country $pin did not move egress (still $after); cycling..."
        rotate_until_moved "$before" >/dev/null || exit 1
      fi
    else
      rotate_until_moved "$before" >/dev/null || exit 1
    fi
    if [[ "$mode" == "scheduled" && "$drained_clean" == "1" ]]; then
      log "clean proactive rotation; nothing died, no notices written"
    else
      reason="$mode"
      [ "$mode" = "auto" ] && reason="rate-limit"
      write_notices "$reason" "$before" "$(egress)"
    fi
    log "done ($mode rotation)"
  ) 9>"$STAMP.flock"
}

# Transcript files recently appended with the session-visible rate-limit
# message. Prints matching paths; empty when quiet. Scoped to spark sessions
# so an unrelated provider error never rotates the exit.
poll_transcripts() {
  local now cutoff f mtime
  now=$(date +%s)
  cutoff=$((now - HIT_WINDOW_SECS))
  for f in "$HOME"/.claude/projects/*/*.jsonl; do
    [ -f "$f" ] || continue
    mtime=$(stat -c %Y "$f" 2>/dev/null || echo 0)
    [ "$mtime" -gt "$cutoff" ] || continue
    tail -n 15 "$f" 2>/dev/null | grep -q "$HIT_MARKER" || continue
    tail -n 40 "$f" 2>/dev/null | grep -q "$SPARK_MODEL" || continue
    printf '%s\n' "$f"
  done
}

log "watcher started (pid $$)"
if [[ "${1:-}" == "--rotate-now" ]]; then
  # Manual rotation with the same protection as the automatic path: drain
  # in-flight turns first (bounded), then rotate once, then wake sessions.
  # Explicit operator intent bypasses the 120 s auto-throttle but keeps the
  # flock, so a concurrent automatic rotation still serialises. Optional
  # second arg pins the country instead of cycling the list.
  if rotate manual "${2:-}"; then
    exit 0
  else
    exit 1
  fi
fi
next_proactive_at=$(schedule_next)
log "proactive rotation scheduled (hourly ±10min jitter)"
tail -n0 -F "$LOG" 2>/dev/null | while true; do
  if IFS= read -r -t "$POLL_SECS" line; then
    if printf '%s' "$line" | grep -q 'local_model_upstream_error'; then
      if printf '%s' "$line" | grep -q '"status":429'; then
        rotate auto
        next_proactive_at=$(schedule_next)
      fi
    fi
  elif [ -n "$(poll_transcripts)" ]; then
    log "rate-limit message in session transcript; rotating..."
    rotate auto
    next_proactive_at=$(schedule_next)
  fi
  if (( $(date +%s) >= next_proactive_at )); then
    n=$(inflight)
    if [[ "$n" == "0" || "$n" == "-1" ]]; then
      log "proactive rotation due (in_flight=$n); rotating into the quiet moment..."
      rotate scheduled
      next_proactive_at=$(schedule_next)
    else
      log "proactive rotation deferred: $n turns in flight; rechecking in ${PROACTIVE_DEFER_SECS}s"
      next_proactive_at=$(( $(date +%s) + PROACTIVE_DEFER_SECS ))
    fi
  fi
done

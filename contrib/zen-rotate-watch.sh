#!/usr/bin/env bash
# Watch headroom-proxy.log for Zen/Spark rate limits; rotate the VPN exit
# and leave a notice for the sleeping sessions, then keep watching.
#
# Works with whatever VPN the operator already pays for — the provider is
# auto-detected, or pinned explicitly. No supported VPN is also fine: the
# watcher still runs (429 detection + cooldown + notices) and just skips
# the reconnect step.
#
# Configuration (all optional, environment only):
#   VPN_PROVIDER        auto (default) | nordvpn | mullvad | expressvpn |
#                       protonvpn | surfshark | pia | tailscale |
#                       wireguard | openvpn | custom | none
#                       `auto` probes for a known CLI, in that order, and
#                       falls back to `none`. HEADROOM_VPN_PROVIDER is
#                       honoured as a fallback name for the same setting.
#   VPN_LOCATIONS       space-separated location list, overriding the
#                       provider default below. Names are provider-flavoured:
#                       NordVPN Title_Case, Mullvad/Proton 2-letter codes,
#                       PIA region ids, Tailscale exit-node names/IPs,
#                       wg-quick/openvpn config basenames (no suffix).
#   VPN_CONFIG_DIR      *.conf / *.ovpn directory for wireguard/openvpn
#                       (default ~/.config/headroom/vpn).
#   VPN_CONNECT_CMD     template for `custom`: `%s` is replaced with the
#                       location, otherwise the location is appended as an
#                       argument. Required when VPN_PROVIDER=custom.
#   VPN_CONNECT_TIMEOUT connect timeout in seconds (default 120).
#   VPN_SETTLE_SECS     sleep after connect before checking egress (default 8).
#
# Provider notes (log in / set up the VPN app first, outside this script):
#   nordvpn    `nordvpn connect <Country>`; needs the `nordvpn` group +
#              fresh shell, and `nordvpn allowlist add subnet 127.0.0.0/8`
#              + reconnect so tunneled loopback to the proxy keeps working.
#   mullvad    `mullvad relay set location <cc> && mullvad connect`
#              (2-letter codes, e.g. `se`). Daemon via the Mullvad app.
#   expressvpn `expressvpnctl connect ["<Location>"]` (v14+; legacy `expressvpn`
#              binary also tried). `expressvpnctl get regions` lists exact
#              names for VPN_LOCATIONS; default re-connects (`smart`), which
#              usually moves egress on its own.
#   protonvpn  new `protonvpn connect --country <CC>` preferred, legacy
#              `protonvpn-cli connect --cc <CC>` (needs sudo) as fallback.
#   surfshark  legacy `surfshark-vpn attack` quick-connect only — it takes no
#              location argument, so rotation just re-establishes the tunnel.
#              For country control use wireguard below with downloaded .confs.
#   pia        `piactl set region <id> && piactl connect`; needs the PIA
#              daemon running. Regions auto-listed via `piactl get regions`,
#              or pin them with VPN_LOCATIONS.
#   tailscale  `tailscale set --exit-node=<name|ip>`; exit nodes are yours,
#              so set VPN_LOCATIONS to their names/IPs
#              (`tailscale exit-node list` to enumerate).
#   wireguard  cycles `*.conf` in VPN_CONFIG_DIR via `wg-quick down/up`
#              (usually needs sudo — see VPN_CONNECT_CMD/custom if yours
#              differs). VPN_LOCATIONS pins a subset of basenames.
#   openvpn    cycles `*.ovpn` in VPN_CONFIG_DIR (`openvpn --config … --daemon`,
#              old tunnel killed first; auth/certs stay in your configs).
#   custom     runs VPN_CONNECT_CMD per location, e.g.
#              `VPN_CONNECT_CMD='sudo wg-quick up %s'`.
#   none       no VPN: rotations become wait-out-the-cooldown + notices.
#
# Start once (it runs forever):
#   setsid nohup "$HOME/.local/bin/zen-rotate-watch.sh" >>"$HOME/zen-rotate-watch.log" 2>&1 &
# Stop:  pkill -f zen-rotate-watch
# Follow: tail -f ~/zen-rotate-watch.log
#
# Manual rotation (same drain + notices as the automatic path — never
# reconnect bare-handed while turns are in flight; it RSTs every stream):
#   zen-rotate-watch.sh --rotate-now [location]
#
# Introspection:
#   zen-rotate-watch.sh --list-providers   # supported provider names
#   zen-rotate-watch.sh --detect-provider  # what `auto` would pick here
#
# Trigger 1: `local_model_upstream_error` with `"status":429` — the exact line
# the proxy emits when Zen refuses a routed turn (12h wait included) — or
# `zen_hold_waiting`, the line the proxy emits while it holds that refusal
# instead of returning it (the hold is unbounded by default, so this is the
# only 429 signal in the log while the hold is on).
# Trigger 2: the session-visible form of the same refusal, `temporarily
# limiting requests`, polled in recent spark transcripts. The proxy log only
# shows what arrived while the tail was running; the transcript poll catches
# a refusal the log trigger missed.
# Rotation cycles the provider's location list — reshuffled and recycled until
# api.ipify.org reports a different egress, bounded by ROTATE_DEADLINE_SECS
# rather than by list exhaustion (locations are reusable; giving up after one
# pass through the list would strand us on a congested exit). No proxy restart, ever:
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
# Europe (NordVPN names; Turkey not Turkiye). Default location list for the
# nordvpn provider; other providers bring their own defaults below.
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
# Same geography as COUNTRIES, as 2-letter codes for the mullvad and protonvpn
# providers (Mullvad takes lowercase, Proton takes either case).
MULLVAD_COUNTRIES=(al ad am at az be ba bg cy hr cz dk ee 'fi' fr ge de gr hu
is ie it lv li lt lu mt md mc me nl mk no pl pt ro rs sk si es se ch ua gb
bh iq il jo kw lb qa tr ae ye)
# PIA fallback when `piactl get regions` is unavailable (region ids vary by
# account/server list — prefer VPN_LOCATIONS with your own).
PIA_REGIONS=(us-east us-west us-central uk-london nl netherlands de-berlin
france switzerland sweden norway spain italy poland czech austria)
# ── VPN provider layer ─────────────────────────────────────────────────
# VPN_PROVIDER selects the backend (default `auto`). HEADROOM_VPN_PROVIDER is
# honoured as an alias so the setting matches the HEADROOM_* convention.
VPN_PROVIDER="${VPN_PROVIDER:-${HEADROOM_VPN_PROVIDER:-auto}}"
VPN_CONFIG_DIR="${VPN_CONFIG_DIR:-$HOME/.config/headroom/vpn}"
VPN_CONNECT_TIMEOUT="${VPN_CONNECT_TIMEOUT:-120}"
VPN_SETTLE_SECS="${VPN_SETTLE_SECS:-8}"
VPN_IFACE_STATE="$HOME/.zen-rotate.iface"
VPN_OVPN_PID="$HOME/.zen-rotate.ovpn.pid"

SUPPORTED_PROVIDERS=(nordvpn mullvad expressvpn protonvpn surfshark pia tailscale wireguard openvpn custom none)

# What `auto` would pick on this machine: first known CLI on PATH, else none.
detect_vpn_provider() {
  if command -v nordvpn >/dev/null 2>&1; then echo nordvpn
  elif command -v mullvad >/dev/null 2>&1; then echo mullvad
  elif command -v expressvpnctl >/dev/null 2>&1 || command -v expressvpn >/dev/null 2>&1; then echo expressvpn
  elif command -v protonvpn >/dev/null 2>&1 || command -v protonvpn-cli >/dev/null 2>&1; then echo protonvpn
  elif command -v surfshark-vpn >/dev/null 2>&1 || command -v surfshark >/dev/null 2>&1; then echo surfshark
  elif command -v piactl >/dev/null 2>&1; then echo pia
  elif command -v tailscale >/dev/null 2>&1; then echo tailscale
  elif command -v wg-quick >/dev/null 2>&1; then echo wireguard
  elif command -v openvpn >/dev/null 2>&1; then echo openvpn
  else echo none
  fi
}

# Effective provider: explicit VPN_PROVIDER wins, otherwise whatever `auto`
# detects on this machine (`command -v` probes — cheap enough to run per call).
vpn_provider() {
  if [[ "$VPN_PROVIDER" == "auto" ]]; then
    detect_vpn_provider
  else
    printf '%s' "$VPN_PROVIDER"
  fi
}

# Connect the VPN to $1 (a location in the provider's flavour). Returns 0 on
# apparent success — egress movement is verified separately by the caller.
vpn_connect() {
  local loc="$1" provider
  provider=$(vpn_provider)
  case "$provider" in
    nordvpn)
      timeout "$VPN_CONNECT_TIMEOUT" nordvpn connect "$loc" >>"$WATCHLOG" 2>&1 ;;
    mullvad)
      timeout "$VPN_CONNECT_TIMEOUT" mullvad relay set location "$loc" >>"$WATCHLOG" 2>&1 || return 1
      timeout "$VPN_CONNECT_TIMEOUT" mullvad connect >>"$WATCHLOG" 2>&1 ;;
    expressvpn)
      if command -v expressvpnctl >/dev/null 2>&1; then
        if [[ -z "$loc" || "$loc" == "smart" ]]; then
          timeout "$VPN_CONNECT_TIMEOUT" expressvpnctl connect >>"$WATCHLOG" 2>&1
        else
          timeout "$VPN_CONNECT_TIMEOUT" expressvpnctl connect "$loc" >>"$WATCHLOG" 2>&1
        fi
      else
        if [[ -z "$loc" || "$loc" == "smart" ]]; then
          timeout "$VPN_CONNECT_TIMEOUT" expressvpn connect >>"$WATCHLOG" 2>&1
        else
          timeout "$VPN_CONNECT_TIMEOUT" expressvpn connect "$loc" >>"$WATCHLOG" 2>&1
        fi
      fi ;;
    protonvpn)
      if command -v protonvpn >/dev/null 2>&1; then
        timeout "$VPN_CONNECT_TIMEOUT" protonvpn connect --country "$loc" >>"$WATCHLOG" 2>&1
      else
        timeout "$VPN_CONNECT_TIMEOUT" sudo protonvpn-cli connect --cc "$loc" >>"$WATCHLOG" 2>&1
      fi ;;
    surfshark)
      # Legacy CLI takes no location argument: cycle the tunnel and hope the
      # exit moves. Country control needs the wireguard provider instead.
      if command -v surfshark-vpn >/dev/null 2>&1; then
        timeout "$VPN_CONNECT_TIMEOUT" sudo surfshark-vpn down >>"$WATCHLOG" 2>&1 || true
        timeout "$VPN_CONNECT_TIMEOUT" sudo surfshark-vpn attack >>"$WATCHLOG" 2>&1
      else
        timeout "$VPN_CONNECT_TIMEOUT" sudo surfshark attack >>"$WATCHLOG" 2>&1
      fi ;;
    pia)
      timeout "$VPN_CONNECT_TIMEOUT" piactl set region "$loc" >>"$WATCHLOG" 2>&1 || return 1
      timeout "$VPN_CONNECT_TIMEOUT" piactl connect >>"$WATCHLOG" 2>&1 ;;
    tailscale)
      timeout "$VPN_CONNECT_TIMEOUT" sudo tailscale set --exit-node="$loc" >>"$WATCHLOG" 2>&1 ;;
    wireguard)
      local cur=""
      [[ -f "$VPN_IFACE_STATE" ]] && cur=$(cat "$VPN_IFACE_STATE" 2>/dev/null || true)
      if [[ -n "$cur" && "$cur" != "$loc" ]]; then
        timeout "$VPN_CONNECT_TIMEOUT" sudo wg-quick down "$cur" >>"$WATCHLOG" 2>&1 || true
      fi
      timeout "$VPN_CONNECT_TIMEOUT" sudo wg-quick up "$loc" >>"$WATCHLOG" 2>&1 || return 1
      printf '%s' "$loc" >"$VPN_IFACE_STATE" ;;
    openvpn)
      if [[ -f "$VPN_OVPN_PID" ]]; then
        kill "$(cat "$VPN_OVPN_PID" 2>/dev/null || echo none)" 2>/dev/null || true
        rm -f "$VPN_OVPN_PID"
        sleep 2
      fi
      pkill -f "openvpn.*--config $VPN_CONFIG_DIR" 2>/dev/null || true
      timeout "$VPN_CONNECT_TIMEOUT" sudo openvpn --config "$VPN_CONFIG_DIR/$loc.ovpn" --daemon --writepid "$VPN_OVPN_PID" >>"$WATCHLOG" 2>&1 ;;
    custom)
      if [[ -z "${VPN_CONNECT_CMD:-}" ]]; then
        log "custom provider needs VPN_CONNECT_CMD (e.g. VPN_CONNECT_CMD='sudo wg-quick up %s')"
        return 1
      fi
      if [[ "$VPN_CONNECT_CMD" == *"%s"* ]]; then
        timeout "$VPN_CONNECT_TIMEOUT" bash -c "${VPN_CONNECT_CMD//\%s/$loc}" >>"$WATCHLOG" 2>&1
      else
        timeout "$VPN_CONNECT_TIMEOUT" bash -c "$VPN_CONNECT_CMD \"\$0\"" "$loc" >>"$WATCHLOG" 2>&1
      fi ;;
    none)
      log "no VPN provider configured; skipping reconnect (waiting out the cooldown)"
      return 1 ;;
    *)
      log "unknown VPN_PROVIDER='$provider' (see --list-providers); skipping reconnect"
      return 1 ;;
  esac
}

# Location candidates for the active provider: explicit VPN_LOCATIONS wins,
# then provider defaults / local discovery. Prints one location per line.
vpn_locations() {
  local provider
  provider=$(vpn_provider)
  if [[ -n "${VPN_LOCATIONS:-}" ]]; then
    # VPN_LOCATIONS is intentionally space-split.
    # shellcheck disable=SC2086
    printf '%s\n' $VPN_LOCATIONS
    return 0
  fi
  case "$provider" in
    nordvpn|custom) printf '%s\n' "${COUNTRIES[@]}" ;;
    mullvad) printf '%s\n' "${MULLVAD_COUNTRIES[@]}" ;;
    expressvpn) printf 'smart\n' ;;
    protonvpn) printf '%s\n' "${MULLVAD_COUNTRIES[@]}" | tr '[:lower:]' '[:upper:]' ;;
    surfshark) printf 'quick\n' ;;
    pia)
      if command -v piactl >/dev/null 2>&1; then
        piactl get regions 2>/dev/null | tr ', ' '\n' | grep -v '^$' || printf '%s\n' "${PIA_REGIONS[@]}"
      else
        printf '%s\n' "${PIA_REGIONS[@]}"
      fi ;;
    tailscale)
      if command -v tailscale >/dev/null 2>&1; then
        tailscale exit-node list 2>/dev/null | awk 'NR>1 {print $2}' | grep -v '^$' || true
      fi ;;
    wireguard)
      shopt -s nullglob
      local f
      for f in "$VPN_CONFIG_DIR"/*.conf; do basename "$f" .conf; done
      shopt -u nullglob ;;
    openvpn)
      shopt -s nullglob
      local f
      for f in "$VPN_CONFIG_DIR"/*.ovpn; do basename "$f" .ovpn; done
      shopt -u nullglob ;;
    none) return 1 ;;
    *) return 1 ;;
  esac
}
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
# Tunnel-sickness trigger: bursts of fresh-connect/send failures in the proxy
# log mean the egress path itself is degrading — 2026-09-14 13:01 showed
# send failures plus mid-stream RSTs minutes before the rate-limit rotation
# fired. Distinctive lines only: decode-error retries happen on ordinary
# provider blips and must not rotate the exit. Fires at threshold, then
# suppresses until the window slides past so one burst rotates once.
TRANSPORT_BURST_THRESHOLD=3
TRANSPORT_STAMP="$HOME/.zen-rotate.transport-last"
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
# recently-active spark session. $1 = reason
# (rate-limit|egress-degraded|proactive|manual), $2/$3 = egress
# before/after, $4 = optional message override (default assumes the exit
# actually moved). Never billed, never loads a context.
write_notices() {
  local reason="$1" before="$2" after="$3" ts sid f
  local message
  if [[ -n "${4:-}" ]]; then
    message="$4"
  elif [[ "$reason" == "egress-degraded" ]]; then
    message="VPN exit rotated ($before -> $after): fresh connections were failing on the old exit. Retry your last failed request. If the connection dropped mid-response and a tool call was discarded, re-issue it -- nothing ran."
  else
    message="VPN exit rotated ($before -> $after): upstream rate limits clear. Retry your last failed request. If the connection dropped mid-response and a tool call was discarded, re-issue it -- nothing ran."
  fi
  ts=$(date +%s)
  mkdir -p "$NOTICE_DIR" 2>/dev/null || return 0
  for sid in $(notice_sessions); do
    f="$NOTICE_DIR/$sid.rotation.json"
    # Refresh unconditionally: a newer rotation supersedes whatever the
    # session hasn't seen yet. The hook marks .rotation.reported on relay.
    rm -f "$NOTICE_DIR/$sid.rotation.reported"
    cat >"$f" <<EOF
{"ts": $ts, "reason": "$reason", "egress_before": "$before", "egress_after": "$after",
 "message": "$message"}
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
# Turns parked in the proxy's Zen 429 hold (`zen_held`) are waiting for this
# rotation, not generating, so they are subtracted: draining on them stalled
# the 2026-09-14 rotation for 4m40s while the holds ran out and returned 429s.
inflight() {
  local n
  n=$(curl -s --max-time 5 "http://127.0.0.1:8787/debug/inflight" 2>/dev/null \
    | python3 -c 'import json,sys; j=json.load(sys.stdin); print(max(0, j.get("in_flight", -1) - j.get("zen_held", 0)) if "in_flight" in j else -1)' 2>/dev/null) || n=-1
  printf '%s' "$n"
}

# Wait for in-flight turns to land before killing the tunnel: a rotation
# RSTs every active stream by design. Progress-aware: while the count keeps
# falling, turns are landing and the deadline extends (bounded) — only a
# stalled count dies on schedule. 2026-09-14 13:08 killed a live turn after
# a flat 90s; the proxy closes those turns marked, but not killing them is
# strictly better. Returns 0 on a clean drain (or drain-blind), 1 when
# stragglers remain.
DRAIN_SECS=90
# Extra drain granted each time the count falls, and the hard ceiling on all
# extensions: a landing trickle gets room, a stuck turn still dies on time.
DRAIN_PROGRESS_EXTEND_SECS=30
DRAIN_MAX_EXTEND_SECS=180
drain() {
  local deadline=$(( $(date +%s) + DRAIN_SECS ))
  local max_deadline=$(( $(date +%s) + DRAIN_SECS + DRAIN_MAX_EXTEND_SECS ))
  # First read decides: no endpoint (old proxy binary, pre-drain support)
  # means drain-blind — rotate at once rather than burning the timeout.
  local n last_n
  n=$(inflight)
  if [[ "$n" == "-1" ]]; then
    log "drain: no inflight endpoint (old proxy); rotating without drain"
    return 0
  fi
  last_n="$n"
  while (( $(date +%s) < deadline )); do
    if [[ "$n" == "0" ]]; then
      log "drain: no turns in flight, rotating"
      return 0
    fi
    sleep 2
    n=$(inflight)
    # Progress short of completion: turns are landing, so give the rest more
    # room — bounded by max_deadline so a trickle never holds the rotation
    # hostage. A non-numeric read (endpoint hiccup) neither extends nor kills.
    if [[ "$n" =~ ^[0-9]+$ && "$last_n" =~ ^[0-9]+$ ]] && (( n < last_n )); then
      if (( $(date +%s) + DRAIN_PROGRESS_EXTEND_SECS < max_deadline )); then
        deadline=$(( $(date +%s) + DRAIN_PROGRESS_EXTEND_SECS ))
      else
        deadline=$max_deadline
      fi
      log "drain: $last_n -> $n in flight, turns landing; extended deadline"
    fi
    last_n="$n"
  done
  log "drain: timed out with in_flight=${n:-unknown}; rotating anyway"
  return 1
}

# Cycle reshuffled locations — recycled, not one pass — until the egress
# moves or ROTATE_DEADLINE_SECS elapse. Prints the winning location.
# Returns 1 when nothing moved (dead VPN path): loud failure, not a spin.
rotate_until_moved() {
  local before="$1" deadline=$(( $(date +%s) + ROTATE_DEADLINE_SECS ))
  local c after locs
  locs=$(vpn_locations) || {
    log "no locations for provider '$(vpn_provider)' — set VPN_LOCATIONS (see header)"
    return 1
  }
  [[ -n "$locs" ]] || {
    log "empty location list for provider '$(vpn_provider)' — set VPN_LOCATIONS (see header)"
    return 1
  }
  while (( $(date +%s) < deadline )); do
    for c in $(printf '%s\n' "$locs" | shuf); do
      (( $(date +%s) < deadline )) || break
      vpn_connect "$c" || continue
      sleep "$VPN_SETTLE_SECS"
      after=$(egress)
      if [[ -n "$after" && "$after" != unknown && "$after" != "$before" ]]; then
        log "rotated via $c: $before -> $after"
        printf '%s' "$c"
        return 0
      fi
      log "egress still $after after $c, trying next location"
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
#               notifies. Optional $2 pins the location (falls back to cycling
#               when the pin misses). Optional $3 overrides the notice reason
#               (default: the mode, with auto mapped to rate-limit).
rotate() {
  local mode="${1:-auto}" pin="${2:-}" reason_override="${3:-}"
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
    # STAMP is written only after a reconnect completes below, so the
    # cooldown protects the new exit rather than the drain window.
    # No VPN backend: nothing to reconnect, but the 429 still happened —
    # record the event, tell sessions to wait out the upstream window, move on.
    if [[ "$(vpn_provider)" == "none" ]]; then
      before=$(egress)
      log "$mode rotation requested (egress=$before) but VPN_PROVIDER=none; no reconnect to attempt"
      if [[ "$mode" != "scheduled" ]]; then
        reason="$mode"
        [ "$mode" = "auto" ] && reason="rate-limit"
        write_notices "$reason" "$before" "$before" \
          "Upstream rate limit hit and no VPN is configured (egress still $before): nothing to rotate. Wait out the upstream retry window, then retry your last failed request. Set VPN_PROVIDER (zen-rotate-watch.sh --list-providers) for automatic exit rotation."
      else
        log "clean proactive tick with no VPN; nothing died, no notices written"
      fi
      date +%s >"$STAMP"
      log "done ($mode rotation, no VPN)"
      exit 0
    fi
    before=$(egress)
    log "$mode rotation requested (provider=$(vpn_provider), egress=$before); draining..."
    drained_clean=1
    drain || drained_clean=0
    if [[ -n "$pin" ]]; then
      vpn_connect "$pin"
      sleep "$VPN_SETTLE_SECS"
      after=$(egress)
      if [[ -n "$after" && "$after" != unknown && "$after" != "$before" ]]; then
        log "rotated via $pin: $before -> $after"
      else
        log "pinned location $pin did not move egress (still $after); cycling..."
        rotate_until_moved "$before" >/dev/null || exit 1
      fi
    else
      rotate_until_moved "$before" >/dev/null || exit 1
    fi
    date +%s >"$STAMP"
    if [[ "$mode" == "scheduled" && "$drained_clean" == "1" ]]; then
      log "clean proactive rotation; nothing died, no notices written"
    else
      reason="${reason_override:-$mode}"
      if [[ -z "$reason_override" && "$mode" == "auto" ]]; then
        reason="rate-limit"
      fi
      write_notices "$reason" "$before" "$(egress)"
    fi
    log "done ($mode rotation)"
  ) 9>"$STAMP.flock"
}

# Fresh-connect/send failures in the recent proxy-log tail: the tunnel-side
# counterpart to Trigger1's 429 watch. Prints the in-window count (0 when
# quiet or the log is missing). Scans only the tail — the log is tens of MB
# and this runs every POLL_SECS.
poll_transport() {
  [ -f "$LOG" ] || { echo 0; return 0; }
  tail -n 2000 "$LOG" 2>/dev/null | python3 -c '
import json, sys, time
cutoff = time.time() - int(sys.argv[1])
n = 0
for line in sys.stdin:
    if "retry of a dropped stream failed to send" not in line \
            and "failed to connect to local model upstream" not in line:
        continue
    try:
        ts = json.loads(line).get("timestamp", "")
        import datetime
        t = datetime.datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
    except Exception:
        continue
    if t >= cutoff:
        n += 1
print(n)' "$HIT_WINDOW_SECS" 2>/dev/null || echo 0
}

# True (once) when a transport burst may fire: suppresses refires until the
# window slides past the last firing, so one burst rotates once. The rotation
# cooldown still guards the actual rotate call.
transport_fire_due() {
  local last now
  now=$(date +%s)
  last=$(cat "$TRANSPORT_STAMP" 2>/dev/null || echo 0)
  if (( now - last > HIT_WINDOW_SECS )); then
    date +%s >"$TRANSPORT_STAMP" 2>/dev/null
    return 0
  fi
  return 1
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

main() {
log "watcher started (pid $$, provider=$(vpn_provider))"
if [[ "${1:-}" == "--rotate-now" ]]; then
  # Manual rotation with the same protection as the automatic path: drain
  # in-flight turns first (bounded), then rotate once, then wake sessions.
  # Explicit operator intent bypasses the 120 s auto-throttle but keeps the
  # flock, so a concurrent automatic rotation still serialises. Optional
  # second arg pins the location instead of cycling the list.
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
    elif printf '%s' "$line" | grep -q '"event":"zen_hold_waiting"'; then
      # The proxy holds a Zen 429 instead of returning it (unbounded by
      # default), so the `local_model_upstream_error` line above no longer
      # appears while a hold is on. Every hold probe that still sees 429
      # asks for a rotation; the cooldown collapses the burst into one.
      rotate auto
      next_proactive_at=$(schedule_next)
    fi
  elif [ -n "$(poll_transcripts)" ]; then
    log "rate-limit message in session transcript; rotating..."
    rotate auto
    next_proactive_at=$(schedule_next)
  elif [ "$(poll_transport)" -ge "$TRANSPORT_BURST_THRESHOLD" ] && transport_fire_due; then
    log "egress send-failure burst in proxy log; rotating..."
    rotate auto "" "egress-degraded"
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
}

# Introspection flags run without starting the watcher. Everything else is
# wrapped in main() so the file can also be sourced (e.g. in tests) to
# exercise detect_vpn_provider / vpn_connect / vpn_locations in isolation.
if [[ "${BASH_SOURCE[0]:-$0}" == "$0" ]]; then
  case "${1:-}" in
    --list-providers) printf '%s\n' "${SUPPORTED_PROVIDERS[@]}"; exit 0 ;;
    --detect-provider) detect_vpn_provider; exit 0 ;;
    *) main "$@" ;;
  esac
fi

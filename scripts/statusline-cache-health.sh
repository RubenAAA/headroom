#!/usr/bin/env bash
# CTX-7: Claude Code statusline with headroom re-cache watchdog.
#
# Claude Code invokes the statusline command with a JSON payload on
# stdin and renders the first stdout line. Wire it up in
# ~/.claude/settings.json:
#
#   "statusLine": {
#     "type": "command",
#     "command": "/home/user/headroom/scripts/statusline-cache-health.sh"
#   }
#
# Shows: <model> | <cwd basename> | cache ✓ NN%   (healthy)
#    or: <model> | <cwd basename> | ⚠ recache Xs ago: <reason>, ~NK tok wasted
#
# With --segment, prints ONLY the cache part (for appending to an
# existing statusline) and prints NOTHING when the proxy is
# unreachable or has no samples yet — zero statusline space unless
# headroom is actually running.
#
# The warning stays visible for RECACHE_WARN_WINDOW seconds after the
# most recent re-cache event, then falls back to the ambient ratio.
set -u

HEALTH_URL="${HEADROOM_CACHE_HEALTH_URL:-http://127.0.0.1:8787/cache-health}"
RECACHE_WARN_WINDOW="${HEADROOM_RECACHE_WARN_WINDOW:-600}"

segment_only=0
if [ "${1:-}" = "--segment" ]; then
    segment_only=1
fi

prefix=""
if [ "$segment_only" -eq 0 ]; then
    input=$(cat 2>/dev/null || true)
    model=$(printf '%s' "$input" | jq -r '.model.display_name // empty' 2>/dev/null)
    cwd=$(printf '%s' "$input" | jq -r '.workspace.current_dir // empty' 2>/dev/null)
    [ -n "$model" ] && prefix="$model"
    [ -n "$cwd" ] && prefix="${prefix:+$prefix | }$(basename "$cwd")"
fi

health=$(curl -fsS --max-time 1 "$HEALTH_URL" 2>/dev/null)
if [ -z "$health" ]; then
    # Segment mode: headroom not running → occupy no space at all.
    [ "$segment_only" -eq 1 ] && exit 0
    printf '%s\n' "${prefix:+$prefix | }cache: proxy unreachable"
    exit 0
fi

age=$(printf '%s' "$health" | jq -r '.last_event_age_seconds // empty')
if [ -n "$age" ] && [ "$age" -lt "$RECACHE_WARN_WINDOW" ]; then
    reason=$(printf '%s' "$health" | jq -r '.last_event.drift_dims // "unknown cause"')
    wasted=$(printf '%s' "$health" | jq -r '.last_event.wasted_tokens // 0')
    if [ "$wasted" -ge 1000 ]; then
        wasted="$((wasted / 1000))K"
    fi
    printf '%s\n' "${prefix:+$prefix | }⚠ recache ${age}s ago: ${reason}, ~${wasted} tok wasted"
    exit 0
fi

rate=$(printf '%s' "$health" | jq -r 'if .recent_hit_rate == null then empty else (.recent_hit_rate * 100 | floor) end')
if [ -n "$rate" ]; then
    printf '%s\n' "${prefix:+$prefix | }cache ✓ ${rate}%"
elif [ "$segment_only" -eq 0 ]; then
    printf '%s\n' "${prefix:+$prefix | }cache: no samples yet"
fi

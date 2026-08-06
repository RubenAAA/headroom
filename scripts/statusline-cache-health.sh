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
# Events are classified by the proxy (last_event.event_kind):
#   drift    → ⚠ genuine structural cache bust, shown RECACHE_DRIFT_WINDOW s
#   expected → ℹ session reset (subagent close, /clear) — tokens not
#              actually wasted, shown RECACHE_EXPECTED_WINDOW s
# then falls back to the ambient ratio.
set -u

HEALTH_URL="${HEADROOM_CACHE_HEALTH_URL:-http://127.0.0.1:8787/cache-health}"
RECACHE_DRIFT_WINDOW="${HEADROOM_RECACHE_DRIFT_WINDOW:-180}"
RECACHE_EXPECTED_WINDOW="${HEADROOM_RECACHE_EXPECTED_WINDOW:-60}"

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
if [ -n "$age" ]; then
    kind=$(printf '%s' "$health" | jq -r '.last_event.event_kind // "drift"')
    if [ "$kind" = "expected" ]; then
        window="$RECACHE_EXPECTED_WINDOW"
    else
        window="$RECACHE_DRIFT_WINDOW"
    fi
    if [ "$age" -lt "$window" ]; then
        wasted=$(printf '%s' "$health" | jq -r '.last_event.wasted_tokens // 0')
        if [ "$wasted" -ge 1000 ]; then
            wasted="$((wasted / 1000))K"
        fi
        if [ "$kind" = "expected" ]; then
            printf '%s\n' "${prefix:+$prefix | }ℹ cache drop ${age}s ago: session reset (subagent/clear), ~${wasted} tok re-cached"
        else
            reason=$(printf '%s' "$health" | jq -r '.last_event.drift_dims // "unknown cause"')
            printf '%s\n' "${prefix:+$prefix | }⚠ recache ${age}s ago: ${reason}, ~${wasted} tok wasted"
        fi
        exit 0
    fi
fi

rate=$(printf '%s' "$health" | jq -r 'if .recent_hit_rate == null then empty else (.recent_hit_rate * 100 | floor) end')
if [ -n "$rate" ]; then
    printf '%s\n' "${prefix:+$prefix | }cache ✓ ${rate}%"
elif [ "$segment_only" -eq 0 ]; then
    printf '%s\n' "${prefix:+$prefix | }cache: no samples yet"
fi

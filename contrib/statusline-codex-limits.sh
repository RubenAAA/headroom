#!/usr/bin/env bash
# Codex quota segment for the Claude Code statusline.
#
# Claude Code renders its own `rate_limits` for Anthropic subscription auth,
# but there is no hook to fill that field for a model routed elsewhere. So the
# proxy records the quota it sees on codex turns and serves it at
# /codex-limits, and this script renders it.
#
# With --segment, prints ONLY the codex part (for appending to an existing
# statusline) and prints NOTHING when the proxy is unreachable, has never seen
# a codex turn, or the snapshot has gone stale — zero statusline space unless
# you are actually using a codex model.
#
# Shows: codex 7d:3%(5d02h)                  one window
#    or: codex 5h:12%(1h20m) 7d:3%(4d23h)    both, shortest window first
#    or: codex 7d:3% ⚠ limited               when the backend reports a limit hit
#
# Optional first argument after --segment is the active model name; when given,
# nothing prints unless it looks like a codex model. Without it the segment
# falls back to snapshot freshness, which is a good enough proxy for "the last
# thing you sent went to codex".
set -u

LIMITS_URL="${HEADROOM_CODEX_LIMITS_URL:-http://127.0.0.1:8787/codex-limits}"
# Beyond this the snapshot describes a session you have probably moved on from.
STALE_AFTER="${HEADROOM_CODEX_LIMITS_STALE_AFTER:-900}"

segment_only=0
if [ "${1:-}" = "--segment" ]; then
    segment_only=1
    shift
fi
active_model="${1:-}"

# An explicit non-codex model means this segment has nothing to say.
if [ -n "$active_model" ] && ! printf '%s' "$active_model" | grep -qi codex; then
    exit 0
fi

command -v jq >/dev/null 2>&1 || exit 0
snapshot=$(curl -s --max-time 1 "$LIMITS_URL" 2>/dev/null) || exit 0
[ -n "$snapshot" ] || exit 0

observed=$(printf '%s' "$snapshot" | jq -r '.observed_at // empty' 2>/dev/null)
[ -n "$observed" ] && [ "$observed" != "null" ] || exit 0

age=$(printf '%s' "$snapshot" | jq -r '.age_seconds // 0' 2>/dev/null)
[ "$age" -le "$STALE_AFTER" ] 2>/dev/null || exit 0

# Window length decides the label: 10080 minutes is the weekly bucket, 300 the
# five-hour one. Anything else is reported in whatever unit divides cleanly.
window_label() {
    local minutes=$1
    [ -n "$minutes" ] && [ "$minutes" != "null" ] || { printf 'quota'; return; }
    if [ "$minutes" -ge 1440 ] && [ $((minutes % 1440)) -eq 0 ]; then
        printf '%dd' $((minutes / 1440))
    elif [ "$minutes" -ge 60 ] && [ $((minutes % 60)) -eq 0 ]; then
        printf '%dh' $((minutes / 60))
    else
        printf '%dm' "$minutes"
    fi
}

fmt_reset() {
    local ts=$1 now diff d h m
    [ -n "$ts" ] && [ "$ts" != "null" ] || return
    now=$(date +%s)
    diff=$((ts - now))
    [ "$diff" -le 0 ] && { printf '0m'; return; }
    d=$((diff / 86400))
    h=$(((diff % 86400) / 3600))
    m=$(((diff % 3600) / 60))
    if [ "$d" -gt 0 ]; then
        printf '%dd%02dh' "$d" "$h"
    elif [ "$h" -gt 0 ]; then
        printf '%dh%02dm' "$h" "$m"
    else
        printf '%dm' "$m"
    fi
}

# Prints "<window_minutes> <rendered>", so callers can order windows by length.
# Codex puts the weekly bucket in `primary`, the reverse of how Claude Code
# renders its own quota (five_hour then seven_day), so the slot name is not a
# usable sort key.
window_part() {
    local which=$1 pct minutes resets label out
    pct=$(printf '%s' "$snapshot" | jq -r ".rate_limits.${which}.used_percent // empty" 2>/dev/null)
    [ -n "$pct" ] && [ "$pct" != "null" ] || return
    minutes=$(printf '%s' "$snapshot" | jq -r ".rate_limits.${which}.window_minutes // empty" 2>/dev/null)
    resets=$(printf '%s' "$snapshot" | jq -r ".rate_limits.${which}.resets_at // empty" 2>/dev/null)
    label=$(window_label "$minutes")
    out="${label}:$(printf '%.0f' "$pct")%"
    local r
    r=$(fmt_reset "$resets")
    [ -n "$r" ] && out="${out}(${r})"
    # Unknown window length sorts last rather than ahead of everything.
    case "$minutes" in
    '' | null | *[!0-9]*) minutes=999999 ;;
    esac
    printf '%s %s' "$minutes" "$out"
}

# Shortest window first, matching the 5h-then-7d order Claude Code uses.
parts=()
while read -r _minutes rendered; do
    [ -n "$rendered" ] && parts+=("$rendered")
done < <(
    for which in primary secondary; do
        p=$(window_part "$which")
        [ -n "$p" ] && printf '%s\n' "$p"
    done | sort -n -k1,1
)

# No parsed windows: fall back to the raw header the backend does send, so the
# segment degrades to something rather than nothing if the payload shape moves.
if [ ${#parts[@]} -eq 0 ]; then
    active=$(printf '%s' "$snapshot" | jq -r '.headers["x-codex-active-limit"] // empty' 2>/dev/null)
    [ -n "$active" ] && [ "$active" != "null" ] && parts+=("$active")
fi
[ ${#parts[@]} -gt 0 ] || exit 0

segment="codex ${parts[*]}"

reached=$(printf '%s' "$snapshot" | jq -r '.rate_limits.rate_limit_reached_type // .headers["x-codex-rate-limit-reached-type"] // empty' 2>/dev/null)
if [ -n "$reached" ] && [ "$reached" != "null" ]; then
    segment="$segment ⚠ ${reached}"
fi

balance=$(printf '%s' "$snapshot" | jq -r '.rate_limits.credits.balance // empty' 2>/dev/null)
if [ -n "$balance" ] && [ "$balance" != "null" ]; then
    segment="$segment cr:${balance}"
fi

if [ "$segment_only" -eq 1 ]; then
    printf '%s\n' "$segment"
else
    input=$(cat 2>/dev/null || true)
    model=$(printf '%s' "$input" | jq -r '.model.display_name // empty' 2>/dev/null)
    [ -n "$model" ] && printf '%s | ' "$model"
    printf '%s\n' "$segment"
fi

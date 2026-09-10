#!/usr/bin/env bash
# Spark context segment for the Claude Code statusline.
#
# Claude Code sends no usable `context_window` for a routed Spark model, so the
# `ctx:` segment in statusline-usage-dump.sh never fires there. The proxy sees
# every turn's full transcript (each Anthropic turn resends it whole) and
# serves the last Spark turn at /spark-context; this script renders it.
#
# With --segment, prints ONLY the spark part (for appending to an existing
# statusline) and prints NOTHING when the proxy is unreachable, has never seen
# a Spark turn, or the snapshot has gone stale — zero statusline space unless
# you are actually using a Spark model.
#
# Shows: spark ctx:223.8k/1048.6k
#
# Optional first argument after --segment is the active model name; when given,
# nothing prints unless it looks like a spark model. Without it the segment
# falls back to snapshot freshness, which is a good enough proxy for "the last
# thing you sent went to spark".
set -u

CONTEXT_URL="${HEADROOM_SPARK_CONTEXT_URL:-http://127.0.0.1:8787/spark-context}"
# Beyond this the snapshot describes a session you have probably moved on from.
STALE_AFTER="${HEADROOM_SPARK_CONTEXT_STALE_AFTER:-900}"

segment_only=0
if [ "${1:-}" = "--segment" ]; then
    segment_only=1
    shift
fi
active_model="${1:-}"

# An explicit non-spark model means this segment has nothing to say.
if [ -n "$active_model" ] && ! printf '%s' "$active_model" | grep -qi spark; then
    exit 0
fi

command -v jq >/dev/null 2>&1 || exit 0
snapshot=$(curl -s --max-time 1 "$CONTEXT_URL" 2>/dev/null) || exit 0
[ -n "$snapshot" ] || exit 0

observed=$(printf '%s' "$snapshot" | jq -r '.observed_at // empty' 2>/dev/null)
[ -n "$observed" ] && [ "$observed" != "null" ] || exit 0

age=$(printf '%s' "$snapshot" | jq -r '.age_seconds // 0' 2>/dev/null)
[ "$age" -le "$STALE_AFTER" ] 2>/dev/null || exit 0

used=$(printf '%s' "$snapshot" | jq -r '.input_tokens // empty' 2>/dev/null)
size=$(printf '%s' "$snapshot" | jq -r '.context_window // empty' 2>/dev/null)
[ -n "$used" ] && [ "$used" != "null" ] || exit 0
[ -n "$size" ] && [ "$size" != "null" ] || exit 0

fmt_tokens() {
    local n=$1
    if [ "$n" -ge 1000 ] 2>/dev/null; then
        awk -v n="$n" 'BEGIN{printf "%.1fk", n/1000}'
    else
        printf "%d" "$n" 2>/dev/null || printf '%s' "$n"
    fi
}

segment="spark ctx:$(fmt_tokens "$used")/$(fmt_tokens "$size")"

if [ "$segment_only" -eq 1 ]; then
    printf '%s\n' "$segment"
else
    input=$(cat 2>/dev/null || true)
    model=$(printf '%s' "$input" | jq -r '.model.display_name // empty' 2>/dev/null)
    [ -n "$model" ] && printf '%s | ' "$model"
    printf '%s\n' "$segment"
fi

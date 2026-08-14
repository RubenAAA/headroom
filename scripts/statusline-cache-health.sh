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
#   drift    → ⚠ directly attributed cache bust, shown RECACHE_DRIFT_WINDOW s
#   expected → ℹ no direct causal evidence; the event is unattributed, shown
#              RECACHE_EXPECTED_WINDOW s
# then falls back to the ambient ratio.
#
# Ahead of all of that: if upstream is refusing a meaningful share of forwarded
# turns (`upstream.verdict` from the proxy), say so instead. That costs more
# than any cache behaviour this script reports.
set -u

HEALTH_URL="${HEADROOM_CACHE_HEALTH_URL:-http://127.0.0.1:8787/cache-health}"
RECACHE_DRIFT_WINDOW="${HEADROOM_RECACHE_DRIFT_WINDOW:-180}"
RECACHE_EXPECTED_WINDOW="${HEADROOM_RECACHE_EXPECTED_WINDOW:-60}"

# Focused in-file regression test. Keeping the fixture here lets this standalone
# script prove its rendering without adding a test harness or touching another
# file in a deliberately narrow patch.
if [ "${1:-}" = "--self-test" ]; then
    fixture='{"upstream":{"verdict":"healthy"},"last_event_age_seconds":4,"last_event":{"event_kind":"branch","attribution_reason":"inbound_tail_replaced","origin":"inbound","scope":"final_message","wasted_tokens":0,"cache_creation_input_tokens":12345}}'
    actual=$(HEADROOM_STATUSLINE_TEST_HEALTH="$fixture" "${BASH_SOURCE[0]}" --segment)
    expected='ℹ branch/cache build 4s ago: inbound final message replaced, ~12K tok cached (not waste)'
    if [ "$actual" != "$expected" ]; then
        printf 'not ok - inbound tail build\nexpected: %s\nactual:   %s\n' "$expected" "$actual" >&2
        exit 1
    fi
    fixture='{"upstream":{"verdict":"healthy"},"last_event_age_seconds":13,"last_event":{"event_kind":"unexplained","attribution_reason":"unexplained_after_replay","origin":"unknown","scope":"replayed_prefix","wasted_tokens":48669,"cache_creation_input_tokens":48669,"replayed_prefix":true,"replay_chain_id":2,"breakpoints_placed":2,"system_markers_dropped":0}}'
    actual=$(HEADROOM_STATUSLINE_TEST_HEALTH="$fixture" "${BASH_SOURCE[0]}" --segment)
    expected='⚠ recache 13s ago: replay applied, cause unexplained, ~48K tok wasted'
    if [ "$actual" != "$expected" ]; then
        printf 'not ok - provider miss after replay\nexpected: %s\nactual:   %s\n' "$expected" "$actual" >&2
        exit 1
    fi
    printf 'ok - branch build and provider miss evidence are rendered\n'
    exit 0
fi

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

if [ -n "${HEADROOM_STATUSLINE_TEST_HEALTH:-}" ]; then
    health="$HEADROOM_STATUSLINE_TEST_HEALTH"
else
    health=$(curl -fsS --max-time 1 "$HEALTH_URL" 2>/dev/null)
fi
if [ -z "$health" ]; then
    # Segment mode: headroom not running → occupy no space at all.
    [ "$segment_only" -eq 1 ] && exit 0
    printf '%s\n' "${prefix:+$prefix | }cache: proxy unreachable"
    exit 0
fi

# Refusals outrank any cache news. A re-cached prefix costs tokens; a refused
# turn loses the work and re-caches anyway. This is the line that was missing
# while a fifth of subagent turns were being rejected.
verdict=$(printf '%s' "$health" | jq -r '.upstream.verdict // "healthy"')
# An empty result means jq failed or the proxy predates this field — not a
# refusal. Only an explicit non-healthy verdict takes over the line.
if [ -n "$verdict" ] && [ "$verdict" != "healthy" ]; then
    pct=$(printf '%s' "$health" | jq -r '.upstream.recent_refused_pct // 0')
    why=$(printf '%s' "$health" | jq -r '.upstream.last_error_type // "unknown"')
    # "elevated" is under the alert threshold — worth seeing, not worth alarm.
    if [ "$verdict" = "elevated" ]; then mark="⚠"; else mark="✖"; fi
    printf '%s\n' "${prefix:+$prefix | }${mark} upstream refusing ${pct}% of turns (${why})"
    exit 0
fi

age=$(printf '%s' "$health" | jq -r '.last_event_age_seconds // empty')
if [ -n "$age" ]; then
    kind=$(printf '%s' "$health" | jq -r '.last_event.event_kind // "drift"')
    if [ "$kind" = "expected" ] || [ "$kind" = "branch" ]; then
        window="$RECACHE_EXPECTED_WINDOW"
    else
        window="$RECACHE_DRIFT_WINDOW"
    fi
    if [ "$age" -lt "$window" ]; then
        wasted=$(printf '%s' "$health" | jq -r '.last_event.wasted_tokens // 0')
        if [ "$wasted" -ge 1000 ]; then
            wasted="$((wasted / 1000))K"
        fi
        if [ "$kind" = "branch" ]; then
            reason=$(printf '%s' "$health" | jq -r '.last_event.attribution_reason // empty')
            origin=$(printf '%s' "$health" | jq -r '.last_event.origin // empty')
            scope=$(printf '%s' "$health" | jq -r '.last_event.scope // empty')
            created=$(printf '%s' "$health" | jq -r '.last_event.cache_creation_input_tokens // 0')
            if [ "$created" -ge 1000 ]; then
                created="$((created / 1000))K"
            fi
            if [ "$reason" = "inbound_tail_replaced" ] && [ "$origin" = "inbound" ] && [ "$scope" = "final_message" ]; then
                printf '%s\n' "${prefix:+$prefix | }ℹ branch/cache build ${age}s ago: inbound final message replaced, ~${created} tok cached (not waste)"
            else
                printf '%s\n' "${prefix:+$prefix | }ℹ branch/cache build ${age}s ago: ~${created} tok cached (not waste)"
            fi
        elif [ "$kind" = "expected" ]; then
            printf '%s\n' "${prefix:+$prefix | }ℹ cache drop ${age}s ago: cause unattributed, ~${wasted} tok re-cached"
        elif [ "$kind" = "unexplained" ]; then
            printf '%s\n' "${prefix:+$prefix | }⚠ recache ${age}s ago: replay applied, cause unexplained, ~${wasted} tok wasted"
        else
            # `drift_dims` keeps this useful against older proxy payloads.
            reason=$(printf '%s' "$health" | jq -r '([.last_event.attribution_reason, .last_event.drift_dims] | map(select(type == "string" and length > 0)) | first) // "unknown cause"')
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

#!/bin/bash
# statusline-usage-dump
input=$(cat)
echo "$input" | jq -c '{
  rate_limits: .rate_limits,
  context_window: .context_window,
  model: .model,
  ts: now
}' > /tmp/claude-usage-latest.json 2>/dev/null

fmt_reset() {
  local ts=$1
  [ -z "$ts" ] || [ "$ts" = "null" ] && return
  local now diff h m
  now=$(date +%s)
  diff=$((ts - now))
  [ "$diff" -le 0 ] && { printf "0m"; return; }
  h=$((diff / 3600))
  m=$(((diff % 3600) / 60))
  if [ "$h" -gt 0 ]; then printf "%dh%02dm" "$h" "$m"; else printf "%dm" "$m"; fi
}

fmt_tokens() {
  local n=$1
  [ -z "$n" ] || [ "$n" = "null" ] && { printf "?"; return; }
  if [ "$n" -ge 1000 ]; then
    awk -v n="$n" 'BEGIN{printf "%.1fk", n/1000}'
  else
    printf "%d" "$n"
  fi
}

five_pct=$(echo "$input" | jq -r '.rate_limits.five_hour.used_percentage // empty')
five_reset=$(echo "$input" | jq -r '.rate_limits.five_hour.resets_at // empty')
week_pct=$(echo "$input" | jq -r '.rate_limits.seven_day.used_percentage // empty')
week_reset=$(echo "$input" | jq -r '.rate_limits.seven_day.resets_at // empty')

# Claude Code fills `rate_limits` only for Anthropic subscription auth, so a
# session on a routed model (codex, spark) reports none and the 5h/7d segments
# would vanish mid-session. Keep the last values we did see and show them
# marked `~`.
# They stay meaningful until their own reset time, which is an absolute stamp,
# so a stale reading expires on its own rather than going quietly wrong.
cache="$HOME/.claude/rate-limits-cache.json"
stale=""
if [ -n "$five_pct" ] || [ -n "$week_pct" ]; then
  echo "$input" | jq -c '.rate_limits' >"$cache" 2>/dev/null
elif [ -f "$cache" ]; then
  stale="~"
  five_pct=$(jq -r '.five_hour.used_percentage // empty' "$cache" 2>/dev/null)
  five_reset=$(jq -r '.five_hour.resets_at // empty' "$cache" 2>/dev/null)
  week_pct=$(jq -r '.seven_day.used_percentage // empty' "$cache" 2>/dev/null)
  week_reset=$(jq -r '.seven_day.resets_at // empty' "$cache" 2>/dev/null)
  now_s=$(date +%s)
  [ -n "$five_reset" ] && [ "$five_reset" -le "$now_s" ] 2>/dev/null && five_pct=""
  [ -n "$week_reset" ] && [ "$week_reset" -le "$now_s" ] 2>/dev/null && week_pct=""
fi

ctx_in=$(echo "$input" | jq -r '.context_window.current_usage.input_tokens // 0')
ctx_cc=$(echo "$input" | jq -r '.context_window.current_usage.cache_creation_input_tokens // 0')
ctx_cr=$(echo "$input" | jq -r '.context_window.current_usage.cache_read_input_tokens // 0')
ctx_size=$(echo "$input" | jq -r '.context_window.context_window_size // 0')
case "$ctx_size" in '' | null | *[!0-9]*) ctx_size=0 ;; esac
ctx_used=$((ctx_in + ctx_cc + ctx_cr))
# Some payloads carry only the flat total; use it when the parts sum to zero.
if [ "$ctx_used" -eq 0 ]; then
  ctx_total=$(echo "$input" | jq -r '.context_window.total_input_tokens // 0')
  case "$ctx_total" in '' | null | *[!0-9]*) ctx_total=0 ;; esac
  [ "$ctx_total" -gt 0 ] 2>/dev/null && ctx_used=$ctx_total
fi
# Routed spark models report no (or zero-sized) context_window, so the ctx
# segment vanishes. Fall back to the known 1MiB window (1,048,576 tokens per
# Meta's model docs, shared by every muse-spark variant) when usage is present
# but the size is missing.
if [ "$ctx_size" -eq 0 ] && [ "$ctx_used" -gt 0 ] 2>/dev/null; then
  model_id=$(echo "$input" | jq -r '.model.id // .model.display_name // empty')
  if printf '%s' "$model_id" | grep -qi spark; then
    ctx_size=1048576
  fi
fi

cwd=$(echo "$input" | jq -r '.workspace.project_dir // .cwd // empty')

dir_line=""
if [ -n "$cwd" ]; then
  dir_line="dir:${cwd#"$HOME"/}"
fi

parts=()
if [ -n "$five_pct" ]; then
  r=$(fmt_reset "$five_reset")
  s="5h:$(printf '%.0f' "$five_pct")%${stale}"
  [ -n "$r" ] && s="$s(${r})"
  parts+=("$s")
fi
if [ -n "$week_pct" ]; then
  r=$(fmt_reset "$week_reset")
  s="7d:$(printf '%.0f' "$week_pct")%${stale}"
  [ -n "$r" ] && s="$s(${r})"
  parts+=("$s")
fi
if [ "$ctx_used" -gt 0 ] && [ "$ctx_size" -gt 0 ]; then
  parts+=("ctx:$(fmt_tokens "$ctx_used")/$(fmt_tokens "$ctx_size")")
fi

[ -n "$dir_line" ] && echo "$dir_line"
(IFS=' '; echo "${parts[*]}")

#!/bin/bash
# Chains the existing usage-dump statusline with the headroom
# re-cache watchdog segment (CTX-7). The cache segment prints
# nothing when headroom isn't running, so the statusline is
# byte-identical to the original in that case.
input=$(cat)
base=$(printf '%s' "$input" | ~/.claude/statusline-usage-dump.sh)
cache=$(${HEADROOM_REPO:-$HOME/headroom}/scripts/statusline-cache-health.sh --segment)

# Codex quota. Claude Code only fills its own `rate_limits` for Anthropic
# subscription auth, so a routed codex model shows nothing there; this reads
# what the proxy saw instead. Silent unless a codex model is active.
model=$(printf '%s' "$input" | jq -r '.model.id // .model.display_name // empty' 2>/dev/null)
codex=$(${HEADROOM_REPO:-$HOME/headroom}/scripts/statusline-codex-limits.sh --segment "$model")

line="$base"
[ -n "$cache" ] && line="$line | $cache"
[ -n "$codex" ] && line="$line | $codex"
printf '%s\n' "$line"

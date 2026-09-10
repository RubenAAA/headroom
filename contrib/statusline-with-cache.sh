#!/bin/bash
# Chains the usage-dump statusline with the headroom re-cache watchdog
# segment (CTX-7), the Codex quota segment, the Spark context segment, and the
# cache-performance line.
# Every segment prints nothing when headroom isn't running, so the statusline
# then reads byte-identical to the usage dump alone.
#
# Install: ln -sf "$HEADROOM_REPO/contrib/statusline-with-cache.sh" ~/.claude/
# and point settings.json statusLine.command at that path. Every helper lives
# next to this file, so one symlink is enough.
here="${HEADROOM_REPO:-$HOME/headroom}/contrib"
input=$(cat)
base=$(printf '%s' "$input" | "$here/statusline-usage-dump.sh")
cache=$("$here/statusline-cache-health.sh" --segment)

# Codex quota. Claude Code only fills its own `rate_limits` for Anthropic
# subscription auth, so a routed codex model shows nothing there; this reads
# what the proxy saw instead. Silent unless a codex model is active.
model=$(printf '%s' "$input" | jq -r '.model.id // .model.display_name // empty' 2>/dev/null)
codex=$("$here/statusline-codex-limits.sh" --segment "$model")

# Spark context. Claude Code sends no usable `context_window` for a routed
# spark model, so the `ctx:` segment above never fires there; this reads the
# last spark turn the proxy saw instead. Skipped when the base line already
# carries a `ctx:` reading (Claude's own counting wins when present).
spark=""
if [[ "$base" != *ctx:* ]]; then
  spark=$("$here/statusline-spark-context.sh" --segment "$model")
fi
perf=$("$here/statusline-cache-perf.sh")

line="$base"
# Healthy cache percentage belongs on the performance line; keep alert text on
# the main line so recache warnings still take priority.
if [[ "$cache" != cache\ ✓\ * ]]; then
  [ -n "$cache" ] && line="$line | $cache"
fi
[ -n "$codex" ] && line="$line | $codex"
[ -n "$spark" ] && line="$line | $spark"
printf '%s\n' "$line"
[ -n "$perf" ] && printf '%s\n' "$perf"
# Claude Code drops the statusline when the command exits non-zero; the last
# test above returns 1 whenever the perf line is empty.
exit 0

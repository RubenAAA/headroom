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
# Resolved through the symlink: HEADROOM_REPO is not in Claude Code's
# environment, so a fallback to ~/headroom missed any other checkout.
if self=$(readlink -f "${BASH_SOURCE[0]}" 2>/dev/null) && [ -n "$self" ]; then
  here=$(dirname "$self")
else
  here="${HEADROOM_REPO:-$HOME/headroom}/contrib"
fi
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

# Fold a `|`-separated line so no segment is cut by the terminal edge.
# Width comes from the tty when there is one; statusline commands run without
# one, so fall back to COLUMNS and then 80. Segments are never split — each
# moves whole to the next line, joined with ` | ` while it fits.
# Newlines in the input (the base line carries its own dir: line) are kept:
# each line folds on its own.
# ANSI colors are kept for display but excluded from the width count.
fold_line() {
  local line=$1 cols=${2:-0}
  [ "$cols" -le 0 ] 2>/dev/null && cols=$(stty size 2>/dev/null </dev/tty | awk '{print $2}')
  [ -z "$cols" ] && cols=${COLUMNS:-80}
  case "$cols" in ''|*[!0-9]*) cols=80 ;; esac
  local raw out="" cur="" curlen=0 s plain slen
  while IFS= read -r raw || [ -n "$raw" ]; do
    local IFS='|'
    # shellcheck disable=SC2206
    segs=($raw)
    out=""; cur=""; curlen=0
    for s in "${segs[@]}"; do
      s=$(printf '%s' "$s" | sed 's/^ *//;s/ *$//')
      [ -z "$s" ] && continue
      plain=$(printf '%s' "$s" | sed "s/$(printf '\033')\[[0-9;]*m//g")
      slen=${#plain}
      if [ "$curlen" -eq 0 ]; then
        if [ "$slen" -gt "$cols" ]; then
          # One segment wider than the terminal: wrap word-wise so
          # nothing is cut. ANSI codes hold no spaces, so splitting
          # on spaces never breaks them.
          local w wplain wlen
          local IFS=$' \t\n'
          for w in $s; do
            wplain=$(printf '%s' "$w" | sed "s/$(printf '\033')\[[0-9;]*m//g")
            wlen=${#wplain}
            if [ "$curlen" -eq 0 ]; then
              cur="$w"; curlen=$wlen
            elif [ $((curlen + 1 + wlen)) -le "$cols" ]; then
              cur="$cur $w"; curlen=$((curlen + 1 + wlen))
            else
              out="$out$cur
"
              cur="$w"; curlen=$wlen
            fi
          done
        else
          cur="$s"; curlen=$slen
        fi
      elif [ $((curlen + 3 + slen)) -le "$cols" ]; then
        cur="$cur | $s"; curlen=$((curlen + 3 + slen))
      elif [ "$slen" -gt "$cols" ]; then
        # Segment wider than the terminal on a non-empty line: flush
        # the line, then wrap the segment word-wise so nothing is cut.
        out="$out$cur
"
        cur=""; curlen=0
        local w wplain wlen
        local IFS=$' \t\n'
        for w in $s; do
          wplain=$(printf '%s' "$w" | sed "s/$(printf '\033')\[[0-9;]*m//g")
          wlen=${#wplain}
          if [ "$curlen" -eq 0 ]; then
            cur="$w"; curlen=$wlen
          elif [ $((curlen + 1 + wlen)) -le "$cols" ]; then
            cur="$cur $w"; curlen=$((curlen + 1 + wlen))
          else
            out="$out$cur
"
            cur="$w"; curlen=$wlen
          fi
        done
      else
        out="$out$cur
"
        cur="$s"; curlen=$slen
      fi
    done
    [ -n "$cur" ] && printf '%s\n' "$out$cur"
  done <<<"$line"
}

line="$base"
[ -n "$spark" ] && line="$line | $spark"
# Healthy cache percentage belongs on the performance line; keep alert text on
# the main line so recache warnings still take priority.
if [[ "$cache" != cache\ ✓\ * ]]; then
  [ -n "$cache" ] && line="$line | $cache"
fi
[ -n "$codex" ] && line="$line | $codex"
fold_line "$line"
[ -n "$perf" ] && fold_line "$perf"
# Claude Code drops the statusline when the command exits non-zero; the last
# test above returns 1 whenever the perf line is empty.
exit 0

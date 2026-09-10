#!/bin/bash
# rotation-notice.sh: relay VPN-rotation notices on the next user prompt.
#
# zen-rotate-watch.sh leaves one small JSON file per recently-active session
# when the exit rotates (reactive, stragglers, or manual) instead of waking
# sessions with billed `claude --resume` turns. This hook prints the pending
# notice into the session once, then marks it reported. Informational only:
# always exits 0, never blocks, never fails closed.
# Installed into ~/.claude/hooks by install.sh, registered on UserPromptSubmit.
INPUT=$(cat) || exit 0
SESSION=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null | tr -cd 'A-Za-z0-9_-')
[ -n "$SESSION" ] || exit 0

OUTDIR="$HOME/.local/state/spark-review"
NOTICE="$OUTDIR/$SESSION.rotation.json"
MARKED="$OUTDIR/$SESSION.rotation.reported"
[ -f "$NOTICE" ] || exit 0
[ -f "$MARKED" ] && exit 0

MSG=$(jq -r '.message // empty' "$NOTICE" 2>/dev/null)
TS=$(jq -r '.ts // empty' "$NOTICE" 2>/dev/null)
REASON=$(jq -r '.reason // empty' "$NOTICE" 2>/dev/null)
if [ -n "$MSG" ]; then
  WHEN=""
  if [ -n "$TS" ]; then
    STAMP=$(date -r "$TS" '+%F %T' 2>/dev/null || date -d "@$TS" '+%F %T' 2>/dev/null || echo "$TS")
    WHEN=" (at $STAMP)"
  fi
  echo "VPN EXIT ROTATED${REASON:+ ($REASON)}$WHEN: $MSG"
  touch "$MARKED" 2>/dev/null || true
fi
exit 0

#!/bin/bash
# spark-review: log session -> transcript mapping. Read-only observer, never blocks.
# Hook input JSON arrives on stdin with session_id, transcript_path, cwd.
# Installed by install.sh into ~/.claude/hooks (copied, or symlinked with --link).
INPUT=$(cat)
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)
OUT="$HOME"/.local/state/spark-review/session-map.jsonl
mkdir -p "$(dirname "$OUT")"
jq -n --arg s "$SESSION_ID" --arg t "$TRANSCRIPT" --arg c "$CWD" --arg e "$EVENT" \
  '{ts: now, event: $e, session_id: $s, transcript_path: $t, cwd: $c}' >> "$OUT" 2>/dev/null
exit 0

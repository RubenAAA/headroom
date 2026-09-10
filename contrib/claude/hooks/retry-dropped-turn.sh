#!/bin/bash
# retry-dropped-turn.sh: Stop hook — auto-continue turns parked on a dropped
# API connection instead of leaving the session stuck on an error.
#
# When the connection drops mid-response, the turn ends with the pending tool
# call discarded ("dropped mid-response ... did NOT run"). There is nothing
# wrong with the session itself, so block the stop (exit 2) and hand the model
# a re-issue instruction. A drop that lands in plain text instead carries only
# the bare truncation marker (no tool call lost); that stalls the agent loop
# the same way, so it gets the same treatment with a continue prompt. The
# transcript tail is the detector: the marker text lands there verbatim.
#
# Circuit breaker: at most MAX_RETRIES consecutive error-continuations per
# session, then let it stop — a persistent outage must not spin forever. A
# clean stop resets the counter. Never fails closed: any internal error exits
# 0 so a broken guard never traps a session.
# Installed into ~/.claude/hooks by install.sh, registered on Stop in
# settings.json. Takes effect for sessions started after registration
# (hooks load at session start).
MAX_RETRIES=3
STATE_DIR="${XDG_RUNTIME_DIR:-/tmp}/claude-retry-dropped"

INPUT=$(cat) || exit 0
SESSION=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null | tr -cd 'A-Za-z0-9_-')
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
[ -n "$SESSION" ] || exit 0
[ -n "$TRANSCRIPT" ] && [ -f "$TRANSCRIPT" ] || exit 0

TAIL=$(tail -n 30 "$TRANSCRIPT" 2>/dev/null) || exit 0
if printf '%s' "$TAIL" | grep -qF 'dropped mid-response'; then
  if printf '%s' "$TAIL" | grep -qF 'did NOT run'; then
    MSG="The API connection dropped mid-response and a pending tool call was discarded without running"
    TAIL_INSTR="Check the transcript first: if that call already ran since the drop, do not re-issue it. Otherwise re-issue the discarded tool call now; do not ask, do not narrate."
  else
    MSG="The API connection dropped mid-response and the reply was cut off"
    TAIL_INSTR="Continue from where the reply was cut off; do not repeat tool calls that already ran — check the transcript first, then continue."
  fi
  COUNT=$(cat "$STATE_DIR/$SESSION" 2>/dev/null || echo 0)
  case "$COUNT" in '' | *[!0-9]*) COUNT=0 ;; esac
  if [ "$COUNT" -ge "$MAX_RETRIES" ]; then
    rm -f "$STATE_DIR/$SESSION"
    exit 0
  fi
  mkdir -p "$STATE_DIR" 2>/dev/null || exit 0
  echo $((COUNT + 1)) >"$STATE_DIR/$SESSION" 2>/dev/null || exit 0
  echo "$MSG (retry $((COUNT + 1))/$MAX_RETRIES). $TAIL_INSTR" >&2
  exit 2
fi

rm -f "$STATE_DIR/$SESSION"
exit 0

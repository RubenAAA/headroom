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
# Debug log: every stop records what it saw, so a false fire leaves evidence
# instead of a guessing game. Best-effort only — never blocks the stop.
mkdir -p "${XDG_RUNTIME_DIR:-/tmp}/claude-retry-dropped" 2>/dev/null
{
  echo "ts=$(date -u +%FT%TZ) session=$SESSION transcript=$TRANSCRIPT"
  echo "--- tail markers (counts in last 30 lines):"
  printf '%s' "$TAIL" | grep -cF '[truncated: the connection to the API dropped mid-response' | sed 's/^/trunc=/'
  printf '%s' "$TAIL" | grep -cF 'did NOT run' | sed 's/^/didnotrun=/'
  printf '%s' "$TAIL" | grep -cF '"isApiErrorMessage":true' | sed 's/^/apierr=/'
  printf '%s' "$TAIL" | grep -cF '[headroom: a proxy tool call was dropped and did NOT run; re-issue it]' | sed 's/^/retrieval-marker=/'
  echo "--- matching lines (first 200 chars):"
  printf '%s\n' "$TAIL" | grep -F '[truncated: the connection to the API dropped mid-response' | cut -c1-200 | head -3
  printf '%s\n' "$TAIL" | grep -F '[headroom: a proxy tool call was dropped and did NOT run; re-issue it]' | cut -c1-200 | head -3
  echo "---"
} >>"${XDG_RUNTIME_DIR:-/tmp}/claude-retry-dropped/stop-debug.log" 2>/dev/null
# Match the literal proxy-injected marker (TRUNCATION_MARKER in
# crates/headroom-proxy/src/sse/stream_finisher.rs), not the bare words —
# those also appear in this script's own comments and in docs, so a Read/grep
# of this file (or a reply quoting it) landing in the last 30 lines used to
# self-trigger a false "dropped" retry. Gated on assistant text lines for the
# same reason: the marker also appears in tool_use inputs (an Edit patching a
# detector, a Bash command grepping a transcript) and tool_result outputs,
# which land in user lines and must never re-arm the hook. The truncation
# branch needs the same gate — a Bash command I ran grepping this transcript
# for the marker text matched its own command line and blocked the stop.
MSG=""
TAIL_INSTR=""
RETRY_TAIL=$(printf '%s\n' "$TAIL" | grep -F '"type":"assistant"' | grep -F '"type":"text"')
if printf '%s' "$RETRY_TAIL" | grep -qF '[truncated: the connection to the API dropped mid-response'; then
  if printf '%s' "$RETRY_TAIL" | grep -qF 'did NOT run'; then
    MSG="The API connection dropped mid-response and a pending tool call was discarded without running"
    TAIL_INSTR="Check the transcript first: if that call already ran since the drop, do not re-issue it. Otherwise re-issue the discarded tool call now; do not ask, do not narrate."
  else
    MSG="The API connection dropped mid-response and the reply was cut off"
    TAIL_INSTR="Continue from where the reply was cut off; do not repeat tool calls that already ran — check the transcript first, then continue."
  fi
elif API_ERR_LINES=$(printf '%s\n' "$TAIL" | grep -F '"isApiErrorMessage":true') && [ -n "$API_ERR_LINES" ] && printf '%s' "$API_ERR_LINES" | grep -qiE 'status (429|5[0-9][0-9])|rate.?limit|overloaded|upstream.*(error|unavailable|timeout)|service unavailable|internal server error'; then
  # A completed upstream error (Zen 429 after the hold budget, 5xx, transport
  # failure) also kills the turn as far as the agent loop is concerned — e.g.
  # a headroom_retrieve continuation that gets rate-limited on the free Zen
  # route. Same treatment as a dropped turn: re-enter rather than stall.
  # Gated on Claude Code's own isApiErrorMessage flag, not a bare text scan,
  # so tool output or prose that merely mentions these words (e.g. reading
  # this script, or proxy source discussing rate limits) can't be mistaken
  # for a real error that ended the turn.
  MSG="The upstream request failed and the turn ended on an error"
  TAIL_INSTR="Check the transcript first: re-issue whatever was in flight when the error hit (tool call or reply), without repeating work that already ran; if the error text names rate limiting, wait a few seconds before retrying — do not ask, do not narrate."
elif printf '%s' "$RETRY_TAIL" | grep -qF '[headroom: a proxy tool call was dropped and did NOT run; re-issue it]'; then
  # A retrieval-ended turn: the proxy dropped a tool call the client expected
  # and downgraded stop_reason to end_turn, leaving an apology instead of
  # content (empty_turn_text in crates/headroom-proxy/src/sse/ccr_stream.rs).
  # The agent loop stalls the same way as a dropped connection, so it gets
  # the same continue treatment. Matched on the bracketed machine marker, not
  # the apology prose: matching prose false-fired on a reply quoting it.
  # Gated on assistant text lines: the marker also appears in tool_use inputs
  # (e.g. an Edit patching this script) and tool_result outputs, which land
  # in user lines and must never re-arm the hook.
  MSG="The proxy dropped a tool call this turn and the reply came back empty"
  TAIL_INSTR="Check the transcript first: re-issue the dropped tool call now (it never ran), without repeating work that already ran; do not ask, do not narrate."
fi

# All three branches share the circuit breaker: without it, the elif-only reset
# added alongside the upstream-error branch left the dropped-connection case
# (the original, more common trigger) setting MSG but never actually
# blocking the stop.
if [ -z "$MSG" ]; then
  rm -f "$STATE_DIR/$SESSION"
  exit 0
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

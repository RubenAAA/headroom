#!/bin/bash
# retry-dropped-turn.sh: Stop/SubagentStop hook — auto-continue turns that
# stalled on infrastructure instead of leaving the session stuck.
#
# When the connection drops mid-response, the turn ends with the pending tool
# call discarded ("dropped mid-response ... did NOT run"). There is nothing
# wrong with the session itself, so block the stop (exit 2) and hand the model
# a re-issue instruction. A drop that lands in plain text instead carries only
# the bare truncation marker (no tool call lost); that stalls the agent loop
# the same way, so it gets the same treatment with a continue prompt. A
# retrieval answered in place (<retrieved_context> splice: the store lookup
# succeeded but no continuation answer was ever generated, so the fetched
# content sits in the transcript as assistant prose with end_turn and the
# agent — subagents especially — goes quiet holding content it never acted
# on) gets a continue-using-the-context prompt. The transcript tail is the
# detector: the marker text lands there verbatim.
#
# Circuit breaker: at most MAX_RETRIES consecutive error-continuations per
# session, then let it stop — a persistent outage must not spin forever. A
# clean stop resets the counter. Never fails closed: any internal error exits
# 0 so a broken guard never traps a session.
# Installed into ~/.claude/hooks by install.sh, registered on Stop and
# SubagentStop in settings.json. Takes effect for sessions started after
# registration (hooks load at session start).
MAX_RETRIES=3
STATE_DIR="${XDG_RUNTIME_DIR:-/tmp}/claude-retry-dropped"
# Detection window. The detector used to snapshot only the last 30 lines at
# the instant Stop fired -- and Stop races the transcript flush, so a marker
# written milliseconds earlier is missed (2026-09-16: marker at .966, hook
# read in the same second, trunc=0). The next Stop then finds the marker
# scrolled out, and the persisted-drop recovery only armed after a match, so
# a first-sighting miss was forgotten forever (0 fired in 103 decisions).
# Now every Stop scans a wide window and derives the pending state from the
# transcript itself: a marker with no recovery after it is still a stall,
# whenever the sighting happens.
# The wide window does not cover the other half of the race: the turn's
# final lines can land AFTER the hook reads (2026-09-17: splice written
# .263, hook scanned .400, saw nothing, and no later stop ever came to
# recover on). The settle-wait below polls for size stability first.
WIDE_N=300

INPUT=$(cat) || exit 0
SESSION=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null | tr -cd 'A-Za-z0-9_-')
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
[ -n "$SESSION" ] || exit 0
[ -n "$TRANSCRIPT" ] && [ -f "$TRANSCRIPT" ] || exit 0

# Settle-wait for the transcript flush. Proceed after two consecutive stable
# size polls (≈0.5 s on a settled file) or a 4 s budget, whichever comes
# first. Bounded and best-effort: every command is guarded, so a weird
# filesystem degrades to an immediate scan, never a trapped stop.
prev_size=""
stable_polls=0
wait_i=0
while [ "$wait_i" -lt 16 ]; do
  cur_size=$(wc -c < "$TRANSCRIPT" 2>/dev/null || echo 0)
  if [ "$cur_size" = "$prev_size" ]; then
    stable_polls=$((stable_polls + 1))
    [ "$stable_polls" -ge 2 ] && break
  else
    stable_polls=0
    prev_size=$cur_size
  fi
  sleep 0.25 2>/dev/null || break
  wait_i=$((wait_i + 1))
done

TAIL=$(tail -n 30 "$TRANSCRIPT" 2>/dev/null) || exit 0
WIDE=$(tail -n "$WIDE_N" "$TRANSCRIPT" 2>/dev/null) || exit 0
# Debug log: every stop records what it saw, so a false fire leaves evidence
# instead of a guessing game. Best-effort only — never blocks the stop.
# INPUT_KEYS + ISSUBAGENT are the live-observation channel for SubagentStop:
# the input schema and exit-2 continuation semantics there are asserted, not
# evidenced — the next real subagent stop records both the fields it arrived
# with and (via the session transcript afterwards) whether exit 2 re-entered.
mkdir -p "${XDG_RUNTIME_DIR:-/tmp}/claude-retry-dropped" 2>/dev/null
INPUT_KEYS=$(echo "$INPUT" | jq -r '[keys_unsorted[]] | join(",")' 2>/dev/null || echo "?")
case "$TRANSCRIPT" in
  */subagents/*) ISSUBAGENT=1 ;;
  *) ISSUBAGENT=0 ;;
esac
TRUNC_START='"text":"[truncated: the connection to the API dropped mid-response'
RETR_MARKER='[headroom: a proxy tool call was dropped and did NOT run; re-issue it]'
{
  echo "ts=$(date -u +%FT%TZ) session=$SESSION transcript=$TRANSCRIPT settle_polls=$wait_i input_keys=$INPUT_KEYS subagent=$ISSUBAGENT"
  echo "--- tail markers (counts in last 30 lines):"
  printf '%s' "$TAIL" | grep -cF '[truncated: the connection to the API dropped mid-response' | sed 's/^/trunc=/'
  printf '%s' "$TAIL" | grep -cF 'did NOT run' | sed 's/^/didnotrun=/'
  printf '%s' "$TAIL" | grep -cF '"isApiErrorMessage":true' | sed 's/^/apierr=/'
  printf '%s' "$TAIL" | grep -cF "$RETR_MARKER" | sed 's/^/retrieval-marker=/'
  printf '%s' "$TAIL" | grep -cF '<retrieved_context>' | sed 's/^/spliced-context=/'
  printf '%s' "$TAIL" | grep -cF 'a memory answer was dropped before delivery' | sed 's/^/memory-dropped=/'
  echo "--- wide scan (last $WIDE_N lines):"
  printf '%s\n' "$WIDE" | grep -F '"type":"assistant"' | grep -cF "$TRUNC_START" | sed 's/^/wide_trunc=/'
  printf '%s\n' "$WIDE" | grep -nF "$TRUNC_START" | tail -1 | cut -d: -f1 | sed 's/^/wide_trunc_last_rel=/'
  printf '%s\n' "$WIDE" | grep -F '"type":"assistant"' | grep -cF "$RETR_MARKER" | sed 's/^/wide_retrieval=/'
  echo "--- matching lines (first 200 chars):"
  printf '%s\n' "$TAIL" | grep -F '[truncated: the connection to the API dropped mid-response' | cut -c1-200 | head -3
  printf '%s\n' "$TAIL" | grep -F "$RETR_MARKER" | cut -c1-200 | head -3
  echo "---"
} >>"${XDG_RUNTIME_DIR:-/tmp}/claude-retry-dropped/stop-debug.log" 2>/dev/null
# Match the literal proxy-injected marker (TRUNCATION_MARKER in
# crates/headroom-proxy/src/sse/stream_finisher.rs), not the bare words —
# those also appear in this script's own comments and in docs, so a Read/grep
# of this file (or a reply quoting it) landing in the window used to
# self-trigger a false "dropped" retry. Gated on assistant text lines for the
# same reason: the marker also appears in tool_use inputs (an Edit patching a
# detector, a Bash command grepping a transcript) and tool_result outputs,
# which land in user lines and must never re-arm the hook. The truncation
# branch additionally requires the text to START with the marker: a reply
# discussing an old drop quotes it mid-sentence, while the synthesised
# message's content IS the marker. The retrieval marker instead trails the
# apology prose, so it stays a substring match there.
#
# A marker with no recovery after it is a stall, whenever it is sighted:
# recovered_after answers "did the session already move past this drop" for
# a marker at relative line $1 inside $WIDE. Tool activity after the marker
# (a re-issued call, or any result) or a later assistant message means the
# drop was handled -- by the model, the user, or an earlier firing of this
# hook -- and the stop may proceed.
recovered_after() {
  local after
  after=$(printf '%s\n' "$WIDE" | tail -n +"$(($1 + 1))" 2>/dev/null) || return 0
  # All three tool_use spellings: a later server/mcp tool call is the model
  # acting too (today only "type":"tool_use" appears in transcripts; the
  # others ride the wire and must not become a false-continue hole).
  printf '%s' "$after" | grep -qF -e '"type":"tool_use"' -e '"type":"server_tool_use"' -e '"type":"mcp_tool_use"' && return 0
  printf '%s' "$after" | grep -qF '"type":"tool_result"' && return 0
  printf '%s\n' "$after" | grep -F '"type":"assistant"' | grep -qF '"type":"text"' && return 0
  return 1
}

# Stricter sibling for the spliced-retrieval detector below: a tool_result
# after the splice is NOT an answer -- it is the mixed-turn case, where real
# tool calls ran after the retrieval was spliced in and the model then went
# quiet without answering. Only a later model action (tool_use) or later
# assistant prose counts as carrying on.
answered_after() {
  local after
  after=$(printf '%s\n' "$WIDE" | tail -n +"$(($1 + 1))" 2>/dev/null) || return 0
  printf '%s' "$after" | grep -qF -e '"type":"tool_use"' -e '"type":"server_tool_use"' -e '"type":"mcp_tool_use"' && return 0
  printf '%s\n' "$after" | grep -F '"type":"assistant"' | grep -qF '"type":"text"' && return 0
  return 1
}

MSG=""
TAIL_INSTR=""
# The `"text":"` prefix is the assistant gate: the synthesised message's
# content starts with the marker, while a reply quoting an old drop has
# prose before it and tool_use inputs carry it in user lines.
MARK_LINE=$(printf '%s\n' "$WIDE" | grep -nF "$TRUNC_START" | tail -1 | cut -d: -f1)
if [ -n "$MARK_LINE" ] &&
   printf '%s\n' "$WIDE" | sed -n "${MARK_LINE}p" | grep -qF '"type":"assistant"'; then
  if ! recovered_after "$MARK_LINE"; then
    if printf '%s\n' "$WIDE" | sed -n "${MARK_LINE}p" | grep -qF 'did NOT run'; then
      MSG="The API connection dropped mid-response and a pending tool call was discarded without running"
      TAIL_INSTR="Check the transcript first: if that call already ran since the drop, do not re-issue it. Otherwise re-issue the discarded tool call now; do not ask, do not narrate."
    else
      MSG="The API connection dropped mid-response and the reply was cut off"
      TAIL_INSTR="Continue from where the reply was cut off; do not repeat tool calls that already ran — check the transcript first, then continue."
    fi
  fi
fi
if [ -z "$MSG" ]; then
  API_ERR_LINE=$(printf '%s\n' "$WIDE" | grep -nF '"isApiErrorMessage":true' | tail -1 | cut -d: -f1)
  if [ -n "$API_ERR_LINE" ] &&
     printf '%s\n' "$WIDE" | sed -n "${API_ERR_LINE}p" | grep -qiE 'status (429|5[0-9][0-9])|rate.?limit|overloaded|upstream.*(error|unavailable|timeout)|service unavailable|internal server error' &&
     ! recovered_after "$API_ERR_LINE"; then
    # A completed upstream error (Zen 429 after the hold budget, 5xx,
    # transport failure) also kills the turn as far as the agent loop is
    # concerned — e.g. a headroom_retrieve continuation that gets
    # rate-limited on the free Zen route. Same treatment as a dropped turn:
    # re-enter rather than stall.
    # Gated on Claude Code's own isApiErrorMessage flag, not a bare text
    # scan, so tool output or prose that merely mentions these words (e.g.
    # reading this script, or proxy source discussing rate limits) can't
    # be mistaken for a real error that ended the turn.
    MSG="The upstream request failed and the turn ended on an error"
    TAIL_INSTR="Check the transcript first: re-issue whatever was in flight when the error hit (tool call or reply), without repeating work that already ran; if the error text names rate limiting, wait a few seconds before retrying — do not ask, do not narrate."
  fi
fi
if [ -z "$MSG" ]; then
  RETR_LINE=$(printf '%s\n' "$WIDE" | grep -nF "$RETR_MARKER" | tail -1 | cut -d: -f1)
  if [ -n "$RETR_LINE" ] &&
     printf '%s\n' "$WIDE" | sed -n "${RETR_LINE}p" | grep -qF '"type":"assistant"' &&
     ! recovered_after "$RETR_LINE"; then
      # A retrieval-ended turn: the proxy dropped a tool call the client
      # expected and downgraded stop_reason to end_turn, leaving an apology
      # instead of content (empty_turn_text in
      # crates/headroom-proxy/src/sse/ccr_stream.rs). The agent loop stalls
      # the same way as a dropped connection, so it gets the same continue
      # treatment. Matched on the bracketed machine marker, not the apology
      # prose: matching prose false-fired on a reply quoting it.
      # Gated on assistant text lines: the marker also appears in tool_use
      # inputs (e.g. an Edit patching this script) and tool_result outputs,
      # which land in user lines and must never re-arm the hook. (The
      # marker trails the apology prose, so unlike the truncation branch it
      # cannot require a starts-with match.)
      MSG="The proxy dropped a tool call this turn and the reply came back empty"
      TAIL_INSTR="Check the transcript first: re-issue the dropped tool call now (it never ran), without repeating work that already ran; do not ask, do not narrate."
    fi
fi
if [ -z "$MSG" ]; then
  # A retrieval answered in place with no answer built on it: the store
  # lookup succeeded (mixed-tools skip, all-failed skip, or the
  # store-hit/continuation-died fallback in proxy.rs handle_ccr_response),
  # so the fetched content sits in the transcript as assistant prose inside
  # <retrieved_context> and the turn ends end_turn. The agent loop treats
  # the turn as complete and goes quiet holding content it never acted on.
  # Matched on the splice tag, gated on assistant lines for the same
  # tool_use-input/tool_result-output reason as the marker branches above.
  # Fires only when nothing answers it: any later model action (tool_use)
  # or later assistant prose means the session carried on, and the stop
  # may proceed. A tool_result after the splice is not an answer -- it is
  # the mixed-turn pending calls running, after which the model still owes
  # its reply.
  SPLICE_TAG='<retrieved_context>'
  SPLICE_LINE=$(printf '%s\n' "$WIDE" | grep -nF "$SPLICE_TAG" | tail -1 | cut -d: -f1)
  if [ -n "$SPLICE_LINE" ] &&
     printf '%s\n' "$WIDE" | sed -n "${SPLICE_LINE}p" | grep -qF '"type":"assistant"' &&
     ! answered_after "$SPLICE_LINE"; then
      MSG="The turn ended with retrieved context but no answer built on it"
      TAIL_INSTR="Continue the original task now using the retrieved context above — reference it directly instead of calling headroom_retrieve for it again, and do not repeat tool calls that already ran; do not ask, do not narrate."
    fi
fi
if [ -z "$MSG" ]; then
  # A memory answer dropped before delivery (round cap stranded it; the
  # trace block names the call). The client cannot re-run proxy-owned
  # memory tools, so the only way on is answering without the lookup.
  # The marker lives inside the trace block at the head of its message,
  # so "after" starts mid-line: the model's own answer (if any) follows
  # in later blocks of the SAME line. Fire only when neither the line
  # remainder nor any later line shows the model acting or answering.
  MEM_MARKER='[headroom: a memory answer was dropped before delivery; continue without it]'
  MEM_LINE=$(printf '%s\n' "$WIDE" | grep -nF "$MEM_MARKER" | tail -1 | cut -d: -f1)
  if [ -n "$MEM_LINE" ] &&
     printf '%s\n' "$WIDE" | sed -n "${MEM_LINE}p" | grep -qF '"type":"assistant"'; then
      mem_rest=$(printf '%s\n' "$WIDE" | sed -n "${MEM_LINE}p" | awk -v m="$MEM_MARKER" '{i=index($0,m); print substr($0, i+length(m))}')
      mem_after=$(printf '%s\n' "$WIDE" | tail -n +"$((MEM_LINE + 1))" 2>/dev/null) || mem_after=""
      mem_acted=0
      printf '%s' "$mem_rest" | grep -qF -e '"type":"tool_use"' -e '"type":"server_tool_use"' -e '"type":"mcp_tool_use"' -e '"type":"text"' && mem_acted=1
      printf '%s' "$mem_after" | grep -qF -e '"type":"tool_use"' -e '"type":"server_tool_use"' -e '"type":"mcp_tool_use"' && mem_acted=1
      printf '%s\n' "$mem_after" | grep -F '"type":"assistant"' | grep -qF '"type":"text"' && mem_acted=1
      if [ "$mem_acted" -eq 0 ]; then
        MSG="The turn ended with a dropped memory lookup and no answer built without it"
        TAIL_INSTR="Continue the original task now without the dropped memory lookup — answer from the context you already have, do not try to re-run the memory call (it is proxy-run and will not resolve), and do not repeat tool calls that already ran; do not ask, do not narrate."
      fi
    fi
fi

# All five branches share the circuit breaker: without it, the elif-only reset
# added alongside the upstream-error branch left the dropped-connection case
# (the original, more common trigger) setting MSG but never actually
# blocking the stop.
#
# No persisted drop file: the pending state derives from the transcript
# itself on every Stop (last marker vs. any recovery after it), so there is
# no match-once-remember-forever flag to rot -- and no first-sighting miss
# that leaves a stall forgotten.

# Decision log: one line per invocation so the next failure leaves evidence
# (fired / breaker-tripped / no-match with the tail counts from above).
DECISION="no-match"
if [ -z "$MSG" ]; then
  rm -f "$STATE_DIR/$SESSION"
  echo "ts=$(date -u +%FT%TZ) session=$SESSION subagent=$ISSUBAGENT decision=$DECISION" >>"$STATE_DIR/stop-debug.log" 2>/dev/null || true
  exit 0
fi

COUNT=$(cat "$STATE_DIR/$SESSION" 2>/dev/null || echo 0)
case "$COUNT" in '' | *[!0-9]*) COUNT=0 ;; esac
if [ "$COUNT" -ge "$MAX_RETRIES" ]; then
  rm -f "$STATE_DIR/$SESSION"
  rm -f "$STATE_DIR/$SESSION.drop" 2>/dev/null || true  # pre-window-scan state, if any
  echo "ts=$(date -u +%FT%TZ) session=$SESSION subagent=$ISSUBAGENT decision=breaker-tripped" >>"$STATE_DIR/stop-debug.log" 2>/dev/null || true
  exit 0
fi
mkdir -p "$STATE_DIR" 2>/dev/null || exit 0
echo $((COUNT + 1)) >"$STATE_DIR/$SESSION" 2>/dev/null || exit 0
echo "ts=$(date -u +%FT%TZ) session=$SESSION subagent=$ISSUBAGENT decision=fired retry=$((COUNT + 1))/$MAX_RETRIES" >>"$STATE_DIR/stop-debug.log" 2>/dev/null || true
echo "$MSG (retry $((COUNT + 1))/$MAX_RETRIES). $TAIL_INSTR" >&2
exit 2

#!/bin/bash
# ticket-gate.sh: preempt YouTrack ticket filing, hand the last 10 turns to a worker.
# Serves UserPromptSubmit (an explicit "file the ticket" instruction) and
# PreToolUse on Bash (the model composing the API call itself). Never blocks
# benign work: exits 0 on every path except a diverted filing attempt (exit 2).
# Installed by install.sh into $HOME/.claude/hooks (copied, or symlinked with --link).
[ -n "$TICKET_FILE_WORKER" ] && exit 0
INPUT=$(cat)
EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ] && exit 0
OUTDIR="$HOME/.local/state/spark-review"
TICKET_WORKER="$HOME/headroom/contrib/spark-poster/ticket_file.py"
mkdir -p "$OUTDIR" 2>/dev/null

# ── no arming: the phrases below are explicit enough to act on directly ──
#
# The review gate needs arming because "post the threads" is ambiguous without
# a review session behind it. "File the ticket" names the action and its
# object in one breath, so the instruction alone is the divert condition.

# done = worker finished AND filed (proof on disk). done + .failed = worker
# finished without filing; the reason is in .failed. diverted without done =
# worker still running (or crashed; worker_alive says).
ticket_state() {
  if [ -f "$OUTDIR/$SESSION_ID.ticket.done" ]; then
    if [ -f "$OUTDIR/$SESSION_ID.ticket.failed" ]; then echo "failed"; else echo "done"; fi
  elif [ -f "$OUTDIR/$SESSION_ID.ticket.diverted" ]; then
    echo "running"
  else
    echo "idle"
  fi
}

ticket_alive() {
  local pid
  pid=$(cat "$OUTDIR/$SESSION_ID.ticket.started" 2>/dev/null)
  [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null || return 1
  # kill -0 alone trusts a recycled pid. The supervisor is a fork of this
  # script, so its command line still names it; anything else behind that
  # pid is not our worker. /proc is Linux-only -- elsewhere, kill -0 stands.
  [ -r "/proc/$pid/cmdline" ] || return 0
  tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q "ticket-gate"
}

spawn_ticket_worker() {
  # Atomic spawn guard (same race as review-gate's spawn_worker: prompt vs
  # Stop backstop, or two rapid prompts, both passing the alive-check before
  # either forks). flock -n decides once; the loser returns. See review-gate
  # for why the RETURN trap is safe with the background worker below.
  exec 9>"$OUTDIR/$SESSION_ID.ticket.spawn.flock" 2>/dev/null || return 0
  if ! flock -n 9 2>/dev/null; then
    exec 9>&- 2>/dev/null || true
    return 0
  fi
  trap 'flock -u 9 2>/dev/null; exec 9>&- 2>/dev/null; trap - RETURN' RETURN
  # Done means finished -- a finished filing always leaves a proof, so a
  # .done with no proof behind it is a crash, not a filing. Restart instead
  # of wedging every later prompt on "already filed".
  if [ -f "$OUTDIR/$SESSION_ID.ticket.done" ]; then
    if [ ! -f "$OUTDIR/$SESSION_ID.ticket.proof.json" ]; then
      rm -f "$OUTDIR/$SESSION_ID.ticket.diverted" "$OUTDIR/$SESSION_ID.ticket.done" \
        "$OUTDIR/$SESSION_ID.ticket.started"
    else
      return 0
    fi
  fi
  ticket_alive && return 0
  # A stale .diverted with no live worker is a previous crash, not a running
  # worker: drop it so this attempt is a real one.
  rm -f "$OUTDIR/$SESSION_ID.ticket.diverted"

  # Tickets live in the same env file as the review chain (mode 600):
  # source it so YOUTRACK_TOKEN and YOUTRACK_* reach the worker through the
  # hook environment. Refusing here, loudly, beats spawning a worker that
  # can only fail.
  set -a
  [ -f "$HOME/.config/spark-poster/env" ] && . "$HOME/.config/spark-poster/env"
  set +a
  # The bearer token lives in the mode-600 env file (or the ambient
  # environment) and reaches the worker through the hook environment. The
  # worker passes it to curl from that variable only -- never echoed, never
  # written. Refusing here, loudly, beats spawning a worker that can only
  # fail.
  if [ -z "$YOUTRACK_TOKEN" ]; then
    echo "YOUTRACK_TOKEN is not set in the hook environment" >"$OUTDIR/$SESSION_ID.ticket.failed"
    return 1
  fi
  touch "$OUTDIR/$SESSION_ID.ticket.diverted"
  # Stale state from a previous run must not leak into this one.
  rm -f "$OUTDIR/$SESSION_ID.ticket.failed" "$OUTDIR/$SESSION_ID.ticket.reported"

  (
    SESSLOG="$OUTDIR/$SESSION_ID.ticket.worker.log"
    {
      echo "=== ticket worker start: session $SESSION_ID ==="
      # 10 minutes, not 45: one headless call plus a handful of curls.
      if TICKET_FILE_WORKER=1 timeout 600 \
          python3 "$TICKET_WORKER" "$SESSION_ID" "$TRANSCRIPT" "$OUTDIR"; then
        touch "$OUTDIR/$SESSION_ID.ticket.done"
        command -v notify-send >/dev/null 2>&1 &&
          notify-send "ticket-gate" "ticket filed; proof saved" 2>/dev/null || true
      else
        rc=$?
        {
          echo "ticket worker failed (rc=$rc); last lines:"
          tail -8 "$SESSLOG"
        } >"$OUTDIR/$SESSION_ID.ticket.failed"
        touch "$OUTDIR/$SESSION_ID.ticket.done"
      fi
    } >"$SESSLOG" 2>&1
    echo "session $SESSION_ID: ticket worker finished (see $SESSION_ID.ticket.worker.log)" >>"$OUTDIR/ticket-workers.log" 2>&1
    rm -f "$OUTDIR/$SESSION_ID.ticket.started"
  ) >>"$OUTDIR/ticket-workers.log" 2>&1 &
  echo $! >"$OUTDIR/$SESSION_ID.ticket.started"
  disown
}

# A proof nobody has been told about yet. One-shot: the caller marks it
# reported after relaying it. The next user prompt after the filing carries
# the ticket id, no file-touching chore.
unreported_ticket_proof() {
  [ -f "$OUTDIR/$SESSION_ID.ticket.reported" ] && return 1
  [ -f "$OUTDIR/$SESSION_ID.ticket.proof.json" ] || return 1
  echo "$OUTDIR/$SESSION_ID.ticket.proof.json"
}

ticket_summary() {
  local proof="$1" line
  line=$(jq -r '"\(.idReadable // "?"): \(.summary // "ticket filed")"' \
    "$proof" 2>/dev/null) || line="ticket filed (unreadable proof)"
  echo "$line -- proof: $proof"
}

if [ "$EVENT" = "UserPromptSubmit" ]; then
  # The ping: a proof nobody has been told about yet rides on the next
  # prompt, whatever it says. One-shot via .ticket.reported.
  PING=$(unreported_ticket_proof) || true
  if [ -n "$PING" ]; then
    echo "TICKET FILED: $(ticket_summary "$PING"). Do not file anything yourself."
    touch "$OUTDIR/$SESSION_ID.ticket.reported"
  fi

  PROMPT=$(echo "$INPUT" | jq -r '.prompt // empty' 2>/dev/null |
           tr '[:upper:]' '[:lower:]')

  # Said outright: "file the ticket", "file the ticket in youtrack",
  # "file the youtrack ticket", "create a youtrack issue".
  #
  # A verb and an object, matched separately -- the same shape as the review
  # gate's intent check. Literals only, no bracket ranges: a range over
  # multibyte characters does not survive the locale this hook runs under.
  INTENT=""
  if echo "$PROMPT" | grep -qE 'file|create|open|submit|raise|post|заведи|завести|создай|открыть|открой' &&
     echo "$PROMPT" | grep -qE 'ticket|issue|youtrack|mvp-|тикет|задач'; then
    INTENT=1
  fi

  # "do not file yet" names the action and forbids it. Diverting on it would
  # start the worker against an explicit refusal, which is the one outcome
  # worse than not diverting at all.
  echo "$PROMPT" | grep -qE "^(no|nope|not? |don'?t|do not|stop|wait|hold|not yet|нет|не |не надо|погоди|стоп)" &&
    INTENT=""

  if [ -n "$INTENT" ]; then
    ST=$(ticket_state)
    # Diverted but no live worker is a crash, not a running worker. Saying
    # "already running" here would wedge the session: every later prompt
    # gets the same answer while nothing runs. Restart instead. (This is
    # also the crash backstop -- there is no Stop handler by design.)
    if [ "$ST" = "running" ] && ! ticket_alive; then ST="idle"; fi
    case $ST in
      done)
        PROOF="$OUTDIR/$SESSION_ID.ticket.proof.json"
        if [ -f "$PROOF" ]; then
          echo "TICKET ALREADY FILED: $(ticket_summary "$PROOF"). Nothing further to do; do not file anything yourself."
        else
          echo "TICKET DONE, NO PROOF: inconsistent state; say 'file the ticket' again to retry."
          rm -f "$OUTDIR/$SESSION_ID.ticket.diverted" "$OUTDIR/$SESSION_ID.ticket.done" \
            "$OUTDIR/$SESSION_ID.ticket.started"
        fi
        ;;
      failed)
        echo "TICKET WORKER FAILED. Reason: $(head -3 "$OUTDIR/$SESSION_ID.ticket.failed"). Say 'file the ticket' again to retry."
        rm -f "$OUTDIR/$SESSION_ID.ticket.diverted" "$OUTDIR/$SESSION_ID.ticket.done" \
          "$OUTDIR/$SESSION_ID.ticket.failed" "$OUTDIR/$SESSION_ID.ticket.started"
        ;;
      running)
        echo "TICKET WORKER ALREADY RUNNING. It files the ticket by itself, then pings this session with the ticket id; do not call the YouTrack API yourself."
        ;;
      idle)
        if [ -z "$YOUTRACK_TOKEN" ]; then
          echo "TICKET DIVERT NOT STARTED: YOUTRACK_TOKEN is not set in this session's environment. Export it and say 'file the ticket' again. The worker reads the bearer from that variable at runtime and never writes it anywhere."
        elif spawn_ticket_worker; then
          echo "TICKET DIVERTED. Worker started: it files the YouTrack ticket from the last 10 turns of this session, then pings with the ticket id. Do not call the YouTrack API yourself; do not compose the summary."
        else
          echo "TICKET DIVERT NOT STARTED: $(head -3 "$OUTDIR/$SESSION_ID.ticket.failed"). Fix it and say 'file the ticket' again."
        fi
        ;;
    esac
  fi
  exit 0
fi

# ── PreToolUse: divert the model composing the filing call itself ──
TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null)
if [ "$TOOL" = "Bash" ]; then
  CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
  LOW=$(echo "$CMD" | tr '[:upper:]' '[:lower:]')
  # Subject AND shape, never shape alone. `curl -X POST` to something else is
  # not ticket filing, and a heredoc about anything else is just a heredoc.
  SUBJECT=""
  echo "$LOW" | grep -qE 'ai-youtrack|youtrack|api/issues' && SUBJECT=1

  WRITES=""
  # Transport verbs, not serialization calls: -X/--data is how a write leaves
  # the machine. requests.post is the python equivalent. The filing flow
  # goes through the skill's ai-youtrack CLI, so its mutating subcommands
  # (create/publish/update/...) count as writes too -- while get/search/
  # list/types/help stay reads.
  echo "$LOW" | grep -qE '\-x (post|put|patch)|--data|requests\.post' && WRITES=1
  echo "$LOW" | grep -qE 'ai-youtrack +(create|publish|update|add|remove|set)' && WRITES=1

  COMPOSES=""
  # A heredoc building the issue body: summary/description/project keys are
  # what a filing payload carries, wherever it is assembled.
  case "$LOW" in
    *'<<'*)
      echo "$LOW" | grep -qE '"summary"|"description"|"project"' && COMPOSES=1 ;;
  esac

  if [ -n "$SUBJECT" ] && { [ -n "$WRITES" ] || [ -n "$COMPOSES" ]; }; then
    spawn_ticket_worker >/dev/null 2>&1
    echo "TICKET WRITE DIVERTED: the ticket worker files by itself from the session turns, then pings with the ticket id. Do not compose or send the API call yourself."
    exit 2
  fi
  exit 0
fi
exit 0

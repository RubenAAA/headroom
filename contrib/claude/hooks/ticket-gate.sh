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
CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
[ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ] && exit 0
OUTDIR="$HOME/.local/state/spark-review"
TICKET_WORKER="${HEADROOM_REPO:-$HOME/headroom}/contrib/spark-poster/ticket_file.py"
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
        "$OUTDIR/$SESSION_ID.ticket.started" "$OUTDIR/$SESSION_ID.ticket.project"
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
  # The worker needs all three; spawning with a token but no URL/project only
  # produces a doomed worker while still blocking the manual call (exit 2).
  # Refuse here with the same message the worker would fail with.
  if [ -z "$YOUTRACK_URL" ]; then
    echo "ticket filing is not configured: set YOUTRACK_URL in ~/.config/spark-poster/env (exported into the hook/worker environment alongside YOUTRACK_TOKEN)" >"$OUTDIR/$SESSION_ID.ticket.failed"
    return 1
  fi
  # Resolve the project for this session cwd against the operator's
  # project map (YOUTRACK_PROJECT_MAP="prefix=id,...", longest prefix wins;
  # pairs with an empty side are skipped). The worker never routes: it reads
  # the `.ticket.project` this writes. A set YOUTRACK_PROJECT_ID
  # still wins (single-project setups keep working); the map covers the
  # multi-project case with no global default. No personal paths live here --
  # the table is operator config in ~/.config/spark-poster/env.
  route_project() {
    local cwd="$1" map="$2" best="" best_len=-1 pair prefix pid
    local old_ifs="$IFS"
    IFS=','
    # shellcheck disable=SC2162
    for pair in $map; do
      case "$pair" in *=*) : ;; *) continue ;; esac
      prefix="${pair%%=*}"; pid="${pair#*=}"
      # trim spaces without spawning: parameter expansion only
      prefix="${prefix#"${prefix%%[! ]*}"}"; prefix="${prefix%"${prefix##*[! ]}"}"
      pid="${pid#"${pid%%[! ]*}"}"; pid="${pid%"${pid##*[! ]}"}"
      # An empty prefix would match every cwd.
      if [ -z "$prefix" ] || [ -z "$pid" ]; then continue; fi
      case "$cwd" in
        "$prefix"*)
          if [ "${#prefix}" -gt "$best_len" ]; then
            best="$pid"; best_len="${#prefix}"
          fi
          ;;
      esac
    done
    IFS="$old_ifs"
    echo "$best"
  }
  if [ -z "$YOUTRACK_PROJECT_ID" ] && [ -n "$YOUTRACK_PROJECT_MAP" ] && [ -n "$CWD" ]; then
    YOUTRACK_PROJECT_ID="$(route_project "$CWD" "$YOUTRACK_PROJECT_MAP")"
  fi
  if [ -z "$YOUTRACK_PROJECT_ID" ]; then
    echo "ticket filing is not configured: set YOUTRACK_PROJECT_ID (or YOUTRACK_PROJECT_MAP=\"prefix=id,...\" for cwd-based routing) in ~/.config/spark-poster/env (exported into the hook/worker environment alongside YOUTRACK_TOKEN)" >"$OUTDIR/$SESSION_ID.ticket.failed"
    return 1
  fi
  touch "$OUTDIR/$SESSION_ID.ticket.diverted"
  # The cwd routing above runs inside spawn too (UserPromptSubmit and
  # PreToolUse both pass .cwd), so persist the resolved project next to the
  # divert marker: the second watcher (UserPromptSubmit relay on the next
  # prompt) reads this file to say which project the ticket lands in, without
  # re-deriving it from a cwd it may no longer see.
  echo "$YOUTRACK_PROJECT_ID" >"$OUTDIR/$SESSION_ID.ticket.project"
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
          if [ $rc -eq 124 ]; then
            echo "ticket worker timed out after 600s (still running or hung); last lines:"
          else
            echo "ticket worker failed (rc=$rc); last lines:"
          fi
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

# A configuration gap is said once per session: it does not change between
# prompts, and repeating it turns every later matching prompt into noise.
warn_unconfigured() {
  [ -f "$OUTDIR/$SESSION_ID.ticket.unconfigured-warned" ] && return 0
  touch "$OUTDIR/$SESSION_ID.ticket.unconfigured-warned"
  echo "$1"
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
  # A verb, then its object within two words of the same clause, on one
  # line, both whole words. Matched anywhere as bare substrings, a code
  # review that named `ticket_file.py` was a filing request ("file" +
  # "ticket"), and with YouTrack configured it would have filed a real
  # ticket. The words between carry no clause punctuation, so "post the
  # threads; ticket MVP-12 ..." is not a filing. No bracket ranges over
  # letters: a range over multibyte characters does not survive the locale
  # this hook runs under. Classes and ASCII punctuation do.
  #
  # review-gate.sh stands down on this exact pattern; check-drift.sh fails
  # if the two copies differ.
  INTENT=""
  TICKET_INTENT='(^|[^[:alnum:]_./-])(file|create|open|submit|raise|post|заведи|завести|создай|открыть|открой)( +[^ .;:!?,]+){0,2} +((tickets?|issues?|youtrack|mvp-[0-9]+)([^[:alnum:]_./-]|[.]([^[:alnum:]]|$)|$)|тикет|задач)'
  if echo "$PROMPT" | grep -qE "$TICKET_INTENT"; then
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
            "$OUTDIR/$SESSION_ID.ticket.started" "$OUTDIR/$SESSION_ID.ticket.project"
        fi
        ;;
      failed)
        FAILED_MSG=$(head -3 "$OUTDIR/$SESSION_ID.ticket.failed")
        case "$FAILED_MSG" in
          *"timed out"*) KIND="TICKET WORKER TIMED OUT" ;;
          *) KIND="TICKET WORKER FAILED" ;;
        esac
        echo "$KIND. Reason: $FAILED_MSG. Say 'file the ticket' again to retry."
        rm -f "$OUTDIR/$SESSION_ID.ticket.diverted" "$OUTDIR/$SESSION_ID.ticket.done" \
          "$OUTDIR/$SESSION_ID.ticket.failed" "$OUTDIR/$SESSION_ID.ticket.started" \
          "$OUTDIR/$SESSION_ID.ticket.project"
        ;;
      running)
        PROJ_MSG=""
        [ -f "$OUTDIR/$SESSION_ID.ticket.project" ] && PROJ_MSG=" (project $(cat "$OUTDIR/$SESSION_ID.ticket.project" 2>/dev/null))."
        echo "TICKET WORKER ALREADY RUNNING.$PROJ_MSG It files the ticket by itself, then pings this session with the ticket id; do not call the YouTrack API yourself."
        ;;
      idle)
        if [ -z "$YOUTRACK_TOKEN" ]; then
          warn_unconfigured "TICKET DIVERT NOT STARTED: YOUTRACK_TOKEN is not set in this session's environment. Export it and say 'file the ticket' again. The worker reads the bearer from that variable at runtime and never writes it anywhere."
        elif spawn_ticket_worker; then
          PROJ_MSG=""
          [ -f "$OUTDIR/$SESSION_ID.ticket.project" ] && PROJ_MSG=" Project: $(cat "$OUTDIR/$SESSION_ID.ticket.project" 2>/dev/null)."
          echo "TICKET DIVERTED. Worker started: it files the YouTrack ticket from the last 10 turns of this session, then pings with the ticket id.$PROJ_MSG Do not call the YouTrack API yourself; do not compose the summary."
        else
          warn_unconfigured "TICKET DIVERT NOT STARTED: $(head -3 "$OUTDIR/$SESSION_ID.ticket.failed"). Fix it and say 'file the ticket' again."
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
  # Existing-issue work is never filing: state moves, comments, links,
  # worklogs, and field updates all name an issue that already exists --
  # either as an ai-youtrack subcommand or as an /issues/<id> URL. Let them
  # through before any divert check. (Without this, POST /api/issues/<id>
  # with a State payload matches the filing shape below and gets diverted,
  # then fails in the worker that only knows how to file new tickets.)
  if echo "$LOW" | grep -qE 'ai-youtrack(\.py)? +(update-|add-|remove-|delete-)'; then
    exit 0
  fi
  # Bare POST /api/issues (create) and POST /issues?draftId= (publish) carry
  # no issue suffix and still fall through to the divert check.
  if echo "$LOW" | grep -qE 'api/issues/[a-z0-9_.-]+|/issue/[a-z]+-[0-9]+'; then
    exit 0
  fi
  # Subject AND shape, never shape alone. `curl -X POST` to something else is
  # not ticket filing, and a heredoc about anything else is just a heredoc.
  SUBJECT=""
  echo "$LOW" | grep -qE 'ai-youtrack|youtrack|api/issues' && SUBJECT=1

  WRITES=""
  # Transport verbs, not serialization calls: -X/--data is how a write leaves
  # the machine. requests.post is the python equivalent. The filing flow
  # goes through the skill's ai-youtrack CLI, so only its filing subcommands
  # (create-issue/create-draft/publish-draft) count as writes here --
  # update-*/add-*/remove-*/delete-* are routine field work on an existing
  # ticket (already exempted above) while get/search/list/types/help stay
  # reads.
  echo "$LOW" | grep -qE '\-x (post|put|patch)|--data|requests\.post' && WRITES=1
  echo "$LOW" | grep -qE 'ai-youtrack(\.py)? +(create|publish)' && WRITES=1

  COMPOSES=""
  # A heredoc building the issue body: summary/description/project keys are
  # what a filing payload carries, wherever it is assembled.
  case "$LOW" in
    *'<<'*)
      echo "$LOW" | grep -qE '"summary"|"description"|"project"' && COMPOSES=1 ;;
  esac

  if [ -n "$SUBJECT" ] && { [ -n "$WRITES" ] || [ -n "$COMPOSES" ]; }; then
    # A worker that cannot run must not brick the manual call: the spawn
    # refuses (reason in .ticket.failed) when token/URL/project are missing,
    # and then the call goes through instead of exit 2.
    if spawn_ticket_worker >/dev/null 2>&1; then
      echo "TICKET WRITE DIVERTED: the ticket worker files by itself from the session turns, then pings with the ticket id. Do not compose or send the API call yourself."
      exit 2
    else
      echo "TICKET WORKER NOT STARTED: $(head -3 "$OUTDIR/$SESSION_ID.ticket.failed" 2>/dev/null). Proceeding with your own API call."
      exit 0
    fi
  fi
  exit 0
fi
exit 0

#!/bin/bash
# review-gate.sh: preempt review articulation, hand traces to spark worker.
# Serves PreToolUse (divert gate) and Stop (retry backstop). Never blocks
# benign work: exits 0 on every path except a diverted articulation attempt.
# Installed by install.sh into ~/.claude/hooks (copied, or symlinked with --link).
[ -n "$SPARK_REVIEW_WORKER" ] && exit 0
INPUT=$(cat)
EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ] && exit 0
OUTDIR="$HOME"/.local/state/spark-review
mkdir -p "$OUTDIR" 2>/dev/null

# ── armed? one of the review commands was actually run in this session ──
#
# The invocation has to be a real one. A plain grep over the transcript arms on
# any line that merely contains the command's name, and three kinds of line do:
# tool output from reading this file, the assistant's own Bash command text,
# and a compaction summary describing how arming works. All three armed a
# session where nobody ran the command, and an armed session diverts.
#
# So look at where the string sits. Claude Code writes a slash command as a
# user message that OPENS with the command wrapper; prose that discusses one
# has it somewhere in the middle, and tool results carry `toolUseResult`.
# Position and provenance separate the invocation from every mention of it.
# grep narrows to candidate lines, then ONE jq decides. Per-line jq would spawn
# a process for every mention, and mentions are exactly what a long review
# session accumulates. `fromjson?` drops anything that will not parse rather
# than failing the whole check on one bad line.
REVIEW_COMMANDS='gitlab-review|fix-mr-comments'

armed_by_invocation() {
  [ -f "$OUTDIR/$SESSION_ID.armed" ] && return 0
  grep -E "command-name>/($REVIEW_COMMANDS)" "$TRANSCRIPT" 2>/dev/null |
  jq -e -R -s --arg cmds "$REVIEW_COMMANDS" '
    split("\n") | map(select(length > 0) | fromjson?)
    | any(.[];
        .toolUseResult == null
        and (.isSidechain != true)
        and .message.role == "user"
        and ((.message.content
              | if type == "array"
                then (map(select(.type == "text") | .text // "") | join("\n"))
                else tostring end)
             | test("^\\s*<command-(message|name)>")
               and test("<command-name>/(" + $cmds + ")</command-name>")))
   ' >/dev/null 2>&1 || return 1
  arm_mode >"$OUTDIR/$SESSION_ID.armed"
}

mr_in_transcript() {
  grep -oE 'merge_requests/[0-9]+|MR![0-9]+|!\[0-9]+' "$TRANSCRIPT" 2>/dev/null |
    grep -oE '[0-9]+' | tail -1
}

POSTER="$HOME"/headroom/contrib/spark-poster

# ── which review command armed this session ──
#
# The scope depends on it: /gitlab-review drafts follow-ups on threads I
# opened, /fix-mr-comments drafts answers to reviewers' open threads on my
# own MR. Arming itself stays strict (armed_by_invocation); this only picks
# the scope once armed, so a loose last-mention scan is enough here.
arm_mode() {
  local m
  if [ -s "$OUTDIR/$SESSION_ID.armed" ]; then
    m=$(cat "$OUTDIR/$SESSION_ID.armed" 2>/dev/null)
    case "$m" in
      gitlab-review|fix-mr-comments) echo "$m"; return 0 ;;
    esac
  fi
  m=$(grep -oE "command-name>/($REVIEW_COMMANDS)" "$TRANSCRIPT" 2>/dev/null |
      grep -oE "($REVIEW_COMMANDS)" | tail -1)
  [ -n "$m" ] && echo "$m" || echo "gitlab-review"
}

# done = worker finished WITH a draft ready (listener chained, see below).
# done + .failed = worker finished with NO draft; the reason is in .failed.
# diverted without done = worker still running (or crashed; worker_alive says).
worker_state() {
  if [ -f "$OUTDIR/$SESSION_ID.done" ]; then
    if [ -f "$OUTDIR/$SESSION_ID.failed" ]; then echo "failed"; else echo "done"; fi
  elif [ -f "$OUTDIR/$SESSION_ID.diverted" ]; then
    echo "running"
  else
    echo "idle"
  fi
}

worker_alive() {
  local pid
  pid=$(cat "$OUTDIR/$SESSION_ID.started" 2>/dev/null)
  [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null || return 1
  # kill -0 alone trusts a recycled pid. The supervisor is a fork of this
  # script, so its command line still names it; anything else behind that
  # pid is not our worker. /proc is Linux-only -- elsewhere, kill -0 stands.
  [ -r "/proc/$pid/cmdline" ] || return 0
  tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q "review-gate"
}

spawn_worker() {
  [ -f "$OUTDIR/$SESSION_ID.done" ] && return 0
  worker_alive && return 0
  # A stale .diverted with no live worker is a previous crash, not a running
  # worker: drop it so this attempt is a real one.
  rm -f "$OUTDIR/$SESSION_ID.diverted"

  # The MR number is the only thing taken from the transcript. Everything the
  # drafter reasons about it fetches for itself.
  #
  # This used to scrape the assistant's own analysis out of the transcript and
  # ask a worker to reshape it into comments -- which needed the analysis to
  # exist before it ran, so it moved formatting off the reviewing model and
  # left every expensive part where it was. spark_draft.py pulls the threads
  # and the git history behind them, and does the deciding.
  #
  # Checked BEFORE touching .diverted: a divert marker with no worker behind
  # it reads as "drafting" to every later check, which is how an approval can
  # name a worker that never existed.
  MR=$(mr_in_transcript)
  if [ -z "$MR" ]; then
    echo "no MR number in transcript; not drafting" >>"$OUTDIR/worker.log"
    return 1
  fi
  MODE=$(arm_mode)
  touch "$OUTDIR/$SESSION_ID.diverted"
  # A go-ahead approves the draft it was given for. A stale one must not
  # auto-post a draft that lands later, so every fresh run starts clean.
  rm -f "$OUTDIR/$SESSION_ID.goahead" "$OUTDIR/$SESSION_ID.failed"

  (
    SESSLOG="$OUTDIR/$SESSION_ID.worker.log"
    {
      echo "=== worker start: MR !$MR mode $MODE ==="
      if SPARK_REVIEW_WORKER=1 timeout 900 \
          python3 "$POSTER/spark_draft.py" "$MR" "$SESSION_ID" "$MODE"; then
        DRAFT="$OUTDIR/$SESSION_ID.draft.json"
        if [ -f "$DRAFT" ]; then
          touch "$OUTDIR/$SESSION_ID.done"
          # Chain the listener HERE. Nothing else in the system launches
          # spark-goahead.sh, and without this the go-ahead file is an
          # approval nobody hears -- every session before this change ended
          # exactly that way: "touch the go-ahead" followed by silence.
          "$POSTER/spark-goahead.sh" "$SESSION_ID" >>"$OUTDIR/worker.log" 2>&1 &
        else
          echo "drafter exited 0 but no draft at $DRAFT" >"$OUTDIR/$SESSION_ID.failed"
          touch "$OUTDIR/$SESSION_ID.done"
        fi
      else
        rc=$?
        {
          echo "worker failed (rc=$rc) for MR !$MR mode $MODE; last lines:"
          tail -8 "$SESSLOG"
        } >"$OUTDIR/$SESSION_ID.failed"
        touch "$OUTDIR/$SESSION_ID.done"
      fi
    } >"$SESSLOG" 2>&1
    echo "session $SESSION_ID: MR !$MR $MODE finished (see $SESSION_ID.worker.log)" >>"$OUTDIR/worker.log"
    rm -f "$OUTDIR/$SESSION_ID.started"
  ) >>"$OUTDIR/worker.log" 2>&1 &
  echo $! >"$OUTDIR/$SESSION_ID.started"
  disown
}

# The listener dies with its timeout or its post; if the draft outlives it
# (timeout, crash, a session from before the chaining fix), start another one.
# spark-goahead.sh refuses to post twice when a proof exists, so relaunching
# it is always safe.
ensure_listener() {
  [ -f "$OUTDIR/$SESSION_ID.draft.json" ] || return 0
  command -v pgrep >/dev/null 2>&1 || return 0
  pgrep -f "spark-goahead.sh $SESSION_ID" >/dev/null 2>&1 && return 0
  "$POSTER"/spark-goahead.sh "$SESSION_ID" >>"$OUTDIR/worker.log" 2>&1 &
  disown
}

draft_stats() {
  jq -r '"\(.replies | length) replies, \([.replies[] | select(.resolve)] | length) to close, MR !\(.iid)"' \
    "$OUTDIR/$SESSION_ID.draft.json" 2>/dev/null || echo "unreadable draft"
}

proof_for_draft() {
  local sid
  sid=$(jq -r '.session_id // empty' "$OUTDIR/$SESSION_ID.draft.json" 2>/dev/null)
  [ -n "$sid" ] && echo "$OUTDIR/$sid.proof.json"
}

# ── UserPromptSubmit: the approval IS the divert ──
#
# This is the moment that matters. The model asks "shall I reply on those
# threads?", the answer is yes, and everything expensive happens next: reading
# the diffs, weighing each objection, writing eleven paragraphs. A gate on the
# Write tool fires after all of that is already spent and can only stop the
# file from landing.
#
# So the yes routes straight to the worker. The model is told the drafting is
# not its job before it starts, rather than after it finishes.
#
# Two conditions, both required. The session must have entered a review through
# /gitlab-review or /fix-mr-comments, and the user must now be asking for the
# threads to be answered. Arming alone would divert every "yes" in a long
# review; the instruction alone would fire in any session that mentions an MR.
if [ "$EVENT" = "UserPromptSubmit" ]; then
  armed_by_invocation || exit 0

  PROMPT=$(echo "$INPUT" | jq -r '.prompt // empty' 2>/dev/null |
           tr '[:upper:]' '[:lower:]')

  # Said outright: "post the threads", "answer the comments", "запости ответы".
  # Length is not a filter here -- an instruction that names the action is an
  # instruction however it is phrased.
  #
  # A verb and an object, matched separately. One combined pattern needed a
  # Cyrillic bracket range to skip the words between them, and a range over
  # multibyte characters does not survive the locale this hook runs under -- it
  # silently matched nothing, so every Russian instruction and "post the
  # threads" itself went through. Two greps need no ranges at all.
  INTENT=""
  if echo "$PROMPT" | grep -qE 'post|repl|answer|respond|resolve|close|запост|ответ|отвеч|закр' &&
     echo "$PROMPT" | grep -qE 'thread|comment|note|discussion|review|mr|тред|коммент|ветк|замечан|ответ'; then
    INTENT=1
  fi

  # "do not post the threads yet" names the action and forbids it. Diverting on
  # it would start the worker against an explicit refusal, which is the one
  # outcome worse than not diverting at all.
  echo "$PROMPT" | grep -qE "^(no|nope|not? |don'?t|do not|stop|wait|hold|нет|не |стоп|погоди)" &&
    INTENT=""

  # Or said as a yes to the model's own question about posting.
  if [ -z "$INTENT" ]; then
    BARE=$(echo "$PROMPT" | tr -d '[:punct:]' | tr -s ' ' | sed 's/^ *//;s/ *$//')
    if echo "$BARE" | grep -qxE '(yes|y|yep|yeah|yup|ok|okay|sure|go|go ahead|do it|please do|post it|send it|post|go for it|да|ага|давай|давай да|запости|отвечай|ответь)'; then
      ASKED=$(tail -c 200000 "$TRANSCRIPT" 2>/dev/null | jq -R -s '
        split("\n") | map(select(length > 0) | fromjson?)
        | map(select(.message.role == "assistant" and (.isSidechain != true)))
        | last
        | (.message.content // [] | map(select(.type == "text") | .text // "") | join("\n"))
        // ""' 2>/dev/null | tr '[:upper:]' '[:lower:]')
      echo "$ASKED" | grep -qE '\?' &&
      echo "$ASKED" | grep -qE 'post|repl|answer|comment|resolv|close .*thread|отвеч|запост|закрыв' &&
        INTENT=1
    fi
  fi

  if [ -n "$INTENT" ]; then
    # State-aware, and every word of it checkable: the old message said
    # "the worker is drafting" on every path, including no-MR (no worker),
    # finished-with-no-draft (no worker any more), and already-posted. The
    # model relayed it as fact, the user approved into a void.
    GO="$OUTDIR/$SESSION_ID.goahead"
    MR=$(mr_in_transcript)
    ST=$(worker_state)
    # Diverted but no live worker is a crash, not a running worker. Saying
    # "already running" here would wedge the session: every later prompt
    # gets the same answer while nothing runs. Restart instead.
    if [ "$ST" = "running" ] && ! worker_alive; then ST="idle"; fi
    case $ST in
      done)
        PROOF=$(proof_for_draft)
        if [ -n "$PROOF" ] && [ -f "$PROOF" ]; then
          echo "REVIEW REPLIES POSTED AND VERIFIED. Proof: $PROOF. Nothing further to do; do not post anything yourself."
        else
          ensure_listener
          if [ -f "$GO" ]; then
            echo "REVIEW GO-AHEAD RECORDED. The poster is working through the draft; the proof lands next to the draft when it verifies. Do not post anything yourself."
          else
            echo "REVIEW DRAFT READY: $(draft_stats). It posts ONLY on your go-ahead, exactly once: touch $GO -- that file is the approval, nothing else is. Do NOT write the reply text, do NOT draft it in a file, do NOT post anything yourself."
          fi
        fi
        ;;
      failed)
        echo "REVIEW WORKER FINISHED WITH NO DRAFT. Reason: $(head -3 "$OUTDIR/$SESSION_ID.failed" 2>/dev/null | tr '\n' ' '). Say 'post the threads' again to run it once more, or leave the threads. Do not draft replies yourself."
        rm -f "$OUTDIR/$SESSION_ID.diverted" "$OUTDIR/$SESSION_ID.done" \
          "$OUTDIR/$SESSION_ID.failed" "$OUTDIR/$SESSION_ID.started"
        ;;
      running)
        echo "REVIEW WORKER ALREADY RUNNING for MR !${MR:-unknown}. It drafts, then waits for your go-ahead: touch $GO -- nothing posts before that file exists. Do NOT write the reply text, do NOT post anything yourself."
        ;;
      idle)
        if [ -n "$MR" ]; then
          spawn_worker
          echo "REVIEW DIVERTED. Worker started for MR !$MR ($(arm_mode)): it reads the threads and the commits itself, drafts the replies, then waits. It posts ONLY on your go-ahead: touch $GO. Do NOT write the reply text, do NOT draft it in a file, do NOT post anything yourself."
        else
          echo "REVIEW DIVERT NOT STARTED: no merge_requests/NNN number in this session's transcript, so no worker was launched. Name the MR (e.g. !554) and repeat the instruction."
          # A .diverted marker alongside no MR is a phantom -- nothing is
          # behind it and nothing ever will be. Leaving it makes every Stop
          # backstop log another "no MR number" line, forever.
          rm -f "$OUTDIR/$SESSION_ID.diverted" "$OUTDIR/$SESSION_ID.started"
        fi
        ;;
    esac
  fi
  exit 0
fi

# ── everything below is the write-time backstop; it needs the session armed ──
armed_by_invocation || exit 0

if [ "$EVENT" = "Stop" ]; then
  # Backstop only: retry if a divert fired but no worker finished. spawn_worker
  # no-ops when one is alive or done; it respawns only after a crash.
  if [ -f "$OUTDIR/$SESSION_ID.diverted" ] && [ ! -f "$OUTDIR/$SESSION_ID.done" ]; then
    spawn_worker
  fi
  exit 0
fi

# ── PreToolUse: divert articulation-shaped actions ──
TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null)
if [ "$TOOL" = "Bash" ]; then
  CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
  LOW=$(echo "$CMD" | tr '[:upper:]' '[:lower:]')
  # Subject AND shape, never shape alone. A heredoc is how you write a review
  # comment and also how you write everything else; blocking every `<<` stopped
  # a commit whose message said "json.load" and two diagnostics that were
  # reading an auth file. Each of those cost a round trip and taught nothing.
  #
  # A command earns a divert when it concerns a tracker or a draft AND either
  # writes to one or composes text for one. Either half on its own is ordinary
  # work: `curl -X POST` to something else is not review articulation, and a
  # heredoc about anything else is just a heredoc.
  SUBJECT=""
  echo "$LOW" | grep -qE 'gitlab|youtrack|merge_requests|api/v4|discussion_id|spark_post' && SUBJECT=1

  WRITES=""
  # Transport verbs, not serialization calls. curl's -X/--data and glab's
  # note/comment are how a write leaves the machine; requests.post/put/patch
  # is the python equivalent. json.dump(s) is deliberately NOT here: every
  # observed hit on it was a read summary printed to stdout, and a POST
  # without any json call in the command (dict literal) walked past it
  # anyway -- the transport is the layer that cannot lie about direction.
  echo "$LOW" | grep -qE '\-x (post|put|patch)|--data|glab .*(note|comment)|requests\.(post|put|patch)' && WRITES=1

  COMPOSES=""
  # heredoc builds text; json.load only reads it, and json.dump(s) proved
  # the same (see WRITES above). Loading sat here since the start and every
  # one of its diverts was a read-summarize pipeline: the !597 session alone
  # diverted both `curl ... merge_requests/597 | python3 -c "...json.load..."`
  # (SUBJECT on api/v4) and `<sanctioned-script> > file; python3 -c
  # "...json.load(open(...))"` (SUBJECT on the project's own script path).
  # A load cannot articulate a review no matter what it reads.
  case "$LOW" in
    *'<<'*) COMPOSES=1 ;;
  esac

  ARTICULATE=""
  if [ -n "$SUBJECT" ] && { [ -n "$WRITES" ] || [ -n "$COMPOSES" ]; }; then
    ARTICULATE=1
  fi
  if [ -n "$ARTICULATE" ]; then
    spawn_worker
    echo "REVIEW WRITE DIVERTED: analysis captured from transcript; spark poster is drafting and will post after go-ahead. Do not compose comments, scripts, or drafts yourself — acknowledge briefly and wait." >&2
    exit 2
  fi
  exit 0
fi

case "$TOOL" in
  Write|Edit|MultiEdit)
    FP=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null)

    # Machinery, not drafts. These files necessarily contain the same words a
    # draft does -- discussion_id, body, resolve -- because they are what reads
    # and posts one. Matching them would wall the model off from its own tools.
    case "$FP" in
      */spark-poster/gitlab_api.py|*/spark-poster/spark_post.py \
      |*/spark-poster/triage.py|*/spark-poster/read_threads.py \
      |*/spark-poster/since_review.py|*/spark-poster/thread_dossier.py \
      |*/spark-poster/README.md|*/hooks/review-gate.sh)
        exit 0 ;;
    esac

    DIVERT=""
    # Where drafts live. The old matcher was three /tmp globs and a draft
    # written anywhere else -- a drafts/ dir in the repo, say -- walked
    # straight through it.
    case "$FP" in
      */spark-review/*|*/drafts/*|/tmp/mr*|/tmp/*post*|docs/mr-*) DIVERT=1 ;;
    esac

    # What a draft looks like, wherever it was written. Path rules only catch
    # the paths someone thought of; a reply body carrying a thread id is a
    # draft no matter which directory it lands in.
    if [ -z "$DIVERT" ]; then
      BODY=$(echo "$INPUT" | jq -r '
        [.tool_input.content // empty,
         .tool_input.new_string // empty,
         (.tool_input.edits // [] | map(.new_string // empty) | join("\n"))]
        | join("\n")' 2>/dev/null)
      if echo "$BODY" | grep -qE 'discussion_id' \
         && echo "$BODY" | grep -qiE '"body"|resolve|закрываю|verdict'; then
        DIVERT=1
      fi
    fi

    if [ -n "$DIVERT" ]; then
      spawn_worker
      echo "REVIEW DRAFT DIVERTED. Do not compose the verdicts yourself -- that is the expensive half and it is what the offload exists to move. Delegate the assessment to a subagent (Task tool): give it the thread list, the commit range and the repo path, and have it write the draft. Then wait for the go-ahead. Composing here and posting there saves nothing." >&2
      exit 2
    fi
    ;;
esac
exit 0

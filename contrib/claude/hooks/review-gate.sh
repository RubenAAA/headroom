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
  touch "$OUTDIR/$SESSION_ID.armed"
}

mr_in_transcript() {
  grep -oE 'merge_requests/[0-9]+|MR![0-9]+|!\[0-9]+' "$TRANSCRIPT" 2>/dev/null |
    grep -oE '[0-9]+' | tail -1
}

spawn_worker() {
  [ -f "$OUTDIR/$SESSION_ID.done" ] && return 0
  [ -f "$OUTDIR/$SESSION_ID.diverted" ] && return 0
  touch "$OUTDIR/$SESSION_ID.diverted"

  # The MR number is the only thing taken from the transcript. Everything the
  # drafter reasons about it fetches for itself.
  #
  # This used to scrape the assistant's own analysis out of the transcript and
  # ask a worker to reshape it into comments -- which needed the analysis to
  # exist before it ran, so it moved formatting off the reviewing model and
  # left every expensive part where it was. spark_draft.py pulls the threads
  # and the git history behind them, and does the deciding.
  MR=$(mr_in_transcript)
  [ -n "$MR" ] || { echo "no MR number in transcript; not drafting" >>"$OUTDIR/worker.log"; return 0; }

  (
    SPARK_REVIEW_WORKER=1 timeout 900 \
      python3 "$HOME"/headroom/contrib/spark-poster/spark_draft.py \
      "$MR" "$SESSION_ID"
    touch "$OUTDIR/$SESSION_ID.done"
  ) >>"$OUTDIR/worker.log" 2>&1 &
  disown
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
    spawn_worker
    echo "REVIEW REPLIES DIVERTED AT APPROVAL. The spark worker is drafting the thread replies from the MR and the repo; it reads the threads and the commits itself and must not be given your analysis. Do NOT write the reply text, do NOT draft it in a file, and do NOT summarise what you would have said. Tell the user the worker is drafting and that it posts on go-ahead: touch ~/.local/state/spark-review/<session>.goahead"
  fi
  exit 0
fi

# ── everything below is the write-time backstop; it needs the session armed ──
armed_by_invocation || exit 0

if [ "$EVENT" = "Stop" ]; then
  # Backstop only: retry if a divert fired but no draft landed.
  if [ -f "$OUTDIR/$SESSION_ID.diverted" ] && [ ! -f "$OUTDIR/$SESSION_ID.done" ]; then
    rm -f "$OUTDIR/$SESSION_ID.diverted"
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
  echo "$LOW" | grep -qE '\-x (post|put|patch)|--data|glab .*(note|comment)' && WRITES=1

  COMPOSES=""
  case "$LOW" in
    *'<<'*|*json.dump*|*json.load*) COMPOSES=1 ;;
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

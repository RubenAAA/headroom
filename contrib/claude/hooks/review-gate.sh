#!/bin/bash
# review-gate.sh: hand review drafting to the offload worker and require an
# explicit reviewed action before writing to GitLab. Serves UserPromptSubmit,
# PreToolUse and Stop; ordinary work passes through.
# Installed by install.sh into ~/.claude/hooks (copied, or symlinked with --link).
if [ -n "${OFFLOADED_REVIEW_WORKER:-}" ] || [ -n "${SPARK_REVIEW_WORKER:-}" ]; then
  exit 0
fi
INPUT=$(cat)
EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ] && exit 0
STATE_ROOT="$HOME/.local/state/offload-workers"
LEGACY_STATE_ROOT="$HOME/.local/state/spark-review"
if [ ! -e "$STATE_ROOT" ] && [ -d "$LEGACY_STATE_ROOT" ]; then
  ln -s spark-review "$STATE_ROOT" 2>/dev/null || true
fi
OUTDIR="$STATE_ROOT"
umask 077
mkdir -p "$OUTDIR" 2>/dev/null
chmod 700 "$OUTDIR" 2>/dev/null || true

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

# FINDING-046: honor HEADROOM_REPO like statusline-with-cache.sh and
# restart-headroom.sh do; $HOME/headroom is only the fallback.
WORKERS="$HOME/headroom/contrib/offload-workers"
[ -n "$HEADROOM_REPO" ] && WORKERS="$HEADROOM_REPO/contrib/offload-workers"

# The last actual slash-command invocation is the scope. Extract the MR only
# from that invocation's command args; scanning the whole transcript can pick
# up an unrelated !123 and tell the operator that another MR will be written.
command_invocation() {
  jq -r -R -s --arg cmds "$REVIEW_COMMANDS" '
    split("\n") | map(select(length > 0) | fromjson?)
    | map(select(.toolUseResult == null)
      | select(.isSidechain != true) | select(.message.role == "user")
      | ((.message.content
         | if type == "array" then
             map(select(.type == "text") | .text // "") | join("\n")
           else tostring end) as $text
      | select($text | test("^\\s*<command-(message|name)>"))
      | select($text | test("<command-name>/(" + $cmds + ")</command-name>"))
      | {text: $text, mode: (try ($text | capture("<command-name>/(?<mode>[^<]+)</command-name>").mode) catch ""),
         args: (try ($text | capture("<command-args>(?<args>[^<]*)</command-args>").args) catch ""),
         key: ((.uuid // .timestamp // $text) | tostring)}))
    | last // empty | [.mode, .args, .key] | join("\u001f")
  ' "$TRANSCRIPT" 2>/dev/null
}

mr_from_command_args() {
  local args="$1" matches
  matches=$(printf '%s\n' "$args" | grep -oE 'merge_requests/[0-9]+' | grep -oE '[0-9]+' | sort -u)
  if [ -z "$matches" ]; then
    matches=$(printf '%s\n' "$args" | grep -oE 'MR![0-9]+|![0-9]+' | grep -oE '[0-9]+' | sort -u)
  fi
  [ "$(printf '%s\n' "$matches" | wc -l)" -eq 1 ] || return 1
  printf '%s' "$matches"
}

arm_context() {
  local context mode args key mr old_key disarmed_key archive_key
  context=$(command_invocation)
  [ -n "$context" ] || return 1
  IFS=$'\x1f' read -r mode args key <<<"$context"
  case "$mode" in gitlab-review|fix-mr-comments) ;; *) return 1 ;; esac
  disarmed_key=$(cat "$OUTDIR/$SESSION_ID.disarmed-key" 2>/dev/null)
  [ "$key" != "$disarmed_key" ] || return 1
  old_key=$(cat "$OUTDIR/$SESSION_ID.armed-key" 2>/dev/null)
  [ "$key" = "$old_key" ] && return 0
  if worker_alive || poster_alive; then
    echo "REVIEW COMMAND WAITING: the current worker/poster for MR !$(armed_mr) is still active. This command's MR args are pinned after that process ends; no state was replaced."
    return 2
  fi
  if [ -n "$old_key" ]; then
    archive_key=$(python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest()[:12])' <<<"$old_key")
  elif [ -f "$OUTDIR/$SESSION_ID.notes.md" ] \
       || [ -f "$OUTDIR/$SESSION_ID.draft.json" ] \
       || [ -f "$OUTDIR/$SESSION_ID.proof.json" ]; then
    archive_key="legacy-$(date +%s)"
  fi
  if [ -n "$archive_key" ]; then
    [ ! -f "$OUTDIR/$SESSION_ID.notes.md" ] || \
      mv "$OUTDIR/$SESSION_ID.notes.md" "$OUTDIR/$SESSION_ID.notes.$archive_key.md"
    [ ! -f "$OUTDIR/$SESSION_ID.draft.json" ] || \
      mv "$OUTDIR/$SESSION_ID.draft.json" "$OUTDIR/$SESSION_ID.draft.$archive_key.json"
    [ ! -f "$OUTDIR/$SESSION_ID.proof.json" ] || \
      mv "$OUTDIR/$SESSION_ID.proof.json" "$OUTDIR/$SESSION_ID.proof.$archive_key.json"
  fi
  mr=$(mr_from_command_args "$args")
  rm -f "$OUTDIR/$SESSION_ID.diverted" "$OUTDIR/$SESSION_ID.drafting" \
    "$OUTDIR/$SESSION_ID.drafted" "$OUTDIR/$SESSION_ID.posting" \
    "$OUTDIR/$SESSION_ID.posted" "$OUTDIR/$SESSION_ID.failed" \
    "$OUTDIR/$SESSION_ID.reported" "$OUTDIR/$SESSION_ID.started" \
    "$OUTDIR/$SESSION_ID.draft-reported" "$OUTDIR/$SESSION_ID.done"
  printf '%s' "$mode" >"$OUTDIR/$SESSION_ID.armed"
  printf '%s' "$mr" >"$OUTDIR/$SESSION_ID.armed-mr"
  printf '%s' "$key" >"$OUTDIR/$SESSION_ID.armed-key"
}

arm_mode() { cat "$OUTDIR/$SESSION_ID.armed" 2>/dev/null; }
armed_mr() { cat "$OUTDIR/$SESSION_ID.armed-mr" 2>/dev/null; }
armed_by_invocation() { [ -s "$OUTDIR/$SESSION_ID.armed" ] && [ -s "$OUTDIR/$SESSION_ID.armed-key" ]; }

poster_alive() {
  local pid
  pid=$(cat "$OUTDIR/$SESSION_ID.posting" 2>/dev/null)
  [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null || return 1
  [ -r "/proc/$pid/cmdline" ] || return 0
  tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q 'review_post.py'
}

# State names follow the actual hand-off phases. Legacy .done files are read
# only so sessions created by the old hook still report their draft correctly.
worker_state() {
  if [ -f "$OUTDIR/$SESSION_ID.failed" ]; then echo "failed"
  elif [ -f "$OUTDIR/$SESSION_ID.posted" ] || { [ -f "$OUTDIR/$SESSION_ID.proof.json" ] && jq -e '.ok == true' "$OUTDIR/$SESSION_ID.proof.json" >/dev/null 2>&1; }; then echo "posted"
  elif [ -f "$OUTDIR/$SESSION_ID.proof.json" ]; then echo "failed"
  elif [ -f "$OUTDIR/$SESSION_ID.posting" ]; then
    if poster_alive; then echo "posting"
    else
      echo "poster process ended before recording a result" >"$OUTDIR/$SESSION_ID.failed"
      rm -f "$OUTDIR/$SESSION_ID.posting"
      echo "failed"
    fi
  elif [ -f "$OUTDIR/$SESSION_ID.drafted" ] || { [ -f "$OUTDIR/$SESSION_ID.done" ] && [ -f "$OUTDIR/$SESSION_ID.draft.json" ]; }; then echo "drafted"
  elif [ -f "$OUTDIR/$SESSION_ID.drafting" ] || [ -f "$OUTDIR/$SESSION_ID.diverted" ]; then
    echo "running"
  else
    echo "idle"
  fi
}

# The head SHA the last completed round posted against. Empty when unknown
# (proofs predating the head_sha field, or no proof at all).
posted_head() {
  local proof
  proof=$(proof_for_draft)
  [ -n "$proof" ] && [ -f "$proof" ] || return 1
  jq -r '.head_sha // empty' "$proof" 2>/dev/null
}

# Non-empty message when the local checkout is ahead of the MR head, so a
# divert would draft against commits the server never saw. Compares the
# LOCAL branch tip with the MR head SHA. Empty means push state is fine or
# unknowable (no repo configured, no local branch, fetch failed) -- the gate
# fails open; the poster's own head check is the backstop.
local_ahead_of_mr() {
  local mr="$1" repo branch head_sha tip
  repo="${OFFLOAD_REPO:-${SPARK_REPO:-}}"
  if [ -z "$repo" ]; then
    repo=$(python3 -c "import sys; sys.path.insert(0, '$WORKERS'); import gitlab_api as gl; print(gl.local_repo())" 2>/dev/null)
  fi
  [ -n "$repo" ] && [ -d "$repo/.git" ] || return 0
  branch=$(python3 -c "
import sys; sys.path.insert(0, '$WORKERS')
import gitlab_api as gl
try:
    mr = gl.merge_request('$mr') or {}
    print((mr.get('source_branch') or '') + ' ' + (mr.get('sha') or ''))
except Exception:
    pass
" 2>/dev/null) || return 0
  set -- $branch
  branch="$1"; head_sha="$2"
  [ -n "$branch" ] && [ -n "$head_sha" ] || return 0
  git -C "$repo" fetch origin "$branch" >/dev/null 2>&1 || return 0
  # Unpushed commits live on the local branch, not on the tracking ref --
  # and a bare `git fetch origin <branch>` leaves origin/<branch> stale
  # anyway (explicit refspec without a colon lands in FETCH_HEAD only), so
  # the tracking ref can never answer "is local ahead". Read the local tip.
  tip=$(git -C "$repo" rev-parse --verify "$branch" 2>/dev/null) || return 0
  [ -n "$tip" ] || return 0
  if [ "$tip" != "$head_sha" ]; then
    # Local tip moved past the MR head: unpushed commits exist. (A tip
    # behind the head cannot happen from pushing; ignore that direction.)
    if git -C "$repo" merge-base --is-ancestor "$head_sha" "$tip" 2>/dev/null; then
      echo "local $branch ($tip) is ahead of MR !$mr head ($head_sha): unpushed commits would be invisible to the draft."
    fi
  fi
  return 0
}

# True when the MR moved since the last completed round -- the stored proof
# describes commits the draft saw, and the new head needs a fresh round.
mr_moved_since_post() {
  local posted now mr
  posted=$(posted_head) || return 1
  [ -n "$posted" ] || return 1
  mr=$(armed_mr) || return 1
  [ -n "$mr" ] || return 1
  now=$(python3 -c "
import sys; sys.path.insert(0, '$WORKERS')
import gitlab_api as gl
try:
    mr = gl.merge_request('$mr') or {}
    print(mr.get('sha') or '')
except Exception:
    pass
" 2>/dev/null) || return 1
  [ -n "$now" ] && [ "$now" != "$posted" ]
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
  # Atomic spawn guard: two hook events (a prompt racing the Stop backstop,
  # or two rapid prompts) can both pass the alive-check below before either
  # forks — two supervisors, two posters, true double-posts that marker
  # idempotency cannot catch (identical markers). flock -n decides once: the
  # loser returns and lets the winner's worker serve both events. Held for
  # the checks AND the fork (released on every return via the trap).
  # Async-subshell trap reset means the background worker below never runs
  # this trap itself; worst case (exotic shell) it degrades to today's
  # behavior, never worse. Never fails closed: lock failure just returns.
  exec 9>"$OUTDIR/$SESSION_ID.spawn.flock" 2>/dev/null || return 0
  if ! flock -n 9 2>/dev/null; then
    exec 9>&- 2>/dev/null || true
    return 0
  fi
  trap 'flock -u 9 2>/dev/null; exec 9>&- 2>/dev/null; trap - RETURN' RETURN
  case "$(worker_state)" in drafted|posting|posted) return 0 ;; esac
  worker_alive && return 0
  # A stale marker with no live worker is a previous crash, not a running
  # worker: clear it so this attempt is a real one.
  rm -f "$OUTDIR/$SESSION_ID.diverted" "$OUTDIR/$SESSION_ID.drafting"

  # The MR is pinned from this command's args. Never infer it from unrelated
  # mentions elsewhere in the transcript.
  MR=$(armed_mr)
  if [ -z "$MR" ]; then
    echo "no MR URL or !number in the review command args; not drafting" >>"$OUTDIR/worker.log"
    return 1
  fi
  MODE=$(arm_mode)
  touch "$OUTDIR/$SESSION_ID.diverted" "$OUTDIR/$SESSION_ID.drafting"
  if [ ! -f "$OUTDIR/$SESSION_ID.notes.md" ]; then
    cat >"$OUTDIR/$SESSION_ID.notes.md" <<EOF
# Verified hand-off notes for MR !$MR

Only include facts you checked. For each test or measurement, record the
command or method, exact result, scope, and that this session ran it. Label
anything inferred or unverified. The drafter reads this file before writing
replies; it must not present your checks as its own.
EOF
  fi
  rm -f "$OUTDIR/$SESSION_ID.failed" "$OUTDIR/$SESSION_ID.reported"

  (
    SESSLOG="$OUTDIR/$SESSION_ID.worker.log"
    {
      echo "=== worker start: MR !$MR mode $MODE ==="
      # Local-only configuration (host, project, repo): lives in
      # ~/.config/offload-workers/env (legacy spark-poster config also works).
      # Without it the worker fails loud naming the missing vars.
      set -a
      [ -f "$HOME/.config/spark-poster/env" ] && . "$HOME/.config/spark-poster/env"
      [ -f "$HOME/.config/offload-workers/env" ] && . "$HOME/.config/offload-workers/env"
      set +a
      # Review drafting has no YouTrack use; keep its credential away from the
      # model subprocess. GitLab auth is read from the poster's private token file.
      unset YOUTRACK_TOKEN YOUTRACK_URL YOUTRACK_PROJECT_ID YOUTRACK_PROJECT_MAP \
        AI_YOUTRACK_BIN GITLAB_TOKEN OPENCODE_API_KEY
      HANDOFF_WAIT="${OFFLOAD_REVIEW_HANDOFF_SECONDS:-${SPARK_REVIEW_HANDOFF_SECONDS:-20}}"
      case "$HANDOFF_WAIT" in ''|*[!0-9]*) HANDOFF_WAIT=20 ;; esac
      [ "$HANDOFF_WAIT" -le 120 ] || HANDOFF_WAIT=120
      echo "waiting ${HANDOFF_WAIT}s for operator notes at $OUTDIR/$SESSION_ID.notes.md"
      sleep "$HANDOFF_WAIT"
      # Drafting can take several minutes. Posting is a separate, explicit
      # approval after the operator has inspected the saved draft.
      if OFFLOADED_REVIEW_WORKER=1 timeout 2700 \
          python3 "$WORKERS/review_draft.py" "$MR" "$SESSION_ID" "$MODE" "$TRANSCRIPT" \
            "$OUTDIR/$SESSION_ID.notes.md"; then
        DRAFT="$OUTDIR/$SESSION_ID.draft.json"
        if [ -f "$DRAFT" ]; then
          touch "$OUTDIR/$SESSION_ID.drafted"
          rm -f "$OUTDIR/$SESSION_ID.drafting" "$OUTDIR/$SESSION_ID.diverted"
        else
          echo "drafter exited 0 but no draft at $DRAFT" >"$OUTDIR/$SESSION_ID.failed"
          rm -f "$OUTDIR/$SESSION_ID.drafting" "$OUTDIR/$SESSION_ID.diverted"
        fi
      else
        rc=$?
        # Keep a partial draft for inspection, but never post it implicitly.
        if [ -f "$OUTDIR/$SESSION_ID.draft.json" ]; then
          echo "worker ended (rc=$rc) with a partial draft on disk; inspect before approving" >>"$SESSLOG"
          touch "$OUTDIR/$SESSION_ID.drafted"
          rm -f "$OUTDIR/$SESSION_ID.drafting" "$OUTDIR/$SESSION_ID.diverted"
        else
          {
            echo "worker failed (rc=$rc) for MR !$MR mode $MODE with no draft; last lines:"
            tail -8 "$SESSLOG"
          } >"$OUTDIR/$SESSION_ID.failed"
          rm -f "$OUTDIR/$SESSION_ID.drafting" "$OUTDIR/$SESSION_ID.diverted"
        fi
      fi
    } >>"$SESSLOG" 2>&1
    echo "session $SESSION_ID: MR !$MR $MODE finished (see $SESSION_ID.worker.log)" >>"$OUTDIR/worker.log"
    rm -f "$OUTDIR/$SESSION_ID.started"
  ) >>"$OUTDIR/worker.log" 2>&1 &
  echo $! >"$OUTDIR/$SESSION_ID.started"
  disown
}

# A proof nobody has been told about yet. One-shot: the caller marks it
# reported after relaying it. This is the "ping me when done" -- the next
# user prompt after the post carries the outcome, no file-touching chore.
unreported_proof() {
  [ -f "$OUTDIR/$SESSION_ID.reported" ] && return 1
  local proof
  proof=$(proof_for_draft)
  [ -n "$proof" ] && [ -f "$proof" ] || return 1
  echo "$proof"
}

# One line on what the proof says, for the ping.
proof_summary() {
  local proof="$1" line
  line=$(jq -r '"MR !\(.iid // "?"): " + (if (.ok // false) then "posted \(.verified | length)/\(.planned + (.skipped_already_present // 0)) verified" + (if ((.resolved // []) | length) > 0 then ", resolved \([.resolved[]] | length)" else "" end) else "FAILED: \(.refused // "see the worker log")" end)' \
    "$proof" 2>/dev/null) || line="proof saved (unreadable)"
  echo "$line -- proof: $proof"
}

draft_stats() {
  jq -r '"\(.replies | length) replies, MR !\(.iid)" + (if (.partial // false) then "; PARTIAL, no verdict: \([.unverdict_threads[]?] | join(","))" else "" end)' \
    "$OUTDIR/$SESSION_ID.draft.json" 2>/dev/null || echo "unreadable draft"
}

proof_for_draft() {
  local sid
  sid=$(jq -r '.session_id // empty' "$OUTDIR/$SESSION_ID.draft.json" 2>/dev/null)
  [ -n "$sid" ] && echo "$OUTDIR/$sid.proof.json"
}

disarm_session() {
  local key
  key=$(cat "$OUTDIR/$SESSION_ID.armed-key" 2>/dev/null)
  [ -z "$key" ] || printf '%s' "$key" >"$OUTDIR/$SESSION_ID.disarmed-key"
  rm -f "$OUTDIR/$SESSION_ID.armed" "$OUTDIR/$SESSION_ID.armed-mr"
}

# ── UserPromptSubmit: an explicit request drafts; posting needs approval ──
if [ "$EVENT" = "UserPromptSubmit" ]; then
  arm_context
  ARM_RC=$?
  [ "$ARM_RC" -ne 2 ] || exit 0
  [ "$ARM_RC" -eq 0 ] || armed_by_invocation || exit 0

  # A proof nobody has been told about yet rides on the next prompt. Once the
  # one-shot hand-off is reported, this command invocation is disarmed.
  PING=$(unreported_proof) || true
  if [ -n "$PING" ]; then
    echo "OFFLOAD WORKER RESULT: $(proof_summary "$PING")."
    touch "$OUTDIR/$SESSION_ID.reported"
    disarm_session
  fi

  if [ -f "$OUTDIR/$SESSION_ID.failed" ]; then
    if [ -f "$OUTDIR/$SESSION_ID.drafted" ] && [ -f "$OUTDIR/$SESSION_ID.draft.json" ]; then
      echo "OFFLOAD WORKER FAILED after drafting. Draft remains at $OUTDIR/$SESSION_ID.draft.json; inspect it before retrying --approve. Reason: $(head -3 "$OUTDIR/$SESSION_ID.failed" 2>/dev/null | tr '\n' ' ')."
    else
      echo "OFFLOAD WORKER FAILED: no usable draft. Reason: $(head -3 "$OUTDIR/$SESSION_ID.failed" 2>/dev/null | tr '\n' ' '). Ask for a fresh draft to retry."
    fi
  fi

  MR=$(armed_mr)
  NOTES="$OUTDIR/$SESSION_ID.notes.md"
  DRAFT="$OUTDIR/$SESSION_ID.draft.json"
  ST=$(worker_state)
  if [ "$ST" = "drafted" ] && [ ! -f "$OUTDIR/$SESSION_ID.draft-reported" ]; then
    echo "OFFLOAD WORKER DRAFT READY for MR !${MR:-unknown}: $(draft_stats). Review/edit $DRAFT; verified hand-off facts go in $NOTES. To post after review run: bash $WORKERS/review-post.sh --approve $DRAFT. Drafting never resolves threads; close one separately with --resolve."
    touch "$OUTDIR/$SESSION_ID.draft-reported"
    DRAFT_ANNOUNCED=1
  fi

  PROMPT=$(echo "$INPUT" | jq -r '.prompt // empty' 2>/dev/null |
           tr '[:upper:]' '[:lower:]')

  # The slash command arms the scope but does not count as a draft request.
  PROMPT=$(echo "$PROMPT" | sed -E 's/<command-message>.*<\/command-message>//g; s/<command-name>.*<\/command-name>//g; s/<command-args>.*<\/command-args>//g; s#/(gitlab-review|fix-mr-comments)##g')

  # Said outright: "post the threads", "answer the comments", "запости ответы".
  #
  # An instruction, not a mention: the verb opens a clause (start of a line,
  # after punctuation, or after "please", "then", "can you" and the like), and
  # its object follows within three words of the same clause. Matching the
  # verb and the object anywhere in the prompt fired on ordinary review talk
  # -- "why did you close that thread?", "the answer in that thread is
  # wrong", "resolve the merge conflict, then re-read the comments". Something
  # phrased longer than this misses; the model or a rephrase covers it, and
  # a miss posts nothing.
  #
  # English uses base forms only: "posted the replies" reports, it does not
  # ask. Russian lists imperative and infinitive forms, so the noun "ответ"
  # ("какой ответ?") is no longer a verb and an object at once. No bracket
  # ranges over letters: a range over multibyte characters does not survive
  # the locale this hook runs under.
  INTENT=""
  EN_INTENT='(^|[.;:!?,] *|\b(please|pls|now|then|and|so|just|go ahead and|can you|could you|would you) +)(post|reply|answer|respond)( +[^ .;:!?,]+){0,3} +(threads?|comments?|discussions?|replies|responses)\b'
  RU_INTENT='(^|[^[:alnum:]])(запости|запостить|ответь|ответьте|отвечай|ответить)( +[^ .;:!?,]+){0,3} +(тред|коммент|ветк|замечан|ответ)'
  if echo "$PROMPT" | grep -qE "$EN_INTENT" || echo "$PROMPT" | grep -qE "$RU_INTENT"; then
    INTENT=1
  fi

  # An explicit refusal wins before any sequencing hints.
  echo "$PROMPT" | grep -qE "^(no|nope|not? |don'?t|do not|stop|wait|hold|нет|не |стоп|погоди)" && INTENT=""

  # A one-off addendum uses review-post.sh --note and must not start the
  # full-thread drafter. Defer any write request in the same prompt as a
  # commit or push: this hook runs before those requested tool actions happen.
  WRITE_REQUEST="$INTENT"
  echo "$PROMPT" | grep -qE -- '(^|[[:space:]])--(note|approve|resolve)([[:space:]]|$)|one[- ]line|addendum|\b(resolve|close) +[^ .;:!?,]+ +(threads?|comments?)\b' && WRITE_REQUEST=1
  if [ -n "$WRITE_REQUEST" ] && echo "$PROMPT" | grep -qE '\b(commit|push|pushes|pushing|commitment)\b'; then
    echo "REVIEW DRAFT DEFERRED: this prompt also asks for a commit or push. UserPromptSubmit runs before those actions, so finish and push first, then ask for the review draft in a later prompt. Nothing was drafted or posted."
    INTENT=""
  elif echo "$PROMPT" | grep -qE -- '(^|[[:space:]])--note([[:space:]]|$)|one[- ]line|addendum'; then
    INTENT=""
  fi

  # Or said as a yes to the model's own question about posting.
  if [ -z "$INTENT" ]; then
    BARE=$(echo "$PROMPT" | tr -d '[:punct:]' | tr -s ' ' | sed 's/^ *//;s/ *$//')
    if echo "$BARE" | grep -qxE '(yes|y|yep|yeah|yup|ok|okay|sure|go|go ahead|do it|please do|post it|send it|post them|send them|post|go for it|да|ага|давай|давай да|запости|отвечай|ответь)'; then
      ASKED=$(tail -c 200000 "$TRANSCRIPT" 2>/dev/null | jq -R -s '
        split("\n") | map(select(length > 0) | fromjson?)
        | map(select(.message.role == "assistant" and (.isSidechain != true)))
        | last
        | (.message.content // [] | map(select(.type == "text") | .text // "") | join("\n"))
        // ""' 2>/dev/null | tr '[:upper:]' '[:lower:]')
      # Word boundaries here too: bare `answer` diverted a "yes" to any
      # question with "the answer" in it, and `repl` to "replication".
      echo "$ASKED" | grep -qE '\?' &&
      echo "$ASKED" | grep -qE '\b(post|posts|posting|reply|replies|comment|comments|thread|threads)\b|отвеч|запост' &&
        INTENT=1
    fi
  fi

  # Bare yes/no replies are classified against the preceding assistant
  # question below; apply the same commit/push ordering guard after that pass.
  if [ -n "$INTENT" ] && echo "$PROMPT" | grep -qE '\b(commit|push|pushes|pushing|commitment)\b'; then
    echo "REVIEW DRAFT DEFERRED: this prompt also asks for a commit or push. UserPromptSubmit runs before those actions, so finish and push first, then ask for the review draft in a later prompt. Nothing was drafted or posted."
    INTENT=""
  fi

  # Ticket territory belongs to ticket-gate.sh: when that gate would divert,
  # the ticket worker owns the prompt and review stands down -- both would
  # otherwise spawn (disjoint state dirs, no mutual exclusion). So this is
  # ticket-gate's own pattern, copied exactly: a looser copy stood review
  # down on prompts ticket-gate ignores ("post the threads; ticket MVP-12
  # ...") and nothing fired at all. check-drift.sh fails if they differ.
  TICKET_INTENT='(^|[^[:alnum:]_./-])(file|create|open|submit|raise|заведи|завести|создай|открыть|открой)( +[^ .;:!?,]+){0,2} +((tickets?|issues?|youtrack|mvp-[0-9]+)([^[:alnum:]_./-]|[.]([^[:alnum:]]|$)|$)|тикет|задач)'
  if echo "$PROMPT" | grep -qE "$TICKET_INTENT"; then
    INTENT=""
  fi

  if [ -n "$INTENT" ]; then
    # State-aware, and every word of it checkable.
    MR=$(armed_mr)
    ST=$(worker_state)
    # Diverted but no live worker is a crash, not a running worker. Saying
    # "already running" here would wedge the session: every later prompt
    # gets the same answer while nothing runs. Restart instead.
    if [ "$ST" = "running" ] && ! worker_alive; then ST="idle"; fi
    # A new push re-opens a completed round: the proof describes the old head.
    if [ "$ST" = "posted" ] && mr_moved_since_post; then
      rm -f "$OUTDIR/$SESSION_ID.diverted" "$OUTDIR/$SESSION_ID.drafted" \
        "$OUTDIR/$SESSION_ID.posted" "$OUTDIR/$SESSION_ID.proof.json" \
        "$OUTDIR/$SESSION_ID.reported" "$OUTDIR/$SESSION_ID.draft-reported" \
        "$OUTDIR/$SESSION_ID.started" "$OUTDIR/$SESSION_ID.draft.json"
      ST="idle"
    fi
    case $ST in
      posted)
        PROOF=$(proof_for_draft)
        if [ -n "$PROOF" ] && [ -f "$PROOF" ]; then
          echo "REVIEW POSTED AT $(posted_head | cut -c1-12): $(proof_summary "$PROOF"). The MR has not moved since this proof."
        else
          echo "REVIEW STATE ERROR: posted marker has no proof; inspect $OUTDIR/$SESSION_ID.worker.log."
        fi
        ;;
      drafted)
        if [ -z "$DRAFT_ANNOUNCED" ]; then
          echo "REVIEW DRAFT READY for MR !${MR:-unknown}: $(draft_stats). Review/edit $DRAFT; verified hand-off facts go in $NOTES. To post after review run: bash $WORKERS/review-post.sh --approve $DRAFT. For one line use --note IID DISCUSSION_ID TEXT; close a thread separately with --resolve."
        fi
        ;;
      posting)
        echo "REVIEW POST IN PROGRESS for MR !${MR:-unknown}; progress is in $OUTDIR/$SESSION_ID.worker.log."
        ;;
      failed)
        if [ -f "$OUTDIR/$SESSION_ID.drafted" ] && [ -f "$DRAFT" ]; then
          echo "REVIEW POSTER FAILED for MR !${MR:-unknown}. Reason: $(head -3 "$OUTDIR/$SESSION_ID.failed" 2>/dev/null | tr '\n' ' '). Draft remains at $DRAFT; inspect it before retrying --approve."
        else
          echo "REVIEW DRAFTER FAILED for MR !${MR:-unknown}. Reason: $(head -3 "$OUTDIR/$SESSION_ID.failed" 2>/dev/null | tr '\n' ' '). Retrying after this explicit draft request."
          rm -f "$OUTDIR/$SESSION_ID.failed" "$OUTDIR/$SESSION_ID.diverted" \
            "$OUTDIR/$SESSION_ID.drafting" "$OUTDIR/$SESSION_ID.started"
          PUSH_CHECK=$(local_ahead_of_mr "$MR") || true
          if [ -n "$MR" ] && [ -z "$PUSH_CHECK" ]; then
            spawn_worker
            echo "REVIEW DRAFT RETRY STARTED for MR !$MR. Draft only; it will wait for inspection before posting."
          elif [ -n "$PUSH_CHECK" ]; then
            echo "REVIEW NOT RETRIED: $PUSH_CHECK Push first, then ask again."
          fi
        fi
        ;;
      running)
        echo "REVIEW WORKER RUNNING for MR !${MR:-unknown}. It is drafting only. Log: $OUTDIR/$SESSION_ID.worker.log. Notes file: $NOTES."
        ;;
      idle)
        if [ -n "$MR" ]; then
          # Push gate: the dossier reads the MR head, so fixes that exist
          # only locally are invisible and verdicts describe code that is not
          # on the server yet (MR !597: replies citing pushed code while the
          # fixes sat unpushed). Push first, then divert.
          PUSH_CHECK=$(local_ahead_of_mr "$MR") || true
          if [ -n "$PUSH_CHECK" ]; then
            echo "REVIEW NOT STARTED: $PUSH_CHECK Push first, then ask for a draft in a later prompt. Nothing was drafted or posted."
          else
            spawn_worker
            echo "REVIEW REQUEST DIVERTED to the offload worker for MR !$MR ($(arm_mode)). Log: $OUTDIR/$SESSION_ID.worker.log. Do not post yourself yet; inspect $DRAFT when ready. Add verified facts and exact command results to $NOTES during the handoff window. Headroom transport requires OFFLOAD_ALLOW_EXTERNAL_DATA=1 because the worker cannot verify where the selected alias routes; OFFLOAD_MODEL_TRANSPORT=local-direct calls the configured loopback model directly."
          fi
        else
          echo "REVIEW DRAFT NOT STARTED: the slash command did not include an MR URL or !number in its command args. Re-run /fix-mr-comments with the exact MR URL."
        fi
        ;;
    esac
  fi
  exit 0
fi

# ── everything below is the write-time backstop; it needs the session armed ──
armed_by_invocation || exit 0

if [ "$EVENT" = "Stop" ]; then
  # Backstop only: retry if the draft worker died before writing a draft.
  if [ -f "$OUTDIR/$SESSION_ID.diverted" ] \
     && [ ! -f "$OUTDIR/$SESSION_ID.drafting" ] \
     && [ ! -f "$OUTDIR/$SESSION_ID.drafted" ]; then
    spawn_worker
  fi
  exit 0
fi

# ── PreToolUse: divert articulation-shaped actions ──
TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null)
if [ "$TOOL" = "Bash" ]; then
  CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
  LOW=$(echo "$CMD" | tr '[:upper:]' '[:lower:]')
  # These are explicit, reviewable write modes. A --note call is the one-off
  # addendum path; --approve posts the inspected draft; --resolve is a separate
  # close action. The poster validates and verifies each operation itself.
  # Only a lone invocation is waved through. Matched anywhere in the command,
  # `review-post.sh --note x; curl -X POST .../api/v4/...` walked past every
  # check below on the strength of its first half; a command that chains,
  # pipes or substitutes falls through to the SUBJECT/WRITES checks instead.
  if echo "$LOW" | grep -qE '^[[:space:]]*((bash|python3)[[:space:]]+)?[^][:space:];&|<>$`]*(review-post\.sh|review_post\.py)[[:space:]]+--(approve|note|resolve)([[:space:]][^;&|<>$`]*)?$'; then
    exit 0
  fi
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
  # youtrack deliberately excluded: ticket filing belongs to ticket-gate.sh,
  # which owns that territory (a `curl ... youtrack ... --data ...` matches
  # both gates' SUBJECT+WRITES and would divert twice).
  echo "$LOW" | grep -qE 'gitlab|merge_requests|api/v4|discussion_id|review_post' && SUBJECT=1

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
  #
  # Nor can any heredoc: a read-only script fetching the discussions, or a
  # commit message naming merge_requests/NNN, diverted too, and each divert
  # starts a worker that posts. A heredoc composes a reply when it sets a
  # note body -- a `"body":` key or a `body=` field. Reading one
  # (`note["body"]`) does neither.
  case "$LOW" in
    *'<<'*)
      echo "$LOW" | grep -qE "[\"']body[\"'] *:|(^|[^[:alnum:]_])body=" && COMPOSES=1 ;;
  esac

  ARTICULATE=""
  if [ -n "$SUBJECT" ] && { [ -n "$WRITES" ] || [ -n "$COMPOSES" ]; }; then
    ARTICULATE=1
  fi
  if [ -n "$ARTICULATE" ]; then
    MR=$(armed_mr)
    DRAFT="$OUTDIR/$SESSION_ID.draft.json"
    echo "REVIEW WRITE BLOCKED for MR !${MR:-unknown}: review-gate.sh stopped this direct GitLab write. Worker state: $(worker_state); log: $OUTDIR/$SESSION_ID.worker.log; draft: $DRAFT. Inspect it, then use review-post.sh --approve for reviewed replies, --note IID DISCUSSION_ID TEXT for a one-line addendum, or --resolve IID DISCUSSION_ID as a separate close action. No reply was posted." >&2
    exit 2
  fi
  exit 0
fi

case "$TOOL" in
  Write|Edit|MultiEdit)
    FP=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null)

    # The generated draft and its verified-facts sidecar are intentionally
    # editable by the operator after the worker has finished.
    case "$FP" in
      "$OUTDIR/$SESSION_ID.draft.json"|"$OUTDIR/$SESSION_ID.notes.md") exit 0 ;;
    esac

    # Machinery, not drafts. These files necessarily contain the same words a
    # draft does -- discussion_id, body, resolve -- because they are what reads
    # and posts one. Matching them would wall the model off from its own tools.
    case "$FP" in
      */offload-workers/gitlab_api.py|*/offload-workers/review_post.py \
      |*/offload-workers/review_draft.py|*/offload-workers/review-post.sh \
      |*/offload-workers/triage.py|*/offload-workers/read_threads.py \
      |*/offload-workers/since_review.py|*/offload-workers/thread_dossier.py \
      |*/offload-workers/README.md|*/hooks/review-gate.sh)
        exit 0 ;;
    esac

    DIVERT=""
    # Where drafts live. The old matcher was three /tmp globs and a draft
    # written anywhere else -- a drafts/ dir in the repo, say -- walked
    # straight through it.
    case "$FP" in
      */offload-workers/*|*/spark-review/*|*/drafts/*|/tmp/mr*|/tmp/*post*|docs/mr-*) DIVERT=1 ;;
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
      echo "REVIEW DRAFT WRITE BLOCKED: this path looks like an outbound review draft. The current generated draft is $OUTDIR/$SESSION_ID.draft.json; inspect/edit that file, or ask for the review worker to draft replies. Nothing was posted." >&2
      exit 2
    fi
    ;;
esac
exit 0

#!/usr/bin/env bash
# Contract test for two Claude hooks: docker-autostart.sh (when it starts
# Docker) and ticket-gate.sh (which Bash calls it treats as a ticket write).
# Runs each hook with a temp HOME and fake docker/systemctl/uname binaries, so
# nothing real starts and nothing is filed. Exit 0 with "ok" lines, else the
# first failing case.
set -euo pipefail

REPO_DIR=$(cd "$(dirname "$0")/.." && pwd)
HOOKS="$REPO_DIR/contrib/claude/hooks"
T=$(mktemp -d "${TMPDIR:-/tmp}/headroom-hooks-test.XXXXXX")
trap 'rm -rf "$T"' EXIT
export HOME="$T/home" XDG_RUNTIME_DIR="$T/run"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$T/bin"
unset WSL_DISTRO_NAME WSL_INTEROP HEADROOM_REPO YOUTRACK_TOKEN

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

# ── docker-autostart ────────────────────────────────────────────────────────
# The fake engine is "up" once systemctl has been asked to start it.
cat >"$T/bin/docker" <<EOF
#!/bin/sh
[ "\$1" = info ] && [ -f "$T/engine-up" ]
EOF
cat >"$T/bin/systemctl" <<EOF
#!/bin/sh
echo "\$@" >>"$T/systemctl.calls"
touch "$T/engine-up"
EOF
cat >"$T/bin/uname" <<'EOF'
#!/bin/sh
case "$1" in -s) echo Linux ;; -r) echo 6.0.0-generic ;; *) echo Linux ;; esac
EOF
chmod +x "$T/bin/"*

docker_hook() { # event session message -> stdout+stderr in $OUT, status in $RC
  local input
  input=$(jq -nc --arg e "$1" --arg s "$2" --arg m "$3" \
    '{hook_event_name:$e,session_id:$s,last_assistant_message:$m}')
  RC=0
  OUT=$(printf '%s' "$input" | PATH="$T/bin:$PATH" bash "$HOOKS/docker-autostart.sh" 2>&1) || RC=$?
}
engine_down() { rm -f "$T/engine-up" "$T/systemctl.calls"; }

engine_down
docker_hook Stop s1 "Docker is not needed for this change."
[[ $RC -eq 0 && -z $OUT && ! -f "$T/systemctl.calls" ]] || fail "incidental mention started Docker (rc=$RC out=$OUT)"
ok "an incidental mention does not start Docker"

docker_hook Stop s1 "Next run \`docker ps\` to see the container."
[[ $RC -eq 2 && $OUT == *"started automatically"* && -f "$T/systemctl.calls" ]] ||
  fail "a docker command did not start the engine (rc=$RC out=$OUT)"
ok "a docker command with the engine down starts it and continues once"

engine_down
docker_hook Stop s1 "Then docker compose up."
[[ $RC -eq 0 && ! -f "$T/systemctl.calls" ]] || fail "second Stop in one turn started Docker again (rc=$RC)"
ok "the per-session marker suppresses a repeat"

docker_hook UserPromptSubmit s1 ""
docker_hook Stop s1 "Then docker compose up."
[[ $RC -eq 2 ]] || fail "the marker survived the next prompt (rc=$RC)"
ok "the next prompt clears the marker"

touch "$T/engine-up"
docker_hook Stop s2 "Run docker ps."
[[ $RC -eq 0 && -z $OUT ]] || fail "a running engine still produced output (rc=$RC out=$OUT)"
ok "a running engine is left alone"

# ── ticket-gate ─────────────────────────────────────────────────────────────
# YouTrack is unconfigured here, so a call the gate would divert reports
# "TICKET WORKER NOT STARTED" and one it lets through says nothing.
: >"$T/transcript.jsonl"
export HEADROOM_REPO="$REPO_DIR"
ticket_hook() { # bash command [session] -> $OUT, $RC
  local input
  input=$(jq -nc --arg c "$1" --arg t "$T/transcript.jsonl" --arg s "${2:-t1}" \
    '{hook_event_name:"PreToolUse",tool_name:"Bash",session_id:$s,cwd:"/tmp",
      transcript_path:$t,tool_input:{command:$c}}')
  RC=0
  OUT=$(printf '%s' "$input" | bash "$HOOKS/ticket-gate.sh" 2>&1) || RC=$?
}
diverts() { [[ $OUT == *"TICKET WORKER NOT STARTED"* || $RC -eq 2 ]]; }

ticket_hook "curl -X POST https://yt.example/api/issues -d @body.json"
diverts || fail "curl -X POST to the tracker was not treated as a write (rc=$RC out=$OUT)"
ticket_hook "curl https://yt.example/api/issues -d @body.json"
diverts || fail "curl -d without -X was not treated as a write (rc=$RC out=$OUT)"
ok "tracker writes are recognised, including curl -d without -X"

ticket_hook "curl -s https://yt.example/api/issues?fields=id"
[[ $RC -eq 0 && -z $OUT ]] || fail "a tracker read was diverted (rc=$RC out=$OUT)"
ticket_hook "curl -d q=1 https://example.com/search"
[[ $RC -eq 0 && -z $OUT ]] || fail "a non-tracker POST was diverted (rc=$RC out=$OUT)"
ticket_hook "$(printf 'cat > notes.md <<EOF\nyoutrack summary and description\nEOF')"
[[ $RC -eq 0 && -z $OUT ]] || fail "a heredoc that only names the tracker was diverted (rc=$RC out=$OUT)"
ticket_hook "curl -X POST https://yt.example/api/issues/PROJ-12/comments -d @c.json"
[[ $RC -eq 0 && -z $OUT ]] || fail "a comment on an existing issue was diverted (rc=$RC out=$OUT)"
ok "reads, other hosts, heredocs and existing-issue work pass through"

# -d belongs to curl only in curl's own pipeline segment: reading the token
# with `tr -d` and then GETting the tracker is a read. An issue id held in a
# variable is still an existing issue.
ticket_hook "T=\$(cat ~/.config/youtrack/token | tr -d '\n'); curl -sS \"\$D/api/issues?query=x\" -H \"Authorization: Bearer \$T\""
[[ $RC -eq 0 && -z $OUT ]] || fail "tr -d next to a tracker read was diverted (rc=$RC out=$OUT)"
ticket_hook "cut -d' ' -f1 youtrack-ids.txt"
[[ $RC -eq 0 && -z $OUT ]] || fail "cut -d on a tracker-named file was diverted (rc=$RC out=$OUT)"
ticket_hook "curl -sS -X POST \"\$D/api/issues/\$k/comments\" --data @c.json"
[[ $RC -eq 0 && -z $OUT ]] || fail "a comment on an issue id in a variable was diverted (rc=$RC out=$OUT)"
ok "tr -d, cut -d and issue ids in variables are not filings"

ticket_hook "$(printf 'curl -sS -X POST "https://yt.example/api/issues?fields=id" \\\n  -H "Content-Type: application/json" \\\n  --data @body.json')"
diverts || fail "a create split over continuation lines was not treated as a write (rc=$RC out=$OUT)"
ticket_hook "T=\$(tr -d '\n' <tok); curl https://yt.example/api/issues -d @body.json"
diverts || fail "a create after a tr -d segment was not treated as a write (rc=$RC out=$OUT)"
ok "creates still divert across continuation lines and after other segments"

# A prompt that asks for a filing is diverted; one that posts a report to an
# existing ticket is not. YOUTRACK_TOKEN is unset, so a match prints
# "TICKET DIVERT NOT STARTED" and a non-match prints nothing.
PROMPTS=0
ticket_prompt() { # prompt [session] -> $OUT, $RC (new session by default: the warning is once per session)
  local input
  PROMPTS=$((PROMPTS + 1))
  input=$(jq -nc --arg p "$1" --arg t "$T/transcript.jsonl" --arg s "${2:-p$PROMPTS}" \
    '{hook_event_name:"UserPromptSubmit",session_id:$s,cwd:"/tmp",
      transcript_path:$t,prompt:$p}')
  RC=0
  OUT=$(printf '%s' "$input" | bash "$HOOKS/ticket-gate.sh" 2>&1) || RC=$?
}
for p in "file the ticket" "create a youtrack issue" "file it in youtrack"; do
  ticket_prompt "$p"
  [[ $OUT == *"TICKET DIVERT"* ]] || fail "filing prompt '$p' was not diverted (rc=$RC out=$OUT)"
done
ok "prompts that ask for a ticket are diverted"

for p in \
  "push both branches and open the merge requests. post the YouTrack report" \
  "post the youtrack report on ANL-1426" \
  "post the issue summary as a comment"; do
  ticket_prompt "$p"
  [[ $RC -eq 0 && -z $OUT ]] || fail "report prompt '$p' was diverted (rc=$RC out=$OUT)"
done
ok "posting a report is not filing a ticket"

# YOUTRACK_PROJECT_MAP routes a mapped directory and everything below it, not
# a sibling that shares the leading characters. A fake worker stands in for
# ticket_file.py; the routed id is read back from .ticket.project.
mkdir -p "$T/repo/contrib/offload-workers" "$HOME/.config/offload-workers"
printf 'import sys\nsys.exit(0 if "--check-backend" in sys.argv else 1)\n' \
  >"$T/repo/contrib/offload-workers/ticket_file.py"
cat >"$HOME/.config/offload-workers/env" <<'EOF'
YOUTRACK_URL=https://yt.example
YOUTRACK_PROJECT_MAP=/w/alpha=0-1,/w/alpha/inner=0-2,/w/beta/=0-3
EOF
routed() { # cwd -> project id the hook wrote for a "file the ticket" prompt, or empty
  local sid="route$((PROMPTS += 1))"
  jq -nc --arg c "$1" --arg t "$T/transcript.jsonl" --arg s "$sid" \
    '{hook_event_name:"UserPromptSubmit",session_id:$s,cwd:$c,
      transcript_path:$t,prompt:"file the ticket"}' |
    YOUTRACK_TOKEN=test HEADROOM_REPO="$T/repo" bash "$HOOKS/ticket-gate.sh" >/dev/null 2>&1 || true
  cat "$HOME/.local/state/offload-workers/$sid.ticket.project" 2>/dev/null || true
}
for c in "/w/alpha 0-1" "/w/alpha/x/y/z 0-1" "/w/alpha/inner/deep 0-2" \
         "/w/beta 0-3" "/w/beta/x 0-3" "/w/alpha-old " "/w/other "; do
  got=$(routed "${c% *}")
  [[ $got == "${c##* }" ]] || fail "cwd ${c% *} routed to '$got', want '${c##* }'"
done
rm -f "$HOME/.config/offload-workers/env"
ok "the project map routes a directory and its children, not its siblings"

# A filing-shaped command starts the worker only after the user asked for a
# ticket in the same session. Unarmed it is blocked and no worker starts, so
# a matcher misfire cannot file anything. "TICKET WORKER NOT STARTED" below
# means the hook tried to start one (YOUTRACK_TOKEN is unset here).
FILING="curl -X POST https://yt.example/api/issues -d @body.json"
STATE="$HOME/.local/state/offload-workers"
ticket_hook "$FILING" u1
[[ $RC -eq 2 && $OUT == *"TICKET WRITE BLOCKED"* ]] || fail "an unarmed filing was not blocked (rc=$RC out=$OUT)"
[[ ! -e $STATE/u1.ticket.diverted && ! -e $STATE/u1.ticket.failed ]] || fail "an unarmed filing tried to start the worker"
ticket_prompt "don't file the ticket yet" n1
ticket_hook "$FILING" n1
[[ $RC -eq 2 && $OUT == *"TICKET WRITE BLOCKED"* ]] || fail "a refused filing armed the session (rc=$RC out=$OUT)"
ok "an unarmed filing is blocked and starts no worker"

ticket_prompt "file the ticket" a1
ticket_hook "$FILING" a1
[[ $OUT == *"TICKET WORKER NOT STARTED"* ]] || fail "an armed filing did not reach the worker (rc=$RC out=$OUT)"
ok "after \"file the ticket\" a filing goes to the worker"

# ── review-gate ─────────────────────────────────────────────────────────────
# Arming pins the MR from the last review command's own args, never from an
# unrelated !NNN elsewhere in the transcript, and never guesses between two.
review_prompt() { # session command-args -> state files under $HOME
  local sid="$1" args="$2" uuid="${3:-u-$RANDOM}"
  jq -nc --arg u "$uuid" --arg a "$args" \
    '{type:"user",uuid:$u,message:{role:"user",content:
      ("<command-message>gitlab-review</command-message>\n<command-name>/gitlab-review</command-name>\n<command-args>" + $a + "</command-args>")}}' \
    >>"$T/review-$sid.jsonl"
  jq -nc --arg s "$sid" --arg t "$T/review-$sid.jsonl" \
    '{hook_event_name:"UserPromptSubmit",session_id:$s,cwd:"/tmp",transcript_path:$t,
      prompt:"<command-name>/gitlab-review</command-name>"}' |
    bash "$HOOKS/review-gate.sh" >/dev/null 2>&1 || true
}
REVIEW_STATE="$HOME/.local/state/offload-workers"

# An unrelated !99 earlier in the transcript is not this command's MR.
jq -nc '{type:"assistant",uuid:"a0",message:{role:"assistant",content:[{type:"text",text:"see !99 for context"}]}}' >"$T/review-r1.jsonl"
review_prompt r1 "!42"
[[ $(cat "$REVIEW_STATE/r1.armed-mr" 2>/dev/null) == 42 ]] || fail "review arming did not pin MR 42 from the command args"
ok "review arming pins the MR from the command args"

review_prompt r2 "!7 and !8"
[[ -f "$REVIEW_STATE/r2.armed" && -z $(cat "$REVIEW_STATE/r2.armed-mr") ]] ||
  fail "two MRs in the args were not left unpinned"
ok "two MRs in the args are not guessed between"

review_prompt r3 "https://git.example/g/p/-/merge_requests/311"
[[ $(cat "$REVIEW_STATE/r3.armed-mr" 2>/dev/null) == 311 ]] || fail "an MR URL was not pinned"
ok "an MR URL is pinned"

# An explicit poster call passes alone, but not as the front half of a chain
# that also writes to GitLab by another route.
review_tool() { # session bash-command -> $RC, $OUT
  RC=0
  OUT=$(jq -nc --arg s "$1" --arg t "$T/review-r1.jsonl" --arg c "$2" \
    '{hook_event_name:"PreToolUse",tool_name:"Bash",session_id:$s,cwd:"/tmp",
      transcript_path:$t,tool_input:{command:$c}}' | bash "$HOOKS/review-gate.sh" 2>&1) || RC=$?
}
review_tool r1 "bash $REPO_DIR/contrib/offload-workers/review-post.sh --approve $REVIEW_STATE/r1.draft.json"
[[ $RC -eq 0 ]] || fail "a lone --approve call was blocked (rc=$RC out=$OUT)"
review_tool r1 "bash review-post.sh --note 42 abc 'one line'"
[[ $RC -eq 0 ]] || fail "a lone --note call was blocked (rc=$RC out=$OUT)"
review_tool r1 "bash review-post.sh --note 42 abc x; curl -X POST https://git.example/api/v4/projects/1/merge_requests/42/notes -d body=hi"
[[ $RC -eq 2 ]] || fail "a chained direct GitLab write rode on a --note call (rc=$RC out=$OUT)"
review_tool r1 "curl -X POST https://git.example/api/v4/projects/1/merge_requests/42/notes -d body=hi"
[[ $RC -eq 2 ]] || fail "a direct GitLab write was not blocked (rc=$RC out=$OUT)"
ok "poster calls pass alone; a chained direct write is blocked"

# ── ticket_file.py ──────────────────────────────────────────────────────────
# The ticket id comes from the CLI's JSON, not from a scan that "UTF-8" on
# stderr would fool.
cat >"$T/ytcli" <<'CLI'
#!/bin/sh
case "$1" in
  create-draft) echo '{"id":"draft-1"}' ;;
  publish-draft) echo "encoding UTF-8 ok" >&2; echo '{"idReadable":"MC-1611"}' ;;
esac
CLI
chmod +x "$T/ytcli"
GOT=$(cd "$REPO_DIR/contrib/offload-workers" && PYTHONDONTWRITEBYTECODE=1 YOUTRACK_TOKEN=x python3 - "$T" <<'PY'
import sys
import ticket_file as t
plan = {"summary": "s", "description": "d", "type": "default", "publish": True, "assignee": None}
print(t.file_ticket_locally(plan, sys.argv[1], sys.argv[1] + "/ytcli", "1")[0])
plan["publish"] = False
print(t.file_ticket_locally(plan, sys.argv[1], sys.argv[1] + "/ytcli", "1"))
PY
)
[[ $(head -1 <<<"$GOT") == MC-1611 ]] || fail "ticket id read as '$(head -1 <<<"$GOT")', want MC-1611"
ok "the ticket id is read from the CLI's JSON"
[[ $(tail -1 <<<"$GOT") == "('draft-1', None, False)" ]] || fail "an unpublished draft was not a success: $(tail -1 <<<"$GOT")"
ok "a draft the turns asked to leave unpublished is a success, not a failure"

# ── gitlab_api.py ───────────────────────────────────────────────────────────
# The token reaches curl in a private header file, not in argv, and no payload
# or header file outlives the call.
cat >"$T/bin/rtk" <<CLI
#!/bin/sh
echo "\$@" >"$T/rtk.argv"
for a in "\$@"; do
  case "\$a" in
    @*) f=\${a#@}; echo "\$(stat -c %a "\$f") \$f" >>"$T/rtk.files"; cat "\$f" >>"$T/rtk.content" ;;
  esac
done
printf '{}\n200'
CLI
chmod +x "$T/bin/rtk"
: >"$T/rtk.files"
PATH="$T/bin:$PATH" GITLAB_TOKEN=s3cret-token OFFLOAD_GITLAB_BASE_URL=https://git.example/api/v4 \
  OFFLOAD_GITLAB_PROJECT=g/p PYTHONDONTWRITEBYTECODE=1 \
  python3 -c "
import sys; sys.path.insert(0, '$REPO_DIR/contrib/offload-workers')
import gitlab_api as gl
print(gl.call('POST', '/x', {'body': 'hi'}))" >/dev/null
! grep -q s3cret-token "$T/rtk.argv" || fail "the GitLab token is on the curl command line"
grep -q s3cret-token "$T/rtk.content" || fail "the token never reached curl through the header file"
[[ $(awk '{print $1}' "$T/rtk.files" | sort -u) == 600 ]] || fail "a temp file was not mode 600: $(cat "$T/rtk.files")"
while read -r _ f; do [[ ! -e $f ]] || fail "temp file left behind: $f"; done <"$T/rtk.files"
ok "the token and payload go to curl in private files that are removed after"

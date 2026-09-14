#!/bin/bash
# peer-awareness.sh: notice when other agents work in the same repo alongside
# this session — at session start AND when a peer joins midway.
#
# SessionStart: full report of every peer detected (once per session).
# UserPromptSubmit: throttled re-scan (every PEER_RECHECK_SECS); reports only
#   peers that appeared since the last scan, so a newcomer mid-session gets
#   noticed without re-noticing on every prompt.
#
# Detection (best-effort, every source optional):
#   1. Live `claude` processes whose /proc/<pid>/cwd sits in the same git
#      toplevel as this session (Linux; skipped where /proc is absent).
#   2. Recently-modified Claude Code transcripts (*.jsonl) under
#      ~/.claude{,-work,-personal}/projects whose last "cwd" line is inside
#      the same toplevel and whose session id differs from this one.
#      Subagent transcripts (*/subagents/*) are excluded — they belong to a
#      parent session, not a peer.
#   3. Extra git worktrees (`git worktree list`) — structural sharing that
#      outlives any single process.
#   4. spark-review session-map entries (supplementary; only sessions that
#      run the review hooks are logged there, so its absence means nothing).
#   5. Proxy in-flight turns (/debug/active-conversations): conversations
#      streaming through the proxy right now with their resolved project
#      dir. Live presence only — idle agents never appear here.
#   6. Test runs in progress: live test-runner processes (cargo test,
#      pytest, go test, vitest, …) whose cwd sits in this repo but outside
#      MY session's process tree — so your own runs never self-report. One
#      full-repo result covers every session; parallel full runs thrash
#      shared target dirs/caches and CPU while proving nothing twice.
#
# State per session under ~/.local/state/headroom: .peer.reported (the
# SessionStart report went out), .peer.known (peer keys already reported),
# .peer.lastcheck (epoch of the last scan, throttles prompt-time rescans).
# Silent when alone. Never blocks: every error path exits 0. Pure git +
# filesystem — no gh/glab/API, so GitHub and GitLab alike (and no remote).
# Installed by install.sh into ~/.claude/hooks, registered on SessionStart
# and UserPromptSubmit.
PEER_RECHECK_SECS=600
PEER_WINDOW_MIN=180
MAX_PEERS=5
export PATH="$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"
INPUT=$(cat) || exit 0
SESSION=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null | tr -cd 'A-Za-z0-9_-')
CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)
SOURCE=$(echo "$INPUT" | jq -r '.source // empty' 2>/dev/null)
[ -n "$SESSION" ] || exit 0
[ -n "$CWD" ] || exit 0
[ -d "$CWD" ] || exit 0
case "$EVENT" in
  SessionStart|UserPromptSubmit) ;;
  *) exit 0 ;;
esac
# Compaction replays context; re-injecting here would duplicate the notice.
[ "$SOURCE" = "compact" ] && exit 0

STATE_DIR="$HOME/.local/state/headroom"
MARKER="$STATE_DIR/$SESSION.peer.reported"
KNOWN="$STATE_DIR/$SESSION.peer.known"
LASTCHECK="$STATE_DIR/$SESSION.peer.lastcheck"
mkdir -p "$STATE_DIR" 2>/dev/null

now=$(date +%s)
if [ "$EVENT" = "SessionStart" ]; then
  [ -f "$MARKER" ] && exit 0
  MODE="baseline"
else
  # Prompt-time: throttle so every turn does not pay for a rescan.
  if [ -f "$LASTCHECK" ]; then
    last=$(cat "$LASTCHECK" 2>/dev/null) || last=0
    case "$last" in *[!0-9]*) last=0 ;; esac
    if [ $((now - last)) -lt "$PEER_RECHECK_SECS" ]; then exit 0; fi
  fi
  if [ -f "$KNOWN" ]; then MODE="watch"; else MODE="baseline"; fi
fi

TOPLEVEL=$(git -C "$CWD" rev-parse --show-toplevel 2>/dev/null) || TOPLEVEL="$CWD"
REPO=$(basename "$TOPLEVEL")
SCAN_TMP="$STATE_DIR/$SESSION.peer.scan.tmp"
rm -f "$SCAN_TMP"
touch "$SCAN_TMP" 2>/dev/null || exit 0

add_peer() {
  # $1 = stable key, $2 = one-line description. Caps the list so the
  # injection stays small. Dedups against this scan, not against history —
  # the caller diffs history afterwards.
  _key="$1"; _desc="$2"
  grep -qF "$_key|" "$SCAN_TMP" 2>/dev/null && return 0
  _n=$(wc -l <"$SCAN_TMP" 2>/dev/null | tr -d ' ') || _n=0
  [ "$_n" -ge "$MAX_PEERS" ] && return 0
  printf '%s|%s\n' "$_key" "$_desc" >>"$SCAN_TMP"
}

# ── 1. live claude processes in the same toplevel ──
if [ -d /proc ]; then
  # Build one self identity for every process-based detector below. The hook's
  # immediate parent is not guaranteed to be Claude (a shell/launcher may sit
  # between them), so excluding only $$ and $PPID can report this session's
  # own Claude process as a peer. The nearest Claude ancestor is also the root
  # used later to exclude this session's test-runner descendants.
  MINE=" "
  MYROOT=""
  _a=$$
  _depth=0
  while [ -n "$_a" ] && [ "$_a" != "0" ] && [ "$_a" != "1" ] && [ "$_depth" -lt 20 ]; do
    MINE="$MINE$_a "
    if [ -z "$MYROOT" ] && [ -r "/proc/$_a/cmdline" ] \
      && tr '\0' ' ' <"/proc/$_a/cmdline" 2>/dev/null | grep -qE '(^|/)claude( |$)'; then
      MYROOT=$_a
    fi
    _depth=$((_depth + 1))
    _a=$(awk '{print $4}' "/proc/$_a/stat" 2>/dev/null) || break
  done

  # pgrep is not everywhere; ps + grep is. The bracket trick keeps grep
  # itself out of the list without a second filter process.
  for pid in $(ps -eo pid,args 2>/dev/null | grep '[c]laude' | awk '{print $1}'); do
    case "$MINE" in *" $pid "*) continue ;; esac
    # Race: pid may exit between ps and read; -r check keeps the
    # shell's own redirection error off stderr.
    [ -r "/proc/$pid/cmdline" ] || continue
    # Only CLI/agent processes, not our own hook chain or the proxy.
    if ! tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -qE '(^|/)claude( |$)'; then
      continue
    fi
    if tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q 'peer-awareness'; then
      continue
    fi
    pcwd=$(readlink "/proc/$pid/cwd" 2>/dev/null) || continue
    case "$pcwd" in
      "$TOPLEVEL" | "$TOPLEVEL"/*) ;;
      *) continue ;;
    esac
    add_peer "pid:$pid" "live 'claude' process pid $pid in $pcwd"
  done
fi

# ── 2. recent transcripts in the same toplevel ──
# Window: PEER_WINDOW_MIN. Older than that is yesterday's work, not a
# beside-you agent, and warning on it trains everyone to ignore the notice.
_n=$(wc -l <"$SCAN_TMP" 2>/dev/null | tr -d ' ') || _n=0
if [ "$_n" -lt "$MAX_PEERS" ]; then
  for cfg in "$HOME/.claude" "$HOME/.claude-work" "$HOME/.claude-personal"; do
    [ -d "$cfg/projects" ] || continue
    while IFS= read -r t; do
      [ -n "$t" ] || continue
      case "$t" in */subagents/*) continue ;; esac
      base=$(basename "$t" .jsonl)
      [ "$base" = "$SESSION" ] && continue
      tcwd=$(grep -o '"cwd":"[^"]*"' "$t" 2>/dev/null | tail -1 | cut -d'"' -f4) || continue
      [ -n "$tcwd" ] || continue
      case "$tcwd" in
        "$TOPLEVEL" | "$TOPLEVEL"/*) ;;
        *) continue ;;
      esac
      mtime=$(stat -c %Y "$t" 2>/dev/null || stat -f %m "$t" 2>/dev/null) || continue
      age_min=$(((now - mtime) / 60))
      [ "$age_min" -lt 0 ] && age_min=0
      if [ "$age_min" -lt 60 ]; then age_txt="${age_min}m ago"
      else age_txt="$((age_min / 60))h$((age_min % 60))m ago"; fi
      tbranch=$(grep -o '"gitBranch":"[^"]*"' "$t" 2>/dev/null | tail -1 | cut -d'"' -f4)
      [ -n "$tbranch" ] && tbranch=" (branch $tbranch)" || tbranch=""
      add_peer "sess:$base" "session ${base:0:8} cwd $tcwd$tbranch, active $age_txt"
      _n=$(wc -l <"$SCAN_TMP" 2>/dev/null | tr -d ' ') || _n=0
      [ "$_n" -ge "$MAX_PEERS" ] && break
    done <<EOF
$(find "$cfg/projects" -type f -name '*.jsonl' -mmin -$PEER_WINDOW_MIN 2>/dev/null | grep -v '/subagents/' | head -40)
EOF
  done
fi

# ── 3. extra git worktrees ──
WORKTREES=""
if git -C "$CWD" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  WT=$(git -C "$CWD" worktree list --porcelain 2>/dev/null | grep '^worktree ' | cut -d' ' -f2-)
  if [ -n "$WT" ]; then
    n=$(printf '%s\n' "$WT" | wc -l | tr -d ' ')
    if [ "$n" -gt 1 ]; then
      WORKTREES=$(printf '%s\n' "$WT" | grep -vFx "$TOPLEVEL" | head -5 | tr '\n' ' ')
      for _wt in $(printf '%s\n' "$WT" | grep -vFx "$TOPLEVEL" | head -5); do
        add_peer "wt:$_wt" "extra git worktree at $_wt"
      done
    fi
  fi
fi

# ── 4. session-map (supplementary) ──
# Pretty-printed multi-line JSON, so grep — not jq — extracts the fields.
_n=$(wc -l <"$SCAN_TMP" 2>/dev/null | tr -d ' ') || _n=0
MAP="$HOME/.local/state/spark-review/session-map.jsonl"
if [ "$_n" -lt "$MAX_PEERS" ] && [ -f "$MAP" ]; then
  MAP_TMP="$STATE_DIR/$SESSION.peer.map.tmp"
  rm -f "$MAP_TMP"
  tail -c 200000 "$MAP" 2>/dev/null | grep -E '"(session_id|ts|cwd)"' 2>/dev/null |
  paste - - - 2>/dev/null | head -60 | while IFS= read -r line; do
    msid=$(echo "$line" | grep -o '"session_id": *"[^"]*"' | head -1 | cut -d'"' -f4)
    mcwd=$(echo "$line" | grep -o '"cwd": *"[^"]*"' | head -1 | cut -d'"' -f4)
    mts=$(echo "$line" | grep -o '"ts": *[0-9.]*' | head -1 | grep -o '[0-9.]*')
    [ -n "$msid" ] && [ -n "$mcwd" ] || continue
    [ "$msid" = "$SESSION" ] && continue
    case "$mcwd" in "$TOPLEVEL" | "$TOPLEVEL"/*) ;; *) continue ;; esac
    mts_i=$(echo "$mts" | cut -d. -f1)
    age_min=$(((now - mts_i) / 60)) 2>/dev/null || continue
    [ "$age_min" -lt 0 ] && continue
    [ "$age_min" -gt 240 ] && continue
    # Subshell: stash key|desc for the parent to merge.
    printf '%s|%s\n' "sess:$msid" "session ${msid:0:8} cwd $mcwd, seen ${age_min}m ago (session-map)" >>"$MAP_TMP"
  done
  if [ -f "$MAP_TMP" ]; then
    while IFS= read -r entry; do
      _k=$(echo "$entry" | cut -d'|' -f1)
      _d=$(echo "$entry" | cut -d'|' -f2-)
      add_peer "$_k" "$_d"
    done <"$MAP_TMP"
    rm -f "$MAP_TMP"
  fi
fi

# ── 5. proxy in-flight turns (supplementary) ──
# Conversations with a turn streaming through the proxy RIGHT NOW, each with
# the canonical project dir it resolved to (/debug/active-conversations).
# Live presence only: an idle agent has nothing in flight and never appears
# here, so this complements — never replaces — the transcript/process
# signals. Best-effort: proxy down, another listen port, or missing curl/jq
# all skip silently. Own turns cannot appear: hooks run between turns, never
# mid-stream.
_n=$(wc -l <"$SCAN_TMP" 2>/dev/null | tr -d ' ') || _n=0
if [ "$_n" -lt "$MAX_PEERS" ] && command -v curl >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
  PROXY_URL="${HEADROOM_ACTIVE_CONVERSATIONS_URL:-http://127.0.0.1:8787/debug/active-conversations}"
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    _k=$(echo "$entry" | cut -d'|' -f1)
    _p=$(echo "$entry" | cut -d'|' -f2-)
    [ -n "$_k" ] && [ -n "$_p" ] || continue
    case "$_p" in
      "$TOPLEVEL" | "$TOPLEVEL"/*) ;;
      *) continue ;;
    esac
    add_peer "proxy:$_k" "conversation ${_k:0:8} streaming now via proxy in $_p"
  done <<EOF
$(curl -fsS --max-time 2 "$PROXY_URL" 2>/dev/null | jq -r '.conversations[]? | "\(.conversation)|\(.project // empty)"' 2>/dev/null)
EOF
fi

echo "$now" >"$LASTCHECK" 2>/dev/null || true

# ── 6. test runs in progress (same repo, other trees) ──
# Best-effort and advisory only: never blocks, silent when nothing runs.
_n=$(wc -l <"$SCAN_TMP" 2>/dev/null | tr -d ' ') || _n=0
if [ "$_n" -lt "$MAX_PEERS" ] && [ -d /proc ]; then
  # Runner match on the joined command line. Leading (^|[/ ]) and trailing
  # ([ /]|$) boundaries keep `jest` out of `suggests` and `go test` out of
  # `mongo test`; plain advisory output tolerates the residue.
  TEST_RE='(^|[/ ])(cargo (test|nextest)|pytest|py\.test|vitest|jest|mocha|rspec|phpunit|ctest)([ /]|$)|(playwright test|go test|dotnet test|make test)|(npm|yarn|pnpm|bun) (test|run test)|(mvn (test|verify)|(gradle|gradlew) test)'
  ps -eo pid,ppid,etime,args 2>/dev/null | while IFS= read -r line; do
    _pid=$(echo "$line" | awk '{print $1}')
    case "$_pid" in ''|*[!0-9]*|$$) continue ;; esac
    _cmd=$(echo "$line" | awk '{$1=$2=$3=""; sub(/^ +/, ""); print}')
    [ -n "$_cmd" ] || continue
    case "$_cmd" in *peer-awareness*) continue ;; esac
    echo "$_cmd" | grep -qE "$TEST_RE" || continue
    # Mine (my tree) or unmappable: never report.
    _p=$_pid
    _mine=0
    [ "$_p" = "$PPID" ] && _mine=1
    _d=0
    while [ "$_mine" -eq 0 ] && [ -n "$_p" ] && [ "$_p" != "0" ] && [ "$_d" -lt 30 ]; do
      if [ -n "$MYROOT" ] && [ "$_p" = "$MYROOT" ]; then _mine=1; break; fi
      _d=$((_d + 1))
      _p=$(ps -o ppid= -p "$_p" 2>/dev/null | tr -d ' ')
    done
    [ "$_mine" -eq 1 ] && continue
    _pcwd=$(readlink "/proc/$_pid/cwd" 2>/dev/null) || continue
    case "$_pcwd" in
      "$TOPLEVEL" | "$TOPLEVEL"/*) ;;
      *) continue ;;
    esac
    _etime=$(echo "$line" | awk '{print $3}')
    # Full-run guess on the arguments AFTER the runner keyword (argv[0]
    # paths contain slashes, so the whole line always looks scoped).
    # Scoped runs name a file, path, or filter; bare invocations run the
    # whole suite. `go test ./...` is full despite the slash.
    case "$_cmd" in
      *"cargo test"*|*"cargo nextest"*)
        _rest=${_cmd#*cargo }; _rest=${_rest#test }; _rest=${_rest#nextest }; _rest=${_rest#run } ;;
      *"pytest"*|*"py.test"*)
        _rest=${_cmd#*pytest }; _rest=${_rest#py.test } ;;
      *"go test"*) _rest=${_cmd#*go test } ;;
      *"playwright test"*) _rest=${_cmd#*playwright test } ;;
      *"vitest"*) _rest=${_cmd#*vitest }; _rest=${_rest#run } ;;
      *"jest"*) _rest=${_cmd#*jest } ;;
      *"mocha"*) _rest=${_cmd#*mocha } ;;
      *"rspec"*) _rest=${_cmd#*rspec } ;;
      *"dotnet test"*) _rest=${_cmd#*dotnet test } ;;
      *"phpunit"*) _rest=${_cmd#*phpunit } ;;
      *) _rest="" ;;
    esac
    case "$_cmd" in
      *"go test"*) _rest=$(echo "$_rest" | sed 's|\./\.\.\.||g') ;;
    esac
    case "$_rest" in
      */*|*::*|*.py*|*.rs*|*.ts*|*.js*|*.go*|*.rb*|*.php*|*" -k "*|*" -t "*|*" -run "*|*--run*) _scope="scoped subset" ;;
      *) _scope="looks like a FULL run" ;;
    esac
    _short=$(echo "$_cmd" | cut -c1-160)
    # $_cmd is untrusted peer text interpolated below inside double quotes:
    # strip backticks and neutralize `$` so a peer's $(...) or $VAR can
    # neither execute here nor vanish into expansions.
    _short=$(printf '%s' "$_short" | tr -d '`' | sed 's/\$/\\$/g')
    add_peer "test:$_pid" "TEST RUN ($_scope): \`$_short\` (pid $_pid, elapsed ${_etime:-?}, cwd $_pcwd) — started outside your session. One full-repo result covers every agent: do NOT start a duplicate full run (parallel full runs thrash shared target dirs/caches and CPU while proving nothing twice). Wait for it, or run only scoped tests for files you touched."
    _n=$(wc -l <"$SCAN_TMP" 2>/dev/null | tr -d ' ') || _n=0
    [ "$_n" -ge "$MAX_PEERS" ] && break
  done
fi

# ── diff against already-reported peers ──
REPORT_TMP="$STATE_DIR/$SESSION.peer.report.tmp"
rm -f "$REPORT_TMP"
touch "$REPORT_TMP" 2>/dev/null || exit 0
if [ "$MODE" = "watch" ] && [ -f "$KNOWN" ]; then
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    _k=$(echo "$entry" | cut -d'|' -f1)
    grep -qxF "$_k" "$KNOWN" 2>/dev/null && continue
    printf '%s\n' "$entry" >>"$REPORT_TMP"
  done <"$SCAN_TMP"
else
  cp "$SCAN_TMP" "$REPORT_TMP" 2>/dev/null || exit 0
fi
# Remember everything seen, reported or not, so the next watch diff is exact.
awk -F'|' '{print $1}' "$SCAN_TMP" 2>/dev/null | sort -u >"$KNOWN.tmp" 2>/dev/null
if [ -f "$KNOWN" ]; then
  cat "$KNOWN" "$KNOWN.tmp" 2>/dev/null | sort -u >"$KNOWN.new" 2>/dev/null && mv "$KNOWN.new" "$KNOWN" 2>/dev/null
else
  mv "$KNOWN.tmp" "$KNOWN" 2>/dev/null
fi
rm -f "$SCAN_TMP"

NEED_REPORT=0
[ -s "$REPORT_TMP" ] && NEED_REPORT=1
# Worktrees are structural: mention them on the baseline report even if their
# peer keys were somehow already known.
if [ "$MODE" = "baseline" ] && [ -n "$WORKTREES" ]; then NEED_REPORT=1; fi
[ "$NEED_REPORT" -eq 0 ] && { rm -f "$REPORT_TMP"; exit 0; }

# ── short shared-worktree protocol (from SHARED-WORKTREE-PROTOCOL.md) ──
# Rule 4 names the formatters that actually threaten THIS repo: probe its
# stack markers so a Python/Go/TS checkout does not get Rust-only advice.
FMTS=""
[ -f "$TOPLEVEL/Cargo.toml" ] && FMTS="cargo fmt (rewrites the whole workspace even with file args)"
if [ -f "$TOPLEVEL/go.mod" ]; then FMTS="${FMTS:+$FMTS, }gofmt/goimports, go fmt ./..."; fi
if [ -f "$TOPLEVEL/package.json" ]; then FMTS="${FMTS:+$FMTS, }prettier --write, eslint --fix"; fi
if [ -f "$TOPLEVEL/pyproject.toml" ] || [ -f "$TOPLEVEL/setup.py" ] || [ -f "$TOPLEVEL/setup.cfg" ] || [ -n "$(ls "$TOPLEVEL"/requirements*.txt 2>/dev/null)" ]; then
  FMTS="${FMTS:+$FMTS, }black, ruff format"
fi
[ -n "$FMTS" ] || FMTS="repo-wide formatters (anything with --write/--fix)"
if [ -f "$TOPLEVEL/SHARED-WORKTREE-PROTOCOL.md" ]; then
  RULES="Shared-worktree rules (SHARED-WORKTREE-PROTOCOL.md is binding in this repo): 1) NEVER run worktree-wide destructive commands: git checkout -- ., git restore ., git clean (any flags), git reset --hard, git stash -u. 2) Stage/commit ONLY files you authored: git add <your paths>, never -A/.; never stage, unstage, or commit another session's files. 3) Commit your own work promptly so it survives other sessions' mistakes. 4) Whole-tree formatters ($FMTS): scope them to your files; check git status/diff --stat after and revert hunks in files you don't own (read them first). Check the protocol's Current file claims section and add your own claim."
elif [ -f "$TOPLEVEL/AGENTS.md" ]; then
  RULES="Shared-worktree rules: never run repo-wide destructive commands (checkout -- ., restore ., clean, reset --hard, stash -u); stage/commit ONLY files you authored (git add <paths>, never -A/.); commit promptly; scope whole-tree formatters ($FMTS) to your files and check git status/diff --stat after; check AGENTS.md for repo-local coordination rules."
else
  RULES="Shared-worktree rules: never run repo-wide destructive commands (checkout -- ., restore ., clean, reset --hard, stash -u); stage/commit ONLY files you authored (git add <paths>, never -A/.); commit promptly so your work survives other sessions; scope whole-tree formatters ($FMTS) to your files and check git status/diff --stat after."
fi

if [ "$MODE" = "watch" ]; then
  HEAD="NEW PEER AGENT(S) in repo '$REPO' ($TOPLEVEL) — joined since your last check:"
else
  HEAD="PARALLEL AGENTS in repo '$REPO' ($TOPLEVEL) — you are NOT the only session here:"
fi
LINES=$(awk '{ if (match($0, /\|/)) print "- " substr($0, RSTART+1); else print "- " $0 }' "$REPORT_TMP" | head -"$MAX_PEERS")
MSG="$HEAD
$LINES"
if [ "$MODE" = "baseline" ] && [ -n "$WORKTREES" ]; then
  MSG="$MSG
Extra git worktree(s): $WORKTREES"
fi
MSG="$MSG
$RULES Before editing: git status --short and git diff --stat to see what moved under you."
rm -f "$REPORT_TMP"

[ "$EVENT" = "SessionStart" ] && touch "$MARKER" 2>/dev/null || true

if command -v jq >/dev/null 2>&1; then
  jq -n --arg ctx "$MSG" --arg ev "$EVENT" '{hookSpecificOutput:{hookEventName:$ev,additionalContext:$ctx}}'
else
  printf '%s\n' "$MSG"
fi
exit 0

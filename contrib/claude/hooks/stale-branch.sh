#!/bin/bash
# stale-branch.sh: SessionStart hook — one-time notice when the checked-out
# branch is stale relative to its origin counterpart.
#
# Pure git plumbing, no forge CLI or API, so GitHub and GitLab (and any other
# remote) behave identically: the only question asked is what the remote
# tracking ref and — when reachable — the live remote say.
#
# What it reports (only when actionable):
#   - behind: local branch trails origin/<branch> by N commits
#   - upstream gone: origin/<branch> was deleted upstream
#   - remote moved: `git ls-remote` (5s, read-only, no state change) shows a
#     SHA the local origin/<branch> does not have yet — i.e. the last fetch
#     is stale and the behind-count above understates reality
# Silent when the branch is up to date or ahead-only, when there is no remote
# or no origin counterpart yet, outside a git repo, or on detached HEAD.
# Never fetches (no state change, no auth prompts: GIT_TERMINAL_PROMPT=0).
# Never blocks: every error path exits 0.
# Installed by install.sh into ~/.claude/hooks, registered on SessionStart.
export PATH="$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"
export GIT_TERMINAL_PROMPT=0
INPUT=$(cat) || exit 0
SESSION=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null | tr -cd 'A-Za-z0-9_-')
CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
SOURCE=$(echo "$INPUT" | jq -r '.source // empty' 2>/dev/null)
[ -n "$CWD" ] || exit 0
[ -d "$CWD" ] || exit 0
[ "$SOURCE" = "compact" ] && exit 0

STATE_DIR="$HOME/.local/state/headroom"
MARKER=""
[ -n "$SESSION" ] && MARKER="$STATE_DIR/$SESSION.stale.reported"
[ -n "$MARKER" ] && [ -f "$MARKER" ] && exit 0

git -C "$CWD" rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0
BRANCH=$(git -C "$CWD" branch --show-current 2>/dev/null) || exit 0
[ -n "$BRANCH" ] || exit 0  # detached HEAD: nothing to compare

# ── resolve the comparison ref ──
UPSTREAM=$(git -C "$CWD" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null)
UPSTREAM_GONE=""
if [ -n "$UPSTREAM" ]; then
  if ! git -C "$CWD" rev-parse --verify --quiet "$UPSTREAM" >/dev/null 2>&1; then
    # Upstream configured but its tracking ref is gone (branch deleted at
    # origin). rev-parse prints the literal "@{u}" when nothing resolves,
    # so rebuild the display name from the branch config instead.
    RCFG=$(git -C "$CWD" config --get "branch.$BRANCH.remote" 2>/dev/null)
    [ -n "$RCFG" ] || RCFG="origin"
    MCFG=$(git -C "$CWD" config --get "branch.$BRANCH.merge" 2>/dev/null)
    MB=${MCFG##refs/heads/}
    if [ -n "$MB" ] && [ "$MB" != "$MCFG" ]; then UPSTREAM_GONE="$RCFG/$MB"
    else UPSTREAM_GONE="$UPSTREAM"; fi
    UPSTREAM=""
  fi
fi
if [ -z "$UPSTREAM" ] && [ -z "$UPSTREAM_GONE" ]; then
  # No upstream configured: fall back to origin/<branch> when this session's
  # remote actually has it. Remote name follows the branch's config, else origin.
  REMOTE=$(git -C "$CWD" config --get "branch.$BRANCH.remote" 2>/dev/null)
  [ -n "$REMOTE" ] || REMOTE="origin"
  if git -C "$CWD" rev-parse --verify --quiet "refs/remotes/$REMOTE/$BRANCH" >/dev/null 2>&1; then
    UPSTREAM="$REMOTE/$BRANCH"
  else
    exit 0  # local-only branch, nothing pushed yet: not stale, stay silent
  fi
fi

if [ -n "$UPSTREAM_GONE" ]; then
  MSG="STALE BRANCH: '$BRANCH' tracks '$UPSTREAM_GONE', which no longer exists upstream (deleted at origin or never fetched). Your local branch may be based on abandoned work. Check with the branch owner before pushing; 'git branch --unset-upstream' clears it if the deletion was intentional."
  mkdir -p "$STATE_DIR" 2>/dev/null
  [ -n "$MARKER" ] && touch "$MARKER" 2>/dev/null || true
  if command -v jq >/dev/null 2>&1; then
    jq -n --arg ctx "$MSG" '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$ctx}}'
  else
    printf '%s\n' "$MSG"
  fi
  exit 0
fi

# ── ahead/behind against the tracking ref (no network) ──
COUNTS=$(git -C "$CWD" rev-list --left-right --count "$UPSTREAM...HEAD" 2>/dev/null) || exit 0
BEHIND=$(echo "$COUNTS" | awk '{print $1}')
AHEAD=$(echo "$COUNTS" | awk '{print $2}')
case "$BEHIND$AHEAD" in *[!0-9]*) exit 0 ;; esac
[ -n "$BEHIND" ] && [ -n "$AHEAD" ] || exit 0

# ── has origin moved since the last fetch? (read-only ls-remote, 5s) ──
REMOTE_MOVED=""
FETCH_AGE_TXT=""
GITDIR=$(git -C "$CWD" rev-parse --git-dir 2>/dev/null)
if [ -n "$GITDIR" ]; then
  case "$GITDIR" in /*) FETCH_HEAD="$GITDIR/FETCH_HEAD" ;; *) FETCH_HEAD="$CWD/$GITDIR/FETCH_HEAD" ;; esac
  if [ -f "$FETCH_HEAD" ]; then
    fts=$(stat -c %Y "$FETCH_HEAD" 2>/dev/null || stat -f %m "$FETCH_HEAD" 2>/dev/null) || fts=""
    if [ -n "$fts" ]; then
      now=$(date +%s)
      age_h=$(((now - fts) / 3600))
      if [ "$age_h" -lt 1 ]; then FETCH_AGE_TXT="last fetch <1h ago"
      elif [ "$age_h" -lt 48 ]; then FETCH_AGE_TXT="last fetch ${age_h}h ago"
      else FETCH_AGE_TXT="last fetch $((age_h / 24))d ago"; fi
    fi
  else
    FETCH_AGE_TXT=""
  fi
fi
REMOTE_NAME=$(echo "$UPSTREAM" | cut -d/ -f1)
if command -v timeout >/dev/null 2>&1; then
  LIVE_SHA=$(timeout 5 git -C "$CWD" ls-remote "$REMOTE_NAME" "refs/heads/$BRANCH" 2>/dev/null | awk '{print $1}')
else
  LIVE_SHA=$(git -C "$CWD" ls-remote "$REMOTE_NAME" "refs/heads/$BRANCH" 2>/dev/null | awk '{print $1}')
fi
if [ -n "$LIVE_SHA" ]; then
  LOCAL_UP_SHA=$(git -C "$CWD" rev-parse --verify --quiet "$UPSTREAM" 2>/dev/null)
  if [ -n "$LOCAL_UP_SHA" ] && [ "$LIVE_SHA" != "$LOCAL_UP_SHA" ]; then
    REMOTE_MOVED="origin/$BRANCH moved since the last fetch (tracking ref $LOCAL_UP_SHA, live origin $LIVE_SHA)"
  fi
fi
# ls-remote unreachable (offline, auth, timeout): degrade to tracking-ref
# counts silently — no warning, the numbers below are labelled as such.

# ── decide: silent unless actionable ──
[ "$BEHIND" -gt 0 ] 2>/dev/null || { [ -n "$REMOTE_MOVED" ] || exit 0; }

# ── notify once ──
if [ "$BEHIND" -gt 0 ] 2>/dev/null; then
  if [ "$AHEAD" -gt 0 ] 2>/dev/null; then
    STATE="is behind $UPSTREAM by $BEHIND commit(s) and ahead by $AHEAD (diverged)"
  else
    STATE="is behind $UPSTREAM by $BEHIND commit(s)"
  fi
else
  STATE="matches the last-fetched $UPSTREAM, but origin has new commits"
fi
MSG="STALE BRANCH: '$BRANCH' $STATE."
[ -n "$FETCH_AGE_TXT" ] && MSG="$MSG ($FETCH_AGE_TXT.)"
[ -n "$REMOTE_MOVED" ] && MSG="$MSG $REMOTE_MOVED — the behind-count above understates it; fetch first."
MSG="$MSG Run: git fetch $REMOTE_NAME && git status -sb && git log --oneline HEAD..$UPSTREAM. Rebase/merge only after checking for parallel agents in this repo; never reset --hard shared work."
# Preview what is missing (capped: 5 lines, short SHAs).
MISSING=$(git -C "$CWD" log --oneline --max-count=5 "$UPSTREAM" --not HEAD 2>/dev/null | head -5)
if [ -z "$MISSING" ] && [ -n "$REMOTE_MOVED" ]; then
  MSG="$MSG (Missing commits unknown until you fetch.)"
elif [ -n "$MISSING" ]; then
  MSG="$MSG Missing:
$MISSING"
fi

mkdir -p "$STATE_DIR" 2>/dev/null
[ -n "$MARKER" ] && touch "$MARKER" 2>/dev/null || true
if command -v jq >/dev/null 2>&1; then
  jq -n --arg ctx "$MSG" '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$ctx}}'
else
  printf '%s\n' "$MSG"
fi
exit 0

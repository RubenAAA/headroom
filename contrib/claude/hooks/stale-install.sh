#!/bin/bash
# stale-install.sh: SessionStart hook — one-time notice when the headroom
# checkout moved under the installed copies.
#
# install.sh copies scripts, hooks, agents and the statusline into ~/.local/bin,
# ~/.claude and $HOME once; a later `git pull` on the checkout leaves those
# copies stale until install.sh runs again. Under --link most of those are
# symlinks that follow the checkout, but the binaries are always copied and
# still need a rebuild plus restart-headroom.sh.
#
# What it reports (only when actionable, only inside the headroom checkout):
#   - installed copies differ from the checkout (copy mode): hook scripts,
#     ~/.local/bin scripts, statusline scripts -> rerun install.sh (or
#     update-headroom.sh for the full pull+install+restart round)
#   - installed binary older than the checkout's built binary: even under
#     --link the proxy binary is a copy, so a Rust change still needs a
#     restart onto the rebuild
#
# Silent everywhere else (no HEADROOM_REPO match, up to date, any error).
# Read-only: compares hashes, changes nothing. Never blocks.
export PATH="$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"
INPUT=$(cat) || exit 0
SESSION=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null | tr -cd 'A-Za-z0-9_-')
CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
SOURCE=$(echo "$INPUT" | jq -r '.source // empty' 2>/dev/null)
[ -n "$CWD" ] || exit 0
[ -d "$CWD" ] || exit 0
[ "$SOURCE" = "compact" ] && exit 0

STATE_DIR="$HOME/.local/state/headroom"
MARKER=""
[ -n "$SESSION" ] && MARKER="$STATE_DIR/$SESSION.stale-install.reported"
[ -n "$MARKER" ] && [ -f "$MARKER" ] && exit 0

# Only fire inside the headroom checkout itself: HEADROOM_REPO names it, and
# the cwd must be at or under it. Anywhere else this hook has nothing to say.
REPO=""
[ -r "$HOME/.headroom-paths.sh" ] && REPO=$( (source "$HOME/.headroom-paths.sh" 2>/dev/null; printf '%s' "${HEADROOM_REPO:-}") )
[ -n "$REPO" ] || exit 0
[ -d "$REPO/contrib" ] || exit 0
case "$CWD" in "$REPO"|"$REPO"/*) ;; *) exit 0 ;; esac

CONTRIB="$REPO/contrib"
STALE=()

# Installed hook copies vs the checkout (copy mode only; under --link these
# are symlinks that track the checkout on their own).
for src in "$CONTRIB"/claude/hooks/*.sh; do
    [ -f "$src" ] || continue
    dst="$HOME/.claude/hooks/$(basename "$src")"
    [ -f "$dst" ] || { STALE+=("$(basename "$src") (missing from ~/.claude/hooks)"); continue; }
    [ -L "$dst" ] && continue
    cmp -s "$src" "$dst" 2>/dev/null || STALE+=("$(basename "$src")")
done

# ~/.local/bin scripts vs the checkout (copy mode only, same reasoning).
for name in claude-launcher restart-headroom.sh zen-rotate-watch.sh headroom-rss-sample update-headroom.sh concurrency-report.sh; do
    src="$CONTRIB/$name"
    [ -f "$src" ] || continue
    dst="$HOME/.local/bin/$name"
    [ -f "$dst" ] || [ -L "$dst" ] || { STALE+=("$name (missing from ~/.local/bin)"); continue; }
    [ -L "$dst" ] && continue
    cmp -s "$src" "$dst" 2>/dev/null || STALE+=("$name")
done

# The proxy binary is always a copy, in either mode: flag it when the
# checkout holds a newer build than what is installed.
NEW_BIN="$REPO/target/release/headroom-proxy"
LIVE_BIN="$HOME/.local/bin/headroom-proxy"
if [ -f "$NEW_BIN" ] && [ -f "$LIVE_BIN" ] && [ ! -L "$LIVE_BIN" ]; then
    cmp -s "$NEW_BIN" "$LIVE_BIN" 2>/dev/null || STALE+=("headroom-proxy binary")
fi

[ "${#STALE[@]}" -gt 0 ] || exit 0

MSG="STALE INSTALL: the headroom checkout moved under the installed copies: ${STALE[*]}. Run update-headroom.sh (pull + reinstall + proxy restart) or ./install.sh in $REPO."

mkdir -p "$STATE_DIR" 2>/dev/null
[ -n "$MARKER" ] && touch "$MARKER" 2>/dev/null || true
if command -v jq >/dev/null 2>&1; then
  jq -n --arg ctx "$MSG" '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$ctx}}'
else
  printf '%s\n' "$MSG"
fi
exit 0

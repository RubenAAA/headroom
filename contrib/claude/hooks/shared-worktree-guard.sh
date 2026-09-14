#!/bin/bash
# shared-worktree-guard.sh: block the destructive git commands banned by
# .agents/SHARED-WORKTREE-PROTOCOL.md while another agent session shares
# this checkout.
#
# The protocol exists because uncommitted work has twice been wiped here by
# one session reverting files it did not own — worktree-wide on 2026-09-11,
# a single file on 2026-09-14. Both were one keystroke and unrecoverable
# except through dangling blobs. A note in a doc did not stop either.
#
# PreToolUse on Bash. Blocked (exit 2) only when BOTH hold:
#   1. the command runs one of the banned forms, and
#   2. another agent is live in the same git toplevel.
# Alone in the repo, everything passes. Peer detection mirrors
# peer-awareness.sh: live `claude` processes outside my own process tree,
# and Claude Code transcripts touched in the last PEER_WINDOW_MIN whose cwd
# sits in this toplevel.
#
# Banned forms:
#   git clean (any flags)          git reset --hard
#   git stash -u / --all           git add -A / .
#   git checkout|restore of the whole tree (. :/ * <toplevel>)
#   git checkout -- <paths> / git restore <paths>   (reverts files that may
#     belong to another session; `git restore --staged` alone is allowed,
#     it cannot lose worktree content)
#
# Override for a deliberate run: prefix the command with
# HEADROOM_WORKTREE_GUARD=off. Never blocks on its own errors: every
# failure path exits 0.

PEER_WINDOW_MIN=180
export PATH="$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"

INPUT=$(cat) || exit 0
command -v jq >/dev/null 2>&1 || exit 0
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
CWD=$(echo "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
SESSION=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null | tr -cd 'A-Za-z0-9_-')
[ -n "$CMD" ] || exit 0
[ -n "$CWD" ] || CWD=$PWD

case "$CMD" in *HEADROOM_WORKTREE_GUARD=off*) exit 0 ;; esac
case "$CMD" in *git*) ;; *) exit 0 ;; esac

TOPLEVEL=$(git -C "$CWD" rev-parse --show-toplevel 2>/dev/null) || exit 0
[ -n "$TOPLEVEL" ] || exit 0

# ── does this command run a banned form? ──
# Split on the shell separators, so `cd x && git clean -fd` is caught and
# `echo "git clean"` is not: a segment counts only when git leads it.
REASON=""
HIT=""
while IFS= read -r seg; do
  [ -n "$REASON" ] && break
  # Strip quoting and leading env assignments / sudo, squeeze blanks.
  g=$(printf '%s' "$seg" | tr -d "\"'" | tr -s ' \t' ' ' | sed 's/^ //; s/ $//')
  while :; do
    case "$g" in
      [A-Za-z_]*=*\ *) g=${g#* } ;;
      "sudo "*) g=${g#sudo } ;;
      *) break ;;
    esac
  done
  case "$g" in "git "*) ;; *) continue ;; esac
  # Drop the global options that take a value, so `git -C dir clean` reads
  # the same as `git clean`.
  g=${g#git }
  while :; do
    case "$g" in
      "-C "*) g=${g#-C } ; g=${g#* } ;;
      "-c "*) g=${g#-c } ; g=${g#* } ;;
      "--git-dir="*|"--work-tree="*) g=${g#* } ;;
      *) break ;;
    esac
  done
  read -ra T <<<"$g"
  sub=${T[0]}
  args=("${T[@]:1}")

  case "$sub" in
    clean)
      REASON="git clean deletes untracked files across the tree"
      ;;
    reset)
      for a in "${args[@]}"; do
        [ "$a" = "--hard" ] && REASON="git reset --hard discards every uncommitted change in the tree"
      done
      ;;
    stash)
      for a in "${args[@]}"; do
        case "$a" in
          -u|--include-untracked|-a|--all)
            REASON="git stash $a sweeps up untracked files, including other sessions'" ;;
        esac
      done
      ;;
    add)
      for a in "${args[@]}"; do
        case "$a" in
          -A|--all|.|:/|"$TOPLEVEL"|"$TOPLEVEL"/)
            REASON="git add $a stages files you did not author" ;;
        esac
      done
      ;;
    checkout|restore)
      dashdash=0
      paths=()
      positional=()
      skip=0
      for a in "${args[@]}"; do
        if [ "$skip" = 1 ]; then skip=0; continue; fi
        if [ "$dashdash" = 1 ]; then paths+=("$a"); continue; fi
        case "$a" in
          --) dashdash=1 ;;
          -s|--source|-b|-B|--orphan|-t|--track) skip=1 ;;
          -*) ;;
          *) positional+=("$a") ;;
        esac
      done
      if [ "$dashdash" = 0 ] && [ ${#positional[@]} -gt 0 ]; then
        if [ "$sub" = restore ]; then
          # `git restore` has no branch-switch form: every positional is a
          # pathspec, whether or not the optional `--` separator is present.
          paths+=("${positional[@]}")
        elif git -C "$CWD" rev-parse --verify --quiet "${positional[0]}^{commit}" >/dev/null 2>&1; then
          # Checkout's first positional is a branch/tree-ish when it resolves
          # as one. Any remaining positionals are pathspecs (`git checkout
          # HEAD file`) even without `--`.
          [ ${#positional[@]} -gt 1 ] && paths+=("${positional[@]:1}")
        else
          # No branch/tree-ish by this name, so Git interprets the arguments
          # as pathspecs (`git checkout file`). The old parser ignored this
          # form and let a single-file destructive checkout past the guard.
          paths+=("${positional[@]}")
        fi
      fi
      wide=0
      for p in "${paths[@]}"; do
        case "$p" in
          .|./|:/|'*'|"$TOPLEVEL"|"$TOPLEVEL"/) wide=1 ;;
        esac
      done
      if [ "$wide" = 1 ]; then
        REASON="git $sub of the whole tree throws away every uncommitted change in it"
      elif [ ${#paths[@]} -gt 0 ]; then
        # --staged on its own only unstages; it cannot lose file content.
        staged_only=0
        if [ "$sub" = restore ]; then
          staged_only=1
          for a in "${args[@]}"; do
            case "$a" in
              --staged|-S) ;;
              --worktree|-W) staged_only=0 ;;
              -*) ;;
            esac
          done
          has_staged=0
          for a in "${args[@]}"; do
            case "$a" in --staged|-S) has_staged=1 ;; esac
          done
          [ "$has_staged" = 1 ] || staged_only=0
        fi
        [ "$staged_only" = 1 ] || REASON="git $sub reverts ${paths[*]} to the committed version, dropping uncommitted work in files another session may own"
      fi
      ;;
  esac
  [ -n "$REASON" ] && HIT="git $g"
done <<EOF
$(printf '%s\n' "$CMD" | sed 's/&&/\n/g; s/||/\n/g; s/;/\n/g; s/|/\n/g')
EOF

[ -n "$REASON" ] || exit 0

# ── is anyone else working here? ──
now=$(date +%s)
PEERS=""
add() { PEERS="$PEERS  - $1
"; }

# Ancestors of this hook are my own session; never report myself.
MINE=" "
p=$$
n=0
while [ -n "$p" ] && [ "$p" != 0 ] && [ "$p" != 1 ] && [ "$n" -lt 20 ]; do
  MINE="$MINE$p "
  p=$(awk '{print $4}' "/proc/$p/stat" 2>/dev/null) || break
  n=$((n + 1))
done

if [ -d /proc ]; then
  for pid in $(ps -eo pid,args 2>/dev/null | grep '[c]laude' | awk '{print $1}'); do
    case "$MINE" in *" $pid "*) continue ;; esac
    [ -r "/proc/$pid/cmdline" ] || continue
    tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -qE '(^|/)claude( |$)' || continue
    pcwd=$(readlink "/proc/$pid/cwd" 2>/dev/null) || continue
    case "$pcwd" in "$TOPLEVEL" | "$TOPLEVEL"/*) ;; *) continue ;; esac
    add "live claude process pid $pid in $pcwd"
  done
fi

for cfg in "$HOME/.claude" "$HOME/.claude-work" "$HOME/.claude-personal"; do
  [ -d "$cfg/projects" ] || continue
  while IFS= read -r t; do
    [ -n "$t" ] || continue
    base=$(basename "$t" .jsonl)
    [ "$base" = "$SESSION" ] && continue
    tcwd=$(grep -o '"cwd":"[^"]*"' "$t" 2>/dev/null | tail -1 | cut -d'"' -f4) || continue
    case "$tcwd" in "$TOPLEVEL" | "$TOPLEVEL"/*) ;; *) continue ;; esac
    mtime=$(stat -c %Y "$t" 2>/dev/null || stat -f %m "$t" 2>/dev/null) || continue
    age=$(((now - mtime) / 60))
    [ "$age" -lt 0 ] && age=0
    add "session ${base:0:8}, active ${age}m ago in $tcwd"
  done <<EOF2
$(find "$cfg/projects" -type f -name '*.jsonl' -mmin -$PEER_WINDOW_MIN 2>/dev/null | grep -v '/subagents/' | head -40)
EOF2
done

[ -n "$PEERS" ] || exit 0
PEERS=$(printf '%s' "$PEERS" | sort -u)

DOC="$TOPLEVEL/.agents/SHARED-WORKTREE-PROTOCOL.md"
[ -f "$DOC" ] || DOC="$TOPLEVEL/SHARED-WORKTREE-PROTOCOL.md"

{
  echo "Blocked: \`$HIT\`"
  echo
  echo "$REASON, and other agents are working in $TOPLEVEL right now:"
  echo "$PEERS"
  echo
  echo "Their uncommitted work is in this tree and git will not give it back."
  [ -f "$DOC" ] && echo "Rules: ${DOC#"$TOPLEVEL"/}"
  echo
  echo "Instead: commit or stash only your own paths (git add <your files>),"
  echo "or copy the file aside before you revert it. If you truly mean to run"
  echo "this, repeat it as: HEADROOM_WORKTREE_GUARD=off $HIT"
} >&2
exit 2

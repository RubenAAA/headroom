#!/bin/bash
# install-credential.sh: move GITLAB_TOKEN out of the shell's ambient env.
#
# Before: `.bashrc` exports GITLAB_TOKEN, so every process the reviewing model
# starts inherits a credential that can post to GitLab. The divert gate is then
# the only thing standing between an articulated review and a live write, and a
# gate that matches on command *shape* is a tripwire, not a lock -- `python3 -c`
# with urllib and no JSON keywords walks straight through it.
#
# After: the token lives in one file that only the poster reads. Opus's shell
# has nothing to offer a process it spawns, so walking through the gate buys
# nothing -- there is no credential on the other side.
#
# What this does NOT do, and it matters: it is not a permission boundary. The
# poster runs as the same Unix user as the reviewing model, so anything that can
# run `cat` can still read the token file. Closing that needs a second Unix user
# (or a keyring with per-process ACLs), which needs root on this box. What this
# buys is the removal of *ambient* authority: the credential stops arriving
# uninvited in every subprocess, and any use of it becomes a deliberate,
# greppable act rather than an accident waiting to happen.
#
# Reversible: the .bashrc line is commented, not deleted.

set -euo pipefail

DEST_DIR="$HOME/.config/spark-poster"
DEST="$DEST_DIR/token"
BASHRC="$HOME/.bashrc"

if [ -z "${GITLAB_TOKEN:-}" ] && [ ! -s "$DEST" ]; then
  echo "GITLAB_TOKEN is not set and $DEST is empty; nothing to move." >&2
  echo "Run this once from a shell that still has the token." >&2
  exit 1
fi

mkdir -p "$DEST_DIR"
chmod 700 "$DEST_DIR"

if [ -n "${GITLAB_TOKEN:-}" ]; then
  umask 077
  printf '%s' "$GITLAB_TOKEN" > "$DEST"
  chmod 600 "$DEST"
  echo "wrote credential to $DEST (mode 600, $(wc -c < "$DEST") bytes)"
else
  echo "keeping existing $DEST"
fi

if grep -qE '^[[:space:]]*export GITLAB_TOKEN=' "$BASHRC"; then
  cp -n "$BASHRC" "$BASHRC.pre-spark-poster"
  sed -i -E 's|^([[:space:]]*export GITLAB_TOKEN=.*)$|# moved to ~/.config/spark-poster/token by contrib/spark-poster/install-credential.sh\n#\1|' "$BASHRC"
  echo "commented the export in $BASHRC (backup: $BASHRC.pre-spark-poster)"
else
  echo "no active export in $BASHRC; nothing to comment"
fi

echo
echo "Verify from a NEW shell:  [ -z \"\$GITLAB_TOKEN\" ] && echo isolated"
echo "The poster reads $DEST directly and needs no environment."

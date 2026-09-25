#!/bin/bash
# scrub-placeholders.sh: block redaction tokens leaking into code, docs, and commands.
#
# Tool output masks real paths/secrets as __HR_PATH_<hex>__, __HR_SECRET_<hex>__,
# __HR_EMAIL_<hex>__, __HR_HOME__, etc. Those tokens are display masks, not real values: pasted
# into a file they land literally and break whatever references them (a bare
# __HR_PATH_...__ line in a shell script fails as "command not found" at load);
# pasted into a command they fail opaquely (a grep for a faked request id
# matches nothing, and on 2026-09-22 a model that had seen the marker format
# began emitting invented tokens in its own tool calls until the session
# wedged on ungreppable ids).
# $HOME is exempt: the tools expand it to the real home dir on write.
#
# PreToolUse on Write|Edit|MultiEdit, Bash and Skill. Reads the hook JSON from stdin,
# scans the whole tool input for the token shape, exits 2 to block with the
# offending token named. Never fails closed: any internal error exits 0 so a
# broken guard never stops work.
# Installed into ~/.claude/hooks by install.sh.
#
# Bash scope is concrete tokens only in effect: the shape regex needs a full
# `__HR_KIND_<hex>__`, so greps for the mechanism itself ("headroom:
# unresolved", `__HR_SECRET` bare, regexes) pass untouched — only a pasted
# concrete token trips it. Two escapes for the rare legit case (searching a
# log for one specific token): set HEADROOM_SCRUB_BASH=0 in the session
# environment to skip Bash blocking while leaving the write paths guarded.
INPUT=$(cat) || exit 0

# Operator override for token-shape archaeology; writes stay guarded.
# Only the Bash tool path is skipped: Write|Edit|MultiEdit must still block.
TOOL_NAME_EARLY=$(echo "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null) || TOOL_NAME_EARLY=""
if [ "$TOOL_NAME_EARLY" = "Bash" ] && [ "${HEADROOM_SCRUB_BASH:-1}" = "0" ]; then
    exit 0
fi

# The guard edits itself out: this file legitimately contains the pattern as a
# regex, so never block writes to this file.
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null)
case "$FILE_PATH" in
  *scrub-placeholders*) exit 0 ;;
esac

# Counted form (`__HR_SECRET_<hex>__`, either case), legacy 4-hex form, and
# the uncounted `__HR_HOME__`: all restore server-side and all break pasted
# literally. Minimum 4 hex keeps short noise (`__HR_X_AB__`) out.
HIT=$(echo "$INPUT" | grep -oE '__HR_[A-Z]+(_[0-9A-Fa-f]{4,})?__' 2>/dev/null | head -1)
if [ -n "$HIT" ]; then
  TOOL_NAME="$TOOL_NAME_EARLY"
  HINT="Rewrite with the real path instead of pasting tool output verbatim."
  if [ "$TOOL_NAME" = "Bash" ]; then
    HINT="That token is a display mask, never a value to grep for — re-issue the command with the real id or path. Searching logs for one specific token on purpose: set HEADROOM_SCRUB_BASH=0 in the session environment to skip this check (writes stay guarded)."
  elif [ "$TOOL_NAME" = "Skill" ]; then
    HINT="No skill has that name — call it by the name in the skill list."
  fi
  echo "BLOCKED: content contains redaction placeholder $HIT. $HINT" >&2
  exit 2
fi
exit 0

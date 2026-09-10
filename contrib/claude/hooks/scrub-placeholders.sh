#!/bin/bash
# scrub-placeholders.sh: block redaction tokens leaking into code and docs.
#
# Tool output masks real paths/secrets as __HR_PATH_<hex>__, __HR_SECRET_<hex>__,
# __HR_EMAIL_<hex>__, __HR_HOME__, etc. Those tokens are display masks, not real values: pasted
# into a file they land literally and break whatever references them (a bare
# __HR_PATH_...__ line in a shell script fails as "command not found" at load).
# $HOME is exempt: the tools expand it to the real home dir on write.
#
# PreToolUse on Write|Edit|MultiEdit. Reads the hook JSON from stdin, scans the
# whole tool input (file_path, old_string, new_string, content, edits) for the
# token shape, exits 2 to block with the offending token named. Never fails
# closed: any internal error exits 0 so a broken guard never stops work.
# Installed into ~/.claude/hooks by install.sh.
INPUT=$(cat) || exit 0

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
  echo "BLOCKED: content contains redaction placeholder $HIT. These tokens are display masks, never real values. Rewrite with the real path instead of pasting tool output verbatim." >&2
  exit 2
fi
exit 0

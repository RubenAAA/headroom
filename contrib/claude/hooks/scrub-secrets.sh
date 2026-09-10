#!/bin/bash
# PreToolUse hook (matcher: Bash): block commands that would print AWS
# credentials, SSH private keys, tokens, or passwords to the conversation.
#
# Reads the tool input JSON from stdin. Checks the command for patterns
# that typically dump secrets. Exit 2 blocks the call.

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)

if [ -z "$COMMAND" ]; then
  exit 0
fi

# Block patterns that extract/print credentials
BLOCKED_PATTERNS=(
  'print.*password'
  'print.*secret'
  'print.*\.login'
  'print.*\.token'
  'conn\.password'
  'conn\.login'
  'printenv.*SECRET'
  'printenv.*PASSWORD'
  'printenv.*TOKEN'
  'printenv.*KEY_ID'
  'cat.*id_ed25519'
  'cat.*id_rsa'
  'cat.*\.pem'
  'cat.*credentials'
)

# Secret stores: reading one is legitimate work, printing the answer into the
# transcript is not. These are blocked unless the command sends the output to
# a file, where it stays out of the conversation and out of the session log.
SECRET_STORE_PATTERNS=(
  'aws .*ssm .*get-parameter'
  'aws .*secretsmanager .*get-secret-value'
  'gcloud .*secrets .*versions .*access'
  'az .*keyvault .*secret .*show'
  'vault (kv )?get'
  'kubectl .*get .*secret.* -o *(yaml|json)'
  'doppler secrets (get|download)'
  'op (item get|read)'
)

COMMAND_LOWER=$(echo "$COMMAND" | tr '[:upper:]' '[:lower:]')

for pattern in "${BLOCKED_PATTERNS[@]}"; do
  if echo "$COMMAND_LOWER" | grep -qiE "$pattern"; then
    echo "BLOCKED: command would expose credentials. Do not print secrets to the conversation." >&2
    exit 2
  fi
done

# A redirect to a file is the escape hatch: `> out.json`, not `2>&1`.
if ! echo "$COMMAND" | grep -qE '>[[:space:]]*[^|&>[:space:]]'; then
  for pattern in "${SECRET_STORE_PATTERNS[@]}"; do
    if echo "$COMMAND_LOWER" | grep -qiE "$pattern"; then
      echo "BLOCKED: this reads a secret store and would print the values into the transcript." >&2
      echo "Send the output to a file instead (e.g. '> /tmp/param.json'), then read only the fields you need." >&2
      exit 2
    fi
  done
fi

exit 0

#!/usr/bin/env bash
# Run the user's statusline command, then append Headroom's statusline output.
# install.sh keeps the user's command in statusline-user-command beside this
# script, so the settings command stays stable across re-installs.
set -u

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
user_command_file="$here/statusline-user-command"
headroom_command="$here/statusline-with-cache.sh"
input=$(cat)

user_command=""
if [ -f "$user_command_file" ]; then
    user_command=$(cat "$user_command_file")
fi

user_output=""
if [ -n "$user_command" ]; then
    user_output=$(printf '%s' "$input" | bash -c "$user_command" 2>/dev/null || true)
fi

headroom_output=$(printf '%s' "$input" | "$headroom_command" 2>/dev/null || true)

if [ -n "$user_output" ]; then
    printf '%s' "$user_output"
    if [ -n "$headroom_output" ]; then
        printf '\n'
    fi
fi
if [ -n "$headroom_output" ]; then
    printf '%s' "$headroom_output"
fi
if [ -n "$user_output" ] || [ -n "$headroom_output" ]; then
    printf '\n'
fi

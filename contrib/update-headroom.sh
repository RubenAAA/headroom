#!/usr/bin/env bash
# Pull the checkout, reinstall, and restart the proxy onto the new binary.
#
# install.sh copies files into ~/.local/bin, ~/.claude and $HOME once; a later
# `git pull` leaves those copies stale. This script closes that gap in one
# command for machines that track this checkout:
#
#   git pull --ff-only, install.sh (same mode as the last install), restart.
#
# The install mode is detected, not asked: if ~/.headroom-flags.sh is a symlink
# into the checkout the last install used --link and this re-runs with --link,
# otherwise it re-runs in copy mode. Same for the build flavour: install.sh
# records HEADROOM_NO_ML in ~/.headroom-paths.sh and this follows it, so an
# update never silently switches a --no-ml box back to the full ML build.
# Pass --link or --copy / --no-ml or --ml to override for this run (the
# override is recorded by install.sh, so it sticks for the next update too).
set -euo pipefail

# Installed to ~/.local/bin by install.sh (symlinked under --link), so $0 says
# nothing about where the checkout is. ~/.headroom-paths.sh does.
[ -r "$HOME/.headroom-paths.sh" ] && source "$HOME/.headroom-paths.sh"
REPO_DIR="${HEADROOM_REPO:-$HOME/headroom}"
[ -d "$REPO_DIR/contrib" ] || {
    echo "update: no checkout at $REPO_DIR — set HEADROOM_REPO in ~/.headroom-paths.sh" >&2
    exit 1
}
FLAGS_FILE="$HOME/.headroom-flags.sh"

LINK=""
ML=""
for arg in "$@"; do
    case "$arg" in
        --link) LINK=1 ;;
        --copy) LINK=0 ;;
        --no-ml) ML=1 ;;
        --ml) ML=0 ;;
        -h|--help)
            echo "usage: $(basename "$0") [--link|--copy] [--no-ml|--ml]"
            echo "  default mode follows the last install: --link iff $FLAGS_FILE"
            echo "  is a symlink into the checkout, else copy mode; --no-ml iff"
            echo "  the last install used it (HEADROOM_NO_ML in ~/.headroom-paths.sh)"
            exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

if [ -z "$LINK" ]; then
    if [ -L "$FLAGS_FILE" ]; then
        LINK=1
    else
        LINK=0
    fi
fi

if [ -z "$ML" ]; then
    ML="${HEADROOM_NO_ML:-0}"
fi

cd "$REPO_DIR"
say() { echo "update: $*"; }

say "pulling $REPO_DIR"
git pull --ff-only || {
    echo "update: git pull failed — resolve it in $REPO_DIR, then rerun" >&2
    exit 1
}

# install.sh paths are relative to its own directory, so call it by path
# rather than relying on the cd above. ARGS stays a plain string (not an
# array): bash 3.2 + `set -u` misfires on `"${empty[@]}"`, and both
# words here are fixed literals, never user input.
ARGS=""
LINK_DESC="copy mode"
ML_DESC="full ML build"
if [ "$LINK" = 1 ]; then
    ARGS="$ARGS --link"
    LINK_DESC="--link"
fi
if [ "$ML" = 1 ]; then
    ARGS="$ARGS --no-ml"
    ML_DESC="--no-ml"
fi
say "reinstalling ($LINK_DESC, $ML_DESC, same as last install)"
# shellcheck disable=SC2086
"$REPO_DIR/install.sh" $ARGS

# install.sh builds and copies the binary but leaves a running proxy on the
# old one, so restart onto what was just built. The restart is detached and
# rolls back to the previous binary if the new one fails to listen.
if [ -x "$HOME/.local/bin/restart-headroom.sh" ]; then
    say "restarting the proxy"
    "$HOME/.local/bin/restart-headroom.sh"
else
    say "restart-headroom.sh not installed — start the proxy with cclaude"
fi

#!/usr/bin/env bash
# Shared paths and helpers for the model bench. Sourced, not run.

BENCH_ROOT="${BENCH_ROOT:-/tmp/hrbench}"
PROXY_LOG="${PROXY_LOG:-$HOME/headroom-proxy.log}"
# Which root holds transcripts depends on the config dir the session was
# started with, so both are searched rather than assumed. A bench pane and
# the session driving it can easily disagree: that mismatch made a passing
# smoke test report zero turns.
projects_roots() {
  [ -n "${CLAUDE_CONFIG_DIR:-}" ] && printf '%s/projects\n' "$CLAUDE_CONFIG_DIR"
  local d
  for d in "$HOME"/.claude "$HOME"/.claude-*; do
    [ -d "$d/projects" ] && printf '%s/projects\n' "$d"
  done
}

# Claude Code stores a session transcript under a project directory named
# after the cwd with every slash turned into a dash. `--session-id` fixes the
# filename, so a pane's transcript path is knowable before it writes a word.
transcript_path() {
  local cwd="$1" uuid="$2" root candidate fallback=""
  while read -r root; do
    candidate="$root/${cwd//\//-}/$uuid.jsonl"
    [ -z "$fallback" ] && fallback="$candidate"
    [ -f "$candidate" ] && { printf '%s' "$candidate"; return 0; }
  done < <(projects_roots)
  printf '%s' "$fallback"
}

# Seconds since the pane's transcript last gained a record, or -1 when there
# is no transcript yet.
#
# The path is resolved on every call, never once up front. A pane that is slow
# to boot has no file when its driver starts, and a path resolved at that
# moment is wrong for the rest of the run: `transcript_path` falls back to the
# first candidate root, which under a non-default CLAUDE_CONFIG_DIR is a tree
# `cclaude` never writes to. Idleness then reads -1 forever, so every task runs
# to its full budget and the pane reports as hung.
transcript_idle() {
  local path
  path="$(transcript_path "$1" "$2")"
  [ -f "$path" ] || { echo -1; return 0; }
  python3 - "$path" <<'IDLEPY'
import json, sys
from datetime import datetime, timezone

last = None
for line in open(sys.argv[1], errors="replace"):
    try:
        rec = json.loads(line)
    except ValueError:
        continue
    ts = rec.get("timestamp")
    if ts:
        last = ts
if not last:
    print(-1)
else:
    seen = datetime.fromisoformat(last.replace("Z", "+00:00"))
    print(int((datetime.now(timezone.utc) - seen).total_seconds()))
IDLEPY
}

run_dir()   { printf '%s/%s' "$BENCH_ROOT" "$1"; }
model_dir() { printf '%s/%s/%s' "$BENCH_ROOT" "$1" "$2"; }
state_dir() { printf '%s/%s/state' "$BENCH_ROOT" "$1"; }

# `/model` aliases, keyed by the short name the bench uses. Kept in step with
# MODELS-SUPPORTED-WITHIN-CLAUDE-CODE-PROXY.md; the flag file wins if they
# ever disagree.
model_alias() {
  case "$1" in
    sonnet)     echo "claude-sonnet-5" ;;
    opus)       echo "claude-opus-5" ;;
    spark)      echo "claude-muse-spark-1.3" ;;
    union)      echo "claude-union-alpha" ;;
    codex-sol)  echo "claude-codex-5.6-sol" ;;
    codex-luna) echo "claude-codex-5.6-luna" ;;
    codex-6-astra) echo "claude-codex-6-astra" ;;
    codex-6-sol)   echo "claude-codex-6-sol" ;;
    codex-6-luna)  echo "claude-codex-6-luna" ;;
    grok-high)  echo "claude-grok-4.6-high" ;;
    grok-xhigh) echo "claude-grok-4.6-xhigh" ;;
    *)          return 1 ;;
  esac
}

# Upstream model names as they appear in proxy-log PERF lines, so the grader
# can attribute continuation failures to the pane that caused them.
model_upstream() {
  case "$1" in
    sonnet)     echo "claude-sonnet-5" ;;
    opus)       echo "claude-opus-5" ;;
    spark)      echo "muse-spark-1.3-contributor-free" ;;
    union)      echo "union-alpha" ;;
    codex-6-*)    echo "gpt-6" ;;
    codex-*)    echo "gpt-5.6" ;;
    # The effort suffix is part of the served name, so it has to be here too:
    # `cursor-grok-4.6` matches every Grok pane and attributes one pane's
    # failures to another.
    grok-*)     echo "cursor-grok-4.6-${1#grok-}" ;;
    *)          echo "$1" ;;
  esac
}

die() { echo "bench: $*" >&2; exit 1; }

# Names a transcript may legitimately record for a pane. The `/model` alias is
# what was asked for; the upstream name is what a routed model answers as, and
# the two never match — `claude-grok-4.6-high` is served by
# `cursor-grok-4.6-high`. A wrong-model check needs both or it fails every
# routed pane.
model_expected_names() { printf '%s %s' "$(model_alias "$1")" "$(model_upstream "$1")"; }

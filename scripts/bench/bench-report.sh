#!/usr/bin/env bash
# One-shot summary of a bench run: what each model did, and whether a bad
# result was the model's fault or the proxy's.
#
# Usage: scripts/bench/bench-report.sh <run-id> [--json]

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$HERE/lib.sh"
# shellcheck source=tasks.sh
source "$HERE/tasks.sh"

RUN="${1:-}"
[ -n "$RUN" ] || die "usage: bench-report.sh <run-id> [--json]"
AS_JSON=0
[ "${2:-}" = "--json" ] && AS_JSON=1

STATE="$(state_dir "$RUN")"
[ -d "$STATE" ] || die "no such run: $RUN (looked in $STATE)"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

for uuid_file in "$STATE"/*.uuid; do
  [ -e "$uuid_file" ] || continue
  m="$(basename "$uuid_file" .uuid)"
  dir="$(model_dir "$RUN" "$m")"
  uuid="$(cat "$uuid_file")"
  transcript="$(transcript_path "$dir" "$uuid")"
  upstream="$(model_upstream "$m")"

  behaviour="$(python3 "$HERE/grade.py" --compact \
    --transcript "$transcript" \
    --upstream-model "$upstream" \
    --tasklog "$STATE/$m.tasklog" 2>/dev/null)"
  [ -n "$behaviour" ] || behaviour='{"tasks":[]}'

  # Artifact grading is the authority on "did it do the job". The behaviour
  # JSON says how it got there, or why it did not.
  passes=""
  while IFS=$'\t' read -r task _sent _done _over; do
    [ -n "$task" ] || continue
    grade_task "$task" "$dir" 2>/dev/null
    case "$?" in
      0) passes="$passes$task=pass " ;;
      2) passes="$passes$task=transcript " ;;
      *) passes="$passes$task=fail " ;;
    esac
  done < "$STATE/$m.tasklog"

  printf '%s\t%s\t%s\t%s\n' "$m" "$(model_expected_names "$m")" "$passes" "$behaviour" >> "$tmp"
done

if [ "$AS_JSON" = 1 ]; then
  python3 - "$tmp" <<'PY'
import json, sys
out = []
for line in open(sys.argv[1]):
    model, expected, passes, behaviour = line.rstrip("\n").split("\t", 3)
    out.append({"model": model, "expected_names": expected.split(),
                "artifacts": passes.split(), "behaviour": json.loads(behaviour)})
print(json.dumps(out, indent=2))
PY
  exit 0
fi

python3 - "$tmp" <<'PY'
import json, sys

rows = []
for line in open(sys.argv[1]):
    model, expected, passes, behaviour = line.rstrip("\n").split("\t", 3)
    rows.append((model, expected.split(), passes.split(), json.loads(behaviour)))

if not rows:
    print("no panes recorded for this run")
    raise SystemExit(0)

print(f"{'model':<12} {'task':<5} {'artifact':<9} {'verdict':<11} "
      f"{'turns':>5} {'tools':>5} {'blank':>5}  notes")
print("-" * 92)

for model, expected, artifacts, behaviour in rows:
    graded = dict(a.split("=") for a in artifacts)
    for task in behaviour.get("tasks", []):
        tid = task.get("task", "?")
        art = graded.get(tid, "-")
        v = task.get("verdict", "-")
        # Transcript-graded tasks (T0) pass on producing visible text that
        # answers the prompt. There is nothing on disk to check.
        if art == "transcript":
            said = task.get("last_text", "")
            art = "pass" if "READY" in said.upper() else (
                "blank" if not task.get("visible_text_turns") else "fail")
        # The artifact is the authority. A correct result on disk means the
        # task was done, whatever the behavioural heuristics inferred; any
        # circling or proxy note still prints alongside.
        if art == "pass":
            v = "completed"
        # And a missing artifact on a task the driver stopped waiting for is a
        # failure, not work in progress. grade.py cannot see the artifact, so
        # it falls back to "running" — which read as a live task next to an
        # artifact column already saying "fail".
        elif art == "fail" and v == "running":
            v = "wrong"
        notes = []
        # A pass proves nothing if another model answered. Checked against the
        # transcript, because a `/model` switch can fail silently and leave the
        # pane happily scoring on the session default.
        served = task.get("served_by") or {}
        wrong = [k for k in served if not any(e in k for e in expected)]
        if wrong:
            v = "WRONG-MODEL"
            notes.append("served by " + ",".join(sorted(wrong))
                         + ", not " + "/".join(expected))
        for c in task.get("circling", []):
            notes.append(f"repeated {c['tool']}x{c['count']}: {c['args_head'][:48]}")
        faults = task.get("proxy_faults", {})
        for key in ("continuation_non_2xx", "fold_failed", "served_fallback",
                    "tool_call_dropped", "empty_turn_notice"):
            if faults.get(key):
                notes.append(f"PROXY {key}={faults[key]}")
        if not faults.get("saw_model_traffic"):
            notes.append("no proxy traffic seen for this model")
        mix = task.get("tool_mix") or {}
        if mix:
            notes.append("tools=" + ",".join(f"{k}:{v}" for k, v in sorted(mix.items())))
        print(f"{model:<12} {tid:<5} {art:<9} {v:<11} "
              f"{task.get('assistant_turns', 0):>5} {task.get('tool_calls', 0):>5} "
              f"{task.get('blank_turns', 0):>5}  {'; '.join(notes)[:120]}")

print()
print("verdicts: completed | wrong | blank (thinking, no text) | circling | hung | over_budget")
print("a PROXY note means the fault was the proxy's, not the model's")
PY

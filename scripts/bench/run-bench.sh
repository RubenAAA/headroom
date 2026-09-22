#!/usr/bin/env bash
# Drives real Claude Code sessions, one tmux window per model, through a fixed
# task suite and records what each one did.
#
# Real interactive sessions rather than `claude -p` on purpose: headless skips
# the spinner sidecar, the statusline and the hooks, and the sidecar has
# already caused one production stall (docs/notes/learnings/
# concurrency-stall-is-sidecar-404.md). Testing without it would miss a whole
# failure class.
#
# Usage:
#   scripts/bench/run-bench.sh --models spark,codex-sol,grok-high --tasks T0,T1,T2
#   scripts/bench/run-bench.sh --models spark --tasks T4 --no-observer
#
# Attach with:  tmux attach -t hrbench-<run>
# Tear down:    tmux kill-session -t hrbench-<run>

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$HERE/lib.sh"
# shellcheck source=tasks.sh
source "$HERE/tasks.sh"

MODELS="sonnet,spark,codex-sol,grok-high"
TASKS="T0,T1,T2"
RUN="$(date -u +%m%d-%H%M)"
OBSERVER=1
OBSERVER_MODEL="opus"
# Seconds of transcript quiet that count as "the turn is over" once a task's
# artifact is already on disk.
SETTLE=8

while [ $# -gt 0 ]; do
  case "$1" in
    --models)   MODELS="$2"; shift 2 ;;
    --tasks)    TASKS="$2"; shift 2 ;;
    --run)      RUN="$2"; shift 2 ;;
    --observer-model) OBSERVER_MODEL="$2"; shift 2 ;;
    --no-observer) OBSERVER=0; shift ;;
    -h|--help)  sed -n '2,20p' "$0"; exit 0 ;;
    *)          die "unknown argument: $1" ;;
  esac
done

command -v tmux >/dev/null || die "tmux is not installed"
command -v cclaude >/dev/null || die "cclaude is not on PATH; run install.sh"

SESSION="hrbench-$RUN"
RUNDIR="$(run_dir "$RUN")"
STATE="$(state_dir "$RUN")"
mkdir -p "$STATE" || die "cannot create $STATE"

IFS=',' read -r -a MODEL_LIST <<< "$MODELS"
IFS=',' read -r -a TASK_LIST <<< "$TASKS"

for m in "${MODEL_LIST[@]}"; do
  model_alias "$m" >/dev/null || die "unknown model key: $m"
done

echo "bench run $RUN"
echo "  models:  ${MODEL_LIST[*]}"
echo "  tasks:   ${TASK_LIST[*]}"
echo "  scratch: $RUNDIR"

# ── panes ────────────────────────────────────────────────────────────────
for m in "${MODEL_LIST[@]}"; do
  dir="$(model_dir "$RUN" "$m")"
  mkdir -p "$dir"
  seed_fixtures "$dir" >/dev/null
  uuid="$(python3 -c 'import uuid; print(uuid.uuid4())')"
  echo "$uuid" > "$STATE/$m.uuid"
  : > "$STATE/$m.tasklog"

  # `--context` is what puts the session on the proxy. Bare `cclaude` is the
  # launcher with no profile: it sets no ANTHROPIC_BASE_URL and no gateway
  # discovery, so the pane talks straight to Anthropic and every routed alias
  # is unknown to it. A whole probe run scored two clean passes that way, on
  # claude-opus-5, for a pane that was meant to be testing Grok.
  #
  # Routed aliases are not accepted as `--model` on the command line either:
  # Claude Code validates that against its own list at startup and answers
  # "there's an issue with the selected model". They arrive through gateway
  # discovery and are selectable with `/model`, so the pane starts on the
  # default model and switches before its first task.
  cmd="cclaude --context --session-id $uuid --permission-mode bypassPermissions"

  if tmux has-session -t "$SESSION" 2>/dev/null; then
    tmux new-window -t "$SESSION" -n "$m" -c "$dir"
  else
    tmux new-session -d -s "$SESSION" -n "$m" -c "$dir"
  fi
  tmux send-keys -t "$SESSION:$m" -l "$cmd"
  tmux send-keys -t "$SESSION:$m" Enter
done

echo "waiting for panes to come up..."
sleep 8

# A scratch directory is new, so Claude Code asks whether the folder is
# trusted before it will accept a prompt. Left unanswered, the first task
# prompt is typed into that dialog and Enter picks "No, exit" — the pane dies
# and every task reads as a model failure.
#
# Answered here rather than by pre-seeding `hasTrustDialogAccepted` in
# ~/.claude.json: live sessions write that file, and a script racing them
# could clobber their state.
for m in "${MODEL_LIST[@]}"; do
  for _ in $(seq 1 20); do
    if tmux capture-pane -p -t "$SESSION:$m" 2>/dev/null | grep -qi "trust this folder"; then
      tmux send-keys -t "$SESSION:$m" Down
      sleep 0.2
      tmux send-keys -t "$SESSION:$m" Enter
      sleep 2
      break
    fi
    sleep 1
  done
done
sleep 6

# Switch each pane onto the model under test. Sent as a slash command because
# that is the only way a routed alias is selectable (see the launch comment).
#
# The switch is then confirmed against the statusline, which prints the model
# actually in force. A failed switch is silent — Claude Code answers "Model
# 'x' not found" and carries on serving the session default — and a bench that
# does not notice reports passes for a model that never ran.
for m in "${MODEL_LIST[@]}"; do
  alias="$(model_alias "$m")"
  dir="$(model_dir "$RUN" "$m")"
  uuid="$(cat "$STATE/$m.uuid")"
  tmux send-keys -t "$SESSION:$m" -l "/model $alias"
  sleep 0.4
  tmux send-keys -t "$SESSION:$m" Enter

  # Wait for the switch to land, rather than sleeping a guessed interval. A
  # slash command can take ten seconds or more to come back on a cold pane,
  # and the first task prompt typed while it is still in flight is swallowed:
  # it never reaches the transcript and the task grades as a silent hang.
  # Observed 2026-09-22, sonnet pane, T0.
  #
  # Success is the `Set model to` record in the transcript. The alias is not
  # matched against it, because Claude Code echoes its own display name for
  # models it knows — `/model claude-sonnet-5` answers "Set model to `Sonnet
  # 5`". Whether the right model answered is the grader's job, from
  # `served_by`.
  switched=""
  switch_deadline=$(( $(date +%s) + 90 ))
  while [ "$(date +%s)" -lt "$switch_deadline" ]; do
    pane="$(tmux capture-pane -p -t "$SESSION:$m" 2>/dev/null)"
    if printf '%s' "$pane" | grep -qiE "not found|issue with the selected model"; then
      echo "bench: FATAL $m — Claude Code rejected $alias; the pane is on the default model." >&2
      echo "bench:   check 'curl -s localhost:8787/v1/models' lists it, and that the" >&2
      echo "bench:   pane really went through the proxy (--context)." >&2
      tmux kill-session -t "$SESSION" 2>/dev/null
      exit 1
    fi
    transcript="$(transcript_path "$dir" "$uuid")"
    if [ -f "$transcript" ] && grep -qF "Set model to" "$transcript"; then
      switched=1
      break
    fi
    sleep 2
  done
  if [ -z "$switched" ]; then
    echo "bench: FATAL $m — /model $alias never came back within 90s." >&2
    echo "bench:   the pane would take its first prompt while still busy and lose it." >&2
    tmux kill-session -t "$SESSION" 2>/dev/null
    exit 1
  fi
done
sleep 3

# ── one driver per model, in parallel ────────────────────────────────────
drive_model() {
  local m="$1"
  local dir uuid log
  dir="$(model_dir "$RUN" "$m")"
  uuid="$(cat "$STATE/$m.uuid")"
  log="$STATE/$m.tasklog"

  for task in "${TASK_LIST[@]}"; do
    local prompt budget sent deadline done_ts over
    prompt="$(task_prompt "$task")"
    budget="$(task_budget "$task")"
    sent="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    tmux send-keys -t "$SESSION:$m" -l "$prompt"
    sleep 0.4
    tmux send-keys -t "$SESSION:$m" Enter

    deadline=$(( $(date +%s) + budget ))
    done_ts=""
    over="ok"
    while [ "$(date +%s)" -lt "$deadline" ]; do
      sleep 5
      if grade_task "$task" "$dir" 2>/dev/null; then
        # The artifact lands when the tool runs; the assistant records that
        # explain it are written once the turn ends, seconds later. Closing
        # the window at artifact time filed those records under the *next*
        # task — a pass credited with zero turns, and the next task credited
        # with two. Wait for the transcript to go quiet first.
        while [ "$(date +%s)" -lt "$deadline" ]; do
          [ "$(transcript_idle "$dir" "$uuid")" -ge "$SETTLE" ] 2>/dev/null && break
          sleep 2
        done
        done_ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        break
      fi
      # A pane that has stopped talking has finished its turn, right or wrong.
      # Idleness is measured from transcript records, never from turn counters.
      idle="$(transcript_idle "$dir" "$uuid")"
      if [ "$idle" -ge 45 ] 2>/dev/null; then
        done_ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        break
      fi
    done
    [ -n "$done_ts" ] || { done_ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"; over="over"; }
    printf '%s\t%s\t%s\t%s\n' "$task" "$sent" "$done_ts" "$over" >> "$log"
  done
  echo "done" > "$STATE/$m.finished"
}

for m in "${MODEL_LIST[@]}"; do
  drive_model "$m" &
done

# ── observer ─────────────────────────────────────────────────────────────
if [ "$OBSERVER" = 1 ]; then
  cp "$HERE/OBSERVER.md" "$RUNDIR/OBSERVER.md"
  printf '%s\n' "$RUN" > "$RUNDIR/RUN_ID"
  printf '%s\n' "${MODEL_LIST[@]}" > "$RUNDIR/MODELS"
  obs_alias="$(model_alias "$OBSERVER_MODEL")"
  tmux new-window -t "$SESSION" -n observer -c "$RUNDIR"
  tmux send-keys -t "$SESSION:observer" -l \
    "cclaude --context --model $obs_alias --permission-mode bypassPermissions"
  tmux send-keys -t "$SESSION:observer" Enter
  sleep 12
  tmux send-keys -t "$SESSION:observer" -l \
    "Read OBSERVER.md in this directory and follow it. The run id is $RUN."
  sleep 0.4
  tmux send-keys -t "$SESSION:observer" Enter
fi

echo
echo "attach:   tmux attach -t $SESSION"
echo "report:   $HERE/bench-report.sh $RUN"
echo "teardown: tmux kill-session -t $SESSION"
wait
echo "all panes finished; run $HERE/bench-report.sh $RUN"

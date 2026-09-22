#!/usr/bin/env bash
# Task definitions. Every task is a prompt plus a grader that reads the
# model's scratch directory, so a pass is a fact on disk, not a judgement.
#
# T0 is graded from the session transcript instead: it is the blank-turn
# detector, and a turn that produces no visible text leaves nothing on disk
# to check. That is exactly the 2026-09-22 Zen failure — thinking on the
# wire, then silence.

TASK_IDS=(T0 T1 T2 T3 T4 T5)

task_budget() {
  case "$1" in
    T0) echo 120 ;;
    T1) echo 180 ;;
    T2) echo 240 ;;
    T3) echo 600 ;;
    T4) echo 300 ;;
    T5) echo 420 ;;
  esac
}

task_prompt() {
  case "$1" in
    T0) echo "Reply with exactly the word READY and nothing else. Do not use any tools." ;;
    T1) echo "Create a file named t1.txt in the current directory whose contents are exactly the two letters OK. Then stop." ;;
    T2) echo "Count the lines in data.txt and write only that number into t2.txt. Then stop." ;;
    T3) echo "Run 'python3 test_calc.py' in this directory. It fails. Fix calc.py until it passes. Do not edit test_calc.py. Then stop." ;;
    T4) echo "Search your long-term memory for anything about the headroom proxy's Zen or Spark route, and write a one-line summary of what you found into t4.txt. If you find nothing, write 'nothing found' into t4.txt. Then stop." ;;
    T5) echo "Read bigfile.txt and write only the number of lines containing the word NEEDLE into t5.txt. Then stop." ;;
  esac
}

# Seeds the fixtures a model needs before its tasks run.
seed_fixtures() {
  local dir="$1"
  seq 1 137 | sed 's/^/line /' > "$dir/data.txt"

  python3 - "$dir/bigfile.txt" <<'PY'
import sys, random
random.seed(11)
needles = 0
with open(sys.argv[1], 'w') as fh:
    for i in range(4000):
        if random.random() < 0.05:
            fh.write(f"row {i} NEEDLE here\n")
            needles += 1
        else:
            fh.write(f"row {i} filler text padding this line out a bit\n")
print(needles)
PY

  cat > "$dir/calc.py" <<'PY'
def add(a, b):
    return a - b


def double(n):
    return n + n
PY

  # Plain asserts rather than pytest: the bench should not need a package
  # installed to run, and a missing dependency would read as a model failure.
  cat > "$dir/test_calc.py" <<'PY'
from calc import add, double

assert add(2, 3) == 5, f"add(2, 3) returned {add(2, 3)}"
assert double(4) == 8, f"double(4) returned {double(4)}"
print("all tests passed")
PY
}

# Graders. Each runs with the model's scratch dir as $1 and returns 0 on pass.
grade_task() {
  local id="$1" dir="$2"
  case "$id" in
    T0)
      # Graded from the transcript, not from disk — a turn that produces no
      # visible text leaves no artifact to check, and that silence is the
      # thing being tested. Exit 2 tells the caller to ask grade.py.
      return 2
      ;;
    T1)
      [ -f "$dir/t1.txt" ] && [ "$(tr -d '[:space:]' < "$dir/t1.txt")" = "OK" ]
      ;;
    T2)
      [ -f "$dir/t2.txt" ] &&
        [ "$(tr -dc '0-9' < "$dir/t2.txt")" = "$(wc -l < "$dir/data.txt" | tr -d ' ')" ]
      ;;
    T3)
      # The test file must be untouched, or "make it pass" was satisfied by
      # deleting the assertion rather than fixing the code.
      # `-B` plus a cache wipe: the broken and fixed `calc.py` are the same
      # byte length, so Python's mtime+size check can serve a stale .pyc and
      # fail a model that fixed the code correctly.
      if [[ -n "${dir:-}" && "$dir" != "/" ]]; then
        rm -rf "$dir/__pycache__"
      fi
      grep -q "assert add(2, 3) == 5" "$dir/test_calc.py" &&
        grep -q "assert double(4) == 8" "$dir/test_calc.py" &&
        (cd "$dir" && python3 -B test_calc.py >/dev/null 2>&1)
      ;;
    T4)
      [ -s "$dir/t4.txt" ]
      ;;
    T5)
      [ -f "$dir/t5.txt" ] &&
        [ "$(tr -dc '0-9' < "$dir/t5.txt")" = "$(grep -c NEEDLE "$dir/bigfile.txt")" ]
      ;;
    *)
      return 1
      ;;
  esac
}

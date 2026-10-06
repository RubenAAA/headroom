#!/usr/bin/env bash
# spark-budget.sh: pre-fan-out lane budget check for Spark sessions.
#
# The egress relay runs one lane per Spark session (Nord lanes plus one
# Proton lane). Fanning out past lane_count does not add capacity: the spare
# turns pile onto the same exits, Zen 429s follow, and with no verified
# distinct exit the rotation fails and every held turn stalls.
#
# Usage:
#   spark-budget.sh                  # print the live lane count
#   spark-budget.sh <unfinished-incl-new>
#                                    # exit 0 if that many unfinished Spark
#                                    # tasks fit the lanes, 1 if over budget.
# The count is the parent's own: every Spark session whose task is still
# running, including the current session if it is a Spark and the new ones
# about to start. The proxy cannot count idle-but-alive sessions from the
# outside, so no hook enforces this — the spawner checks before each fan-out.
# When the relay is unreachable, budget for eight.
set -euo pipefail

FALLBACK_LANES=8

lanes() {
  local n
  n=$(egress-relay status 2>/dev/null | jq -r '.lane_count // 0' 2>/dev/null) || n=0
  if [[ "$n" =~ ^[0-9]+$ ]] && (( n > 0 )); then
    echo "$n"
  else
    echo "$FALLBACK_LANES"
  fi
}

LANES=$(lanes)

if [[ $# -eq 0 ]]; then
  echo "lanes=$LANES"
  exit 0
fi

WANT="$1"
if ! [[ "$WANT" =~ ^[0-9]+$ ]]; then
  echo "usage: spark-budget.sh [unfinished-incl-new]" >&2
  exit 2
fi

if (( WANT <= LANES )); then
  echo "$WANT/$LANES lanes used — fits. Start the new session."
  exit 0
fi
echo "$WANT/$LANES lanes wanted — OVER BUDGET. Wait for a session to finish or run the remaining work sequentially; widening the fan-out stalls 429 recovery for every lane." >&2
exit 1

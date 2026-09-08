#!/bin/bash
# spark-goahead.sh: wait for the go-ahead, then post and prove.
#
# The PoC's drafts sat in /tmp because nothing listened for approval. This is
# the listener: it watches one session's draft for a matching go-ahead file and
# runs the poster exactly once, then leaves a proof behind.
#
# The approval is a file the operator creates, not a flag the model can set in
# passing -- the whole point is that the write happens on a human's say-so, at a
# moment they chose, and that saying so takes a deliberate act:
#
#     touch "$HOME"/.local/state/spark-review/<session>.goahead
#
# Runs as the worker (SPARK_REVIEW_WORKER=1), so the review gate ignores anything it
# does. That is correct here and only here: the gate exists to stop the
# *reviewing model* from posting, and this process is not it.
#
# Usage: spark-goahead.sh <session_id> [timeout_seconds]

set -uo pipefail
export SPARK_REVIEW_WORKER=1

SESSION="${1:?usage: spark-goahead.sh <session_id> [timeout]}"
TIMEOUT="${2:-3600}"
DIR="$HOME"/.local/state/spark-review
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DRAFT="$DIR/$SESSION.draft.json"
GOAHEAD="$DIR/$SESSION.goahead"
LOG="$DIR/$SESSION.goahead.log"

log() { echo "[$(date -Is)] $*" | tee -a "$LOG" >&2; }

mkdir -p "$DIR"

[ -f "$DRAFT" ] || { log "no draft at $DRAFT; nothing to wait for"; exit 1; }

# The poster names its proof after the draft's session_id, which need not match
# the file stem we were invoked with. Deriving it from the draft rather than
# from "$SESSION" is what makes the post-twice refusal below actually look at
# the file the poster writes -- the first live run checked a path that never
# existed and would have happily posted the whole batch again.
PROOF_SESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("session_id") or "nosession")' "$DRAFT")"
PROOF="$DIR/$PROOF_SESSION.proof.json"
if [ -f "$PROOF" ]; then
  log "proof already exists at $PROOF; refusing to post twice"
  exit 0
fi

log "waiting up to ${TIMEOUT}s for $GOAHEAD"
waited=0
while [ ! -f "$GOAHEAD" ]; do
  sleep 2
  waited=$((waited + 2))
  if [ "$waited" -ge "$TIMEOUT" ]; then
    log "timed out after ${TIMEOUT}s with no go-ahead; draft left unposted"
    exit 2
  fi
done

log "go-ahead seen; posting"
python3 "$HERE/spark_post.py" "$DRAFT" >>"$LOG" 2>&1
rc=$?
if [ "$rc" -eq 0 ]; then
  log "posted and verified; proof at $PROOF"
else
  log "poster exited $rc; see $LOG and $PROOF"
fi
exit "$rc"

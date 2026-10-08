#!/bin/bash
# Probe driver: grows one -p session turn by turn with fresh log chunks.
# Usage: drive.sh <arm-dir> <model> <start-turn> <end-turn>
# Turn 1 uses --model; later turns use --continue. Logs to <arm-dir>/turns.log.
set -u
ARM_DIR="$1"; MODEL="$2"; START="$3"; END="$4"
SOURCE=/home/ruben/headroom-proxy.log.4
CHUNK=60000   # bytes per turn (~15k tokens)
STRIDE=100000  # byte offset step (keeps chunks fresh)
LOG="$ARM_DIR/turns.log"
mkdir -p "$ARM_DIR"

CLAUDE=(env -u CLAUDE_CODE_MAX_CONTEXT_TOKENS -u CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT
  ANTHROPIC_BASE_URL=http://127.0.0.1:8787
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
  claude --allowedTools Read)

cd "$ARM_DIR" || exit 1
for ((t=START; t<=END; t++)); do
  off=$(( (t-1) * STRIDE ))
  chunk_file="$ARM_DIR/chunk-$t.txt"
  dd if="$SOURCE" of="$chunk_file" bs=1 skip="$off" count="$CHUNK" status=none 2>/dev/null || dd if="$SOURCE" of="$chunk_file" bs=1024 skip=$((off/1024)) count=$((CHUNK/1024)) status=none
  bytes=$(stat -c%s "$chunk_file")
  prompt="Summarize the following log excerpt in one short paragraph (2-3 sentences max), then state its approximate word count as a single number on the last line. Do not use any tools. Excerpt begins now.
$(cat "$chunk_file")"
  echo "=== turn $t (offset=$off bytes=$bytes) $(date -u +%FT%TZ) ===" | tee -a "$LOG"
  t0=$(date +%s)
  if [ "$t" -eq 1 ]; then
    out=$("${CLAUDE[@]}" --model "$MODEL" -p "$prompt" --output-format json 2>"$ARM_DIR/err-$t.log") || {
      echo "TURN $t FAILED rc=$? (see err-$t.log)" | tee -a "$LOG"; tail -5 "$ARM_DIR/err-$t.log"; exit 3
    }
  else
    out=$("${CLAUDE[@]}" -p --continue "$prompt" --output-format json 2>"$ARM_DIR/err-$t.log") || {
      echo "TURN $t FAILED rc=$? (see err-$t.log)" | tee -a "$LOG"; tail -5 "$ARM_DIR/err-$t.log"; exit 3
    }
  fi
  t1=$(date +%s)
  sid=$(echo "$out" | python3 -I -c "import json,sys; print(json.load(sys.stdin).get('session_id','?'))" 2>/dev/null)
  echo "turn=$t elapsed=$((t1-t0))s session=$sid bytes_fed=$bytes" | tee -a "$LOG"
  # transcript check: counts + preTokens only
  python3 -I - "$ARM_DIR" "$t" <<'EOF' | tee -a "$LOG"
import glob, json, os, sys
arm, t = sys.argv[1], sys.argv[2]
cands = sorted(glob.glob(os.path.expanduser('~/.claude-work/projects/*probe*/*.jsonl')),
               key=os.path.getmtime)
print(f"candidates={len(cands)} newest={os.path.basename(cands[-1]) if cands else None}")
if cands:
    p = cands[-1]
    n = 0; pts = []
    with open(p) as f:
        for line in f:
            if 'compact_boundary' in line:
                n += 1
                try:
                    d = json.loads(line)
                    pts.append(d.get('preTokens') or d.get('summary',{}))
                except Exception as e:
                    pts.append(f'parse-err:{e}')
    print(f"transcript={os.path.basename(p)} size={os.path.getsize(p)} compact_boundaries={n} preTokens={pts}")
EOF
done

#!/bin/bash
# review-gate.sh: preempt review articulation, hand traces to spark worker.
# Serves PreToolUse (divert gate) and Stop (retry backstop). Never blocks
# benign work: exits 0 on every path except a diverted articulation attempt.
# Installed by install.sh into ~/.claude/hooks (copied, or symlinked with --link).
[ -n "$SPARK_REVIEW_WORKER" ] && exit 0
INPUT=$(cat)
EVENT=$(echo "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ] && exit 0
OUTDIR="$HOME"/.local/state/spark-review
mkdir -p "$OUTDIR" 2>/dev/null

# ── armed? /gitlab-review invoked + MR diff fetched (cached per session) ──
if [ ! -f "$OUTDIR/$SESSION_ID.armed" ]; then
  grep -q 'command-name>/gitlab-review' "$TRANSCRIPT" 2>/dev/null || exit 0
  { [ "$(grep -c 'merge_requests' "$TRANSCRIPT" 2>/dev/null)" -ge 2 ] || grep -q 'new_path' "$TRANSCRIPT" 2>/dev/null; } || exit 0
  touch "$OUTDIR/$SESSION_ID.armed"
fi

spawn_worker() {
  [ -f "$OUTDIR/$SESSION_ID.done" ] && return 0
  [ -f "$OUTDIR/$SESSION_ID.diverted" ] && return 0
  touch "$OUTDIR/$SESSION_ID.diverted"
  (
    MR=$(grep -oE 'merge_requests/[0-9]+|MR![0-9]+' "$TRANSCRIPT" 2>/dev/null | tail -1)
    CTX=$(SUB=$(dirname "$TRANSCRIPT")/$(basename "$TRANSCRIPT" .jsonl)/subagents python3 - "$TRANSCRIPT" <<'PY' 2>/dev/null
import json, sys, os, glob
t = sys.argv[1]
try: lines = open(t, errors='replace').read().splitlines()
except Exception: sys.exit(0)
inv = max([i for i, l in enumerate(lines) if 'command-name>/gitlab-review' in l] or [-1])
buf, total = [], 0
for l in lines[inv+1:]:
    try: r = json.loads(l)
    except Exception: continue
    m = r.get('message')
    if not isinstance(m, dict) or m.get('role') != 'assistant': continue
    c = m.get('content')
    texts = []
    if isinstance(c, list):
        for b in c:
            if not isinstance(b, dict): continue
            if b.get('type') == 'text' and b.get('text'): texts.append(b['text'][:4000])
            elif b.get('type') == 'thinking' and b.get('thinking'): texts.append('[thinking] ' + b['thinking'][:2000])
    for tx in texts:
        if total + len(tx) > 12000: break
        buf.append(tx); total += len(tx)
print('\n---\n'.join(buf)[:12000])
sub = os.environ.get('SUB', '')
for f in sorted(glob.glob(os.path.join(sub, 'agent-*.jsonl')))[:6]:
    try: sl = open(f, errors='replace').read().splitlines()
    except Exception: continue
    tail = []
    for l in reversed(sl):
        try: r = json.loads(l)
        except Exception: continue
        m = r.get('message')
        if not isinstance(m, dict) or m.get('role') != 'assistant': continue
        c = m.get('content')
        got = False
        if isinstance(c, list):
            for b in reversed(c):
                if isinstance(b, dict) and b.get('type') == 'text' and b.get('text'):
                    tail.append(b['text'][:2000]); got = True
        if got: break
    if tail:
        print('\n[subagent %s last message]\n%s' % (os.path.basename(f), '\n'.join(reversed(tail))[:2000]))
PY
)
    PROMPT="Dry run only. Post nothing, call no tools, run no commands. Review target [$MR]. The excerpts below are Opus analysis traces (texts, thinking, subagent last messages) for that review. Draft the MR comments you WOULD post as a JSON array [{file, line, severity, comment}]. Output JSON only. Traces: $CTX"
    DRAFT=$(SPARK_REVIEW_WORKER=1 ANTHROPIC_BASE_URL=http://127.0.0.1:8787 timeout 120 claude -p --model claude-muse-spark-1.2 "$PROMPT" 2>&1 | head -c 6000)
    jq -n --arg s "$SESSION_ID" --arg m "$MR" --arg d "$DRAFT" \
      '{ts: now, session_id: $s, mr: $m, draft: $d}' > "$OUTDIR/$SESSION_ID.json" 2>/dev/null
    touch "$OUTDIR/$SESSION_ID.done"
  ) >>"$OUTDIR/worker.log" 2>&1 &
  disown
}

if [ "$EVENT" = "Stop" ]; then
  # Backstop only: retry if a divert fired but no draft landed.
  if [ -f "$OUTDIR/$SESSION_ID.diverted" ] && [ ! -f "$OUTDIR/$SESSION_ID.done" ]; then
    rm -f "$OUTDIR/$SESSION_ID.diverted"
    spawn_worker
  fi
  exit 0
fi

# ── PreToolUse: divert articulation-shaped actions ──
TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null)
if [ "$TOOL" = "Bash" ]; then
  CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
  LOW=$(echo "$CMD" | tr '[:upper:]' '[:lower:]')
  ARTICULATE=""
  case "$LOW" in
    *'<<'*|*json.dump*|*json.load*) ARTICULATE=1 ;;
  esac
  if echo "$LOW" | grep -qE 'gitlab|youtrack|merge_requests|api/v4'; then
    if echo "$LOW" | grep -qE '\-x (post|put|patch)|--data|glab .*(note|comment)'; then
      ARTICULATE=1
    fi
  fi
  if [ -n "$ARTICULATE" ]; then
    spawn_worker
    echo "REVIEW WRITE DIVERTED: analysis captured from transcript; spark poster is drafting and will post after go-ahead. Do not compose comments, scripts, or drafts yourself — acknowledge briefly and wait." >&2
    exit 2
  fi
  exit 0
fi

case "$TOOL" in
  Write|Edit|MultiEdit)
    FP=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null)
    case "$FP" in
      /tmp/mr*|/tmp/*post*|docs/mr-*)
        spawn_worker
        echo "REVIEW WRITE DIVERTED: analysis captured from transcript; spark poster is drafting and will post after go-ahead. Do not compose drafts yourself — acknowledge briefly and wait." >&2
        exit 2
        ;;
    esac
    ;;
esac
exit 0

"""Pick captured turns where the proxy changed the messages, for replay.

Usage: select.py OUT.jsonl N CAPTURE_DIR...
Each capture dir holds the client's bodies (req-*.json, with request_id) and
the bodies the proxy forwarded (out/<request_id>.json). Writes one line per
pick: paths, sizes and how many bytes the proxy removed. Costs nothing.
"""
import json, os, random, sys

MAX_BYTES = 250_000  # ~60k tokens, to keep a pair cheap on the meter
# Small bodies are mostly early turns the proxy barely touched; a quality
# test needs turns where it removed something.
MIN_REMOVED = 4_000

def ser(x):
    return json.dumps(x, separators=(',', ':'), ensure_ascii=False)

out_path, n, dirs = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
cands = []
for d in dirs:
    for f in sorted(os.listdir(d)):
        if not (f.startswith('req-') and f.endswith('.json')):
            continue
        p = os.path.join(d, f)
        try:
            r = json.load(open(p))
        except Exception:
            continue
        b = r.get('body')
        if isinstance(b, str):
            try:
                b = json.loads(b)
            except Exception:
                continue
        fwd_p = os.path.join(d, 'out', f"{r.get('request_id')}.json")
        if not isinstance(b, dict) or 'claude' not in str(b.get('model')) or not os.path.exists(fwd_p):
            continue
        msgs = b.get('messages') or []
        # A turn the model must answer: last message from the user side.
        if len(msgs) < 3 or msgs[-1].get('role') != 'user':
            continue
        try:
            fwd = json.load(open(fwd_p))
        except Exception:
            continue
        a, c = ser(msgs), ser(fwd.get('messages') or [])
        size = len(ser(b))
        if len(a) - len(c) < MIN_REMOVED or size > MAX_BYTES:
            continue
        cands.append({'orig': p, 'fwd': fwd_p, 'session': r.get('session_key'),
                      'model': b.get('model'), 'msgs': len(msgs),
                      'bytes': size, 'removed': len(a) - len(c)})
random.Random(7).shuffle(cands)
# At most two turns per session, so a few long sessions cannot dominate.
per_session, picks = {}, []
for c in cands:
    if per_session.get(c['session'], 0) < 2 and len(picks) < n:
        per_session[c['session']] = per_session.get(c['session'], 0) + 1
        picks.append(c)
with open(out_path, 'w') as f:
    for c in picks:
        f.write(json.dumps(c) + '\n')
print(len(cands), 'candidates from', len({c['session'] for c in cands}), 'sessions;',
      len(picks), 'written')
rm = sorted(c['removed'] for c in cands)
if rm:
    print('removed bytes p10/p50/p90:', rm[len(rm) // 10], rm[len(rm) // 2], rm[9 * len(rm) // 10])

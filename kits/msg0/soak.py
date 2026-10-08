#!/usr/bin/env python3
"""Soak: run send.py batch N times, report any capture with anomalous msg counts."""
import json, subprocess, sys, glob, os

arm, base, log = sys.argv[1], sys.argv[2], sys.argv[3]
n = int(sys.argv[4]) if len(sys.argv) > 4 else 6

def captures():
    out = {}
    for f in glob.glob(os.environ.get('MSG0_CAPTURE_DIR','/tmp/egate-sideproxy/captures') + '/*.json'):
        d = json.load(open(f))
        out[d['seq']] = d
    return out

before = captures()
for i in range(n):
    r = subprocess.run(['python3', '-I', os.environ.get('MSG0_DIR','/tmp/msg0') + '/send.py', arm, base, log],
                       capture_output=True, text=True)
    print(f'--- batch {i+1}: rc={r.returncode}')
    if r.returncode != 0:
        print(r.stdout[-500:]); print(r.stderr[-500:]); break
after = captures()
new = {s: d for s, d in after.items() if s not in before}
print(f'new captures: {len(new)}')
anom = 0
for s in sorted(new):
    d = new[s]; b = d['body'] or {}
    m = b.get('messages', b.get('input'))
    cnt = len(m) if isinstance(m, list) else '?'
    exp = 2 if d['path'] == '/v1/chat/completions' else 1
    if cnt != exp:
        anom += 1
        print(f'ANOMALY seq={s} {d["path"]} msgs={cnt} (expected {exp})')
        print(json.dumps(b, indent=1)[:1200])
print('anomalies:', anom)

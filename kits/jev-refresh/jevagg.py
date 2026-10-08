import json, sys
from collections import Counter

ev = Counter()
lvl = Counter()
n_bad = 0
first = last = None
samples = {}
with open(sys.argv[1]) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except Exception:
            n_bad += 1
            continue
        ts = r.get("timestamp", "?")
        first = first or ts
        last = ts
        lvl[r.get("level", "?")] += 1
        fl = r.get("fields", {})
        e = fl.get("event", "(no event)")
        ev[e] += 1
        samples.setdefault(e, {k: v for k, v in fl.items() if k in ("message", "error", "status", "code")})
print("lines=%d unparsable=%d span=%s..%s" % (sum(ev.values()) + n_bad, n_bad, first, last))
print("levels:", dict(lvl))
print()
for e, n in ev.most_common():
    print("%5d  %s" % (n, e))
    print("       %s" % samples[e])

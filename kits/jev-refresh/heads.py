import json, sys
from collections import defaultdict

def load(path):
    rows = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows

exc = load(sys.argv[1])
# distinct exchanges by (session, index): score range across observations
by_ex = {}
for r in exc:
    e = r["fields"]
    k = (e["session_key_hash"][:8], e["index"])
    by_ex.setdefault(k, {"scores": [], "tokens": e["tokens"], "msgs": e["messages"], "head": e["head"]})
    by_ex[k]["scores"].append(e["score"])

print("distinct exchanges scored: %d (from %d lines)" % (len(by_ex), len(exc)))
print()
print("=== DISTINCT exchanges ever scoring < 0.3 (min score, max score, n_obs, tokens, head) ===")
lows = [(k, v) for k, v in by_ex.items() if min(v["scores"]) < 0.3]
lows.sort(key=lambda kv: min(kv[1]["scores"]))
print("count: %d" % len(lows))
for k, v in lows:
    print("sess=%s idx=%3d min=%.2f max=%.2f n=%2d tok=%6d msg=%3d | %s" % (
        k[0], k[1], min(v["scores"]), max(v["scores"]), len(v["scores"]), v["tokens"], v["msgs"], v["head"][:100]))

print()
print("=== DISTINCT exchanges with min in [0.3, 0.4): count only + token stats ===")
mid = [v for k, v in by_ex.items() if 0.3 <= min(v["scores"]) < 0.4]
print("count: %d, tokens total=%d" % (len(mid), sum(v["tokens"] for v in mid)))
for k, v in sorted(by_ex.items(), key=lambda kv: min(kv[1]["scores"])):
    if 0.3 <= min(v["scores"]) < 0.4:
        print("sess=%s idx=%3d min=%.2f max=%.2f n=%2d tok=%6d msg=%3d | %s" % (
            k[0], k[1], min(v["scores"]), max(v["scores"]), len(v["scores"]), v["tokens"], v["msgs"], v["head"][:100]))

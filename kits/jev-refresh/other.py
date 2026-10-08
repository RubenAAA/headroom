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
by_ex = {}
for r in exc:
    e = r["fields"]
    k = (e["session_key_hash"][:8], e["index"])
    d = by_ex.setdefault(k, {"scores": [], "tokens": e["tokens"], "head": e["head"]})
    d["scores"].append(e["score"])

print("=== distinct NON-teammate-msg exchanges (min/max score, tokens, head prefix) ===")
for k in sorted(by_ex, key=lambda k: min(by_ex[k]["scores"])):
    v = by_ex[k]
    if not v["head"].startswith("Another Claude session"):
        print("sess=%s idx=%3d min=%.2f max=%.2f n=%2d tok=%7d | %s" % (
            k[0], k[1], min(v["scores"]), max(v["scores"]), len(v["scores"]), v["tokens"], v["head"][:90]))

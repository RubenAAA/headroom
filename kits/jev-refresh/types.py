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

def htype(head):
    if head.startswith("Another Claude session sent a message"):
        return "teammate-msg"
    if head.startswith("<command-name>"):
        return "slash-command"
    if head.startswith("[Request interrupted]"):
        return "interrupted"
    if head.startswith("[") and "] " in head[:12]:
        return "typed-task?"
    return "other"

print("=== head-type by score band (lines, tokens) ===")
bands = defaultdict(lambda: defaultdict(lambda: [0, 0]))
for r in exc:
    e = r["fields"]
    s = e["score"]
    band = "<0.3" if s < 0.3 else ("0.3-0.4" if s < 0.4 else ("0.4-0.5" if s < 0.5 else ">=0.5"))
    t = htype(e["head"])
    bands[band][t][0] += 1
    bands[band][t][1] += e["tokens"]
for b in ["<0.3", "0.3-0.4", "0.4-0.5", ">=0.5"]:
    print("--- %s ---" % b)
    for t, (n, tok) in sorted(bands[b].items(), key=lambda kv: -kv[1][0]):
        print("  %-14s lines=%4d tokens=%8d" % (t, n, tok))

print()
print("=== per-exchange score swing (same exchange, different tasks) ===")
by_ex = defaultdict(list)
for r in exc:
    e = r["fields"]
    by_ex[(e["session_key_hash"][:8], e["index"])].append(e["score"])
multi = {k: v for k, v in by_ex.items() if len(v) > 1}
print("exchanges scored more than once: %d" % len(multi))
import statistics
swings = sorted(max(v) - min(v) for v in multi.values())
def q(p): return swings[min(len(swings)-1, int(p*len(swings)))]
print("swing max-min: min=%.2f p10=%.2f p25=%.2f med=%.2f p75=%.2f p90=%.2f max=%.2f" % (
    swings[0], q(.1), q(.25), q(.5), q(.75), q(.9), swings[-1]))
cross03 = sum(1 for v in multi.values() if min(v) < 0.3 <= max(v))
cross04 = sum(1 for v in multi.values() if min(v) < 0.4 <= max(v))
print("exchanges crossing the 0.3 line across tasks: %d of %d" % (cross03, len(multi)))
print("exchanges crossing the 0.4 line across tasks: %d of %d" % (cross04, len(multi)))

print()
print("=== small session aae70305: all 10 exchanges (type + score + tokens) ===")
for r in exc:
    e = r["fields"]
    if e["session_key_hash"].startswith("aae70305"):
        print("idx=%2d score=%.2f tok=%6d msg=%d type=%s" % (e["index"], e["score"], e["tokens"], e["messages"], htype(e["head"])))

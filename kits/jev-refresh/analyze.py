import json, sys
from collections import Counter, defaultdict
from datetime import datetime

def load(path):
    rows = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows

obs = load(sys.argv[1])
exc = load(sys.argv[2])
of = [r["fields"] for r in obs]
ef = [r["fields"] for r in exc]
ots = [r["timestamp"] for r in obs]

print("=== OBSERVED EVENTS (%d) ===" % len(of))
print("span: %s .. %s" % (min(ots), max(ots)))
print("sessions: %d distinct" % len(set(o["session_key_hash"] for o in of)))
print()
print("ts | sess | total_tok | n_exch | scored | err | <0.3 (n/tok/%) | <0.4 (n/tok/%) | <0.5 (n/tok/%)")
for r, o in zip(obs, of):
    t = o["total_tokens"]
    print("%s | %s | %7d | %3d | %3d | %d | %3d/%7d/%5.1f | %3d/%7d/%5.1f | %3d/%7d/%5.1f" % (
        r["timestamp"][:16], o["session_key_hash"][:8], t, o["exchanges"], o["scored"], o["errors"],
        o["stub_lt_0_3_exchanges"], o["stub_lt_0_3_tokens"], 100*o["stub_lt_0_3_tokens"]/t,
        o["stub_lt_0_4_exchanges"], o["stub_lt_0_4_tokens"], 100*o["stub_lt_0_4_tokens"]/t,
        o["stub_lt_0_5_exchanges"], o["stub_lt_0_5_tokens"], 100*o["stub_lt_0_5_tokens"]/t))

nz = [o for o in of if o["errors"] != 0]
print()
print("observed with errors != 0: %d of %d" % (len(nz), len(of)))
for o in nz:
    print("  err=%d sess=%s scored=%d" % (o["errors"], o["session_key_hash"][:8], o["scored"]))

print()
print("=== EXCHANGE SCORE DISTRIBUTION (n=%d) ===" % len(ef))
s = sorted(e["score"] for e in ef)
def q(p): return s[min(len(s)-1, int(p*len(s)))]
print("min=%.2f p10=%.2f p25=%.2f med=%.2f p75=%.2f p90=%.2f max=%.2f" % (
    s[0], q(.10), q(.25), q(.50), q(.75), q(.90), s[-1]))
print()
print("band | n_exch | tokens | median_tok | median_msg")
import statistics
for lo in [0,.1,.2,.3,.4,.5,.6,.7,.8,.9]:
    b = [e for e in ef if lo <= e["score"] < lo+0.1 or (lo==0.9 and e["score"]>=0.9)]
    if b:
        print("%.1f-%.1f | %4d | %8d | %6d | %4d" % (lo, lo+0.1, len(b),
            sum(e["tokens"] for e in b),
            statistics.median(e["tokens"] for e in b),
            statistics.median(e["messages"] for e in b)))
print()
for th in (0.3, 0.4, 0.5):
    b = [e for e in ef if e["score"] < th]
    print("score < %.1f: %d exchanges, %d tokens" % (th, len(b), sum(e["tokens"] for e in b)))

print()
print("=== TOP-TOKEN EXCHANGES: score check (largest 15) ===")
for e in sorted(ef, key=lambda e: -e["tokens"])[:15]:
    print("tok=%6d msg=%4d score=%.2f sess=%s" % (e["tokens"], e["messages"], e["score"], e["session_key_hash"][:8]))

print()
print("=== PER-SESSION exchange counts ===")
c = Counter(e["session_key_hash"][:8] for e in ef)
for k, v in c.most_common():
    print("sess=%s n=%d" % (k, v))

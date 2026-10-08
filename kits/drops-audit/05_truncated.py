#!/usr/bin/env python3 -I
"""Analyze truncated (len=700, non-JSON) shapes: key names only, values masked."""
import json, re, collections
path = "/home/ruben/headroom-proxy.log"
keyre = re.compile(r'"([A-Za-z_][A-Za-z0-9_]*)"\s*:')
by_event_keys = collections.defaultdict(collections.Counter)
by_event_n = collections.Counter()
prefixes = collections.defaultdict(collections.Counter)
for line in open(path, encoding="utf-8", errors="replace"):
    if "responses_stream_event_shape" not in line:
        continue
    obj = json.loads(line)
    f = obj["fields"]
    sh = f.get("shape", "")
    try:
        json.loads(sh)
        continue
    except Exception:
        pass
    ev = f.get("stream_event", "?")
    by_event_n[ev] += 1
    for k in keyre.findall(sh):
        by_event_keys[ev][k] += 1
    # masked prefix: keep structure chars, mask alnum runs to X, show first 120 chars
    masked = re.sub(r'[A-Za-z0-9_]{2,}', 'X', sh[:160])
    prefixes[ev][masked] += 1
print("== truncated-shape line counts ==")
for ev, c in by_event_n.most_common():
    print(f"{c}\t{ev}")
print("== key names per truncated event (key: in-how-many-lines) ==")
for ev in sorted(by_event_keys):
    ks = ",".join(f"{k}({c})" for k, c in by_event_keys[ev].most_common(40))
    print(f"{ev}: {ks}")
print("== masked prefixes (distinct) ==")
for ev in sorted(prefixes):
    for p, c in prefixes[ev].most_common(3):
        print(f"{ev} x{c}: {p}")

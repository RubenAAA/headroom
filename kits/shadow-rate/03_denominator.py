#!/usr/bin/env python3 -I
"""Denominator split: model_route_translate by model bucket x stream flag. Counts only."""
import json, collections
FILES = ["/home/ruben/headroom-proxy.log", "/home/ruben/headroom-proxy.log.1",
         "/home/ruben/headroom-proxy.log.2", "/home/ruben/headroom-proxy.log.3",
         "/home/ruben/headroom-proxy.log.4"]
c = collections.Counter()
n = 0
for path in FILES:
    try:
        fh = open(path, encoding="utf-8", errors="replace")
    except OSError:
        continue
    for line in fh:
        if "model_route_translate" not in line:
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        f = o.get("fields", {})
        if not isinstance(f, dict) or f.get("event") != "model_route_translate":
            continue
        n += 1
        m = str(f.get("model", ""))
        b = "spark" if "spark" in m.lower() else ("codex" if "codex" in m.lower() else "other")
        c[(b, str(f.get("stream")))] += 1
print(f"model_route_translate n={n}")
for k in sorted(c):
    print(f"  {c[k]} model={k[0]} stream={k[1]}")

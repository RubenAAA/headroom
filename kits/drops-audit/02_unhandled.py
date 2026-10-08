#!/usr/bin/env python3 -I
"""Aggregate codex_unhandled_stream_event: names, line counts, count-field stats, time range."""
import json, collections
path = "/home/ruben/headroom-proxy.log"
by_name = collections.Counter()
count_vals = collections.defaultdict(list)
ts_min = ts_max = None
levels = collections.Counter()
targets = collections.Counter()
messages = collections.Counter()
total_lines = 0
for line in open(path, encoding="utf-8", errors="replace"):
    if "codex_unhandled_stream_event" not in line:
        continue
    total_lines += 1
    obj = json.loads(line)
    f = obj["fields"]
    name = f.get("stream_event", "<missing>")
    by_name[name] += 1
    c = f.get("count")
    if isinstance(c, int):
        count_vals[name].append(c)
    ts = obj.get("timestamp", "")
    if ts_min is None or ts < ts_min: ts_min = ts
    if ts_max is None or ts > ts_max: ts_max = ts
    levels[obj.get("level","")] += 1
    targets[obj.get("target","")] += 1
    messages[f.get("message","")] += 1
print(f"total_unhandled_lines={total_lines}")
print(f"ts_min={ts_min}")
print(f"ts_max={ts_max}")
print("== levels ==")
for k,c in levels.most_common(): print(f"{c}\t{k}")
print("== targets ==")
for k,c in targets.most_common(): print(f"{c}\t{k}")
print("== messages ==")
for k,c in messages.most_common(): print(f"{c}\t{k!r}")
print("== by stream_event (lines, min_count, max_count, all_counts_if_few) ==")
for name, n in by_name.most_common():
    vals = sorted(count_vals.get(name, []))
    if vals:
        extra = f" vals={vals}" if len(vals) <= 20 else f" first5={vals[:5]} last5={vals[-5:]}"
        print(f"{n}\tmin={vals[0]}\tmax={vals[-1]}\t{name}{extra}")
    else:
        print(f"{n}\tno-count-field\t{name}")

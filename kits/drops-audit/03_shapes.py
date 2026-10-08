#!/usr/bin/env python3 -I
"""Aggregate responses_stream_event_shape: distinct (stream_event, item_type, shape), time range."""
import json, collections
path = "/home/ruben/headroom-proxy.log"
by_key = collections.Counter()
by_event = collections.Counter()
by_item = collections.Counter()
ts_min = ts_max = None
total = 0
levels = collections.Counter()
for line in open(path, encoding="utf-8", errors="replace"):
    if "responses_stream_event_shape" not in line:
        continue
    total += 1
    obj = json.loads(line)
    f = obj["fields"]
    ev = f.get("stream_event", "<missing>")
    it = f.get("item_type", "<missing>")
    sh = f.get("shape", "<missing>")
    by_key[(ev, it, sh)] += 1
    by_event[ev] += 1
    by_item[it] += 1
    ts = obj.get("timestamp", "")
    if ts_min is None or ts < ts_min: ts_min = ts
    if ts_max is None or ts > ts_max: ts_max = ts
    levels[obj.get("level","")] += 1
print(f"total_shape_lines={total}")
print(f"distinct_keys={len(by_key)}")
print(f"ts_min={ts_min}")
print(f"ts_max={ts_max}")
print("== levels ==")
for k,c in levels.most_common(): print(f"{c}\t{k}")
print("== by stream_event ==")
for k,c in by_event.most_common(): print(f"{c}\t{k}")
print("== by item_type ==")
for k,c in by_item.most_common(): print(f"{c}\t{k}")
print("== distinct (count, stream_event, item_type, shape) ==")
for (ev,it,sh), c in sorted(by_key.items(), key=lambda x: (-by_event[x[0][0]], x[0][0], x[0][1])):
    # shape is already truncated by the proxy (strings<=40ch, arrays<=1 elem); print full shape string
    print(f"--- n={c} event={ev} item_type={it}")
    print(sh)

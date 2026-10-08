#!/usr/bin/env python3 -I
"""Schema discovery for drops audit: emit only key names and value-type histograms, no raw values."""
import json, collections, sys
path = "/home/ruben/headroom-proxy.log"
targets = ("codex_unhandled_stream_event", "responses_stream_event_shape")
top_keys = collections.Counter()
nested = collections.Counter()
field_types = collections.Counter()
n = 0
examples_struct = {}
for line in open(path, encoding="utf-8", errors="replace"):
    if targets[0] not in line and targets[1] not in line:
        continue
    n += 1
    try:
        obj = json.loads(line)
    except Exception as e:
        print(f"non-json line: {type(e).__name__}")
        continue
    for k, v in obj.items():
        top_keys[k] += 1
        field_types[f"{k}:{type(v).__name__}"] += 1
    # look one level deep for likely payload dicts
    for k, v in obj.items():
        if isinstance(v, dict):
            for k2, v2 in v.items():
                nested[f"{k}.{k2}:{type(v2).__name__}"] += 1
print(f"matched_lines={n}")
print("== top_keys ==")
for k, c in top_keys.most_common():
    print(f"{c}\t{k}")
print("== nested keys (one level) ==")
for k, c in nested.most_common(80):
    print(f"{c}\t{k}")
print("== field types ==")
for k, c in field_types.most_common(40):
    print(f"{c}\t{k}")

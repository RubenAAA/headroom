#!/usr/bin/env python3 -I
"""Compact structural summary of responses_stream_event_shape lines. No raw values."""
import json, collections
path = "/home/ruben/headroom-proxy.log"

def struct_sig(v, depth=0):
    if isinstance(v, dict):
        return "{" + ",".join(f"{k}:{struct_sig(v[k], depth+1)}" for k in sorted(v)) + "}"
    if isinstance(v, list):
        if not v: return "[]"
        return "[" + struct_sig(v[0], depth+1) + "]"
    if isinstance(v, str): return "str"
    if isinstance(v, bool): return "bool"
    if isinstance(v, int): return "int"
    if isinstance(v, float): return "float"
    if v is None: return "null"
    return type(v).__name__

by_event = collections.Counter()
by_item = collections.Counter()
struct_by_event = collections.defaultdict(collections.Counter)
keys_by_event = collections.defaultdict(collections.Counter)
usage_keys = collections.Counter()
n = 0
shape_parse_ok = 0
shape_parse_fail = 0
for line in open(path, encoding="utf-8", errors="replace"):
    if "responses_stream_event_shape" not in line:
        continue
    n += 1
    obj = json.loads(line)
    f = obj["fields"]
    ev = f.get("stream_event", "<missing>")
    it = f.get("item_type", "<missing>")
    sh = f.get("shape", "")
    by_event[ev] += 1
    by_item[it] += 1
    try:
        sv = json.loads(sh)
        shape_parse_ok += 1
    except Exception:
        shape_parse_fail += 1
        struct_by_event[ev][f"<non-json len={len(sh)}>"] += 1
        continue
    sig = struct_sig(sv)
    struct_by_event[ev][sig] += 1
    if isinstance(sv, dict):
        for k in sv.keys():
            keys_by_event[ev][k] += 1
        # drill into response.completed usage
        if ev == "response.completed" and isinstance(sv.get("response"), dict):
            u = sv["response"].get("usage")
            if isinstance(u, dict):
                for k in u.keys():
                    usage_keys[k] += 1
print(f"total_shape_lines={n} parse_ok={shape_parse_ok} parse_fail={shape_parse_fail}")
print("== by stream_event ==")
for k, c in by_event.most_common():
    print(f"{c}\t{k}")
print("== by item_type ==")
for k, c in by_item.most_common():
    print(f"{c}\t{k!r}")
print("== distinct structures per event (count, sig truncated to 400ch) ==")
for ev in sorted(struct_by_event):
    d = struct_by_event[ev]
    print(f"--- {ev}: {len(d)} distinct structs over {by_event[ev]} lines")
    for sig, c in d.most_common(6):
        s = sig if len(sig) <= 400 else sig[:400] + "..."
        print(f"  {c}\t{s}")
print("== top-level keys per event ==")
for ev in sorted(keys_by_event):
    ks = ",".join(f"{k}({c})" for k, c in keys_by_event[ev].most_common())
    print(f"{ev}: {ks}")
print("== response.completed usage keys ==")
for k, c in usage_keys.most_common():
    print(f"{c}\t{k}")

#!/usr/bin/env python3 -I
"""List tool/call/name-ish event names (counts only) + key schemas (names+types) for candidates."""
import json, collections

FILES = ["/home/ruben/headroom-proxy.log", "/home/ruben/headroom-proxy.log.1",
         "/home/ruben/headroom-proxy.log.2", "/home/ruben/headroom-proxy.log.3",
         "/home/ruben/headroom-proxy.log.4"]
CANDS = {"model_route_translate", "ccr_retrieval_call", "memory_tools_injected",
         "tool_search_deferral", "routed_function_call_without_added",
         "routed_duplicate_call_id_renamed", "routed_done_with_open_block",
         "routed_response_completed", "ccr_tool_call_dropped_stop_reason_downgraded",
         "responses_stream_event_shape"}
toolish = collections.Counter()
schemas = {c: collections.Counter() for c in CANDS}
n_schema = collections.Counter()
for path in FILES:
    try:
        fh = open(path, encoding="utf-8", errors="replace")
    except OSError:
        continue
    for line in fh:
        if '"event"' not in line:
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        f = o.get("fields", {})
        if not isinstance(f, dict):
            continue
        ev = f.get("event")
        if not isinstance(ev, str):
            continue
        low = ev.lower()
        if "tool" in low or "function" in low or "call" in low or "alias" in low or "shadow" in low or "shape" in low:
            toolish[ev] += 1
        if ev in CANDS and n_schema[ev] < 2000:
            n_schema[ev] += 1
            for k, v in f.items():
                schemas[ev][f"{k}:{type(v).__name__}"] += 1
print("tool/call/name-ish events:")
for k, c in sorted(toolish.items()):
    print(f"  {c} {k}")
for ev in sorted(CANDS):
    print(f"== schema {ev} (n={n_schema[ev]}) ==")
    for k, c in sorted(schemas[ev].items()):
        print(f"  {c} {k}")

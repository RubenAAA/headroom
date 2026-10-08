#!/usr/bin/env python3 -I
"""Shadow-call distinguishability aggregation. Counts/buckets/hashes only; never prints raw log values."""
import json, collections, hashlib

FILES = ["/home/ruben/headroom-proxy.log", "/home/ruben/headroom-proxy.log.1",
         "/home/ruben/headroom-proxy.log.2", "/home/ruben/headroom-proxy.log.3",
         "/home/ruben/headroom-proxy.log.4"]
SHADOW = {"bash", "edit", "glob", "grep", "read"}

def h(v):
    return hashlib.sha256(str(v).encode()).hexdigest()[:12]

levels = collections.Counter()
events = collections.Counter()
ts_range = {}
def bump(ev, ts):
    r = ts_range.get(ev)
    if r is None: ts_range[ev] = [ts, ts]
    else:
        if ts < r[0]: r[0] = ts
        if ts > r[1]: r[1] = ts

alias_n = 0; alias_ren = collections.Counter(); alias_shad = collections.Counter()
shad_n = 0; shad_calls = collections.Counter()
comp_n = 0; comp_model = collections.Counter(); comp_prov = collections.Counter()
comp_saw = collections.Counter(); comp_stop = collections.Counter(); comp_items = collections.Counter()
comp_funccall = 0
drop = {}  # ev -> {n, shadowname Counter, other, otherhash set, null}
for ev in ("ccr_tool_call_dropped", "ccr_tool_call_dropped_stop_reason_downgraded", "ccr_continuation_answer_lost"):
    drop[ev] = {"n": 0, "names": collections.Counter(), "other": 0, "otherh": set(), "null": 0}
notstreamed_n = 0
texhead_n = 0
shadowish = collections.Counter()
total = 0; jsonok = 0

for path in FILES:
    try:
        fh = open(path, encoding="utf-8", errors="replace")
    except OSError as e:
        print(f"SKIP {path}: {type(e).__name__}")
        continue
    for line in fh:
        total += 1
        if '"event"' not in line:
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        jsonok += 1
        lv = o.get("level", "?")
        levels[lv if isinstance(lv, str) else "?"] += 1
        f = o.get("fields", {})
        if not isinstance(f, dict):
            continue
        ev = f.get("event")
        if not isinstance(ev, str):
            continue
        events[ev] += 1
        ts = o.get("timestamp", "")
        bump(ev, ts if isinstance(ts, str) else "")
        if "shadow" in ev or "alias" in ev:
            shadowish[ev] += 1
        if ev == "zen_tool_alias_applied":
            alias_n += 1
            alias_ren[f.get("renamed", "?")] += 1
            alias_shad[f.get("shadowed", "?")] += 1
        elif ev == "zen_shadow_call_answered":
            shad_n += 1
            shad_calls[f.get("calls", "?")] += 1
        elif ev == "routed_response_completed":
            comp_n += 1
            m = f.get("upstream_model", "")
            comp_model["spark" if "spark" in str(m).lower() else ("codex" if "codex" in str(m).lower() else "other:" + h(m))] += 1
            p = f.get("routed_provider", "")
            comp_prov["zen" if "zen" in str(p).lower() else "other:" + h(p)] += 1
            comp_saw[str(f.get("saw_tool_use"))] += 1
            comp_stop[str(f.get("stop_reason"))] += 1
            oi = f.get("output_items", "")
            comp_items[str(oi)] += 1
            if "function_call" in str(oi):
                comp_funccall += 1
        elif ev in drop:
            d = drop[ev]
            d["n"] += 1
            nm = f.get("unresolved_tool_name")
            if nm is None:
                d["null"] += 1
            elif isinstance(nm, str) and nm in SHADOW:
                d["names"][nm] += 1
            else:
                d["other"] += 1
                d["otherh"].add(h(nm))
        elif ev == "routed_completed_call_not_streamed":
            notstreamed_n += 1
        elif ev == "routed_response_text_head":
            texhead_n += 1

print(f"files={len(FILES)} lines_total={total} json_with_event={jsonok}")
print("levels:", dict(sorted(levels.items())))
print(f"distinct_events={len(events)}")
print("top_events:")
for k, c in events.most_common(60):
    print(f"  {c} {k}")
print("shadow_or_alias_events:", dict(sorted(shadowish.items())))
print(f"zen_tool_alias_applied n={alias_n} renamed_hist={dict(sorted(alias_ren.items(), key=str))} shadowed_hist={dict(sorted(alias_shad.items(), key=str))} range={ts_range.get('zen_tool_alias_applied')}")
print(f"zen_shadow_call_answered n={shad_n} calls_hist={dict(sorted(shad_calls.items(), key=str))} range={ts_range.get('zen_shadow_call_answered')}")
print(f"routed_response_completed n={comp_n} range={ts_range.get('routed_response_completed')}")
print(f"  model_bucket={dict(sorted(comp_model.items()))}")
print(f"  provider_bucket={dict(sorted(comp_prov.items()))}")
print(f"  saw_tool_use={dict(sorted(comp_saw.items()))} stop_reason={dict(sorted(comp_stop.items()))}")
print(f"  output_items_with_function_call={comp_funccall}")
print("  output_items_top:")
for k, c in comp_items.most_common(15):
    print(f"    {c} {k[:120]}")
for ev, d in drop.items():
    print(f"{ev} n={d['n']} shadow_names={dict(sorted(d['names'].items()))} other={d['other']} other_distinct_hashes={len(d['otherh'])} null={d['null']} range={ts_range.get(ev)}")
print(f"routed_completed_call_not_streamed n={notstreamed_n} range={ts_range.get('routed_completed_call_not_streamed')}")
print(f"routed_response_text_head(debug_probe) n={texhead_n}")

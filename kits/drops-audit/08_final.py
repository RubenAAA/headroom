#!/usr/bin/env python3 -I
"""Final drops-audit aggregation. Exact fields.event match only."""
import json, collections, re
path = "/home/ruben/headroom-proxy.log"
uh_n = 0; sh_n = 0
uh_ts = [None, None]; sh_ts = [None, None]
uh_by = collections.Counter(); uh_max = {}
sh_by = collections.Counter()
trunc = collections.Counter()
usage = collections.Counter(); comp_n = 0
keyre = re.compile(r'"([A-Za-z_][A-Za-z0-9_]*)"\s*:')
ann = [0, 0]; lp = [0, 0]
enc = collections.Counter(); itype = collections.Counter()

def bump(r, ts):
    if r[0] is None or ts < r[0]: r[0] = ts
    if r[1] is None or ts > r[1]: r[1] = ts

def walk(v):
    if isinstance(v, dict):
        for k, w in v.items():
            if k == "annotations" and isinstance(w, list):
                ann[1] += 1; ann[0] += 1 if w else 0
            elif k == "logprobs" and isinstance(w, list):
                lp[1] += 1; lp[0] += 1 if w else 0
            else: walk(w)
    elif isinstance(v, list):
        for w in v: walk(w)

for line in open(path, encoding="utf-8", errors="replace"):
    if "codex_unhandled_stream_event" not in line and "responses_stream_event_shape" not in line:
        continue
    try: o = json.loads(line)
    except Exception: continue
    f = o.get("fields", {}); ev = f.get("event"); ts = o.get("timestamp", "")
    if ev == "codex_unhandled_stream_event":
        uh_n += 1; bump(uh_ts, ts)
        se = f.get("stream_event", "?"); uh_by[se] += 1
        c = f.get("count", 0)
        if isinstance(c, int) and c > uh_max.get(se, -1): uh_max[se] = c
    elif ev == "responses_stream_event_shape":
        sh_n += 1; bump(sh_ts, ts)
        se = f.get("stream_event", "?"); sh_by[se] += 1
        sh = f.get("shape", "")
        try: p = json.loads(sh)
        except Exception:
            trunc[(se, len(sh))] += 1
            if se == "response.completed":
                comp_n += 1; ks = set(keyre.findall(sh))
                for k in ("usage","input_tokens","output_tokens","total_tokens","input_tokens_details","output_tokens_details","cached_tokens","reasoning_tokens","encrypted_content","output"):
                    if k in ks: usage[k] += 1
            continue
        walk(p)
        if se in ("response.output_item.added", "response.output_item.done"):
            it = p.get("item", {}).get("type", "?")
            itype[(se, it)] += 1
            if it == "reasoning":
                enc[(se, "with_enc" if "encrypted_content" in p.get("item", {}) else "no_enc")] += 1

print(f"unhandled_true={uh_n} range={uh_ts[0]}..{uh_ts[1]}")
for k in sorted(uh_by): print(f"  UH {uh_by[k]} lines maxcount={uh_max.get(k)} {k}")
print(f"shapes_true={sh_n} range={sh_ts[0]}..{sh_ts[1]}")
for k in sorted(sh_by): print(f"  SH {sh_by[k]} {k}")
print("truncated(seclen)=count:")
for k in sorted(trunc): print(f"  {k}={trunc[k]}")
print(f"completed_trunc={comp_n} usage_key_lines={dict(sorted(usage.items()))}")
print(f"annotations_nonempty/total={ann[0]}/{ann[1]} logprobs_nonempty/total={lp[0]}/{lp[1]}")
print("item_types:")
for k in sorted(itype): print(f"  {k}={itype[k]}")
print("reasoning_enc:")
for k in sorted(enc): print(f"  {k}={enc[k]}")

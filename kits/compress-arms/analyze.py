#!/usr/bin/env python3
"""Analyze one arm: join runner records with stub-captured forwarded bodies.

Usage: analyze.py <arm>
Prints per-request table + summary (savings %, client_ms p50/p90, errors).
"""
import json
import statistics
import sys
from pathlib import Path

ARM = sys.argv[1]
BASE = Path("/tmp/compress-arms")

rows = []
for line in (BASE / "results" / f"{ARM}.jsonl").read_text().splitlines():
    r = json.loads(line)
    cap = BASE / "capture" / ARM / f"{r['seq']:03d}.bin"
    fwd = cap.stat().st_size if cap.exists() else None
    saved = (r["orig_bytes"] - fwd) if fwd is not None else None
    rows.append({**r, "fwd_bytes": fwd, "saved_bytes": saved,
                 "saved_pct": round(100 * saved / r["orig_bytes"], 1) if saved is not None else None})

print(f"=== {ARM} ===")
print(f"{'seq':>3} {'shape':<16} {'orig':>7} {'fwd':>7} {'saved':>7} {'pct':>6} {'ms':>8} {'st':>4}")
tot_o = tot_s = 0
lat, errs = [], 0
for r in rows:
    fwd = "-" if r["fwd_bytes"] is None else r["fwd_bytes"]
    sv = "-" if r["saved_bytes"] is None else r["saved_bytes"]
    pc = "-" if r["saved_pct"] is None else r["saved_pct"]
    print(f"{r['seq']:>3} {r['shape']:<16} {r['orig_bytes']:>7} {fwd:>7} {sv:>7} {pc:>6} {r['client_ms']:>8} {r['status']:>4}")
    if r["fwd_bytes"] is not None:
        tot_o += r["orig_bytes"]
        tot_s += r["saved_bytes"]
    lat.append(r["client_ms"])
    if r["status"] != 200:
        errs += 1
lat.sort()
p50 = lat[len(lat) // 2] if lat else 0
p90 = lat[int(len(lat) * 0.9)] if lat else 0
print(f"TOTAL orig={tot_o} saved={tot_s} ({100*tot_s/tot_o:.1f}%) "
      f"tok_saved~{tot_s//4} tok_after~{(tot_o-tot_s)//4} (bytes/4 est)")
print(f"client_ms p50={p50} p90={p90} max={lat[-1] if lat else 0} errors={errs}")
# compression-stage ms from the arm proxy log (last 20 stage_timings)
cms = []
try:
    for line in (BASE / "logs" / f"{ARM}.log").read_text().splitlines():
        try:
            d = json.loads(line)
        except Exception:
            continue
        f = d.get("fields", {})
        if f.get("event") == "stage_timings":
            st = f.get("stages", {})
            if isinstance(st, str):
                try:
                    st = json.loads(st)
                except Exception:
                    st = {}
            if isinstance(st, dict) and st.get("compression") is not None:
                cms.append(round(st["compression"], 2))
except FileNotFoundError:
    pass
cms = cms[-20:]
if cms:
    cms.sort()
    print(f"compression_ms n={len(cms)} p50={cms[len(cms)//2]} p90={cms[int(len(cms)*0.9)]} max={cms[-1]}")
else:
    print("compression_ms: no stage_timings found")

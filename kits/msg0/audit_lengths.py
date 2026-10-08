#!/usr/bin/env python3
"""Check 2: per-request Content-Length audit.

Joins proxy `outbound_body_bytes` events (request_id -> bytes_out) with stub
captures (x-request-id header -> raw_len, content_length_hdr). Also flags:
  - content_length_hdr != raw_len (stub-side short read / transport cut)
  - non-JSON bodies at the stub
  - duplicate stub captures for one x-request-id (retry resends)
  - proxy bytes_out with no matching stub capture (attempt never landed)

Usage: python3 -I audit_lengths.py <proxy_log> <capture_dir>
"""
import glob
import json
import sys

log_path, cap_dirs = sys.argv[1], sys.argv[2:]

out = {}   # request_id -> bytes_out (last wins; attempts overwrite)
retries = set()
perrors = 0
for line in open(log_path):
    try:
        d = json.loads(line)
    except Exception:
        continue
    f = d.get("fields", {})
    ev = f.get("event")
    if ev == "outbound_body_bytes":
        out[f["request_id"]] = f["bytes_out"]
    elif ev == "upstream_transport_retrying":
        retries.add(f["request_id"])
    elif ev == "proxy_error":
        perrors += 1

caps = []
for cap_dir in cap_dirs:
    for fn in glob.glob(cap_dir + "/*.json"):
        d = json.load(open(fn))
        d["_file"] = fn.split("/")[-1]
        caps.append(d)

by_rid = {}
for c in caps:
    rid = (c.get("headers") or {}).get("x-request-id")
    by_rid.setdefault(rid, []).append(c)

print(f"proxy outbound events: {len(out)}  retries: {len(retries)}  proxy_errors: {perrors}")
print(f"stub captures: {len(caps)}")
print(f"captures with x-request-id: {sum(1 for c in caps if (c.get('headers') or {}).get('x-request-id'))}")

bad = 0
for c in sorted(caps, key=lambda c: c["seq"]):
    h = c.get("headers") or {}
    rid = h.get("x-request-id")
    clen, rlen = c.get("content_length_hdr"), c.get("raw_len")
    b = c.get("body")
    issues = []
    if clen is not None and rlen is not None and clen != rlen:
        issues.append(f"CLEN-MISMATCH hdr={clen} read={rlen}")
    if isinstance(b, dict) and "_stub_note" in b:
        issues.append(f"NON-JSON note={b.get('_stub_note')} raw_len={rlen}")
    if rid in out and rlen != out[rid]:
        issues.append(f"SENT-vs-GOT proxy_bytes_out={out[rid]} stub_raw_len={rlen}")
    if rid and len(by_rid.get(rid, [])) > 1:
        issues.append(f"DUP-CAPTURE n={len(by_rid[rid])}")
    if issues:
        bad += 1
        m = (b.get("messages", b.get("input")) if isinstance(b, dict) else None)
        cnt = len(m) if isinstance(m, list) else "?"
        print(f"seq={c['seq']} {c['path']} msgs={cnt} rid={str(rid)[:8]} :: {' | '.join(issues)}")

matched = sum(1 for r in out if r in by_rid)
print(f"proxy reqs matched to >=1 capture: {matched}/{len(out)}")
unmatched = [r for r in out if r not in by_rid]
if unmatched:
    print(f"unmatched proxy reqs (sent but never landed): {len(unmatched)} "
          f"of which retried: {sum(1 for r in unmatched if r in retries)}")
print(f"captures with issues: {bad}")

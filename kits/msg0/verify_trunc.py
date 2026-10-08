#!/usr/bin/env python3
"""Verify request-body integrity under forced response-side truncation.

Checks:
 1. Mode: proxy log shows IncompleteMessage and/or proxy_error.
 2. Integrity: every stub capture's messages[]/input[] count + sha256
    (cache_control stripped) matches the sent manifest for its marker.
 3. Retry accounting: same-RID double-landings (proxy resend) vs
    same-marker multi-RID (client resend); failed responses must not
    corrupt subsequent captures (explicit adjacency check).

Usage: python3 -I verify_trunc.py <sent_jsonl> <capture_dir> <proxy_log> <report_json>
"""
import glob
import hashlib
import json
import sys

sent_path, cap_dir, log_path, report_path = sys.argv[1:5]


def strip_cc(o):
    if isinstance(o, dict):
        return {k: strip_cc(v) for k, v in o.items() if k != "cache_control"}
    if isinstance(o, list):
        return [strip_cc(v) for v in o]
    return o


def msg_hash(msgs):
    canon = json.dumps(strip_cc(msgs), sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canon.encode()).hexdigest()[:16]


sent = [json.loads(l) for l in open(sent_path)]
by_marker = {}
for rec in sent:
    by_marker.setdefault(rec["marker"], []).append(rec)

caps = []
files = sorted(glob.glob(cap_dir + "/*.json"),
               key=lambda f: __import__("os").path.getmtime(f))  # mtime: seq resets on stub restart
for i, fn in enumerate(files):
    d = json.load(open(fn))
    d["_file"] = fn.split("/")[-1]
    d["_i"] = i  # unique position key (seq collides across stub restarts)
    d["_blob"] = json.dumps(d.get("body"))
    caps.append(d)

# ---- 2. integrity per capture ----
missing_markers = set(by_marker)
extra = trunc_mismatch = clen_mismatch = non_json = 0
mismatch_rows = []
matched = 0
cap_marker = {}
for c in caps:
    hits = [mk for mk in by_marker if mk in c["_blob"]]
    if not hits:
        extra += 1
        mismatch_rows.append(f"seq={c['seq']} EXTRA-CAPTURE no-known-marker file={c['_file']}")
        continue
    if len(hits) > 1:
        mismatch_rows.append(f"seq={c['seq']} AMBIG-MARKER {hits}")
    mk = hits[0]
    cap_marker[c["_i"]] = mk
    missing_markers.discard(mk)
    want = by_marker[mk][0]  # pass-1 record defines the body
    b = c.get("body") or {}
    m = b.get("messages", b.get("input"))
    n = len(m) if isinstance(m, list) else "?"
    h = msg_hash(m) if isinstance(m, list) else "?"
    if n == want["sent_n"] and h == want["sent_hash"]:
        matched += 1
    else:
        trunc_mismatch += 1
        mismatch_rows.append(
            f"seq={c['seq']} {mk} {c['path']} mode={c.get('resp_mode')} "
            f"sent_n={want['sent_n']} got_n={n} sent_h={want['sent_hash']} got_h={h}")
    if c.get("content_length_hdr") != c.get("raw_len"):
        clen_mismatch += 1
        mismatch_rows.append(
            f"seq={c['seq']} CLEN-MISMATCH hdr={c.get('content_length_hdr')} read={c.get('raw_len')}")
    if isinstance(b, dict) and "_stub_note" in b:
        non_json += 1
        mismatch_rows.append(f"seq={c['seq']} NON-JSON {b.get('_stub_note')}")

missing = len(missing_markers)

# ---- 3a. retry accounting ----
by_rid = {}
for c in caps:
    rid = (c.get("headers") or {}).get("x-request-id")
    by_rid.setdefault(rid, []).append(c)
same_rid_multi = {r: v for r, v in by_rid.items() if r and len(v) > 1}
no_rid = len(by_rid.get(None, []))

by_mk_rids = {}
for c in caps:
    mk = cap_marker.get(c["_i"])
    if mk:
        by_mk_rids.setdefault(mk, set()).add((c.get("headers") or {}).get("x-request-id"))
client_resent = {mk: rids for mk, rids in by_mk_rids.items() if len(rids) > 1}

# ---- 3b. adjacency: capture right after a truncated response must be clean ----
trunc_modes = {"truncated-clen", "truncated-chunked"}
after_trunc_total = after_trunc_bad = 0
for idx, c in enumerate(caps[:-1]):
    if c.get("resp_mode") in trunc_modes:
        nxt = caps[idx + 1]
        after_trunc_total += 1
        mk = cap_marker.get(nxt["_i"])
        if not mk:
            after_trunc_bad += 1
            continue
        want = by_marker[mk][0]
        b = nxt.get("body") or {}
        m = b.get("messages", b.get("input"))
        n = len(m) if isinstance(m, list) else "?"
        h = msg_hash(m) if isinstance(m, list) else "?"
        if n != want["sent_n"] or h != want["sent_hash"]:
            after_trunc_bad += 1

# ---- 1. proxy log: mode forced? ----
incomplete_lines = perrors = retrying = outbound = 0
mid_err_rids = set()
buf_fail_rids = set()
incomplete_body_lines = 0
perror_rids = set()
perror_status = {}
incomplete_sample = None
n_log = 0
for line in open(log_path, errors="replace"):
    n_log += 1
    low = line.lower()
    if "incompletemessage" in low or "incomplete message" in low:
        incomplete_lines += 1
        if incomplete_sample is None:
            incomplete_sample = line.strip()[:600]
    if "incompletebody" in low:
        incomplete_body_lines += 1
    try:
        d = json.loads(line)
    except Exception:
        continue
    f = d.get("fields", {})
    ev = f.get("event")
    if ev == "proxy_error":
        perrors += 1
        if f.get("request_id"):
            perror_rids.add(f["request_id"])
        st = f.get("status")
        perror_status[st] = perror_status.get(st, 0) + 1
    elif ev == "upstream_transport_retrying":
        retrying += 1
    elif ev == "outbound_body_bytes":
        outbound += 1
    elif ev == "upstream_stream_mid_response_error":
        if f.get("request_id"):
            mid_err_rids.add(f["request_id"])
    elif ev == "non_sse_buffer_failed":
        if f.get("request_id"):
            buf_fail_rids.add(f["request_id"])

# join: proxy_error RIDs seen at stub?
stub_rids = {r for r in by_rid if r}
perror_rids_at_stub = len(perror_rids & stub_rids)

report = {
    "mode": {"incomplete_message_lines": incomplete_lines,
             "incomplete_body_cause_lines": incomplete_body_lines,
             "mid_response_error_rids": len(mid_err_rids),
             "non_sse_buffer_failed_rids": len(buf_fail_rids),
             "incomplete_sample": incomplete_sample,
             "proxy_errors": perrors, "proxy_error_status": perror_status,
             "proxy_error_rids": len(perror_rids),
             "proxy_error_rids_seen_at_stub": perror_rids_at_stub,
             "upstream_transport_retrying": retrying,
             "outbound_body_bytes": outbound, "log_lines": n_log},
    "integrity": {"sent_markers": len(by_marker), "captures": len(caps),
                  "matched": matched, "missing_markers": missing,
                  "missing_list": sorted(missing_markers)[:20],
                  "extra_captures": extra, "body_mismatches": trunc_mismatch,
                  "clen_mismatches": clen_mismatch, "non_json": non_json},
    "retry": {"same_rid_double_landings": len(same_rid_multi),
              "same_rid_details": {str(k)[:8]: [c["_file"] for c in v]
                                   for k, v in list(same_rid_multi.items())[:20]},
              "landing_histogram": {str(n): sum(1 for v in by_rid.values() if len(v) == n)
                                    for n in sorted({len(v) for v in by_rid.values() if v})},
              "client_resent_markers": len(client_resent),
              "captures_without_rid": no_rid},
    "adjacency": {"captures_after_truncated": after_trunc_total,
                  "bad_after_truncated": after_trunc_bad},
}
json.dump(report, open(report_path, "w"), indent=1)

print("== 1. mode ==")
print(f"  IncompleteMessage lines: {incomplete_lines}  IncompleteBody cause lines: {incomplete_body_lines}")
print(f"  mid_response_error rids: {len(mid_err_rids)}  non_sse_buffer_failed rids: {len(buf_fail_rids)}")
print(f"  proxy_error: {perrors} status={perror_status} rids={len(perror_rids)} "
      f"rids_seen_at_stub={perror_rids_at_stub}")
print(f"  upstream_transport_retrying: {retrying}  outbound_body_bytes: {outbound}")
if incomplete_sample:
    print(f"  sample: {incomplete_sample[:300]}")
print("== 2. integrity ==")
print(f"  sent_markers={len(by_marker)} captures={len(caps)} matched={matched} "
      f"missing={missing} extra={extra} body_mismatch={trunc_mismatch} "
      f"clen_mismatch={clen_mismatch} non_json={non_json}")
print("== 3. retry ==")
print(f"  same-RID double-landings: {len(same_rid_multi)}  "
      f"client-resent markers: {len(client_resent)}  no-rid captures: {no_rid}")
print("== 3b. adjacency ==")
print(f"  captures immediately after a truncated response: {after_trunc_total}, "
      f"bad: {after_trunc_bad}")
if mismatch_rows:
    print("== anomaly rows ==")
    for r in mismatch_rows[:40]:
        print("  " + r)
print(f"wrote {report_path}")

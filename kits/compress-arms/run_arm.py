#!/usr/bin/env python3
"""Sequential runner: POST each payload to a side proxy, record latency/status.

Usage: run_arm.py <arm-name> <proxy-port>
Reads payloads in manifest order, POSTs to http://127.0.0.1:<port>/v1/messages,
writes /tmp/compress-arms/results/<arm>.jsonl with one record per request:
  {seq, file, shape, orig_bytes, status, client_ms, error}
The stub captures forwarded bodies separately under capture/<arm>/.
"""
import json
import sys
import time
import urllib.request
from pathlib import Path

ARM, PORT = sys.argv[1], sys.argv[2]
BASE = Path("/tmp/compress-arms")
manifest = json.loads((BASE / "payloads" / "manifest.json").read_text())
out = open(BASE / "results" / f"{ARM}.jsonl", "w")

for seq, m in enumerate(manifest, 1):
    body = (BASE / "payloads" / m["file"]).read_bytes()
    req = urllib.request.Request(
        f"http://127.0.0.1:{PORT}/v1/messages", data=body, method="POST",
        headers={"Content-Type": "application/json",
                 "x-api-key": "stub-dummy-key",
                 "anthropic-version": "2023-06-01"})
    rec = {"seq": seq, "file": m["file"], "shape": m["shape"],
           "orig_bytes": len(body)}
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=180) as r:
            r.read()
            rec["status"] = r.status
    except Exception as e:  # noqa: BLE001 - record and continue
        rec["status"] = 0
        rec["error"] = f"{type(e).__name__}: {e}"[:200]
    rec["client_ms"] = round((time.monotonic() - t0) * 1000, 1)
    out.write(json.dumps(rec) + "\n")
    out.flush()
    print(f"{seq:02d} {m['file']}: status={rec['status']} ms={rec['client_ms']}", flush=True)
out.close()

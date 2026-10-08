#!/usr/bin/env python3
"""Probe which content shapes engage the lossy arms via side proxy :8789."""
import json
import os
import urllib.request

BASE = "/tmp/compress-arms"
CAP = os.path.join(BASE, "capture", "baseline")
TOOLS = [{"name": "Bash", "description": "x",
          "input_schema": {"type": "object", "properties": {}}}]


def post(text, tid):
    msgs = [{"role": "assistant",
             "content": [{"type": "text", "text": "run"},
                         {"type": "tool_use", "id": tid,
                          "name": "Bash", "input": {}}]},
            {"role": "user",
             "content": [{"type": "tool_result", "tool_use_id": tid,
                          "content": text}]}]
    body = json.dumps(
        {"model": "claude-opus-4-6", "max_tokens": 64,
         "tools": TOOLS, "messages": msgs}).encode()
    req = urllib.request.Request(
        "http://127.0.0.1:8789/v1/messages", data=body,
        headers={"Content-Type": "application/json"})
    st = urllib.request.urlopen(req, timeout=60).status
    fn = sorted(f for f in os.listdir(CAP) if f.endswith(".bin"))[-1]
    fwd = json.load(open(os.path.join(CAP, fn)))
    blk = fwd["messages"][-1]["content"][-1]
    out = blk.get("text", "") if blk.get("type") == "text" else str(blk)
    return st, len(body), len(out)


def main():
    for f in os.listdir(CAP):
        if f.endswith((".bin", ".hdr")):
            os.remove(os.path.join(CAP, f))
    lines = ["2026-10-08T10:00:00Z INFO ok id=%d" % i for i in range(100)]
    lines += ["2026-10-08T10:01:00Z ERROR boom id=999"] * 3
    probes = [
        ("rep-log", "\n".join(lines)),
        ("real-code", open("/home/ruben/headroom/crates/headroom-core/src/transforms/detection.rs").read()),
        ("real-diff", open("/tmp/compress-arms/probe-diff.txt").read()),
        ("real-grep", open("/tmp/compress-arms/probe-grep.txt").read() * 8),
        ("prose-md", open("/home/ruben/headroom/docs/notes/ideas/README.md").read() * 6),
    ]
    for i, (name, text) in enumerate(probes):
        st, orig, new = post(text, "tu_%d" % i)
        print("%s st=%s orig=%d new=%d saved=%.1f%%" % (
            name, st, orig, new, 100.0 * (orig - new) / orig))


main()

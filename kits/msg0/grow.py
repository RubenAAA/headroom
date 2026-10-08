#!/usr/bin/env python3
"""Growing-conversation repro: accumulate history turn by turn, like a real chat.

Usage: python3 grow.py <proxy_base>
Sends 6 growing turns on chat + anthropic paths, prints sent-vs-got per turn
by diffing the stub capture dir before/after.
"""
import json, sys, glob, os, urllib.request

BASE = sys.argv[1]
CAP = os.environ.get("MSG0_CAPTURE_DIR", "/tmp/msg0/captures-st")

def post(path, body, key="sk-ant-sub-test"):
    req = urllib.request.Request(
        BASE + path, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {key}",
                 "anthropic-version": "2023-06-01"},
        method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status
    except Exception as e:
        return f"ERR {e}"

def seqs():
    return {json.load(open(f))["seq"] for f in glob.glob(f"{CAP}/*.json")}

def got(seq_before):
    after = sorted(seqs() - seq_before)
    assert len(after) == 1, after
    d = [json.load(open(f)) for f in glob.glob(f"{CAP}/*.json")
         if json.load(open(f))["seq"] == after[0]][0]
    b = d["body"] or {}
    m = b.get("messages", b.get("input"))
    return len(m) if isinstance(m, list) else "?", m

TOOLS_CHAT = [{"type": "function",
               "function": {"name": "fn", "description": "d",
                            "parameters": {"type": "object", "properties": {}}}}]
TOOLS_ANT = [{"name": "t", "description": "d",
              "input_schema": {"type": "object", "properties": {}}}]

# chat: grow [system, u1, a1, u2, a2, ...]
hist = [{"role": "system", "content": "grow probe"}]
for turn in range(1, 7):
    hist.append({"role": "user", "content": f"q{turn}"})
    if turn > 1:
        hist.insert(-1, {"role": "assistant", "content": f"a{turn-1}"})
    b4 = seqs()
    st = post("/v1/chat/completions",
              {"model": "gpt-5-mini", "messages": list(hist),
               "tools": TOOLS_CHAT}, key="sk-gpt-payg")
    n, m = got(b4)
    flag = "" if n == len(hist) else "  <-- DROP"
    print(f"chat turn={turn} sent={len(hist)} got={n} http={st}{flag}")

# anthropic: grow [u1, a1, u2, a2, ...] + system array
hist = []
for turn in range(1, 7):
    hist.append({"role": "user",
                 "content": [{"type": "text", "text": f"q{turn}"}]})
    if turn > 1:
        hist.insert(-1, {"role": "assistant",
                         "content": [{"type": "text", "text": f"a{turn-1}"}]})
    b4 = seqs()
    st = post("/v1/messages",
              {"model": "claude-sonnet-4-5", "max_tokens": 8,
               "system": [{"type": "text", "text": "grow"}],
               "messages": [dict(x) for x in hist], "tools": TOOLS_ANT})
    n, m = got(b4)
    flag = "" if n == len(hist) else "  <-- DROP"
    print(f"anthropic turn={turn} sent={len(hist)} got={n} http={st}{flag}")

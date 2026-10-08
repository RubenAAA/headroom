#!/usr/bin/env python3
"""Edge-shape probes: streaming, tool conversations, concurrent burst, markers.

Usage: python3 edge.py <proxy_base>
Prints sent-vs-got per probe by diffing the stub capture dir.
"""
import json, sys, glob, os, urllib.request, concurrent.futures

BASE = sys.argv[1]
CAP = os.environ.get("MSG0_CAPTURE_DIR", "/tmp/msg0/captures-st")
KEY = "Bearer sk-ant-sideprobe-subscription-shape"
OPS = urllib.request.build_opener(urllib.request.ProxyHandler({}))

def post(path, body, key=KEY, stream=False):
    req = urllib.request.Request(
        BASE + path, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": key})
    try:
        r = OPS.open(req, timeout=30)
        if stream:
            n = 0
            for _ in r: n += 1
            return (r.status, n)
        r.read()
        return (r.status, 0)
    except Exception as e:
        return (f"ERR {e}", 0)

def seq_before():
    m = 0
    for f in glob.glob(f"{CAP}/*.json"):
        try: m = max(m, json.load(open(f))["seq"])
        except Exception: pass
    return m

def got_since(s, label, sent):
    ms = []
    for f in glob.glob(f"{CAP}/*.json"):
        try: d = json.load(open(f))
        except Exception: continue
        if d["seq"] > s:
            b = d["body"] or {}
            m = b.get("messages", b.get("input")) or []
            ms.append((d["seq"], len(m)))
    ms.sort()
    tot = sum(n for _, n in ms)
    flag = "" if tot == sent else "  <-- MISMATCH"
    print(f"{label}: sent={sent} got={tot} http_batches={len(ms)}{flag}")
    return tot == sent

s = seq_before()
st, chunks = post("/v1/chat/completions",
    {"model": "gpt-5-mini", "stream": True,
     "messages": [{"role": "system", "content": "s"},
                  {"role": "user", "content": "hi"}]}, stream=True)
print("streaming chat http=", st, "sse_chunks=", chunks)
got_since(s, "streaming-chat", 2)

s = seq_before()
tools = [{"type": "function", "function": {
    "name": "get_weather",
    "parameters": {"type": "object",
                   "properties": {"city": {"type": "string"}}}}}]
msgs = [{"role": "system", "content": "s"}, {"role": "user", "content": "weather?"}]
sts = []
for turn in range(3):
    st, _ = post("/v1/chat/completions",
                 {"model": "gpt-5-mini", "messages": msgs, "tools": tools})
    sts.append(st)
    msgs.append({"role": "assistant", "tool_calls": [
        {"id": f"c{turn}", "type": "function",
         "function": {"name": "get_weather", "arguments": '{"city":"x"}'}}]})
    msgs.append({"role": "tool", "tool_call_id": f"c{turn}", "content": "sunny"})
    msgs.append({"role": "user", "content": f"and turn {turn}?"})
print("tool-convo http=", sts)
got_since(s, "tool-convo", 2 + 5 + 8)

s = seq_before()
mk = lambda i: {"model": "gpt-5-mini",
                "messages": [{"role": "system", "content": f"s{i}"},
                             {"role": "user", "content": f"hi{i}"}]}
with concurrent.futures.ThreadPoolExecutor(max_workers=10) as ex:
    futs = [ex.submit(post, "/v1/chat/completions", mk(i)) for i in range(10)]
    sts = [f.result()[0] for f in futs]
print("burst http=", sts)
got_since(s, "burst-10x2", 20)

s = seq_before()
st, _ = post("/v1/messages",
    {"model": "claude-sonnet-4-5", "max_tokens": 8,
     "system": [{"type": "text", "text": "s",
                 "cache_control": {"type": "ephemeral"}}],
     "messages": [
         {"role": "user", "content": [
             {"type": "tool_use", "id": "t1", "name": "n", "input": {}}]},
         {"role": "user", "content": [
             {"type": "tool_result", "tool_use_id": "t1",
              "content": "ok",
              "cache_control": {"type": "ephemeral"}}]}],
     "tools": [{"name": "n", "description": "d",
                "input_schema": {"type": "object", "properties": {}}}]})
print("anthropic-markers http=", st)
got_since(s, "anthropic-markers", 2)
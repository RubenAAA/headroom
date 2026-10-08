#!/usr/bin/env python3
"""Send marked multi-message requests through the trunc-test side proxy.

Two passes:
  pass 1: send every marker once (no client retry).
  pass 2: resend once (new request) every marker whose pass-1 http != 200,
          simulating a client retry.

Shapes cycle over 6 (path x stream) variants so both JSON and SSE
response arms are exercised against the truncating stub.

Usage: python3 -I send_trunc.py <proxy_base> <sent_jsonl> <n>
"""
import hashlib
import json
import sys
import urllib.request
import urllib.error

HDR_ANTH = {"Content-Type": "application/json",
            "anthropic-version": "2023-06-01",
            "Authorization": "Bearer test-token",
            "User-Agent": "claude-code/9.9-sideprobe"}
HDR_CHAT = {"Content-Type": "application/json",
            "Authorization": "Bearer test-token",
            "User-Agent": "cursor/9.9-sideprobe"}
HDR_RESP = {"Content-Type": "application/json",
            "Authorization": "Bearer test-token",
            "User-Agent": "codex-cli/9.9-sideprobe"}


def strip_cc(o):
    if isinstance(o, dict):
        return {k: strip_cc(v) for k, v in o.items() if k != "cache_control"}
    if isinstance(o, list):
        return [strip_cc(v) for v in o]
    return o


def msg_hash(msgs):
    canon = json.dumps(strip_cc(msgs), sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canon.encode()).hexdigest()[:16]


def shape(i):
    m = f"mkT{i:05d}"
    v = i % 6
    if v == 0:
        return ("/v1/messages", HDR_ANTH,
                {"model": "claude-sonnet-4-5", "max_tokens": 8, "stream": False,
                 "system": [{"type": "text", "text": "trunc-soak"}],
                 "messages": [{"role": "user", "content": f"first {m}"},
                              {"role": "user", "content": f"second {m}"}],
                 "tools": [{"name": "n", "description": "d",
                            "input_schema": {"type": "object", "properties": {}}}]})
    if v == 1:
        return ("/v1/messages", HDR_ANTH,
                {"model": "claude-sonnet-4-5", "max_tokens": 8, "stream": True,
                 "system": "trunc-soak",
                 "messages": [{"role": "user", "content": f"q1 {m}"},
                              {"role": "assistant", "content": f"a1 {m}"},
                              {"role": "user", "content": f"q2 {m}"}]})
    if v == 2:
        return ("/v1/chat/completions", HDR_CHAT,
                {"model": "gpt-5-mini", "stream": False,
                 "messages": [{"role": "system", "content": f"s {m}"},
                              {"role": "user", "content": f"hi {m}"}]})
    if v == 3:
        return ("/v1/chat/completions", HDR_CHAT,
                {"model": "gpt-5-mini", "stream": True,
                 "messages": [{"role": "system", "content": f"s {m}"},
                              {"role": "user", "content": f"hi {m}"}]})
    if v == 4:
        return ("/v1/responses", HDR_RESP,
                {"model": "gpt-5-mini", "stream": False,
                 "instructions": "trunc-soak",
                 "input": [{"role": "user", "content": f"r1 {m}"},
                           {"role": "user", "content": f"r2 {m}"}]})
    return ("/v1/responses", HDR_RESP,
            {"model": "gpt-5-mini", "stream": True,
             "instructions": "trunc-soak",
             "input": [{"role": "user", "content": f"r1 {m}"},
                       {"role": "assistant", "content": f"ra {m}"},
                       {"role": "user", "content": f"r2 {m}"}]})


def one(opener, base, i, attempt):
    path, hdr, body = shape(i)
    msgs = body.get("messages", body.get("input"))
    rec = {"i": i, "marker": f"mkT{i:05d}", "path": path,
           "stream": body.get("stream"), "attempt": attempt,
           "sent_n": len(msgs), "sent_hash": msg_hash(msgs),
           "sent_len": len(json.dumps(body).encode())}
    data = json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, headers=hdr, method="POST")
    try:
        r = opener.open(req, timeout=60)
        payload = r.read()
        rec["http"] = r.status
        rec["resp_len"] = len(payload)
        rec["resp_note"] = payload[:120].decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        try:
            payload = e.read()
        except Exception:
            payload = b""
        rec["http"] = e.code
        rec["resp_len"] = len(payload)
        rec["resp_note"] = ("HTTPError body: " + payload[:120].decode("utf-8", "replace"))[:140]
    except Exception as e:
        rec["http"] = f"ERR {type(e).__name__}: {e}"[:100]
        rec["resp_len"] = -1
        rec["resp_note"] = ""
    return rec


def main():
    base, sent_path, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    out = open(sent_path, "w")
    oks = errs = 0
    failed = []
    for i in range(n):
        rec = one(opener, base, i, 1)
        out.write(json.dumps(rec) + "\n")
        out.flush()
        if rec["http"] == 200:
            oks += 1
        else:
            errs += 1
            failed.append(i)
    print(f"pass1 sent={n} ok={oks} err={errs}", flush=True)
    r2oks = r2errs = 0
    for i in failed:
        rec = one(opener, base, i, 2)
        out.write(json.dumps(rec) + "\n")
        out.flush()
        if rec["http"] == 200:
            r2oks += 1
        else:
            r2errs += 1
    print(f"pass2 resent={len(failed)} ok={r2oks} err={r2errs}", flush=True)
    out.close()


if __name__ == "__main__":
    main()

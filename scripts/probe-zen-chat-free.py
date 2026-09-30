#!/usr/bin/env python3
"""Phase 1 probe: Zen free chat-completions models through the anon free tier.

Sends OpenAI chat-completions bodies direct to https://opencode.ai/zen/v1
with the same identity headers the proxy injects (real OpenCode session id,
per-POST request nonce, cli client, UA, project). One row per (model, case).

Models: space-bunny-free, longcat-2.5-preview-free, mimo-v2.6-flash-free,
        mimo-v2.5-free, ling-3.0-flash-fin-free, big-pickle.

Cases per model:
  A plain      non-stream, no tools, max_tokens=512, "Reply with: ok"
  B stream     stream=true, no tools, max_tokens=512
  C camel      non-stream + tools [Read, Bash] (Claude spelling)
  D lower      non-stream + tools [read, bash, edit, glob, grep] (gate spelling)
  E cap64      non-stream, no tools, max_tokens=64 (sidecar-budget question)
  F cap512     non-stream, no tools, max_tokens=512 (same prompt as E)

Output: JSONL to stdout (also --out file). Human table on stderr at the end.
Never retries a 429; records retry-after verbatim. 2s gap between calls.

Session source: $HEADROOM_ZEN_SESSION, else most-recently-used real session
from the local opencode.db (same rule as the proxy: newest-used created more
than 5min ago, else newest-used overall). Project read from the same row.
"""
import argparse
import hashlib
import json
import os
import sqlite3
import sys
import time
import urllib.request
import urllib.error

ZEN_BASE = "https://opencode.ai/zen/v1/chat/completions"
UA = "opencode/1.18.31 ai-sdk/provider-utils/4.0.40 runtime/bun/1.3.14"
GRACE_MS = 5 * 60 * 1000

MODELS = [
    "space-bunny-free",
    "longcat-2.5-preview-free",
    "mimo-v2.6-flash-free",
    "mimo-v2.5-free",
    "ling-3.0-flash-fin-free",
    "big-pickle",
]


def now_ms():
    return int(time.time() * 1000)


def db_path():
    custom = os.environ.get("HEADROOM_OPENCODE_DB", "").strip()
    if custom and os.path.isfile(custom):
        return custom
    home = os.environ.get("HOME", "")
    p = os.path.join(home, ".local/share/opencode/opencode.db")
    return p if os.path.isfile(p) else None


def resolve_session():
    pinned = os.environ.get("HEADROOM_ZEN_SESSION", "").strip()
    if pinned:
        return pinned, os.environ.get("HEADROOM_ZEN_PROJECT", "").strip() or None, "env"
    path = db_path()
    if not path:
        return None, None, "no-db"
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        rows = con.execute(
            "SELECT id, time_created, time_updated FROM session ORDER BY time_updated DESC LIMIT 20"
        ).fetchall()
    finally:
        con.close()
    rows = [r for r in rows if isinstance(r[0], str) and r[0].startswith("ses_") and len(r[0]) > 8]
    if not rows:
        return None, None, "no-rows"
    now = now_ms()
    picked = next((r for r in rows if now - r[1] >= GRACE_MS), rows[0])
    project = None
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        row = con.execute(
            "SELECT project_id FROM session WHERE id = ?", (picked[0],)
        ).fetchone()
        if row and row[0] and row[0].strip():
            project = row[0].strip()
    except Exception:
        pass
    finally:
        con.close()
    if os.environ.get("HEADROOM_ZEN_PROJECT", "").strip():
        project = os.environ["HEADROOM_ZEN_PROJECT"].strip()
    return picked[0], project, "db"


def mint_req_id():
    return "msg_" + hashlib.sha256(str(time.time_ns()).encode()).hexdigest()[:25]


def headers(session, project):
    h = {
        "Content-Type": "application/json",
        "x-opencode-session": session,
        "x-opencode-request": mint_req_id(),
        "x-opencode-client": "cli",
        "User-Agent": UA,
    }
    if project:
        h["x-opencode-project"] = project
    return h


def tool_def(name):
    return {
        "type": "function",
        "function": {
            "name": name,
            "description": f"probe tool {name}",
            "parameters": {"type": "object", "properties": {}},
        },
    }


def bodies():
    return {
        "A_plain": {
            "messages": [{"role": "user", "content": "Reply with the single word: ok"}],
            "max_tokens": 512,
            "stream": False,
        },
        "B_stream": {
            "messages": [{"role": "user", "content": "Reply with the single word: ok"}],
            "max_tokens": 512,
            "stream": True,
        },
        "C_camel": {
            "messages": [{"role": "user", "content": "Reply with the single word: ok"}],
            "max_tokens": 512,
            "stream": False,
            "tools": [tool_def("Read"), tool_def("Bash")],
        },
        "D_lower": {
            "messages": [{"role": "user", "content": "Reply with the single word: ok"}],
            "max_tokens": 512,
            "stream": False,
            "tools": [tool_def(n) for n in ["read", "bash", "edit", "glob", "grep"]],
        },
        "E_cap64": {
            "messages": [{"role": "user", "content": "Describe your most recent action in 3-5 words using present tense (-ing)."}],
            "max_tokens": 64,
            "stream": False,
        },
        "F_cap512": {
            "messages": [{"role": "user", "content": "Describe your most recent action in 3-5 words using present tense (-ing)."}],
            "max_tokens": 512,
            "stream": False,
        },
    }


def post(model, body, hdrs, timeout):
    payload = dict(body)
    payload["model"] = model
    data = json.dumps(payload).encode()
    req = urllib.request.Request(ZEN_BASE, data=data, headers=hdrs, method="POST")
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, dict(r.headers), raw, time.monotonic() - t0, None
    except urllib.error.HTTPError as e:
        try:
            raw = e.read()
        except Exception:
            raw = b""
        return e.code, dict(e.headers or {}), raw, time.monotonic() - t0, None
    except Exception as e:
        return -1, {}, b"", time.monotonic() - t0, f"{type(e).__name__}: {e}"


def summarize_text(model, case, status, raw):
    """Extract assistant text (buffered JSON or SSE fold) + usage if present."""
    if status != 200 or not raw:
        return "", None
    try:
        text = raw.decode("utf-8", "replace")
    except Exception:
        return "", None
    if case == "B_stream":
        parts = []
        for line in text.splitlines():
            line = line.strip()
            if not line.startswith("data:"):
                continue
            data = line[5:].strip()
            if data == "[DONE]":
                continue
            try:
                ev = json.loads(data)
            except Exception:
                continue
            for ch in ev.get("choices", []):
                d = ch.get("delta", {}) or {}
                if isinstance(d.get("content"), str):
                    parts.append(d["content"])
        return "".join(parts).strip(), None
    try:
        v = json.loads(text)
    except Exception:
        return "", None
    usage = v.get("usage")
    try:
        content = v["choices"][0]["message"].get("content") or ""
    except Exception:
        content = ""
    calls = ""
    try:
        tc = v["choices"][0]["message"].get("tool_calls")
        if tc:
            calls = f" [tool_calls: {','.join(c.get('function', {}).get('name', '?') for c in tc)}]"
    except Exception:
        pass
    return (content.strip() + calls).strip(), usage


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="", help="JSONL output file (default: stdout only)")
    ap.add_argument("--models", default=",".join(MODELS))
    ap.add_argument("--cases", default="A_plain,B_stream,C_camel,D_lower,E_cap64,F_cap512")
    ap.add_argument("--timeout", type=float, default=60)
    ap.add_argument("--gap", type=float, default=2.0)
    args = ap.parse_args()

    session, project, source = resolve_session()
    if not session:
        print(f"no Zen session (source={source}); set HEADROOM_ZEN_SESSION or run opencode once", file=sys.stderr)
        sys.exit(2)
    print(f"session source={source} id={session[:12]}... project={'set' if project else 'none'}", file=sys.stderr)

    models = [m.strip() for m in args.models.split(",") if m.strip()]
    cases = [c.strip() for c in args.cases.split(",") if c.strip()]
    all_bodies = bodies()
    out_fh = open(args.out, "w") if args.out else None
    rows = []
    for mi, model in enumerate(models):
        for case in cases:
            hdrs = headers(session, project)
            status, rh, raw, dt, terr = post(model, all_bodies[case], hdrs, args.timeout)
            ctype = ""
            for k, v in rh.items():
                if k.lower() == "content-type":
                    ctype = str(v)[:40]
                if k.lower() == "retry-after":
                    retry_after = str(v)[:40]
                    break
            else:
                retry_after = ""
            text, usage = summarize_text(model, case, status, raw)
            try:
                head = raw.decode("utf-8", "replace")[:300]
            except Exception:
                head = ""
            row = {
                "model": model, "case": case, "status": status,
                "latency_s": round(dt, 2), "text": text[:200],
                "text_len": len(text), "empty": (status == 200 and len(text.strip()) == 0),
                "retry_after": retry_after, "content_type": ctype,
                "usage": usage, "transport_error": terr,
                "body_head": head if status != 200 else head[:120],
            }
            line = json.dumps(row)
            print(line, flush=True)
            if out_fh:
                out_fh.write(line + "\n")
            rows.append(row)
            if not (mi == len(models) - 1 and case == cases[-1]):
                time.sleep(args.gap)
    if out_fh:
        out_fh.close()
    # human table on stderr
    print("\nmodel | case | status | lat_s | txt | text-head", file=sys.stderr)
    for r in rows:
        print(f"{r['model'][:24]:24} {r['case']:8} {r['status']:6} {r['latency_s']:5} "
              f"{r['text_len']:4} {r['text'][:60]} {r['retry_after'] and '(retry-after '+r['retry_after']+')' or ''}",
              file=sys.stderr)


if __name__ == "__main__":
    main()

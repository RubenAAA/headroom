#!/usr/bin/env python3
"""Send the E1/E2/E3/E4 synthetic batches through a side proxy.

Usage: python3 send.py <arm> <proxy_base> <log_path>
  arm:        "test" (enforcement disabled) or "control" (enforcement enabled)
  proxy_base: e.g. http://127.0.0.1:8788
  log_path:   proxy log file, snapshotted around each request so the e*
              events each request produced can be attributed exactly.

Prints one line per request: label, http status, and the e*_events seen.
Writes a JSON report to /tmp/egate-sideproxy/report-<arm>.json.
"""
import json
import sys
import urllib.request

arm, base, log_path = sys.argv[1], sys.argv[2], sys.argv[3]

SUB_ANTH = {"User-Agent": "claude-code/9.9-sideprobe",
            "Authorization": "Bearer sk-ant-oat-sideprobe",
            "anthropic-version": "2023-06-01"}
SUB_CHAT = {"User-Agent": "cursor/9.9-sideprobe",
            "Authorization": "Bearer sk-ant-oat-sideprobe"}
SUB_RESP = {"User-Agent": "codex-cli/9.9-sideprobe",
            "Authorization": "Bearer sk-ant-oat-sideprobe"}
PAYG_ANTH = {"User-Agent": "egate-sideprobe/1.0",
             "x-api-key": "sk-ant-api-sideprobe",
             "anthropic-version": "2023-06-01"}
PAYG_OAI = {"User-Agent": "egate-sideprobe/1.0",
            "Authorization": "Bearer sk-payg-sideprobe"}


def anth_tools_unsorted():
    # Deliberately unsorted tool order AND unsorted schema keys.
    return [
        {"name": "zebra_tool", "description": "z",
         "input_schema": {"type": "object",
                          "properties": {"zeta": {"type": "string"},
                                         "alpha": {"type": "number"}},
                          "required": ["zeta"]}},
        {"name": "Apple_tool", "description": "a",
         "input_schema": {"type": "object",
                          "properties": {"mid": {"type": "boolean"},
                                         "aaa": {"type": "string"}}}},
        {"name": "middle_tool", "description": "m",
         "input_schema": {"type": "object",
                          "properties": {"q": {"type": "string"}}}},
    ]


def anth_body(tools, messages=None, system="sideprobe system v1", model="claude-sonnet-4-5"):
    b = {"model": model, "max_tokens": 64, "system": system,
         "messages": messages or [{"role": "user", "content": "hi"}],
         "stream": False}
    if tools is not None:
        b["tools"] = tools
    return b


def oai_chat_tools_unsorted():
    return [
        {"type": "function",
         "function": {"name": "zebra_fn", "description": "z",
                      "parameters": {"type": "object",
                                     "properties": {"zeta": {"type": "string"},
                                                    "alpha": {"type": "number"}}}}},
        {"type": "function",
         "function": {"name": "apple_fn", "description": "a",
                      "parameters": {"type": "object",
                                     "properties": {"mid": {"type": "boolean"},
                                                    "aaa": {"type": "string"}}}}},
    ]


def chat_body(tools, messages=None, model="gpt-5-mini"):
    b = {"model": model, "stream": False,
         "messages": messages or [{"role": "system", "content": "sideprobe system v1"},
                                   {"role": "user", "content": "hi"}]}
    if tools is not None:
        b["tools"] = tools
    return b


def resp_tools_unsorted():
    return [
        {"type": "function", "name": "zebra_fn", "description": "z",
         "parameters": {"type": "object",
                        "properties": {"zeta": {"type": "string"},
                                       "alpha": {"type": "number"}}}},
        {"type": "function", "name": "apple_fn", "description": "a",
         "parameters": {"type": "object",
                        "properties": {"mid": {"type": "boolean"},
                                       "aaa": {"type": "string"}}}},
    ]


def resp_body(tools, inputs="hi", model="gpt-5-mini", instructions="sideprobe system v1",
              extra=None):
    # input MUST be array-form: a string `input` takes the `no_messages`
    # passthrough before Phase E normalization runs (require_array_field).
    b = {"model": model, "instructions": instructions,
         "input": [{"role": "user", "content": inputs}], "stream": False}
    if tools is not None:
        b["tools"] = tools
    if extra:
        b.update(extra)
    return b


REQUESTS = [
    # --- Anthropic, subscription-shape ---
    ("A1-sub-unsorted-nomarkers", "/v1/messages", SUB_ANTH,
     anth_body(anth_tools_unsorted())),
    # NOTE: fully sorted ("properties" < "type", nested keys sorted) so
    # E1 and E2 both no-op silently here.
    ("A2-sub-sorted-nomarkers", "/v1/messages", SUB_ANTH,
     anth_body([{"name": "Apple_tool", "description": "a",
                 "input_schema": {"properties": {"aaa": {"type": "string"},
                                                "mid": {"type": "boolean"}},
                                 "type": "object"}},
                {"name": "middle_tool", "description": "m",
                 "input_schema": {"properties": {"q": {"type": "string"}},
                                 "type": "object"}},
                {"name": "zebra_tool", "description": "z",
                 "input_schema": {"properties": {"alpha": {"type": "number"},
                                                "zeta": {"type": "string"}},
                                 "required": ["zeta"],
                                 "type": "object"}}])),
    ("A3-sub-unsorted-markerpresent", "/v1/messages", SUB_ANTH,
     anth_body([{**anth_tools_unsorted()[0],
                 "cache_control": {"type": "ephemeral"}},
                anth_tools_unsorted()[1], anth_tools_unsorted()[2]])),
    ("A4-sub-notools", "/v1/messages", SUB_ANTH, anth_body(None)),
    # --- Anthropic, PAYG-shape sanity ---
    ("A5-payg-unsorted-nomarkers", "/v1/messages", PAYG_ANTH,
     anth_body(anth_tools_unsorted())),
    # --- OpenAI chat, PAYG-shape (E1/E2/E4 all eligible, both arms) ---
    ("C1-payg-chat-unsorted", "/v1/chat/completions", PAYG_OAI,
     chat_body(oai_chat_tools_unsorted())),
    # E4 key stability: same system+tools, different user message -> same key expected
    ("C2-payg-chat-stability-turn2", "/v1/chat/completions", PAYG_OAI,
     chat_body(oai_chat_tools_unsorted(),
               messages=[{"role": "system", "content": "sideprobe system v1"},
                         {"role": "user", "content": "second turn, different text"}])),
    # E4 key sensitivity: different tools -> different key expected
    ("C3-payg-chat-othertools", "/v1/chat/completions", PAYG_OAI,
     chat_body([oai_chat_tools_unsorted()[1]])),
    # E4 key_present skip
    ("C4-payg-chat-keypresent", "/v1/chat/completions", PAYG_OAI,
     {**chat_body(oai_chat_tools_unsorted()), "prompt_cache_key": "customer-set-key"}),
    # --- OpenAI chat, subscription-shape (raw-auth gates: skip on both arms) ---
    ("C5-sub-chat-unsorted", "/v1/chat/completions", SUB_CHAT,
     chat_body(oai_chat_tools_unsorted())),
    # --- OpenAI responses, PAYG-shape ---
    ("R1-payg-resp-unsorted", "/v1/responses", PAYG_OAI,
     resp_body(resp_tools_unsorted())),
    ("R2-payg-resp-stability-turn2", "/v1/responses", PAYG_OAI,
     resp_body(resp_tools_unsorted(), inputs="second turn, different text")),
    ("R3-payg-resp-othertools", "/v1/responses", PAYG_OAI,
     resp_body([resp_tools_unsorted()[1]])),
    ("R4-payg-resp-keypresent", "/v1/responses", PAYG_OAI,
     resp_body(resp_tools_unsorted(), extra={"prompt_cache_key": "customer-set-key"})),
    # --- OpenAI responses, subscription-shape ---
    ("R5-sub-resp-unsorted", "/v1/responses", SUB_RESP,
     resp_body(resp_tools_unsorted())),
]


def log_len():
    try:
        with open(log_path, "rb") as f:
            return f.tell() if False else len(f.read().split(b"\n"))
    except FileNotFoundError:
        return 0


def new_events(before):
    out = []
    with open(log_path, "rb") as f:
        lines = f.read().split(b"\n")
    for raw in lines[before:]:
        raw = raw.strip()
        if not raw:
            continue
        try:
            d = json.loads(raw)
        except Exception:
            continue
        ev = (d.get("fields") or {}).get("event", "")
        if ev.startswith("e1_") or ev.startswith("e2_") or ev.startswith("e3_") \
                or ev.startswith("e4_"):
            f = d["fields"]
            out.append({k: f.get(k) for k in
                        ("event", "path", "reason", "auth_mode", "tool_count",
                         "placed_count", "locations", "key_prefix") if k in f})
    return out


results = []
for label, path, headers, body in REQUESTS:
    before = log_len()
    data = json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data,
                                 headers={**headers, "Content-Type": "application/json"},
                                 method="POST")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            status = r.status
            rbody = r.read()[:2000].decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        status = e.code
        rbody = e.read()[:2000].decode("utf-8", "replace")
    evs = new_events(before)
    results.append({"label": label, "path": path, "status": status,
                    "resp_body": rbody, "events": evs})
    compact = "; ".join(
        e["event"] + "".join(f" {k}={v}" for k, v in e.items()
                             if k not in ("event", "path"))
        for e in evs) or "(no e* events)"
    print(f"[{arm}] {label}: http={status} :: {compact}", flush=True)

with open(f"/tmp/egate-sideproxy/report-{arm}.json", "w") as f:
    json.dump(results, f, indent=1)
print(f"wrote /tmp/egate-sideproxy/report-{arm}.json", flush=True)

#!/usr/bin/env python3
"""Rapid-probe repro: anthropic 2-user-message shape from captures-prev/034."""
import json, sys, urllib.request

BODY = {
    "model": "claude-sonnet-4-5",
    "max_tokens": 8,
    "system": [{"type": "text", "text": "rapid"}],
    "messages": [
        {"role": "user", "content": "first"},
        {"role": "user", "content": "second"},
    ],
    "tools": [
        {"name": "n", "description": "d",
         "input_schema": {"type": "object", "properties": {}}}
    ],
}

def send(base, tag):
    req = urllib.request.Request(
        base + "/v1/messages", data=json.dumps(BODY).encode(),
        headers={"Content-Type": "application/json",
                 "x-api-key": "sk-ant-sid",
                 "anthropic-version": "2023-06-01",
                 "User-Agent": "rapid-probe/1"},
        method="POST")
    try:
        r = urllib.request.urlopen(req, timeout=20)
        print(tag, r.status)
    except Exception as e:
        print(tag, "ERR", e)

base = sys.argv[1]
n = int(sys.argv[2]) if len(sys.argv) > 2 else 3
for i in range(n):
    send(base, f"probe{i+1}")

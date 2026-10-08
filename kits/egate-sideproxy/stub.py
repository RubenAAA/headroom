#!/usr/bin/env python3
"""Canned upstream stub for the E1/E2/E3/E4 side-proxy test.

Listens on 127.0.0.1:18880. Captures every request body+headers to
/tmp/egate-sideproxy/captures/ and returns minimal canned JSON shaped
like the real upstream for:
  POST /v1/messages          (Anthropic)
  POST /v1/chat/completions  (OpenAI Chat)
  POST /v1/responses         (OpenAI Responses)
Anything else -> 404 JSON. Always 200 on the three known paths, so no
scope/revocation-shaped signal can arise here by construction.

Usage: python3 stub.py  (runs in foreground; Ctrl-C or kill to stop)
"""
import hashlib
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

CAPTURE_DIR = "/tmp/egate-sideproxy/captures"
os.makedirs(CAPTURE_DIR, exist_ok=True)

counter = [0]


def canned(path, body):
    if path == "/v1/messages":
        model = (body.get("model") if isinstance(body, dict) else None) or "stub-model"
        return {
            "id": "msg_stub_001", "type": "message", "role": "assistant",
            "model": model, "stop_reason": "end_turn", "stop_sequence": None,
            "content": [{"type": "text", "text": "stub-ok"}],
            "usage": {"input_tokens": 10, "output_tokens": 5,
                      "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0},
        }
    if path == "/v1/chat/completions":
        model = (body.get("model") if isinstance(body, dict) else None) or "stub-model"
        return {
            "id": "chatcmpl-stub-001", "object": "chat.completion",
            "created": 1790000000, "model": model,
            "choices": [{"index": 0,
                         "message": {"role": "assistant", "content": "stub-ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5,
                      "total_tokens": 15},
        }
    if path == "/v1/responses":
        model = (body.get("model") if isinstance(body, dict) else None) or "stub-model"
        return {
            "id": "resp_stub_001", "object": "response", "created_at": 1790000000,
            "status": "completed", "model": model,
            "output": [{"type": "message", "id": "msg_stub_1", "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "stub-ok",
                                     "annotations": []}]}],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
        }
    return None


class Handler(BaseHTTPRequestHandler):
    # HTTP/1.0: close each connection after the response. (HTTP/1.1
    # keep-alive wedged the single-threaded server when the proxy held
    # a probe connection open; see report.)
    protocol_version = "HTTP/1.0"

    def _handle(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            body = json.loads(raw.decode("utf-8")) if raw else None
        except Exception:
            body = {"_stub_note": "non-JSON body", "_raw_len": len(raw)}
        counter[0] += 1
        n = counter[0]
        slug = self.path.strip("/").replace("/", "_") or "root"
        digest = hashlib.md5(raw).hexdigest()[:8]
        with open(os.path.join(CAPTURE_DIR, f"{n:03d}-{slug}-{digest}.json"), "w") as f:
            json.dump({"seq": n, "method": self.command, "path": self.path,
                       "headers": {k.lower(): v for k, v in self.headers.items()},
                       "body": body}, f, indent=1)
        resp = canned(self.path, body if isinstance(body, dict) else {})
        self._log_post(body)
        if resp is None:
            payload = json.dumps({"error": {"message": "stub: unknown path",
                                            "type": "not_found"}}).encode()
            self.send_response(404)
        else:
            payload = json.dumps(resp).encode()
            self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    do_POST = _handle
    do_GET = _handle

    def log_message(self, *a):
        sys.stderr.write("stub: %s %s\n" % (self.command, self.path))
        sys.stderr.flush()

    def _log_post(self, body):
        try:
            b = body if isinstance(body, dict) else {}
            msgs = b.get("messages")
            if msgs is None:
                msgs = b.get("input")
            nmsg = len(msgs) if isinstance(msgs, list) else "?"
            tools = b.get("tools")
            nto = len(tools) if isinstance(tools, list) else None
            sys.stderr.write(
                "stub: POST %s model=%s msgs=%s tools=%s pck=%s\n"
                % (self.path, b.get("model"), nmsg, nto,
                   repr(b.get("prompt_cache_key"))[:34]))
            sys.stderr.flush()
        except Exception as e:
            sys.stderr.write("stub: POST %s (logfail %r)\n" % (self.path, e))
            sys.stderr.flush()


if __name__ == "__main__":
    from http.server import ThreadingHTTPServer
    ThreadingHTTPServer.daemon_threads = True
    ThreadingHTTPServer.request_queue_size = 64
    srv = ThreadingHTTPServer(("127.0.0.1", 18880), Handler)
    print("stub listening on 127.0.0.1:18880, captures in", CAPTURE_DIR, flush=True)
    srv.serve_forever()

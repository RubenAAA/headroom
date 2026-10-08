#!/usr/bin/env python3
"""Truncating upstream stub: forces response-side IncompleteMessage.

Reads and captures the FULL request body (same capture schema as
stub_mt.py, plus resp_* fields), then either:
  - truncated: declares a full Content-Length (or chunked framing) but
    closes the connection after a partial body -> hyper IncompleteMessage
    on the proxy side, or
  - ok: sends the complete canned body.

Truncation decision is keyed by x-request-id (first sight assigns the
mode; a proxy retry of the same RID gets the same treatment), so proxy
retries are observable as same-RID double-landings with identical mode.

Modes (MSG0_TRUNC_MODE): all | alternate (even distinct-RID index trunc,
odd ok). Framing for truncated responses (MSG0_TRUNC_FRAMING):
clen | chunked | alternate.

Env: MSG0_CAPTURE_DIR, MSG0_STUB_PORT, MSG0_TRUNC_MODE, MSG0_TRUNC_FRAMING.
"""
import hashlib
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CAPTURE_DIR = os.environ.get("MSG0_CAPTURE_DIR", "/tmp/msg0/captures-trunc")
os.makedirs(CAPTURE_DIR, exist_ok=True)
MODE = os.environ.get("MSG0_TRUNC_MODE", "alternate")
FRAMING = os.environ.get("MSG0_TRUNC_FRAMING", "alternate")
HTTP11 = os.environ.get("MSG0_HTTP11", "0") == "1"  # speak HTTP/1.1 (valid chunked)

counter = [0]
lock = threading.Lock()
rid_mode = {}   # rid -> (truncated: bool, framing: str)
distinct = [0]


def canned_json(path, body):
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


def canned_sse(path, body):
    """Full SSE payload for stream=true requests (Content-Length framed)."""
    if path == "/v1/messages":
        evs = [
            'event: message_start\ndata: {"type":"message_start","message":{"id":"msg_stub_001","type":"message","role":"assistant","model":"stub-model","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}\n\n',
            'event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n',
            'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"stub-"}}\n\n',
            'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}\n\n',
            'event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n',
            'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}\n\n',
            'event: message_stop\ndata: {"type":"message_stop"}\n\n',
        ]
    elif path == "/v1/chat/completions":
        evs = [
            'data: {"id":"chatcmpl-stub-001","object":"chat.completion.chunk","created":1790000000,"model":"stub-model","choices":[{"index":0,"delta":{"role":"assistant","content":"stub-"},"finish_reason":null}]}\n\n',
            'data: {"id":"chatcmpl-stub-001","object":"chat.completion.chunk","created":1790000000,"model":"stub-model","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}\n\n',
            'data: {"id":"chatcmpl-stub-001","object":"chat.completion.chunk","created":1790000000,"model":"stub-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n',
            'data: [DONE]\n\n',
        ]
    elif path == "/v1/responses":
        evs = [
            'data: {"type":"response.created","response":{"id":"resp_stub_001","status":"in_progress","model":"stub-model"}}\n\n',
            'data: {"type":"response.output_text.delta","item_id":"msg_stub_1","delta":"stub-ok"}\n\n',
            'data: {"type":"response.completed","response":{"id":"resp_stub_001","status":"completed","model":"stub-model"}}\n\n',
        ]
    else:
        return None
    return "".join(evs).encode()


def decide(rid):
    with lock:
        if rid in rid_mode:
            return rid_mode[rid]
        idx = distinct[0]
        distinct[0] += 1
        if MODE == "all":
            trunc = True
        else:  # alternate
            trunc = (idx % 2 == 0)
        if FRAMING == "alternate":
            framing = "clen" if (idx % 2 == 0) else "chunked"
        else:
            framing = FRAMING
        rid_mode[rid] = (trunc, framing)
        return trunc, framing


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1" if HTTP11 else "HTTP/1.0"

    def _close_hdr(self):
        if HTTP11:
            self.send_header("Connection", "close")

    def _handle(self):
        length = int(self.headers.get("Content-Length") or 0)
        try:
            raw = self.rfile.read(length) if length else b""
        except Exception as e:
            raw = b""
            sys.stderr.write("stub-trunc: body read failed %r\n" % e)
        try:
            body = json.loads(raw.decode("utf-8")) if raw else None
        except Exception:
            body = {"_stub_note": "non-JSON body", "_raw_len": len(raw)}
        rid = self.headers.get("X-Request-Id") or self.headers.get("x-request-id")
        trunc, framing = decide(rid or f"no-rid-{id(self)}")
        is_stream = isinstance(body, dict) and body.get("stream") is True

        with lock:
            counter[0] += 1
            n = counter[0]
        slug = self.path.strip("/").replace("/", "_") or "root"
        digest = hashlib.md5(raw).hexdigest()[:8]

        if is_stream:
            full = canned_sse(self.path, body if isinstance(body, dict) else {})
            ctype = "text/event-stream"
        else:
            resp = canned_json(self.path, body if isinstance(body, dict) else {})
            full = json.dumps(resp if resp is not None else {"error": "stub: unknown path"}).encode()
            ctype = "application/json"
        if full is None:
            full = json.dumps({"error": "stub: unknown path"}).encode()
            ctype = "application/json"
            status = 404
        else:
            status = 200

        resp_mode = "ok"
        sent_len = len(full)
        if trunc and status == 200:
            resp_mode = "truncated-" + framing
            sent_len = max(32, len(full) // 3)

        with open(os.path.join(CAPTURE_DIR, f"{n:03d}-{slug}-{digest}.json"), "w") as f:
            json.dump({"seq": n, "method": self.command, "path": self.path,
                       "headers": {k.lower(): v for k, v in self.headers.items()},
                       "content_length_hdr": length, "raw_len": len(raw),
                       "raw_sha256": hashlib.sha256(raw).hexdigest(),
                       "resp_mode": resp_mode, "resp_framing": framing,
                       "resp_declared_len": len(full), "resp_sent_len": sent_len,
                       "body": body}, f, indent=1)
        self._log_post(body, rid, resp_mode, is_stream)

        try:
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self._close_hdr()
            if resp_mode == "truncated-chunked":
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                # One chunk, then close WITHOUT the terminating 0-chunk.
                half = full[:sent_len]
                self.wfile.write(("%x\r\n" % len(half)).encode())
                self.wfile.write(half)
                self.wfile.write(b"\r\n")
                self.wfile.flush()
            elif resp_mode.startswith("truncated-"):
                self.send_header("Content-Length", str(len(full)))
                self.end_headers()
                self.wfile.write(full[:sent_len])
                self.wfile.flush()
            else:
                self.send_header("Content-Length", str(len(full)))
                self.end_headers()
                self.wfile.write(full)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError) as e:
            sys.stderr.write("stub-trunc: seq=%d write %r (client gone)\n" % (n, e))
        finally:
            self.close_connection = True

    do_POST = _handle
    do_GET = _handle

    def log_message(self, *a):
        pass

    def _log_post(self, body, rid, resp_mode, is_stream):
        try:
            b = body if isinstance(body, dict) else {}
            msgs = b.get("messages")
            if msgs is None:
                msgs = b.get("input")
            nmsg = len(msgs) if isinstance(msgs, list) else "?"
            sys.stderr.write(
                "stub-trunc: POST %s stream=%s msgs=%s rid=%s mode=%s\n"
                % (self.path, is_stream, nmsg, str(rid)[:8], resp_mode))
            sys.stderr.flush()
        except Exception as e:
            sys.stderr.write("stub-trunc: POST %s (logfail %r)\n" % (self.path, e))
            sys.stderr.flush()


if __name__ == "__main__":
    ThreadingHTTPServer.daemon_threads = True
    ThreadingHTTPServer.request_queue_size = 64
    port = int(os.environ.get("MSG0_STUB_PORT", "18883"))
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print("stub-trunc listening port=%d captures=%s mode=%s framing=%s"
          % (port, CAPTURE_DIR, MODE, FRAMING), flush=True)
    srv.serve_forever()

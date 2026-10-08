#!/usr/bin/env python3
"""Canned upstream stub for compression-arm A/B.

Listens on 127.0.0.1:18881, captures every POST body to
/tmp/compress-arms/capture/<arm>/<seq>.bin + headers, and returns a canned
Anthropic non-streaming response. No paid APIs, no network egress.

Usage: stub.py <arm-name>
"""
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

ARM = sys.argv[1] if len(sys.argv) > 1 else "unknown"
CAPDIR = Path("/tmp/compress-arms/capture") / ARM
CAPDIR.mkdir(parents=True, exist_ok=True)
SEQ = 0

CANNED = (
    b'{"id":"msg_stub00000000000000000001","type":"message","role":"assistant",'
    b'"model":"claude-opus-4-6","content":[{"type":"text","text":"stub-ok"}],'
    b'"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":4,'
    b'"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
)


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _handle(self):
        global SEQ
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        # Chunked fallback (proxy should send Content-Length, but be safe).
        if not length and (self.headers.get("Transfer-Encoding") or "").lower() == "chunked":
            chunks = []
            while True:
                line = self.rfile.readline().strip()
                n = int(line.split(b";")[0] or b"0", 16)
                if n == 0:
                    self.rfile.readline()
                    break
                chunks.append(self.rfile.read(n))
                self.rfile.readline()
            body = b"".join(chunks)
        SEQ += 1
        (CAPDIR / f"{SEQ:03d}.bin").write_bytes(body)
        (CAPDIR / f"{SEQ:03d}.hdr").write_text(
            f"{self.command} {self.path}\n{self.headers}", encoding="utf-8"
        )
        if self.path.startswith("/v1/messages"):
            payload, code, ctype = CANNED, 200, "application/json"
        else:
            payload, code, ctype = b'{"error":"stub: unknown path"}', 404, "application/json"
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    do_POST = _handle
    do_GET = _handle

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", 18881), H).serve_forever()

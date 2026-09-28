#!/usr/bin/env python3
"""E2E: `--max-items-after-crush` reaches the live SmartCrusher.

Starts a mock Anthropic upstream that records what it receives, then runs the
real `headroom-proxy` binary once per setting and sends the same request: a
tool_result holding a 60-row JSON array. Counts the rows that come out the
other side and writes them to the artifact file.

    scripts/e2e-crush-max-items.py [path/to/headroom-proxy] [artifact.json]

Exit status is non-zero if the flag has no effect.
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

BINARY = sys.argv[1] if len(sys.argv) > 1 else "target/release/headroom-proxy"
ARTIFACT = sys.argv[2] if len(sys.argv) > 2 else "e2e-crush-max-items.json"
ROWS = 60
NOTE = ("heartbeat from the ingest worker: queue drained, no retries pending, "
        "upstream latency within budget, checkpoint written to the shared volume")

received = []


class Upstream(BaseHTTPRequestHandler):
    def do_POST(self):
        received.append(json.loads(self.rfile.read(int(self.headers["content-length"]))))
        body = json.dumps({
            "id": "msg_e2e", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
            "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def request_body():
    # Low uniqueness so the analyzer will drop rows, and a long repeated value
    # per row so lossless tabling stays under its 30% savings bar and the
    # lossy path (the one the flag caps) runs.
    rows = [{"seq": i, "level": "info", "notes": NOTE} for i in range(ROWS)]
    return {
        "model": "claude-sonnet-5",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "list the rows"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_e2e", "name": "fetch_rows", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_e2e", "content": json.dumps(rows)}]},
        ],
    }


def forwarded_rows(body):
    """Rows left in the forwarded tool_result: each kept row keeps its note."""
    block = body["messages"][-1]["content"][0]
    text = block["content"] if isinstance(block["content"], str) else block["content"][0]["text"]
    return text.count("queue drained")


def run(upstream_port, extra_args):
    port = free_port()
    home = tempfile.mkdtemp(prefix="hr-e2e-home-")
    env = {**os.environ, "HOME": home, "XDG_STATE_HOME": f"{home}/state", "XDG_CACHE_HOME": f"{home}/cache"}
    env = {k: v for k, v in env.items() if not k.startswith("HEADROOM")}
    proc = subprocess.Popen(
        [BINARY, "--listen", f"127.0.0.1:{port}", "--upstream", f"http://127.0.0.1:{upstream_port}",
         "--compression", "--compression-mode", "live_zone", *extra_args],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(100):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1)
                break
            except OSError:
                time.sleep(0.1)
        else:
            raise SystemExit(f"proxy did not come up with {extra_args}")
        before = len(received)
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/v1/messages", data=json.dumps(request_body()).encode(),
            headers={"content-type": "application/json", "x-api-key": "sk-e2e", "anthropic-version": "2023-06-01"},
        )
        urllib.request.urlopen(req, timeout=30).read()
        assert len(received) == before + 1, "upstream saw no request"
        return forwarded_rows(received[-1])
    finally:
        proc.terminate()
        proc.wait(timeout=10)


def main():
    upstream_port = free_port()
    server = HTTPServer(("127.0.0.1", upstream_port), Upstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    result = {
        "rows_sent": ROWS,
        "rows_forwarded": {
            "default": run(upstream_port, []),
            "max_items_3": run(upstream_port, ["--max-items-after-crush", "3"]),
            "max_items_30": run(upstream_port, ["--max-items-after-crush", "30"]),
        },
    }
    rows = result["rows_forwarded"]
    result["pass"] = rows["max_items_3"] < rows["default"] < rows["max_items_30"] <= ROWS
    with open(ARTIFACT, "w") as f:
        json.dump(result, f, indent=2)
    print(json.dumps(result))
    sys.exit(0 if result["pass"] else 1)


if __name__ == "__main__":
    main()

"""Shared model routing for the review and ticket offload workers.

The `headroom` transport runs Claude Code against Headroom, which routes the
selected model alias to its configured provider. The `local-direct` transport
calls a loopback Anthropic Messages or OpenAI Chat Completions endpoint and
bypasses Headroom.
"""

import ipaddress
import json
import os
import socket
import subprocess
import urllib.error
import urllib.request
from urllib.parse import urlsplit


def _setting(name, legacy_name=None, default=""):
    if name in os.environ:
        return os.environ[name]
    if legacy_name and legacy_name in os.environ:
        return os.environ[legacy_name]
    return default


if "OFFLOAD_MODEL_TRANSPORT" in os.environ:
    TRANSPORT = os.environ["OFFLOAD_MODEL_TRANSPORT"].strip().lower()
elif "OFFLOAD_MODEL_BACKEND" in os.environ:  # legacy setting name
    TRANSPORT = os.environ["OFFLOAD_MODEL_BACKEND"].strip().lower()
else:
    TRANSPORT = os.environ.get("SPARK_DRAFT_BACKEND", "proxy").strip().lower()
TRANSPORT = {"proxy": "headroom", "spark": "headroom", "local": "local-direct"}.get(
    TRANSPORT, TRANSPORT)
MODEL = _setting("OFFLOAD_MODEL", "SPARK_DRAFT_MODEL", "claude-muse-spark-1.3").strip()
MODEL_CONFIGURED = bool(
    os.environ.get("OFFLOAD_MODEL", "").strip()
    or os.environ.get("SPARK_DRAFT_MODEL", "").strip()
)
HEADROOM_URL = _setting(
    "OFFLOAD_MODEL_HEADROOM_URL", "OFFLOAD_MODEL_PROXY_URL",
    os.environ.get("SPARK_DRAFT_BASE_URL", "http://127.0.0.1:8787")).strip()
LOCAL_BASE_URL = _setting("OFFLOAD_MODEL_LOCAL_BASE_URL", None, "").strip()
LOCAL_API = _setting("OFFLOAD_MODEL_LOCAL_API", None, "anthropic").strip().lower()
if not LOCAL_BASE_URL and TRANSPORT == "local-direct":
    # SPARK_DRAFT_BASE_URL was also the local endpoint in the first local
    # transport implementation; retain it as a migration alias.
    LOCAL_BASE_URL = os.environ.get("SPARK_DRAFT_BASE_URL", "").strip()
LOCAL_AUTH_TOKEN = _setting(
    "OFFLOAD_MODEL_LOCAL_AUTH_TOKEN", "SPARK_DRAFT_LOCAL_AUTH_TOKEN", "ollama")
try:
    LOCAL_MAX_TOKENS = int(_setting(
        "OFFLOAD_MODEL_LOCAL_MAX_TOKENS", "SPARK_DRAFT_LOCAL_MAX_TOKENS", "8192"))
except ValueError:
    LOCAL_MAX_TOKENS = 0


def configuration_error():
    """Validate model routing before private transcript/review data is read."""
    if TRANSPORT not in ("headroom", "local-direct"):
        return (
            f"unknown OFFLOAD_MODEL_TRANSPORT={TRANSPORT!r}; "
            "choose 'headroom' or 'local-direct'"
        )
    if TRANSPORT == "headroom":
        allowed = _setting(
            "OFFLOAD_ALLOW_EXTERNAL_DATA", "SPARK_REVIEW_ALLOW_EXTERNAL_DATA", "")
        if allowed != "1":
            return (
                "Headroom model transport is disabled until explicitly allowed: "
                "set OFFLOAD_ALLOW_EXTERNAL_DATA=1 in "
                "~/.config/offload-workers/env only if this repository's data "
                "may be sent to the upstream configured for this model alias; "
                "for private data, configure OFFLOAD_MODEL_TRANSPORT=local-direct "
                "with a loopback model server instead"
            )
        return None

    if LOCAL_API not in ("anthropic", "openai"):
        return (
            f"unknown OFFLOAD_MODEL_LOCAL_API={LOCAL_API!r}; "
            "choose 'anthropic' or 'openai'"
        )
    if not MODEL_CONFIGURED:
        return "local-direct transport needs OFFLOAD_MODEL set to the model id served locally"
    if not LOCAL_BASE_URL:
        return (
            "local-direct transport needs OFFLOAD_MODEL_LOCAL_BASE_URL set to its "
            "loopback API base URL"
        )
    try:
        parsed = urlsplit(LOCAL_BASE_URL)
        host = parsed.hostname or ""
        try:
            is_loopback = ipaddress.ip_address(host).is_loopback
        except ValueError:
            is_loopback = host.lower() == "localhost"
        if (parsed.scheme not in ("http", "https") or not is_loopback
                or parsed.username or parsed.password or parsed.query or parsed.fragment):
            return (
                "local-direct transport only accepts an http(s) loopback URL without "
                "embedded credentials, query, or fragment"
            )
        if parsed.port == 8787:
            return (
                "local-direct transport cannot use Headroom's default port 8787; "
                "point it directly at the local model server"
            )
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
        try:
            with socket.create_connection((host, port), timeout=0.5):
                pass
        except OSError:
            return (
                "local model server is not reachable at "
                f"{host}:{port}; start it before asking the worker to run"
            )
    except ValueError:
        return "invalid OFFLOAD_MODEL_LOCAL_BASE_URL for local-direct transport"
    if LOCAL_MAX_TOKENS < 1:
        return "OFFLOAD_MODEL_LOCAL_MAX_TOKENS must be a positive integer"
    return None


def _local_endpoint():
    base = LOCAL_BASE_URL.rstrip("/")
    path = urlsplit(base).path.rstrip("/")
    endpoint = (
        "/v1/messages" if LOCAL_API == "anthropic" else "/v1/chat/completions"
    )
    if path.endswith(endpoint):
        return base
    if path.endswith("/v1"):
        return base + endpoint.removeprefix("/v1")
    return base + endpoint


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


def _call_local(prompt, timeout):
    payload = {
        "model": MODEL,
        "max_tokens": LOCAL_MAX_TOKENS,
        "messages": [{"role": "user", "content": prompt}],
    }
    headers = {"Content-Type": "application/json"}
    if LOCAL_API == "anthropic":
        headers.update({
            "anthropic-version": "2023-06-01",
            "x-api-key": LOCAL_AUTH_TOKEN,
        })
    else:
        payload["stream"] = False
        if LOCAL_AUTH_TOKEN:
            headers["Authorization"] = "Bearer " + LOCAL_AUTH_TOKEN
    request = urllib.request.Request(
        _local_endpoint(), data=json.dumps(payload).encode(), headers=headers,
        method="POST",
    )
    try:
        opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}), _NoRedirect())
        with opener.open(request, timeout=timeout) as response:
            result = json.loads(response.read())
    except urllib.error.HTTPError as error:
        detail = error.read().decode(errors="replace").strip()
        return None, f"local model HTTP {error.code}" + (
            f": {detail[-500:]}" if detail else "")
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError) as error:
        return None, f"local model request failed: {error}"
    if not isinstance(result, dict):
        return None, "local model returned an invalid response"
    if LOCAL_API == "openai":
        choices = result.get("choices", [])
        message = choices[0].get("message", {}) if choices else {}
        content = message.get("content", "")
        if isinstance(content, str):
            texts = [content]
        elif isinstance(content, list):
            texts = [block.get("text", "") for block in content
                     if isinstance(block, dict) and block.get("type") == "text"]
        else:
            texts = []
    else:
        texts = [block.get("text", "") for block in result.get("content", [])
                 if isinstance(block, dict) and block.get("type") == "text"]
    output = "\n".join(text for text in texts if text).strip()
    if not output:
        return None, "local model returned no text block"
    return output, None


def call(prompt, timeout, worker_marker):
    """Run the configured transport. Returns (stdout, diagnostic)."""
    if TRANSPORT == "local-direct":
        return _call_local(prompt, timeout)
    blocked = ("GITLAB_", "OPENCODE_")
    if worker_marker != "OFFLOADED_TICKET_WORKER":
        blocked += ("YOUTRACK_",)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(blocked)}
    env.update({worker_marker: "1", "ANTHROPIC_BASE_URL": HEADROOM_URL})
    try:
        result = subprocess.run(
            ["claude", "-p", "--model", MODEL, prompt],
            capture_output=True, text=True, timeout=timeout, env=env,
        )
    except subprocess.TimeoutExpired as error:
        stderr = error.stderr or ""
        if isinstance(stderr, bytes):
            stderr = stderr.decode(errors="replace")
        tail = "\n".join(stderr.strip().splitlines()[-8:])[-2000:]
        diagnostic = f"worker call timed out after {timeout}s"
        if tail:
            diagnostic += "\nchild stderr tail:\n" + tail
        return None, diagnostic
    except OSError as error:
        return None, f"could not start claude worker: {error}"
    if result.returncode != 0 and not result.stdout.strip():
        tail = (result.stderr or "").strip().splitlines()[-3:]
        return None, f"worker rc={result.returncode} empty stdout" + (
            f" stderr: {' | '.join(tail)}" if tail else "")
    return result.stdout, None

"""SSRF guard for client-supplied upstream base URLs (WEB-01).

Clients may redirect the proxy's upstream via the ``x-headroom-base-url`` header
(BYOK / custom OpenAI-compatible endpoints). Without validation this lets a
caller turn the proxy into a confused deputy — reaching cloud-metadata
(``169.254.169.254``) or internal RFC1918 hosts the caller cannot reach directly.

Policy:
  * Default: reject destinations that resolve to private, loopback, link-local,
    or otherwise non-public addresses. Public hosts (api.openai.com, api.x.ai,
    Azure, ...) are allowed so ordinary BYOK keeps working.
  * When ``HEADROOM_ALLOWED_BASE_URLS`` is set (comma-separated hosts or URLs),
    bare hosts permit every safe scheme/port for that host, while URLs permit
    only their exact normalized origin. Because that is an explicit operator
    choice, allowlisted destinations may point at internal/on-prem endpoints.

This module intentionally depends only on the standard library so it is safe to
import from any handler without risking an import cycle.
"""

from __future__ import annotations

import ipaddress
import os
import socket
from urllib.parse import urlparse

ALLOWED_BASE_URLS_ENV = "HEADROOM_ALLOWED_BASE_URLS"

_SAFE_SCHEMES = {"http", "https", "ws", "wss"}


def _allowlisted_destinations() -> tuple[set[str], set[tuple[str, str, int]]] | None:
    raw = os.environ.get(ALLOWED_BASE_URLS_ENV)
    if not raw or not raw.strip():
        return None
    hosts: set[str] = set()
    origins: set[tuple[str, str, int]] = set()
    for item in raw.split(","):
        item = item.strip()
        if not item:
            continue
        if "://" not in item:
            parsed = urlparse(f"//{item}")
            if parsed.hostname:
                hosts.add(parsed.hostname.lower())
            continue
        parsed = urlparse(item)
        if parsed.scheme.lower() not in _SAFE_SCHEMES or not parsed.hostname:
            continue
        try:
            port = parsed.port
        except ValueError:
            continue
        if port is None:
            port = 443 if parsed.scheme.lower() in {"https", "wss"} else 80
        origins.add((parsed.scheme.lower(), parsed.hostname.lower(), port))
    return hosts, origins


def _is_internal_address(ip: str) -> bool:
    try:
        addr = ipaddress.ip_address(ip)
    except ValueError:
        return True  # unparseable (e.g. scoped link-local) -> treat as unsafe
    return (
        addr.is_private
        or addr.is_loopback
        or addr.is_link_local
        or addr.is_reserved
        or addr.is_multicast
        or addr.is_unspecified
    )


def is_safe_upstream_url(url: str) -> bool:
    """Return True if ``url`` is a safe client-chosen upstream destination.

    In allowlist mode only allowlisted hosts pass. Otherwise the host is
    resolved and rejected if any resolved address is internal/metadata, which
    also catches DNS names that point at private space.
    """
    parsed = urlparse((url or "").strip())
    if parsed.scheme.lower() not in _SAFE_SCHEMES:
        return False
    host = parsed.hostname
    if not host:
        return False

    allow = _allowlisted_destinations()
    if allow is not None:
        hosts, origins = allow
        if host.lower() in hosts:
            return True
        try:
            port = parsed.port
        except ValueError:
            return False
        if port is None:
            port = 443 if parsed.scheme.lower() in {"https", "wss"} else 80
        return (parsed.scheme.lower(), host.lower(), port) in origins

    try:
        infos = socket.getaddrinfo(host, None, proto=socket.IPPROTO_TCP)
    except OSError:
        # Resolution and connection are separate operations, so allowing a DNS
        # miss here would fail open if the name resolves on the later lookup.
        # Operators can explicitly allowlist split-horizon/internal endpoints.
        return False
    return all(not _is_internal_address(str(info[4][0])) for info in infos)

# Implemented: caller-supplied upstreams pinned to approved DNS

- **Status:** done 2026-08-28
- **Source:** `docs/notes/rust-parity-gaps.md` §9.3 (upstream `3e3c4094`)
- **Summary:** Rust never had the SSRF (single guarded call site), but the
  property rested on review discipline. Now structural:
  `ResolvedCallerUpstream` carries hostname + exact approved `SocketAddr` set
  (mixed/inward answers rejected), caller-only transports pin
  `resolve_to_addrs` (hostname SNI preserved, connector restricted) —
  closes the validate-then-resolve rebinding window. Retries, streamed
  retries, continuations, and hook re-drives keep the selected transport;
  128-entry cache preserves pooling. Plus the 5 s resolve timeout half.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

### 9.3 Caller-supplied upstreams — DONE (2026-08-28)

The guard now reaches the connection boundary rather than stopping at a URL
wrapper. `ResolvedCallerUpstream` carries the original hostname URL plus the
exact `SocketAddr` set returned by its bounded DNS lookup; every answer is
rejected if any one points inward unless the operator explicitly allowlisted
the destination. `forward_http` builds/selects a caller-only reqwest transport
with `resolve_to_addrs`, so Host/TLS SNI retain the hostname while the connector
can use only those approved addresses. That closes the validate-then-resolve
DNS-rebinding window.

Caller-selected transports disable ambient/provider proxies and automatic
redirect following, and every retry, streamed retry, CCR/memory continuation,
and turn-hook re-drive keeps the selected transport. A bounded 128-entry cache
keyed by hostname plus the complete approved address set preserves connection
pooling without letting a later DNS answer reuse the earlier transport.
Coverage includes loopback/metadata rejection, mixed-answer rejection,
transition-address cases, hostname-based pinned routing, redirect refusal, and
provider-proxy bypass.

<details><summary>Original scoping notes</summary>

`3e3c4094` closed an SSRF where some resolution paths validated a
caller-supplied upstream and others did not. Upstream's answer was to move the
guard into the resolution helpers themselves — `proxy_routes.py`,
`proxy_targets.py`, `registry.py` all switched to
`is_safe_upstream_url_async`, so a path cannot resolve without validating.

Rust does not have this bug. `header_upstream_override` (`proxy.rs:2360`) has
exactly one call site (`:2513`), and `is_safe_upstream_url` runs on it at
`:2519`. The WebSocket path never reads `x-headroom-base-url` at all:
`websocket.rs` builds its upstream from `state.config.upstream`. The only
other `UpstreamOverride` setter, `foundry/mod.rs:152`, comes from operator
config rather than caller input. The related Vertex SSRF (`7c0b8860`) misses
Rust for the same kind of reason — the proxy never builds a regional hostname
from `location`, it joins a path onto the base the operator configured
(`config.rs:1049`).

So there is nothing to fix. What there is, is a gap between how the two
codebases hold the property. Python enforces it: skipping the guard means not
resolving. Rust achieves it by having one call site that happens to be
correct, and nothing stops a second one appearing. The header is caller-
controlled, the guard is a free function, and the reviewer who adds the next
override path has to know to call it.

Two ways to close that, in rising cost:

- A test that asserts `header_upstream_override` has one caller and it is
  guarded. Cheap, and it fails loudly when someone adds the second.
- Make the guard structural: have the override return a type that can only be
  built by passing through `is_safe_upstream_url`, so an unvalidated upstream
  cannot be expressed. Larger, and it ends the class rather than the instance.

The timeout half of `3e3c4094` is already ported: `RESOLVE_TIMEOUT` (5s) wraps
`lookup_host` in `upstream_guard.rs` and logs
`upstream_guard_resolve_timeout`, matching Python's bounded `getaddrinfo`.

</details>

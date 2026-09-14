# Integration Guide

!!! note "Live implementation: Rust"
    The production proxy is the Rust binary (`crates/headroom-proxy`, launched with `cclaude`). Python paths on this page now live in the read-only `upstream-python/` mirror — re-resolve any `headroom/*.py` cite there. Behavior described here still holds; only the implementation moved.

Three ways to put Headroom in the request path. Pick one; they are alternatives, not layers.

| You have... | Use this | Details |
|-------------|----------|---------|
| Claude Code, Cursor, or any tool that takes a base URL | [Proxy](#1-proxy-start-here) — Rust binary | Point `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` at it |
| Your own Python app where you own the client object | [SDK mirror](#2-sdk-mirror-in-process-python-and-typescript) — `upstream-python/` | [SDK Guide](sdk.md), [TypeScript SDK](typescript-sdk.md) |
| An API gateway (e.g. LiteLLM proxy) that should call Headroom over HTTP | [Gateway sidecar](#3-gateway-sidecar-over-http-mirror-only) — mirror-only `POST /v1/compress` | See below; loopback-only by default |

---

## 1. Proxy (start here)

Run the Rust binary in front of your existing tool. No code changes: the tool keeps speaking its native API and the proxy compresses before forwarding upstream.

```bash
# Preferred: launcher starts the proxy if down, sets ANTHROPIC_BASE_URL, execs claude
cclaude

# By hand: the binary takes --upstream plus --listen
headroom-proxy --upstream https://api.anthropic.com --listen 127.0.0.1:8787
ANTHROPIC_BASE_URL=http://localhost:8787 claude
```

OpenAI-compatible clients use the `/v1` route on the same port:

```bash
OPENAI_BASE_URL=http://localhost:8787/v1 your-app
```

!!! warning "Always `cclaude`, never bare `claude`"
    Plain `claude` talks straight to the API and the proxy does nothing. `cclaude` starts the proxy if it is down, sets `ANTHROPIC_BASE_URL`, and execs `claude` with every argument passed through.

What the proxy accepts (all verified in `crates/headroom-proxy/src/proxy.rs`):

- `POST /v1/messages` — Anthropic format
- `POST /v1/chat/completions` — OpenAI format
- `POST /v1/responses` — OpenAI Responses API format

Health checks:

```bash
curl -s localhost:8787/healthz
curl -s localhost:8787/cache-health
```

See [Proxy](proxy.md) for operations and [Metrics](metrics.md) for `/metrics` and savings endpoints. Model-to-upstream routing rules (`ModelRoute`: per-model `upstream` with `translate` / `target_model`) exist for routed deployments; that is operator config on the same binary, covered at a high level in [Proxy](proxy.md).

---

## 2. SDK mirror (in-process Python and TypeScript)

!!! note "Read-only mirror"
    Everything in this section lives in `upstream-python/` and is not built here. It exists so upstream diffs stay readable when porting. Do not treat it as the live path.

If you own the client object and want compression inside your process, use the mirror SDK instead of running a proxy:

- Python: wrap your client — [SDK Guide](sdk.md)
- TypeScript: `headroom-ai` npm package — [TypeScript SDK](typescript-sdk.md)
- Agent frameworks: [Agno](agno.md), [LangChain](langchain.md)
- LiteLLM in-process: `HeadroomCallback` (`upstream-python/headroom/integrations/litellm_callback.py`, via `async_pre_call_hook`) or the ASGI middleware (`upstream-python/headroom/integrations/asgi.py`, intercepts `/v1/messages`, `/v1/chat/completions`, `/v1/responses`, `/chat/completions`)

This page does not duplicate those guides; follow the links above for API detail.

!!! note "Mirror-only CLI flags"
    `headroom proxy --backend ...`, `headroom wrap copilot`, and `--backend anyllm`-style options you may see in older docs are Python-mirror CLI surface (`upstream-python/headroom/cli/`). There is no `--backend` on the Rust-live path — the Rust proxy routes with `--upstream` plus route flags.

### Copilot subscription note (mirror CLI)

The mirror's `headroom wrap copilot --subscription` flow routes wrapped Copilot traffic through the proxy to GitHub's generic public host, with `GITHUB_COPILOT_API_URL` as the explicit pin for Enterprise / data-residency tenants on a dedicated Copilot host. Test flows and host details live in [testing-copilot-subscription](../docs/notes/testing-copilot-subscription.md).

---

## 3. Gateway sidecar over HTTP (mirror-only)

!!! warning "Not on the Rust proxy"
    `POST /v1/compress` exists only on the Python mirror (`upstream-python/headroom/proxy/server.py`). The Rust binary has no `/v1/compress` route — verified by the route table in `crates/headroom-proxy/src/proxy.rs`. Anything below that calls this endpoint assumes the mirror proxy is running.

The mirror proxy exposes a compression-only endpoint: it returns compressed `messages` without ever making a completion request to an LLM provider (no generation, no provider key). The mirror's own code comments name LiteLLM's `headroom` guardrail as the main consumer, and the TypeScript SDK calls it (`upstream-python/sdk/typescript/src/client.ts`).

**Loopback-only by default.** Non-loopback callers get `404`, not `403`. Gateways on another host must opt in:

```bash
HEADROOM_COMPRESS_ALLOW_REMOTE=1 headroom proxy
```

Two things to get right when calling it from a gateway:

1. Send the real model name (including gateway-prefixed forms such as `bedrock/anthropic.claude-3-5-sonnet`) — it selects the tokenizer and context limit.
2. For multi-turn loops, forward the previously returned messages (not pristine originals) and pin the cached prefix with `config.frozen_message_count`; otherwise re-compression diverges from what the provider cached. Full field contract is in [Proxy](proxy.md).

Leave `config.mode` unset unless you also run the retrieval path: `ccr` mode emits markers that dangle without the `headroom_retrieve` tool.

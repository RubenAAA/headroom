# Headroom

!!! note "Live implementation: Rust"
    The production proxy is the Rust binary (`crates/headroom-proxy`, launched with `cclaude`). Python paths anywhere in this wiki now live in the read-only `upstream-python/` mirror — re-resolve any `headroom/*.py` cite there. Behavior described here still holds; only the implementation moved.

**Headroom is a local reverse proxy between Claude Code and the model API.** It compresses requests, keeps the provider's prompt cache intact, and routes Codex model names to OpenAI in the same session.

## Install

Prerequisites: git, Rust via rustup, plus `jq` and `lsof`.

```bash
git clone https://github.com/RubenAAA/headroom.git ~/headroom
cd ~/headroom
./install.sh
```

This builds the release binaries and installs `headroom-proxy` and `headroom` to `~/.local/bin`, plus the `cclaude` launcher, `restart-headroom.sh`, the flags file (`~/.headroom-flags.sh`), and the status line. Pass `--link` if the machine is for working on Headroom itself.

## Run

```bash
cclaude --context
```

!!! warning "Always `cclaude`, never bare `claude`"
    Plain `claude` talks straight to the API and the proxy does nothing. `cclaude --context` starts the proxy if it is down, sets `ANTHROPIC_BASE_URL`, and execs `claude` with every argument passed through. Without `--context`, `cclaude` is plain passthrough.

Verify it is working:

```bash
curl -s localhost:8787/healthz
curl -s localhost:8787/cache-health
headroom doctor
headroom savings
```

The proxy binary itself takes `--upstream` (required) and `--listen`; every flag also has a `HEADROOM_PROXY_*` environment variable. Tune via `~/.headroom-flags.sh`, then `restart-headroom.sh`. Full option list: `headroom-proxy --help`.

## Where to next

- [Proxy](proxy.md) — what the proxy does and how to operate it
- [Configuration](configuration.md) — flags and environment
- [Troubleshooting](troubleshooting.md) — when something breaks
- [Filesystem contract](filesystem-contract.md) — config and workspace paths
- [Metrics](metrics.md) — health, cache-hit, and savings endpoints
- [Architecture](ARCHITECTURE.md) — how the pipeline works under the hood
- [Benchmarks](benchmarks.md) — accuracy and compression data
- [Limitations](LIMITATIONS.md) — when compression helps and when it doesn't

## Python SDK

!!! note "Read-only mirror"
    The `headroom-ai` pip/uv package and the Python SDK now live in the read-only `upstream-python/` mirror. They are not built here; they exist so upstream diffs stay readable when porting. There is no `headroom proxy --backend` on the Rust-live path — `--backend` (bedrock, vertex_ai, azure, openrouter, …) is Python-mirror-only. The Rust proxy routes with `--upstream` plus route flags. [Full SDK docs](sdk.md).

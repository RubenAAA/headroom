# Quickstart

The fastest copy-paste path: install, launch, confirm savings flowing. For the
fuller tutorial, see [Getting Started](getting-started.md).

!!! note "Live implementation: Rust"
    The production proxy is the Rust binary (`crates/headroom-proxy`, launched with `cclaude`). Python paths anywhere in this wiki now live in the read-only `upstream-python/` mirror — re-resolve any `headroom/*.py` cite there. Behavior described here still holds; only the implementation moved.

## 1. Install

Prerequisites: git, Rust via rustup, plus `jq` and `lsof`.

```bash
git clone https://github.com/RubenAAA/headroom.git ~/headroom
cd ~/headroom
./install.sh
```

This builds the release binaries, installs `headroom-proxy` and `headroom` to
`~/.local/bin`, plus the `cclaude` launcher, `restart-headroom.sh`, and the
flags file (`~/.headroom-flags.sh`). `./install.sh --help` lists the options;
`--link` is for working on Headroom itself.

## 2. Launch

```bash
cclaude --context
```

`--context` routes through the proxy: the launcher starts it if it is down
(with the measured flags from `~/.headroom-flags.sh`), sets
`ANTHROPIC_BASE_URL`, and execs `claude` with the rest of your arguments.

!!! warning "Always `cclaude`, never bare `claude`"
    Plain `claude` talks straight to the API and the proxy does nothing. Bare `cclaude` without `--context` is plain passthrough too — `--context` is what puts the proxy in the path (see `contrib/claude-launcher`).

## 3. Confirm savings are flowing

```bash
curl -s localhost:8787/healthz
# {"ok":true,"service":"headroom-proxy"}

curl -s localhost:8787/cache-health
# hit rates and recent cache events

headroom doctor
# proxy liveness + local ledgers

headroom savings
# durable compression savings over time
```

Do a little work in the session first — `headroom savings` reads the local
ledger, so it only shows numbers once traffic has flowed.

!!! warning "The health path is `/healthz`, not `/health`"
    `curl -s localhost:8787/healthz` is the ground truth: `{"ok": true}` means
    the proxy is up, while a 404 or refused connection means nothing is
    listening. See [Troubleshooting](troubleshooting.md).

## Other clients

```bash
headroom-proxy --upstream https://api.anthropic.com --listen 127.0.0.1:8787
ANTHROPIC_BASE_URL=http://127.0.0.1:8787 claude
```

`--upstream` is required; the full option list is `headroom-proxy --help`.
Every flag also has a `HEADROOM_PROXY_*` environment variable.

## Next steps

- [Getting Started](getting-started.md) — the fuller tutorial
- [Proxy](proxy.md) — what the proxy does and how to operate it
- [Configuration](configuration.md) — flags and environment
- [Metrics](metrics.md) — health, cache-hit, and savings endpoints
- [Troubleshooting](troubleshooting.md) — when something breaks

!!! note "Python SDK mirror"
    The `headroom-ai` pip/uv package and the Python SDK live in the read-only `upstream-python/` mirror. They are not built here. [Full SDK docs](sdk.md).

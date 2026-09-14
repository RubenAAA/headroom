# Getting Started with Headroom

!!! note "Live implementation: Rust"
    The production proxy is the Rust binary (`crates/headroom-proxy`, launched with `cclaude`). Python paths anywhere in this wiki now live in the read-only `upstream-python/` mirror — re-resolve any `headroom/*.py` cite there. Behavior described here still holds; only the implementation moved.

The fuller tutorial: install → run → verify → first useful workflows. For the fastest copy-paste path, see [Quickstart](quickstart.md); for the map of the whole wiki, see [Headroom home](index.md).

## 1. Install

Prerequisites: git, Rust via rustup, plus `jq` and `lsof`.

```bash
git clone https://github.com/RubenAAA/headroom.git ~/headroom
cd ~/headroom
./install.sh
```

This builds the release binaries and installs `headroom-proxy` and `headroom` to `~/.local/bin`, plus the `cclaude` launcher, `restart-headroom.sh`, the flags file (`~/.headroom-flags.sh`), and the status line. `./install.sh --help` lists the options.

!!! tip "`--link` is for working on Headroom itself"
    Pass `./install.sh --link` only if the machine is for developing Headroom: it symlinks the scripts and the flags file into the checkout, so editing the repo edits the live setup. Otherwise the installer copies, and an existing `~/.headroom-flags.sh` is left alone (delete it to regenerate).

## 2. Run

```bash
cclaude --context
```

`--context` routes through the proxy: the launcher starts it if nothing is listening on 8787 (with the measured flags from `~/.headroom-flags.sh`), sets `ANTHROPIC_BASE_URL`, and execs `claude` with the rest of your arguments.

!!! warning "Always `cclaude --context`, never bare `claude`"
    Plain `claude` talks straight to the API and the proxy does nothing. Bare `cclaude` without `--context` is plain passthrough too — `--context` is what puts the proxy in the path (see `contrib/claude-launcher`).

## 3. Verify

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

Do a little work in the session first — `headroom savings` reads the local ledger, so it only shows numbers once traffic has flowed.

!!! warning "The health path is `/healthz`, not `/health`"
    `curl -s localhost:8787/healthz` is the ground truth: `{"ok": true}` means the proxy is up, while a 404 or refused connection means nothing is listening. See [Troubleshooting](troubleshooting.md).

## 4. First useful workflows

**Tune flags, then restart.** The measured settings live in `~/.headroom-flags.sh`, sourced by both the launcher and `restart-headroom.sh`. Edit them there, then apply with:

```bash
restart-headroom.sh
```

This restarts the proxy onto the freshly built binary, and rolls back to the previous one if the new build fails to come up. Note a running proxy is reused and keeps the flags it started with — editing the file alone changes nothing until a restart. Full option list: `headroom-proxy --help`; every flag also has a `HEADROOM_PROXY_*` environment variable. See [Configuration](configuration.md) and [Proxy](proxy.md).

**Read the savings.** `headroom savings` shows durable compression savings over time (see [Metrics](metrics.md) for the health, cache-hit, and savings endpoints).

## 5. Python SDK mirror (not the live path)

!!! note "Read-only mirror"
    The `headroom-ai` pip/uv package, the Python SDK, and `headroom proxy` live in the read-only `upstream-python/` mirror. They are not built here; they exist so upstream diffs stay readable when porting. [Full SDK docs](sdk.md).

# Persistent Installs

!!! note "Live implementation: Rust"
    The production install is `./install.sh` plus `cclaude` (Rust binary in `crates/headroom-proxy`). There is no `headroom install apply|status|start|stop|restart|remove` subcommand in the Rust CLI and `./install.sh` takes no `--runtime` flag — only `--no-build`, `--link`, and `--hooks-into DIR`. The `persistent-service` / `persistent-task` / `persistent-docker` presets, the runtime matrix, and the `--runtime python|docker` flags below describe the old Python installer only; its last copy lives in the read-only `upstream-python/` mirror (`upstream-python/headroom/cli/install.py`). Nothing below uses it.

Persistence here means files on disk, not a process that outlives a reboot. No supervisor in this repo keeps the proxy running across a reboot by itself — after a reboot nothing listens on `127.0.0.1:8787` until something starts it. That something is normally the next `cclaude --context`, which starts the proxy when the port is empty and reuses it otherwise. For login-time startup without opening a shell, see [macOS LaunchAgent](macos-deployment.md) — pick one supervisor, never both at once.

## What survives a reboot

Everything `./install.sh` wrote, plus proxy state and logs. (Full install listing is on [macOS Deployment](macos-deployment.md); this is the subset that matters after a reboot.)

| Path | What it is | Install behavior |
|---|---|---|
| `~/.local/bin/headroom-proxy`, `~/.local/bin/headroom` | release binaries | always copied from `target/release/` |
| `~/.local/bin/claude-launcher`, `cclaude` symlink, `~/.local/bin/restart-headroom.sh` | launcher + restart script | copied; symlinked into `contrib/` with `--link` |
| `~/.headroom-flags.sh` | measured flag set, sourced by both starters | copied from `contrib/headroom-flags.sh`; an existing file is left alone (delete it to regenerate) |
| `~/.headroom-paths.sh` | `HEADROOM_REPO` (+ GNU-tools `PATH` prefix on macOS) | rewritten on every install |
| `~/.claude/statusline-with-cache.sh`, `statusline-usage-dump.sh`, `~/.claude/settings.json` wiring | status line | checkout path baked in; existing settings backed up to `.bak` |
| `~/.claude/agents/`, `~/.claude-work/agents/`, `~/.claude-personal/agents/` | one subagent per routed model | files you already have are left alone |
| `~/.claude/hooks/` + registration, `~/.claude/CLAUDE.md` headroom section | review/ticket hooks, memory-tool instructions | ensured idempotently between markers |
| `~/headroom-proxy.log` (plus `.1`–`.4`) | proxy stdout/stderr | `cclaude` rotates on start; `restart-headroom.sh` appends |
| `~/.headroom/` state (`proxy_savings.json`, `savings_events.jsonl`, `ctx/`, memories, CCR store) | savings ledger, session/memory stores | written at runtime; see [Filesystem Contract](filesystem-contract.md) |

What does **not** survive is the running proxy itself. First session after a reboot:

```bash
cclaude --context   # port empty -> starts proxy with ~/.headroom-flags.sh; port held -> reuses it
curl -s localhost:8787/healthz                  # {"ok":true,...} proves the process is up
curl -s localhost:8787/cache-health | head -20  # proves it helps; low hit rate means it is not
```

!!! warning "One supervisor at a time"
    `cclaude`, `restart-headroom.sh`, and a LaunchAgent all start the same binary onto the same port and the same log, and a running proxy ignores later flags. Use the launcher path *or* the agent in [macOS Deployment](macos-deployment.md) — never both at once.

## Staying healthy after an update

```bash
git pull
./install.sh            # rebuilds release binaries, refreshes everything except your flags file
```

`--no-build` installs whatever `target/release` holds; `--link` symlinks the scripts and flags file into `contrib/` so editing the checkout edits the live setup (binaries are always copied). If `~/.local/bin` is not on your `PATH`, the installer says so — add it or neither `cclaude` nor `headroom` resolves.

For a binary-only swap after `cargo build --release -p headroom-proxy`:

```bash
restart-headroom.sh
```

It copies `target/release/headroom-proxy` over `~/.local/bin/headroom-proxy` (keeping a `~/.local/bin/headroom-proxy.prev` backup), restarts onto port 8787 with your flags file, and rolls back to the previous binary if the new one fails to listen. It refuses to start if `~/.headroom-flags.sh` is missing rather than serve traffic on defaults. Progress lands in `~/headroom-proxy.log`. Do not run it while the LaunchAgent is loaded — unload first (see [macOS Deployment](macos-deployment.md)).

Two habits that prevent the common failures:

- **Flags need a restart.** Edit `~/.headroom-flags.sh`, then `pkill -f headroom-proxy` (launcher path) and rerun `cclaude --context`. A running proxy keeps the flags it started with.
- **One copy of the binary.** `type -pa headroom-proxy` must show a single path and `~/.local/bin` must win; a stale copy (usually `~/.cargo/bin`) rejects flags the flags file has grown and every start dies on usage text. Check with `tail -n 50 ~/headroom-proxy.log`.

Verify after any update:

```bash
headroom doctor
headroom savings
headroom perf --hours 24
```

## Logs

The Rust proxy writes to stdout only — the log lives wherever the starter pointed it, which for both starters is `~/headroom-proxy.log` (JSON lines). Override what `headroom perf` reads with `HEADROOM_PROXY_LOG_PATH`.

!!! note "Per-port logs are the Python path, not this one"
    The Python proxy writes per-port runtime logs at `${HEADROOM_WORKSPACE_DIR}/logs/proxy-<port>.log` (default `~/.headroom/logs/`), plus PID-qualified `proxy-<port>-<pid>.log` files — see `upstream-python/headroom/paths.py` (`proxy_log_path`). The Rust binary has no per-port log. `headroom perf` aggregates both layouts, so history is not orphaned either way.

## Where the old matrix went

The `persistent-service` / `persistent-task` / `persistent-docker` presets, `headroom install ...` lifecycle, provider/user/system scopes, and `docker/docker-compose.native.yml` compose path were the Python installer's story. The compose file's last copy is `upstream-python/docker/docker-compose.native.yml`. For containerized on-demand runs, see [Docker-Native Install](docker-install.md).

## Related guides

- [macOS LaunchAgent](macos-deployment.md) - login-time startup for the Rust binary (the only service setup this repo documents)
- [CLI Reference](cli.md) - `headroom perf`, `doctor`, and `savings`
- [Proxy Server](proxy.md) - core proxy configuration and features
- [Configuration Guide](configuration.md) - flags file and `HEADROOM_PROXY_*` environment
- [Filesystem Contract](filesystem-contract.md) - where state, logs, and config live
- [Troubleshooting](troubleshooting.md) - general troubleshooting guide

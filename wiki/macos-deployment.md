# macOS Deployment Guide

!!! note "Live implementation: Rust"
    The production proxy is the Rust binary (`crates/headroom-proxy`), installed by `./install.sh` and launched with `cclaude --context`. Python paths on this page live in the read-only `upstream-python/` mirror. The old Python installer (`examples/deployment/macos-launchagent/install.sh`) does not exist in this repo — its last copy is `upstream-python/examples/deployment/macos-launchagent/`, and its plist template runs `headroom proxy`, a subcommand the Rust `headroom` CLI does not have. Nothing below uses it.

This guide covers running the Rust proxy on macOS: the standard launcher path first, and an optional hand-written LaunchAgent for login-time startup. There is no repo-shipped plist or installer script for the Rust binary — if you want launchd supervision, you write the plist below by hand.

## Overview

The standard deployment has one supervisor already: `cclaude` starts the proxy when nothing answers on `127.0.0.1:8787` and reuses the running instance otherwise. A LaunchAgent only makes sense if you want the proxy up at login without opening a shell first — crash recovery (`KeepAlive`) and login start (`RunAtLoad`) via `launchctl`.

- **Standard path**: `./install.sh`, then `cclaude --context`. No plist needed.
- **LaunchAgent path**: a small plist that runs `~/.local/bin/headroom-proxy` directly. Pick one supervisor — do not mix this with `cclaude`-started or `restart-headroom.sh`-started proxies (see the warning below).

## Prerequisites

- macOS 10.13+ with [Homebrew](https://brew.sh) (the installer pulls GNU tools and a bash newer than the 3.2 macOS ships)
- Git, Rust via [rustup](https://rustup.rs), plus `jq` and `lsof` (statusline and restart script need them; the installer warns if they are missing)
- No API key configuration on the service: the proxy forwards your own client authentication upstream. There is nothing secret to put in a plist.

## Standard Install (No LaunchAgent)

```bash
git clone https://github.com/RubenAAA/headroom.git ~/headroom
cd ~/headroom
./install.sh
```

Options: `--no-build` skips cargo and installs whatever `target/release` holds; `--link` symlinks the scripts and flag file into `contrib/` so editing the checkout edits the live setup (binaries are always copied).

What it installs and where (existing files are left alone, never overwritten):

- `~/.local/bin/headroom-proxy` and `~/.local/bin/headroom`, built release
- `~/.local/bin/claude-launcher`, with `cclaude` symlinked to it, and `~/.local/bin/restart-headroom.sh`
- `~/.headroom-flags.sh`, the measured flag set copied from `contrib/headroom-flags.sh`
- `~/.headroom-paths.sh`, holding `HEADROOM_REPO` plus the GNU-tools `PATH` prefix on macOS
- `~/.claude/statusline-with-cache.sh` and `statusline-usage-dump.sh`, wired into `~/.claude/settings.json`, plus one subagent per routed model in `~/.claude/agents/` and memory-tool instructions spliced into `~/.claude/CLAUDE.md`

If `~/.local/bin` is not on your `PATH`, the installer says so — add it to your shell profile, or neither `cclaude` nor `headroom` resolves.

### Launch via cclaude

```bash
cclaude --context     # starts the proxy if down, sets ANTHROPIC_BASE_URL, execs claude
```

The launcher starts the proxy with `--listen 127.0.0.1:8787 --upstream https://api.anthropic.com` plus the flags from `~/.headroom-flags.sh`, waits for `/cache-health` to answer, and logs to `~/headroom-proxy.log`. A proxy already on the port is **reused** — flags passed on the command line do not apply to it. To use new flags: `pkill -f headroom-proxy`, then rerun.

!!! warning "One supervisor at a time"
    `cclaude`, `restart-headroom.sh`, and a LaunchAgent all start the same binary onto the same port and the same log. A running proxy ignores later flags, and two starters race for port 8787. Use the LaunchAgent section below *or* the launcher path — never both at once.

### Verify

```bash
curl -s localhost:8787/healthz                  # {"ok":true,"service":"headroom-proxy"}
curl -s localhost:8787/cache-health | head -20  # hit rates; low means it is not helping
tail -f ~/headroom-proxy.log                    # JSON lines
headroom doctor
headroom savings
```

`/healthz` only proves the process is up. `/cache-health` proves it helps.

### Flags

Flags come only from the command line and the environment, never a config file. Every flag has a `HEADROOM_PROXY_*` variable (`--listen` ↔ `HEADROOM_PROXY_LISTEN`, `--upstream` ↔ `HEADROOM_PROXY_UPSTREAM`, and so on — see `headroom-proxy --help`, or the generated `docs/flags.md`).

Edit `~/.headroom-flags.sh`, then restart. Editing alone changes nothing about the process already running.

### Restart and rollback

After rebuilding the Rust binary:

```bash
restart-headroom.sh
```

It swaps `target/release/headroom-proxy` into `~/.local/bin/headroom-proxy` (keeping a `.prev` backup), restarts onto port 8787 with your flags file, and rolls back to the previous binary if the new one fails to listen. Progress lands in `~/headroom-proxy.log`. It refuses to start if `~/.headroom-flags.sh` is missing rather than serve traffic on defaults.

Stop it by hand with `pkill -f headroom-proxy`.

## LaunchAgent (Optional, Rust Binary)

!!! note "When this earns its keep"
    Only if you want the proxy listening after login with no terminal involved. If `cclaude --context` already covers your sessions, skip this section — it adds a second supervisor you then have to keep out of the launcher's way.

There is no template or installer for this in the repo. The unit runs the Rust binary, not Python:

### Step 1: Write the plist

Create `~/Library/LaunchAgents/com.headroom.proxy.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.headroom.proxy</string>

    <key>ProgramArguments</key>
    <array>
        <string>__HOME__/.local/bin/headroom-proxy</string>
        <string>--listen</string>
        <string>127.0.0.1:8787</string>
        <string>--upstream</string>
        <string>https://api.anthropic.com</string>
    </array>

    <!-- The proxy writes to stdout and nothing else, so the log lives
         wherever this points. ~/headroom-proxy.log keeps headroom perf,
         the statusline, and the restart script working unchanged. -->
    <key>StandardOutPath</key>
    <string>__HOME__/headroom-proxy.log</string>
    <key>StandardErrorPath</key>
    <string>__HOME__/headroom-proxy.log</string>

    <key>WorkingDirectory</key>
    <string>__HOME__</string>

    <key>KeepAlive</key>
    <true/>

    <key>RunAtLoad</key>
    <true/>

    <key>ProcessType</key>
    <string>Adaptive</string>

    <key>ThrottleInterval</key>
    <integer>10</integer>
</dict>
</plist>
```

Replace `__HOME__` with your home directory (`echo $HOME` — a plist does no shell expansion):

```bash
mkdir -p ~/Library/LaunchAgents
sed -e "s|__HOME__|$HOME|g" <template-from-above> > ~/Library/LaunchAgents/com.headroom.proxy.plist
chmod 644 ~/Library/LaunchAgents/com.headroom.proxy.plist
```

`--listen` and `--upstream` are required — the binary has no default upstream. Extra flags from `~/.headroom-flags.sh` do **not** carry over: that file is bash, sourced only by `cclaude` and `restart-headroom.sh`. Repeat any non-default flag literally in `ProgramArguments` (check names with `headroom-proxy --help`), or set its `HEADROOM_PROXY_*` equivalent in an `EnvironmentVariables` dict. Either way, the plist and the flags file must be kept in step by hand.

### Step 2: Load it

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.headroom.proxy.plist
```

### Step 3: Verify

```bash
launchctl print gui/$(id -u)/com.headroom.proxy   # state = running
lsof -iTCP:8787 -sTCP:LISTEN
curl -s localhost:8787/healthz
curl -s localhost:8787/cache-health | head -20
```

### Step 4: Use it

With the agent listening, `cclaude --context` reuses the agent's proxy (it finds port 8787 already answering) — just remember its command-line flags will not apply to that process.

### Service management

```bash
# Restart (picks up plist or binary changes)
launchctl kickstart -k gui/$(id -u)/com.headroom.proxy

# Stop / start by hand
launchctl bootout gui/$(id -u)/com.headroom.proxy
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.headroom.proxy.plist

# Disable without uninstalling (and re-enable)
launchctl disable gui/$(id -u)/com.headroom.proxy
launchctl enable gui/$(id -u)/com.headroom.proxy
```

Do not run `restart-headroom.sh` while the agent is loaded: it kills the listener and starts its own proxy, fighting `KeepAlive` for the port. Unload the agent first (`bootout`), or rebuild and `kickstart -k` instead.

### Uninstall

```bash
launchctl bootout gui/$(id -u)/com.headroom.proxy
rm ~/Library/LaunchAgents/com.headroom.proxy.plist
# Logs (optional): rm -f ~/headroom-proxy.log ~/headroom-proxy.log.[1234]
```

## Logs

One file to know: `~/headroom-proxy.log` (JSON lines). Both starters — `cclaude` and `restart-headroom.sh` — append there, and everything that reads proxy output reads that path: `headroom perf`, the `statusline-cache-perf.sh` statusline (`HEADROOM_PROXY_LOG` overrides it), `contrib/reconcile_books.py`, and `contrib/zen-rotate-watch.sh`.

Pointing the LaunchAgent's `StandardOutPath`/`StandardErrorPath` at the same file keeps all of that tooling working. Nothing rotates it under an agent — the file grows until you truncate it. `headroom perf --hours N` bounds what it reads, so a large file stays cheap to query:

```bash
headroom perf --hours 24
headroom perf --raw | head -5
```

!!! note "Per-port logs are the Python path, not this one"
    Since the Sep-09 change, the Python `headroom proxy` / `wrap` path writes per-port runtime logs at `${HEADROOM_WORKSPACE_DIR}/logs/proxy-<port>.log` (default `~/.headroom/logs/`), plus PID-qualified `proxy-<port>-<pid>.log` files for multi-worker runs, with a legacy `proxy.log` fallback — see `upstream-python/headroom/paths.py` (`proxy_log_path`) and `wiki/cli.md`. The Rust binary has no per-port log: it writes to stdout only, and the log lives wherever the starter pointed it (`docs/filesystem-layout.md`). `headroom perf` aggregates both layouts, so history is not orphaned either way. Override the launcher-log path it reads with `HEADROOM_PROXY_LOG_PATH`.

## Troubleshooting

| Symptom | Check / fix |
|---|---|
| `headroom: proxy failed to start` | `tail -n 50 ~/headroom-proxy.log` — the launcher prints exactly this path on failure |
| Proxy exits on a flag error / usage text in the log | A stale `headroom-proxy` shadows the fresh one. The launcher and installer both warn about duplicate copies on `PATH` — `~/.local/bin` must win; remove the other (`~/.cargo/bin` is the usual suspect) |
| Flags edited, nothing changed | A running proxy keeps the flags it started with. `pkill -f headroom-proxy` (launcher path) or `kickstart -k` (agent path), then start again |
| Port already in use | `lsof -iTCP:8787 -sTCP:LISTEN` to find the owner; two starters means two supervisors — unload one |
| Agent won't load / not running after login | `launchctl print gui/$(id -u)/com.headroom.proxy`; `chmod 644` the plist and confirm it is owned by you, not root; confirm `RunAtLoad` is `<true/>` |
| `~/headroom/logs/proxy-8787.log` is stale or missing | That is the Python runtime log, not the Rust one. The Rust proxy's output is in `~/headroom-proxy.log` — check there first |

## Security Considerations

The proxy binds `127.0.0.1` — localhost only, no external exposure. Do not change `--listen` to `0.0.0.0` without firewall rules.

**LaunchAgent** (this page) runs in your user context, needs no root, and starts at login. A **LaunchDaemon** would run system-wide at boot as root — unnecessary for a single-user dev proxy, and not covered here.

## Advanced Configuration

### Multiple instances

One port per process. Duplicate the plist under a new label (e.g. `com.headroom.proxy-2`), change `--listen` to `127.0.0.1:8788`, and give it its own `StandardOutPath` — two agents must not share one log file. A `cclaude` started proxy always takes 8787, so point other clients at the second port explicitly.

### Resource limits

Stock launchd keys work as usual under the agent:

```xml
<key>HardResourceLimits</key>
<dict>
    <key>NumberOfProcesses</key>
    <integer>1</integer>
</dict>
```

After any plist edit: `launchctl kickstart -k gui/$(id -u)/com.headroom.proxy`.

## FAQ

**Q: Why not just run the proxy by hand?**

A: You can: `headroom-proxy --upstream https://api.anthropic.com --listen 127.0.0.1:8787`, then `ANTHROPIC_BASE_URL=http://127.0.0.1:8787` for any client. `cclaude --context` just automates that and keeps one shared instance.

**Q: Do I need the LaunchAgent if I already use `cclaude`?**

A: No. The agent is only for login-time startup without a shell. One supervisor at a time.

**Q: Where did the `./install.sh --port` installer go?**

A: That was the Python-era installer (`uv tool install "headroom-ai[proxy]"` plus a generated plist). It has no Rust equivalent in this repo. Port selection for the Rust proxy is the `--listen` flag / `HEADROOM_PROXY_LISTEN`.

**Q: Does this work on Apple Silicon?**

A: Yes. The Rust binary builds and runs on arm64; the installer pulls the GNU toolchain it needs through Homebrew.

## Related Documentation

- [Proxy Server Documentation](proxy.md) - Core proxy configuration and features
- [Filesystem Contract](filesystem-contract.md) - Where state, logs, and config live
- [CLI Reference](cli.md) - `headroom perf`, `doctor`, and `savings`
- [Configuration Guide](configuration.md) - Detailed configuration options
- [Troubleshooting](troubleshooting.md) - General troubleshooting guide

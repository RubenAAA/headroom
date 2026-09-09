# AGENTS.md

Headroom is a local reverse proxy between Claude Code and the model API. It
compresses requests, keeps the provider's prompt cache intact, and routes Codex
model names to OpenAI in the same session.

**The one rule that matters: after installing, start Claude Code with `cclaude`,
never `claude`.** Plain `claude` talks straight to the API and the proxy does
nothing. `cclaude` starts the proxy if it is down, sets `ANTHROPIC_BASE_URL`,
and execs `claude` with every argument passed through. If you tell the user
"done" and they type `claude`, nothing you installed is in use. Say `cclaude`
in your final message, and check with `curl -s localhost:8787/cache-health`
after their first turn that the request count went up.

## Install on a new machine

Prerequisites: git, Rust via [rustup](https://rustup.rs), plus `jq` and `lsof`
for the statusline and restart script. On macOS the installer needs Homebrew and
uses it to fetch GNU tools and a bash newer than 3.2. ONNX Runtime is optional,
for file type detection and embeddings: `install.sh` checks `ORT_DYLIB_PATH` for
an existing file, then tries importing `onnxruntime` in python3, and prints a
hint if neither works. Without it the proxy still runs.

```bash
git clone https://github.com/RubenAAA/headroom.git ~/headroom
cd ~/headroom
./install.sh
```

Two options. `--no-build` skips cargo and installs whatever `target/release`
holds. `--link` symlinks the scripts and the flag file into `contrib/` instead of
copying, so editing the checkout edits the live setup. Use it if the machine is
for working on Headroom. The maintainer does.

`install.sh` writes:

- `~/.local/bin/headroom-proxy` and `~/.local/bin/headroom`, built release.
- `~/.local/bin/claude-launcher`, with `cclaude` symlinked to it, and
  `~/.local/bin/restart-headroom.sh`.
- `~/.headroom-flags.sh`, a bash array of proxy flags copied from
  `contrib/headroom-flags.sh` with paths written as `$HOME`. An existing file is
  left alone, so tuning is never overwritten. Under `--link` it becomes a symlink
  to the checkout, and any existing real file is moved to `.bak` first.
- `~/.headroom-paths.sh`, holding `HEADROOM_REPO`.
- `~/.claude/statusline-with-cache.sh` and `statusline-usage-dump.sh`, wired into
  `~/.claude/settings.json`. The first is always generated, never symlinked,
  since the checkout path is baked into it.

So `--link` covers the launcher, restart script, flag file and usage dump.
Binaries are always copied, so a Rust change needs a rebuild plus
`restart-headroom.sh`. The script warns if `~/.local/bin` is off `PATH`. Verify:

```bash
cclaude                                # NOT claude: starts the proxy, then execs claude through it
curl -s localhost:8787/healthz
curl -s localhost:8787/cache-health    # hit rates; a low one means it is not helping
tail -f ~/headroom-proxy.log           # JSON lines
```

Stop it with `pkill -f headroom-proxy`. `restart-headroom.sh` swaps the binary,
restarts, and rolls back if the new one fails to come up.

## Point a client at it

`cclaude` does this for Claude Code. Use it. A plain `claude` bypasses the
proxy entirely, and there is no error to tell you so. Only do it by hand for
another client:

```bash
headroom-proxy --upstream https://api.anthropic.com --listen 127.0.0.1:8787
ANTHROPIC_BASE_URL=http://127.0.0.1:8787 claude
```

## Adjusting flags

Flags come only from the command line and the environment, never a config file.
Every flag has a `HEADROOM_PROXY_*` variable, so `--cache-tail-breakpoints 2` and
`HEADROOM_PROXY_CACHE_TAIL_BREAKPOINTS=2` are the same thing.

Edit `~/.headroom-flags.sh`, then run `restart-headroom.sh`. Under `--link` that
file is `contrib/headroom-flags.sh` in the checkout, so edit either. A running
proxy is reused as it is, and flags on a later command line are ignored, so a
change with no restart means you are still measuring the old setting.

All 124 options: `headroom-proxy --help`, or [`docs/flags.md`](docs/flags.md),
generated from that output. Regenerate it when you add a flag.

## Layout for editing

- `crates/headroom-proxy` is the product: the proxy binary, the `headroom` CLI,
  request handlers, and the cache stabilizers.
- `crates/headroom-core` holds compression, the context store and memory.
- `crates/headroom-parity` checks Rust output against Python.
  `crates/headroom-simulators` drives the benchmarks.
- `crates/headroom-py` is a PyO3 module built with maturin, not cargo.
- `contrib/` has the launcher, restart script, statuslines, flag file, and Claude hooks. Only
  `install.sh` reads it.
- `docs/` is reference material. `docs/notes/` is working notes, some stale. Do
  not treat notes as a spec.

`crates/headroom-proxy/src/cache_stabilization/` holds the rewrites that stop the
client invalidating its own cached prefix: TTL forcing, stable tool order, roster
pinning, prefix replay, breakpoint placement. Each module doc names the failure
it exists for. Read it before touching one. These are measured behaviours, not
preferences, and all are off by default.

Integration tests live in `crates/headroom-proxy/tests/`. The pattern is a
wiremock upstream capturing forwarded bodies, a proxy started against it, and
assertions on what came out. Copy an existing file, do not invent a harness.

Before pushing:

```bash
make ci-precheck   # fmt, clippy, tests; the same gate CI runs
make test          # cargo test --workspace alone
```

`rust-toolchain.toml` pins 1.95.0 so a clippy lint from a newer stable cannot
break CI without firing locally. Do not bump it casually. Code must pass
`cargo fmt --check` and `cargo clippy -- -D warnings`.

## Rules

- Never commit secrets. `~/.headroom-flags.sh` names auth file paths, not keys,
  and lives outside the repo. Keep it there.
- The Python tree (`headroom/`, `tests/`, `sdk/`, `plugins/`) is a read-only
  mirror of upstream. It is not built here. It exists so upstream diffs stay
  readable when porting. Do not edit or reformat it.
- Do not reformat code you are not changing. Match the surrounding style.
- Do not commit or push unless asked.

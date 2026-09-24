# AGENTS.md

Headroom is a local reverse proxy between Claude Code and the model API. It
compresses requests, keeps the provider's prompt cache intact, and routes Codex
model names to OpenAI in the same session. Supported models and their
`/model` aliases: @MODELS-SUPPORTED-WITHIN-CLAUDE-CODE-PROXY.md.

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

Optional but recommended for fast test loops: `cargo-nextest` (parallel
runner; what CI shards run) and `sccache` (compiler cache, opt in with
`export RUSTC_WRAPPER=sccache`). `install.sh` offers both; without them
the Makefile falls back to plain `cargo test`.

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

- `~/.local/bin/headroom-proxy`, `~/.local/bin/headroom`, and the Rust
  `~/.local/bin/nord-socks-egress` helper, built release.
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

Under `--link`, the contrib scripts, hooks, flag file and usage dump are
symlinked into the checkout. Binaries are always copied and the main statusline
is always generated, so those still need reinstalling after relevant changes.
In copy mode a `git pull` leaves every installed copy stale. `update-headroom.sh`
(installed in `~/.local/bin`) pulls, infers link mode from the flags-file symlink
unless overridden with `--link`/`--copy`, reinstalls, and restarts the proxy. The
script warns if `~/.local/bin` is off `PATH`. Verify:

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

All 131 options (133 with `-h`/`-V`): `headroom-proxy --help`, or [`docs/flags.md`](docs/flags.md),
generated from that output. Regenerate it when you add a flag.

## Layout for editing

- `crates/headroom-proxy` is the product: the proxy binary, the `headroom` CLI,
  request handlers, and the cache stabilizers. `proxy.rs` holds only
  module wiring and small shared helpers. Its code lives in `proxy/<area>.rs`
  (`state`, `app`, `request_transforms`, `replay`, `continuation`, …). Add
  new code to the module for its area, or start a new one. The pre-push hook
  fails any Rust file that grows past 3000 lines
  (`scripts/check-file-size.sh`). To split one, write a plan and run
  `scripts/split-rust-module.py split`, then `verify`; its docstring has
  the steps.
- `crates/headroom-core` holds compression, the context store and memory.
- `crates/headroom-parity` checks Rust output against Python.
  `crates/headroom-simulators` drives the benchmarks.
- `crates/headroom-py` is a PyO3 module built with maturin, not cargo.
- `contrib/` has the launcher, restart script, statuslines, flag file, and Claude hooks. Only
  `install.sh` reads it. File-by-file: `contrib/README.md`.
- `docs/` is reference material. `docs/notes/` is working notes, some stale. Do
  not treat notes as a spec.
- Before touching proxy behaviour or chasing a saving, read
  `@docs/notes/learnings/README.md` and the relevant `docs/notes/learnings/*.md`.
  One file per durable finding; refuted ideas stay to stop retests. Numbers are
  scoped to their window — do not quote across windows.
- When asked for optimizations/improvements, triage via
  `@docs/notes/ideas/README.md`: root = open, `implemented/` = shipped (names
  commit/flag, do not redo), `rejected/` = measured and declined with the killing
  number (do not retry without new evidence). When an idea ships or dies, move
  the file, don't duplicate it.

`crates/headroom-proxy/src/cache_stabilization/` holds the rewrites that stop the
client invalidating its own cached prefix: TTL forcing, stable tool order, roster
pinning, prefix replay, breakpoint placement. Each module doc names the failure
it exists for. Read it before touching one. These are measured behaviours, not
preferences. Several ship on (notably `--cache-stable-tool-order`,
`--cache-tail-breakpoint`, `--ctx-drop-prior-thinking`); check `--help`
for the default of the one you touch rather than assuming off.

Integration tests live in `crates/headroom-proxy/tests/`. The pattern is a
wiremock upstream capturing forwarded bodies, a proxy started against it, and
assertions on what came out. Copy an existing file, do not invent a harness.

## Testing (fast loop)

Do not run `cargo test --workspace` in a loop — it links 80+ integration
binaries every time. Prefer, in order:

```bash
make test-unit           # --lib --bins only; default loop
make test-nextest-unit   # same, via nextest (needs cargo-nextest)
make test-touched        # only suites covering `git diff HEAD`
make what-to-run         # preview what test-touched would run
```

Full suite is `make test-nextest` (what CI shards run as
`--partition hash:<shard>/4`); plain `make test` is the back-compat
fallback. Scope with `-p <crate>` and nextest `-E` filters, e.g.
`cargo nextest run -p headroom-proxy --profile ci -E 'kind(lib) and test(cache_stabilization)'`.
The `ci` profile sets `PROPTEST_CASES=32` (vs 256 default); rerun with
`PROPTEST_CASES=256` when touching parser code. Skip `--features ml`
unless you touched ONNX paths — it needs a real libonnxruntime and is
much slower.

Before pushing:

```bash
make ci-precheck   # fmt, clippy, tests; the same gate CI runs
```

`rust-toolchain.toml` pins 1.95.0 so a clippy lint from a newer stable cannot
break CI without firing locally. Do not bump it casually. Code must pass
`cargo fmt --check` and `cargo clippy -- -D warnings`.

`target/` is garbage-collected by `make test` / `make build-proxy` /
`make build-wheel` (at most once a day, never fails the build) once
`cargo install cargo-sweep` has run — `install.sh` does that on fresh
setups. `make gc-check` previews, `make gc` forces. See
`scripts/cargo-gc.sh` for the policy.

## Rules

- Never commit secrets. `~/.headroom-flags.sh` names auth file paths, not keys,
  and lives outside the repo. Keep it there.
- The Python tree (`upstream-python/`, holding `headroom/`, `tests/`, `sdk/`,
  `plugins/`) is a read-only mirror of upstream. It is not built here. It
  exists so upstream diffs stay readable when porting. Do not edit or
  reformat it.
- Do not reformat code you are not changing. Match the surrounding style.
- Do not commit or push unless asked.

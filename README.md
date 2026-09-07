# Headroom (Rust)

A local proxy that sits between Claude Code and the model API, cuts what gets
sent, and keeps the prompt cache intact while doing it. It also routes Codex
models through the same Claude Code session, so one conversation, one memory,
one set of tools.

This is a Rust rewrite of the proxy from
[headroomlabs-ai/headroom](https://github.com/headroomlabs-ai/headroom), which
the fork still tracks so upstream changes can be ported across. Upstream's
README covers its Python package, MCP server and hosted docs. Upstream also
publishes a container image, `ghcr.io/headroomlabs-ai/headroom`. It runs the
Python package, not this proxy, and there is no image for this fork. Build from
source.

## What it actually saves

Measured on one developer's traffic, 2026-08-12. Run the same commands and you
get your own figures. None of this is a projection.

```
$ headroom savings
Compression reduction on saving turns
Scope: pre-compression input selected by transforms; not all provider input.
Today        ███████████░░░░░  68.6%  saved 9,708,856 / 14,148,393 selected tokens  $137.21
Last 7 days  ████░░░░░░░░░░░░  24.3%  saved 100,975,119 / 415,353,531 selected tokens  $721.25
Last 30 days ████░░░░░░░░░░░░  24.5%  saved 102,139,076 / 417,154,366 selected tokens  $723.84
```

**24.3% of the input selected on saving turns was removed before forwarding**
over a week. That is transform efficiency, not a share of all provider input:
the ledger counts compression events, and its denominator is those events' own
pre-compression input. Daily figures swing hard, so judge it over a week, and
read the dollar column as an estimate rather than a provider bill.

The proxy also causes cache misses of its own, and counts them against itself.
From `/stats`:

```
savings_verdict: saved 100,847,379 − lost to cache busts 31,923,043 (625 busts)
                 = net 68,924,336 tokens, against 414,037,302 attempted
```

That is **1.20x the work per token spent**, after the tool pays for its own
mistakes, and it could have come out negative. Overhead is small enough to
ignore: across 25,708 requests the proxy added 566,855 bytes and removed
199,499,200. How all of this is counted is written up in
[`docs/measurement.md`](docs/measurement.md).

## Quick start

Needs Rust via [rustup](https://rustup.rs) and git. On macOS the installer also
pulls GNU tools through Homebrew, including a bash newer than the 3.2 it ships.

```bash
git clone https://github.com/RubenAAA/headroom.git
cd headroom
./install.sh
cclaude          # from now on, always this instead of `claude`
```

That builds both binaries, installs them to `~/.local/bin`, writes a flag file
to your home directory, wires the token statusline into
`~/.claude/settings.json`, adds one Claude Code subagent per routed model to
`~/.claude/agents/` (`codex-sol`, `grok-high`, `spark`, and so on), and splices
the memory-tool instructions from
[`contrib/claude/CLAUDE.headroom.md`](contrib/claude/CLAUDE.headroom.md) into
`~/.claude/CLAUDE.md` between marker comments. Existing agent files are left
alone. The statusline helpers live in `contrib/`, so keep the checkout where it
is.

To edit the checkout, use `./install.sh --link`, which is how the maintainer
runs it. That symlinks the scripts and the flag file into `contrib/` instead of
copying, so editing the repo edits the live setup. Binaries are copied either
way, so a Rust change still needs a rebuild and `restart-headroom.sh`.

Then start Claude Code with `cclaude`, not `claude`. The wrapper starts the
proxy if it is down, sets `ANTHROPIC_BASE_URL`, and execs `claude` with your
arguments. Plain `claude` bypasses the proxy without any error. For another
client, do it by hand:

```bash
headroom-proxy --upstream https://api.anthropic.com --listen 127.0.0.1:8787
ANTHROPIC_BASE_URL=http://127.0.0.1:8787 claude
```

To check it is working:

```bash
curl -s localhost:8787/healthz
curl -s localhost:8787/cache-health | head -20
```

`/cache-health` is the one that tells you something. A proxy that runs but does
not help shows up there as a low hit rate.

## What it does

Claude Code sends a request. The proxy rewrites it, forwards it to
`api.anthropic.com` or to OpenAI for a routed model name, and streams the
response back.

**Compression.** Tool results, file reads and search output get shrunk, and a
retrievable copy of anything it touches is kept on disk.

**Prefix replay.** Compression rewrites history, which would break the cached
prefix. So the proxy replays the prefix it forwarded last turn, byte for byte,
and puts its own `cache_control` breakpoints on the tail within the provider's
limit of four. This matters more than compression does, because a compressor
that busts the prompt cache costs more than it saves.

**Cache stabilizers.** Small rewrites that stop the client from invalidating its
own cached prefix. All are off unless a flag turns them on.

| Flag | What it fixes |
| --- | --- |
| `--force-1h-cache-ttl` | Marks entries `1h` so the prefix survives an idle gap past the 5-minute default. Skipped on pay-as-you-go, where 1h input costs more. |
| `--cache-stable-tool-order` | `tools` sits at the head of the cache key, so the first tool whose bytes move invalidates every tool after it plus the system prompt and all history. This replays last turn's order and appends new tools at the end. |
| `--cache-pin-tool-roster` | Claude Code drops a tool from its roster for one turn and re-adds it next turn. The array is hashed as sent, so that flap alone accounts for about half the recache waste in the log. This puts the tool back where it was. |

**Context capture.** Conversations go to a local store, searchable with
`headroom ctx search`.

**Model routing.** A Codex model name goes to OpenAI instead, inside the same
Claude Code session.

**Observability.** A JSON-lines log, four HTTP endpoints, and a savings ledger
that survives restarts.

## Configuration

Flags come from the command line or the environment, never a config file. Every
flag has a `HEADROOM_PROXY_*` variable, so `--cache-tail-breakpoints 2` and
`HEADROOM_PROXY_CACHE_TAIL_BREAKPOINTS=2` do the same thing. Full list in
[`docs/flags.md`](docs/flags.md), or run `headroom-proxy --help`.

The launcher and the restart script source `~/.headroom-flags.sh`, a bash array
`install.sh` writes from [`contrib/headroom-flags.sh`](contrib/headroom-flags.sh).
That is the maintainer's measured set, about 85 flags, worth reading before you
pick your own. An existing file is left alone, unless `--link` moves it aside to
`.bak` and symlinks the checkout copy in its place.

A running proxy is reused as it is, and flags on a later command line are
ignored. Change a flag and you must restart, or you are measuring the old one.

## Operating

```bash
restart-headroom.sh              # swap in a freshly built binary and restart
tail -f ~/headroom-proxy.log     # JSON lines
```

The restart script backs up the live binary first and rolls back if the new one
fails to come up.

| Endpoint | What it gives you |
| --- | --- |
| `/healthz` | Liveness. |
| `/cache-health` | Hit rates and recent cache busts. |
| `/stats` | JSON counters, including the savings verdict above. |
| `/metrics` | Prometheus. |

Counters reset on restart. The savings ledger on disk does not, and the CLI
reads it: `headroom savings` for savings over time, `headroom doctor` for
liveness and ledger health, `headroom ctx search` for captured context.

## Repository layout

| Path | What is in it |
| --- | --- |
| `crates/headroom-proxy` | The proxy, the `headroom` CLI, and the cache stabilizers. This is the product. |
| `crates/headroom-core` | Compression, the context store, memory. |
| `crates/headroom-parity` | Checks Rust output against the Python implementation. |
| `crates/headroom-simulators` | Traffic simulators for benchmarks. |
| `crates/headroom-py` | A PyO3 extension module, built with maturin rather than cargo. |
| `contrib/` | The launcher, the restart script, the statuslines, the flag file. `contrib/claude/` holds the subagent definitions and the CLAUDE.md excerpt. |
| `docs/` | Reference docs, including [`flags.md`](docs/flags.md) and [`measurement.md`](docs/measurement.md). |
| `docs/notes/` | Working notes and measurement logs. Not onboarding material, and parts go stale. |

The Python tree (`headroom/`, `tests/`, `sdk/`, `plugins/`) is upstream's
original: inert, not part of the Rust build, kept so upstream diffs stay
readable when porting.

## Building and testing

`rust-toolchain.toml` pins 1.95.0 so a clippy lint added in a newer stable
cannot break CI without firing locally.

```bash
cargo build --release -p headroom-proxy   # both binaries
make test                                 # cargo test --workspace
make ci-precheck                          # fmt, clippy, tests, the whole gate
```

Run `make ci-precheck` before pushing. It runs what CI runs. `make help` lists
the rest, of which `make fmt` and `make test-parity` are the useful ones.

## Status

Daily-driven by one person on one machine. That is the whole test population.
Linux and WSL2 are the primary targets, and `install.sh` supports macOS but sees
less use there. No releases yet, so build from a checkout and expect flags to
move.

The default `ml` feature loads ONNX Runtime for file type detection and
embeddings, and needs AVX2 on x86. `install.sh` looks for it at `ORT_DYLIB_PATH`
and then as a python3 `onnxruntime` import, and prints a hint if neither works.
Without it the proxy still starts and those transforms turn off with a warning.

## Credits

Most of this code is not original work.

- **[Headroom](https://github.com/headroomlabs-ai/headroom)** by chopratejas,
  the project this forks. The compression pipeline, the proxy architecture, the
  MCP server, and the Python implementation the Rust port was written against
  are all theirs. Apache 2.0.
- **[context-mode](https://github.com/mksglu/context-mode)** by mksglu, the
  context capture and retrieval work behind `headroom ctx` and the context
  store. The `context-mode/` directory here comes from it.
- **[rtk](https://github.com/rtk-ai/rtk)** by rtk-ai, a CLI proxy that filters
  command output before it reaches the model. It handles the other half of the
  problem, trimming at the source rather than in the request.

If you use this, use theirs too.

## License

Apache 2.0, inherited from upstream. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

# Headroom (Rust)

A local proxy that sits between Claude Code and the model API, cuts what gets
sent, and keeps the prompt cache intact while doing it. It also routes Codex
models through the same Claude Code session, so one conversation, one memory,
one set of tools.

This is a fork of [chopratejas/headroom](https://github.com/chopratejas/headroom)
focused on the Rust binaries. The upstream README, which covers the Python
package, the MCP server and the hosted docs, is kept here as
[`README.upstream.md`](README.upstream.md).

## What you get

- **A proxy** (`headroom-proxy`) on `127.0.0.1:8787`. Claude Code talks to it
  instead of `api.anthropic.com`. No code changes, no plugin.
- **Context compression** — tool output, logs, file reads and conversation
  history are shrunk before they leave the machine. Originals stay retrievable.
- **Cache stabilization** — the proxy owns the `cache_control` breakpoints and
  replays the exact prefix it forwarded last turn, so rewriting history does not
  cost you the cached prefix.
- **Codex inside Claude Code** — `--extra-model-route` maps model names like
  `claude-codex-5.6-luna` onto OpenAI models. Same session, same memory, same
  tools. When the Claude window runs out, the conversation carries on.
- **Cross-agent memory** — one store shared by Claude and Codex.
- **A CLI** (`headroom`) for savings, health, context search and log analysis.

## Quick start

Build both binaries and put them on your PATH:

```bash
cargo build --release -p headroom-proxy
install -m755 target/release/headroom-proxy target/release/headroom ~/.local/bin/
```

Then launch Claude Code through the wrapper instead of `claude`:

```bash
cclaude
```

`cclaude` starts the proxy if nothing holds port 8787, reuses it if something
does, and passes Claude Code the environment it needs. To go back, run `claude`
and stop the proxy with `pkill -f headroom-proxy`.

There are no prebuilt binaries yet. Building needs a Rust toolchain and takes
about two and a half minutes on a warm cache.

## What it actually saves

Measured on one developer's traffic, 2026-08-12. Run the same commands and you
get your own figures — none of this is a projection.

```
$ headroom savings
Compression reduction on saving turns
Scope: pre-compression input selected by transforms; not all provider input.
Today        ███████████░░░░░  68.6%  saved 9,708,856 / 14,148,393 selected tokens  $137.21
Last 7 days  ████░░░░░░░░░░░░  24.3%  saved 100,975,119 / 415,353,531 selected tokens  $721.25
Last 30 days ████░░░░░░░░░░░░  24.5%  saved 102,139,076 / 417,154,366 selected tokens  $723.84
```

**24.3% of the input selected on saving turns was removed before forwarding**
over a week. This is transform efficiency, not a share of all provider input:
the ledger records successful compression events and its denominator is those
events' pre-compression input. Daily figures swing hard with what you are doing
— a day of large tool output reads far higher than a day of conversation — so
judge it over a week, not an afternoon. Use `/stats`'s `savings_verdict` for
compression minus cache busts and `wire_verdict` for the provider-reconciled
whole-request view.

The dollar column is a counterfactual estimate, not a provider bill. Proxy
events written after the 2026-08-12 cache-placement fix use the measured
fresh-input or cache-read rate for that turn. Older ledger rows assumed every
saved token was fresh input and are not retroactively repriced because they do
not contain the placement needed to do that honestly.

The proxy also causes some cache misses of its own, and it counts them against
itself. From `/stats`:

```
savings_verdict: saved 100,847,379 − lost to cache busts 31,923,043 (625 busts)
                 = net 68,924,336 tokens, against 414,037,302 attempted
```

That is **1.20x the work per token spent**, after the tool pays for its own
mistakes. It is the number to quote to anyone sceptical, because it is the one
that could have come out negative.

Overhead is small enough to ignore: across 25,708 requests the proxy added
566,855 bytes and removed 199,499,200 — a net 7.7 KB off every request.

## How it works

Claude Code sends a request. The proxy:

1. Compresses tool results, file reads and search output, keeping a retrievable
   copy of anything it shrinks.
2. Replays the prefix it forwarded on the previous turn, byte for byte, so the
   provider's cached prefix still matches after compression rewrote history.
3. Places its own `cache_control` breakpoints on the tail, bounded to stay under
   the provider's limit of four.
4. Forwards to `api.anthropic.com`, or to OpenAI for a routed model name.

Steps 2 and 3 matter more than step 1 for a coding agent. Compression that busts
the prompt cache costs more than it saves, which is why the proxy tracks its own
busts and reports them.

## Configuration

The launcher sources `~/.headroom-flags.sh`, which holds the measured flag set:

```bash
--compression-mode all_messages     # compress the whole conversation, not just the tail
--prefix-replay true                # replay the previously forwarded prefix
--cache-tail-breakpoints 2          # two tail markers beat one by ~5% of the bill
--force-1h-cache-ttl true
--enable-cross-turn-dedup
--memory true
```

Two entries in that file still carry absolute paths (`--ctx-store-dir` and
`--codex-auth-file`); change them for your machine. `--prune-drop-mcp` names the
MCP servers whose tool definitions get dropped from the request — it is personal
to your setup, and uninstalling the servers you do not use works as well.

A running proxy is reused as-is, and flags on a later command line are dropped
when that happens. If you change flags, restart the proxy or you will be
measuring the old ones.

## Commands

```bash
headroom savings          # durable token and cost savings over time
headroom doctor           # proxy liveness and local ledger health
headroom ctx search "..." # search captured conversation context
headroom perf             # analyze proxy performance from logs
```

The proxy also serves `/stats` (JSON, includes the savings verdict above) and
`/metrics` (Prometheus). Both reset when the process restarts; the savings ledger
on disk does not.

## Building and testing

```bash
cargo build --release -p headroom-proxy   # both binaries
cargo test -p headroom-proxy              # unit tests
cargo test -p headroom-proxy --tests      # integration tests
```

The workspace holds five crates: `headroom-core` (compression, context store,
memory), `headroom-proxy` (the proxy, the CLI and cache stabilization),
`headroom-parity` (checks Rust output against the Python implementation),
`headroom-simulators` and `headroom-py` (a PyO3 extension module, built with
maturin rather than plain cargo).

The Python implementation still lives in `headroom/`. It is not part of the Rust
build and is not shipped with these binaries.

## Credits

Most of this code is not original work.

- **[Headroom](https://github.com/chopratejas/headroom)** by chopratejas — the
  project this forks. The compression pipeline, the proxy architecture, the MCP
  server and the Python implementation the Rust port was written against are all
  theirs. Apache 2.0.
- **[context-mode](https://github.com/mksglu/context-mode)** by mksglu — the
  context capture and retrieval work behind `headroom ctx` and the context store.
  The `context-mode/` directory here comes from it.
- **[rtk](https://github.com/rtk-ai/rtk)** by rtk-ai — a CLI proxy that filters
  and summarizes command output before it reaches the model. It handles the
  other half of the problem: trimming tool output at the source rather than in
  the request.

If you use this, use theirs too.

## License

Apache 2.0, inherited from upstream. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

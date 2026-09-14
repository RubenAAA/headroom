# CLI Reference

!!! note "Live implementation: Rust"
    The production CLI is the Rust binary (`crates/headroom-proxy`, launched with `cclaude`). Python paths on this page now live in the read-only `upstream-python/` mirror — re-resolve any `headroom/*.py` cite there. Behavior described here still holds; only the implementation moved.

This page is the authoritative reference for the **Rust CLIs built in this repo**:

- `headroom` — context-mode client plus local analysis/report commands
  (`crates/headroom-proxy/src/bin/headroom_cli.rs`).
- `headroom-proxy` — the transparent reverse proxy
  (`crates/headroom-proxy/src/main.rs`, options defined in
  `crates/headroom-proxy/src/config.rs` as `CliArgs`).

Verify anything here against the built binaries:

```bash
./target/release/headroom --help
./target/release/headroom <command> --help
./target/release/headroom <group> <subcommand> --help
./target/release/headroom-proxy --help
```

The Python `headroom` console script (`python -m headroom.cli`) is **not**
built here. It lives in the read-only `upstream-python/` mirror
(`upstream-python/headroom/cli/`). Python-only command paths are listed under
[Python-only paths removed](#python-only-paths-removed-from-this-page) and are
otherwise not documented on this page.

## Global behavior

### Entry points

| Entry point | Source | Purpose |
|---|---|---|
| `headroom` | `crates/headroom-proxy/src/bin/headroom_cli.rs` | Context-mode operations (`ctx`), savings/perf/doctor reports, bundled-tool passthroughs |
| `headroom-proxy` | `crates/headroom-proxy/src/main.rs` | Run the transparent reverse proxy |
| `python -m headroom.cli` | `upstream-python/headroom/cli/main.py` (mirror only) | Legacy Python CLI; not built or installed from this tree |

### Global options

`headroom` has one global flag plus help. It takes no `--version` flag
(`headroom --version` errors); the proxy reports its own version.

| Option | Scope | Default | Meaning |
|---|---|---|---|
| `--proxy-url <URL>` | `headroom` root (inherited by `ctx` subcommands) | `http://127.0.0.1:8787` (env: `HEADROOM_PROXY_URL`) | Proxy base URL for `ctx search/get/index/fetch/stats` and the `doctor` health probe |
| `-h`, `--help` | `headroom` root, groups, commands; `headroom-proxy` | — | Show help and exit |
| `-V`, `--version` | `headroom-proxy` only | — | Show the proxy version and exit (currently `0.1.0`) |

There is no `-?` help alias and no root `-v`/`--version` on `headroom`.
There is no `headroom proxy` subcommand: the proxy is the separate
`headroom-proxy` binary and requires `--upstream`.

## Command index

All `headroom` top-level commands (from `headroom --help` on the built binary):

| Command | Purpose | Needs proxy? |
|---|---|---|
| `headroom ctx ...` | Context-mode operations: search/get/index/fetch/stats | yes (except `ctx index -` reads stdin locally, then POSTs) |
| `headroom agent-savings` | Render or verify Codex/Claude/Cursor token-savings settings | only with `--check-perf` |
| `headroom capture ...` | Capture/network-diff investigation tooling | no |
| `headroom copilot-auth ...` | Manage Headroom's GitHub Copilot OAuth token | no |
| `headroom output-savings` | Show estimated/measured output-token reduction from the shaper | no (reads the ledger from disk) |
| `headroom perf` | Analyze proxy performance from logs | no (reads log files) |
| `headroom doctor` | Reduced Rust health check for proxy liveness and local ledgers | probes proxy, exits non-zero when unreachable |
| `headroom savings` | Show durable compression savings over time | no (reads the ledger from disk) |
| `headroom sg ...` | Run ast-grep (AST-aware structural search/replace, passthrough) | no |
| `headroom diff ...` | Run difftastic (structural diff, passthrough) | no |
| `headroom loc ...` | Run scc (fast lines-of-code / repo-shape probe, passthrough) | no |
| `headroom tools ...` | Manage bundled CLI tool binaries (`list`/`doctor`/`install`) | no |

## Captured `--help` output

Captured from the built Rust binary (`./target/release/headroom --help`):

```text
Headroom context-mode CLI

Usage: headroom [OPTIONS] <COMMAND>

Commands:
  ctx             Context-mode operations
  agent-savings   Render or verify Codex/Claude/Cursor token-savings settings
  capture         Capture and compare network traffic for Headroom investigations
  copilot-auth    Manage Headroom's GitHub Copilot OAuth token
  output-savings  Show estimated/measured output-token reduction from the shaper
  perf            Analyze proxy performance from logs
  doctor          Run a reduced Rust health check for proxy liveness and local ledgers
  savings         Show durable compression savings over time
  sg              Run ast-grep (AST-aware structural search/replace)
  diff            Run difftastic (structural diff)
  loc             Run scc (fast lines-of-code / repo-shape probe)
  tools           Manage bundled CLI tool binaries
  help            Print this message or the help of the given subcommand(s)

Options:
      --proxy-url <PROXY_URL>  Proxy URL (default: http://127.0.0.1:8787) [env: HEADROOM_PROXY_URL=] [default: http://127.0.0.1:8787]
  -h, --help                   Print help
```

## `headroom ctx`

Context-mode operations. Requires the proxy to run with `--ctx-offload`.
The CLI sends the current working directory as `x-headroom-cwd` so search
hits the project it was run from.

```bash
headroom ctx search "<query>" --sort relevance
headroom ctx get <hash>
headroom ctx index <path>
headroom ctx fetch https://example.com/page
headroom ctx stats
```

| Subcommand | Arguments / options | Defaults | Meaning |
|---|---|---|---|
| `search <QUERY>` | `QUERY` (required); `--sort relevance\|timeline`; `--source <label>`; `--type code\|prose` | `--sort relevance`; `--source`/`--type` unset | Search the content index (`GET /ctx/search`) |
| `get <HASH>` | `HASH` (required, blake3 hash) | — | Retrieve an offloaded original (`GET /ctx/get/<hash>`); prints content directly for piping |
| `index <PATH>` | `PATH` (required, file path or `-` for stdin); `--label <label>` | `--label` defaults to filename (`stdin` for `-`) | Index content (`POST /ctx/index`) |
| `fetch <URL>` | `URL` (required); `--source <label>`; `--force`; `--ttl <seconds>` | `--force` off; `--ttl`/`--source` unset (server applies its 86400s cache TTL) | Fetch a URL, convert to markdown, index (`POST /ctx/fetch`) |
| `stats` | none | — | Show offload/search statistics (`GET /ctx/stats`) |

All five subcommands honor the root `--proxy-url` / `HEADROOM_PROXY_URL`.

See also: context-mode proxy flags (`--ctx-offload`, `--ctx-store-dir`,
`--ctx-capture`) under [`headroom-proxy`](#headroom-proxy).

## `headroom agent-savings`

Render or verify Codex/Claude/Cursor token-savings settings
(`crates/headroom-proxy/src/bin/headroom_cli/agent_savings.rs`).

```bash
headroom agent-savings
headroom agent-savings --format json
headroom agent-savings --check-perf --hours 24
headroom agent-savings --check-perf --hours 0 --require-agents claude,codex,cursor --accuracy-report eval.json
```

| Option | Default | Meaning |
|---|---|---|
| `--profile` | `agent-90` | Savings profile to render or check |
| `--format` | `shell` (`shell`\|`json`) | Output format for the profile environment |
| `--check-perf` | off | Check recent proxy logs against the profile savings target |
| `--hours` | `24` (`0` = all data; must be `>= 0`) | Hours of proxy logs to inspect with `--check-perf` |
| `--accuracy-report` | unset | Headroom eval JSON report proving accuracy preservation |
| `--write-smoke-fixture` | unset | Write deterministic three-agent PERF/eval fixture into the given workspace dir |
| `--require-agents` | `""` | Comma-separated clients that must each meet the savings target |
| `--min-accuracy` | `0.9` | Minimum accepted accuracy preservation rate |

Without `--check-perf`/`--accuracy-report`/`--write-smoke-fixture`, the command
renders the profile environment (`export KEY="value"` per line, or sorted
pretty JSON with `--format json`).

## `headroom capture`

Capture and compare network traffic for Headroom investigations. The only
subcommand is `network-diff`
(`crates/headroom-proxy/src/bin/headroom_cli/network_diff.rs`).

```bash
headroom capture network-diff --direct direct.jsonl --headroom proxied.jsonl
headroom capture network-diff --direct a.jsonl --headroom b.jsonl --output report.md --json-output diff.json
```

| Option | Default | Meaning |
|---|---|---|
| `--direct` | required | JSONL capture from the direct Claude Code lane |
| `--headroom` | required | JSONL capture from the Headroom-proxied Claude Code lane |
| `--output` | stdout | Write a Markdown report to this path |
| `--json-output` | unset | Optional machine-readable JSON diff output path |
| `--pair-by` | `path` (`path`\|`route`) | Pair exchanges by method+path or by method+host+path |

## `headroom copilot-auth`

Manage Headroom's GitHub Copilot OAuth token
(`crates/headroom-proxy/src/bin/headroom_cli/copilot_auth.rs`).

```bash
headroom copilot-auth login
headroom copilot-auth login --domain github.com
headroom copilot-auth status
```

| Subcommand | Options | Meaning |
|---|---|---|
| `login` | `--domain` (default `github.com`) | Sign in with GitHub's Copilot OAuth device-code flow; prints the verification URI and code, then saves the token |
| `status` | none | Show the auth file path and whether a token is saved (with fingerprint) |

`login --domain` accepts a custom hostname only for GitHub Enterprise Server;
use `github.com` for GitHub.com / Enterprise Cloud.

## `headroom output-savings`

Show estimated/measured output-token reduction from the shaper. Takes no
options.

```bash
headroom output-savings
```

Reads the shaper savings ledger from disk (no proxy needed). When the ledger
path does not exist it prints the seed hint (`learn --verbosity --apply`, then
`HEADROOM_OUTPUT_SHAPER=1`). The active verbosity level resolves from
`HEADROOM_OUTPUT_SHAPER` + `HEADROOM_VERBOSITY_LEVEL` (default `2` when the
shaper is on, clamped to `0..=4`); a level with no measured factors renders the
honest empty state rather than a guess. A modelled band is labelled `(range …)`,
not a CI; measured/estimated tiers report a `95% CI`.

## `headroom perf`

Analyze proxy performance from logs.

```bash
headroom perf
headroom perf --hours 24
headroom perf --raw
headroom perf --format json
headroom perf --raw --format csv
```

| Option | Default | Meaning |
|---|---|---|
| `--hours` | `168` (`0` = all data; must be `>= 0`) | Analyze logs from the last N hours (168 = 7 days) |
| `--raw` | off | Show raw PERF records instead of the summarized report |
| `--format` | `text` (`text`\|`json`\|`csv`) | Output format; `json`/`csv` emit machine-readable data |

`--format csv` emits the PERF record table for `--raw`, else the per-model
breakdown. With no records, the text report tells you to run the proxy first.

## `headroom doctor`

Run a reduced Rust health check for proxy liveness and local ledgers. This is
intentionally narrower than the legacy Python `doctor`: it checks proxy
reachability plus workspace/config/ledger/auth paths, not installer manifests
or per-client wrap config.

```bash
headroom doctor
headroom doctor --json
```

| Option | Default | Meaning |
|---|---|---|
| `--json` | off | Emit JSON instead of text |

Probes `<proxy-url>/healthz`, falling back to `<proxy-url>/livez` only on a
404 (legacy Python proxy). The proxy URL comes from the root `--proxy-url` /
`HEADROOM_PROXY_URL`. Exits non-zero when the proxy is unreachable.

## `headroom savings`

Show durable compression savings over time (reads the append-only savings
ledger from disk; aggregated on read).

```bash
headroom savings
headroom savings --days 7
headroom savings --json
headroom savings --reset
```

| Option | Default | Meaning |
|---|---|---|
| `--json` | off | Emit the raw report as JSON |
| `--days` | `30` (1 or more) | Retention/lookback window for the ledger, in days |
| `--reset` | off | Delete the savings ledger and start fresh |

With no calls recorded, the command prints the empty-ledger hint and the
ledger path instead of a zero row.

## `headroom sg` / `headroom diff` / `headroom loc`

Bundled-tool passthroughs. Every argument forwards verbatim (`exec`) to the
resolved tool binary, so `headroom sg --help` shows ast-grep help, not
Headroom help (help flag disabled on the wrapper).

| Command | Tool | Purpose |
|---|---|---|
| `headroom sg ...` | `ast-grep` | AST-aware structural search/replace |
| `headroom diff ...` | `difft` (difftastic) | Structural diff that understands syntax |
| `headroom loc ...` | `scc` | Fast lines-of-code / repo-shape probe |

```bash
headroom sg --help
headroom diff old.rs new.rs
headroom loc ./crates
```

Use `headroom tools doctor` / `headroom tools install` when a passthrough
binary is missing.

## `headroom tools`

Manage bundled CLI tool binaries
(`crates/headroom-proxy/src/bin/headroom_cli/tools.rs`; registry mirrored
from `upstream-python/headroom/tools.json`).

```bash
headroom tools list
headroom tools doctor
headroom tools doctor --json
headroom tools install
headroom tools install --tool ast-grep --tool difft --force
```

| Subcommand | Options | Meaning |
|---|---|---|
| `list` | none | Print the tool registry (platform, cache dir, versions, sources) |
| `doctor` | `--json` (off) | Check the status of every bundled tool; exits non-zero when any tool is `missing` / `unsupported-platform` |
| `install` | `--tool <name>` (repeatable, default: all); `--force` (off) | Pre-fetch binaries into the per-user cache; `--force` re-fetches even when cached |

Related environment overrides: `HEADROOM_BINARIES_MIRROR`,
`HEADROOM_BINARIES_CACHE`, `HEADROOM_BINARIES_OFFLINE`.

## `headroom-proxy`

The proxy is a separate binary, not a `headroom` subcommand. `--upstream` is
required (no default); `--listen` defaults to `0.0.0.0:8787`.

```bash
headroom-proxy --upstream http://127.0.0.1:8788
headroom-proxy --upstream https://api.anthropic.com --listen 127.0.0.1:8787 --compression
headroom-proxy --version
```

### Full flag reference

`headroom-proxy --help` currently lists **128 `--` flags** (plus `-h/--help`
and `-V/--version`). The generated reference is authoritative — this page
groups the surface instead of copying it:

- [`docs/flags.md`](../docs/flags.md) — every option with env var and default,
  generated from the binary. Regenerate after adding/renaming a flag (see the
  header of that file).
- Flag definition: `crates/headroom-proxy/src/config.rs` (`CliArgs`).
- Binary entry: `crates/headroom-proxy/src/main.rs`.
- Maintainer runtime set: `contrib/headroom-flags.sh` (copied to
  `~/.headroom-flags.sh` by `install.sh`).

There is no config file. Each flag also names its `HEADROOM_*` / provider env
var in `--help`; the CLI flag wins over the environment.

### Major option groups

Defaults below are from `headroom-proxy --help` / `docs/flags.md`. Every flag
named here was verified there.

| Group | Representative flags (defaults) | Notes |
|---|---|---|
| Core networking | `--listen` (`0.0.0.0:8787`, env `HEADROOM_PROXY_LISTEN`), `--upstream` (required, env `HEADROOM_PROXY_UPSTREAM`), `--upstream-timeout` (`600s`), `--upstream-connect-timeout` (`10s`), `--upstream-write-timeout` (`150s`), `--pool-idle-timeout` (`90s`), `--http-proxy` (unset), `--max-body-bytes` (`100MB`), `--log-level` (`info`), `--rewrite-host` (`true`) / `--no-rewrite-host`, `--graceful-shutdown-timeout` (`30s`) | `--upstream-write-timeout` bounds pushing request bytes upstream; `--pool-idle-timeout` bounds idle keepalive reuse. `--http-proxy` applies to provider calls only, not the process environment |
| Rollout | `--rollout-channel` (`stable`), `--features` (`""`), `--disable-features` (`""`), `--unsafe-allow-unstable-features` (`false`) | Disable wins over enable; the unsafe override is break-glass only |
| Compression master | `--compression` (off, env `HEADROOM_PROXY_COMPRESSION`), `--compression-mode` (unset → resolved to `all_messages` when interception is on, else `off`; `off`/`live_zone`/`all_messages`), `--compression-max-body-bytes` (unset → falls back to `--max-body-bytes`), `--compression-max-workers` (`4`), `--enable-cross-turn-dedup` (off) | With `--compression` off the proxy is a byte-pipe and never mutates headers |
| Run mode | `--mode` (`token`, env `HEADROOM_MODE`; `token` prioritizes compression, `cache` prioritizes prefix-cache stability) | Legacy Python aliases are not accepted here; see `--help` for the exact choice set |
| Cache stabilization | `--prefix-replay` (`false`), `--cache-tail-breakpoints` (`1`), `--cache-tail-breakpoint` (`true`), `--strip-system-cache-breakpoints` (`false`), `--cache-stable-tool-order` (`true`), `--cache-pin-tool-roster` (`false`), `--cache-control-auto-frozen` (`enabled`), `--auth-mode-policy-enforcement` (`enabled`), `--beta-header-sticky` (`enabled`), `--strip-internal-headers` (`enabled`), `--force-1h-cache-ttl` (`false`), `--split-cache-ttl` (`false`), `--respect-client-5m-ttl` (`false`), `--replay-store-dir` (`""`), `--hold-working-directory` / `--hold-role-sentence` (`false`), `--max-conversation-concurrency` (`0` = unbounded) | Freeze-replay and breakpoint placement keep the forwarded prefix byte-stable so provider prompt caches hold across turns |
| Context offload / recall (ctx) | `--ctx-capture` (`false`), `--ctx-store-dir` (unset → `<workspace>/ctx`), `--ctx-offload` (`false`), `--ctx-offload-min-bytes` (`50000`), `--ctx-offload-stale-messages` / `--ctx-offload-stale-window` (`0`), `--ctx-offload-ttl-seconds` (`604800`), `--ctx-offload-tool-use` (`false`), `--ctx-inject` (`false`), `--ctx-drop-prior-thinking` (`true`), `--max-injection-bytes` (`32768`), `--ccr-context-tracking` / `--ccr-proactive-expansion` (`true`), `--ccr-max-proactive-expansions` (`2`), `--ccr-inject-tool` / `--ccr-handle-responses` (`true`), `--ccr-max-retrieval-rounds` (`8`), `--ccr-inject-marker` (`true`) | Backs `headroom ctx *`; `--ctx-offload` replaces oversized `tool_result` blocks with retrievable pointers |
| Context editing | `--context-edit` (off), `--context-edit-keep-tool-uses` (`6`), `--context-edit-trigger-tokens` (`60000`), `--context-edit-min-messages` (`40`), `--context-edit-clear-at-least` (unset), `--context-edit-keep-thinking` (unset) | Anthropic-native `context_management` (`clear_tool_uses` / `clear_thinking`); off by default |
| Tool pruning | `--prune-drop-mcp` / `--prune-drop-tools` / `--prune-keep-tools` (all unset) | Deterministic, cache-safe; keep-allowlist wins over drops |
| Semantic cache / retry / budget | `--cache` (`true`), `--cache-ttl` (`3600`), `--cache-max-entries` (`1000`), `--retry` (`true`), `--retry-max-attempts` (`3`), `--retry-overload-max-attempts` (`6`), `--retry-stream-hold-bytes` (`8192`), `--retry-base-delay-ms` (`1000`), `--retry-max-delay-ms` (`30000`), `--cost-tracking` (`true`), `--budget-limit-usd` (unset = unlimited), `--budget-period` (`daily`), `--min-tokens-to-crush` (`200`), `--max-items-after-crush` (`15`), `--savings-profile` (`balanced`), `--target-ratio` (`0` = auto) | `--retry-stream-hold-bytes 0` disables the holdback |
| Provider routing | `--bedrock-region` (`us-east-1`), `--bedrock-endpoint` (unset → derived), `--aws-profile` (unset), `--bedrock-validate-eventstream-crc` (`true`), `--vertex-region` (`us-central1`), `--vertex-adc-scope` (`cloud-platform`), `--local-model` / `--local-upstream` (unset; upstream required when model is set), `--sidecar-model` (unset), `--sidecar-route-timeout` (`15s`), `--extra-model-route` (repeatable `MODEL=URL…`), `--codex-auth-file` (unset → `~/.codex/auth.json`), `--foundry-base-url` / `--foundry-resource` (unset), `--cursor-agent-binary` (`agent`), `--enable-responses-streaming` / `--enable-conversations-passthrough` / `--enable-bedrock-native` (`true`), `--enable-batch-api` (`false`) | `--local-model` translates Anthropic↔OpenAI at `--local-upstream`; `--extra-model-route` adds `MODEL=UPSTREAM[:openai[:TARGET]][:auth=ENV]` routes |
| Compression content gates | `--code-aware` (`false`), `--enable-kompress` (`false`), `--disable-kompress` (`true`), `--disable-kompress-fallback` (`true`), `--disable-kompress-anthropic` / `--disable-kompress-openai` (`false`), `--force-kompress-all` (`false`), `--image-optimize` (`true`), `--smart-crusher-compaction` (`true`), `--compress-user-messages` / `--compress-system-messages` (`true`, currently not wired), `--protect-recent` / `--protect-analysis-context` (`false`), `--accuracy-guard` (`""`), `--lossless` (`false`), `--exclude-tools` (default `Read,Glob,Grep,Write,Edit,WebSearch,WebFetch,view,read_file,Skill,headroom_retrieve`), `--protect-tool-results` (`""`), `--read-lifecycle` / `--read-maturation` (`false`) | `--exclude-tools ""` compresses everything; the two `compress-*-messages` gates are documented no-ops in `--help` |
| Safety / operator | `--memory` (`false`, env `HEADROOM_MEMORY_ENABLED`), `--output-shaper` (unset, env `HEADROOM_OUTPUT_SHAPER`), `--verbosity-level` (`2`), `--redact-sensitive` (`false`), `--stateless` (`false`), `--offline` (`false`), `--proxy-token` (unset), `--anthropic-pre-upstream-concurrency` (`1000`) | `--stateless` disables filesystem writes; `--offline` disables outbound egress |

See also: [Proxy Server](proxy.md), [Configuration](configuration.md).

## Python-only paths removed from this page

The previous revision of this page documented the Python CLI (15 of the 30
`headroom --help` entries, ~30 of the ~90 `headroom proxy --help` options).
None of the paths below exists in the Rust `headroom` binary (verified against
`headroom --help` and `crates/headroom-proxy/src/bin/headroom_cli.rs`); the
proxy rows no longer exist as `headroom <subcommand>` at all because the proxy
is the separate `headroom-proxy` binary (128 flags, see above). They live on in
the read-only mirror and are intentionally undocumented here:

- `headroom proxy` (incl. `--host/--port/--mode/--no-optimize/--no-cache/--memory/--backend/--region…`) → `upstream-python/headroom/cli/proxy.py`; replaced by `headroom-proxy --upstream … --listen …` plus [`docs/flags.md`](../docs/flags.md).
- `headroom dashboard` → `upstream-python/headroom/cli/proxy.py`; no Rust equivalent.
- `headroom learn` → `upstream-python/headroom/cli/learn.py`; no Rust equivalent.
- `headroom inspect` → `upstream-python/headroom/cli/inspect.py`; no Rust equivalent (closest read-only views are `ctx stats`, `perf`, `savings`).
- `headroom evals` (`memory`, `memory-v2`, `probes`, `adversarial`; hidden `memory-eval`/`memory-eval-v2` shims) → `upstream-python/headroom/cli/evals.py`; no Rust equivalent.
- `headroom memory` (`list/show/stats/edit/repair-supersession/delete/prune/purge/reindex/export/import`) → `upstream-python/headroom/cli/memory.py`; no Rust equivalent.
- `headroom mcp` (`install/uninstall/reconcile/status/serve`) → `upstream-python/headroom/cli/mcp.py`; no Rust equivalent.
- `headroom install` (`apply/status/start/stop/restart/remove`) and `headroom deploy` alias → `upstream-python/headroom/cli/install.py`; no Rust equivalent.
- `headroom init` (`claude/copilot/codex/openclaw`, hook `ensure`) → `upstream-python/headroom/cli/init.py`; no Rust equivalent.
- `headroom wrap` / `headroom unwrap` (claude/copilot/codex/aider/cursor/openclaw/vscode/…) → `upstream-python/headroom/cli/wrap.py`; no Rust equivalent.
- `headroom audit-reads` → `upstream-python/headroom/cli/audit.py`; no Rust equivalent.
- `headroom recover` (`codex`) → `upstream-python/headroom/cli/recover.py`; no Rust equivalent.
- `headroom rollout status` → `upstream-python/headroom/cli/rollout.py`; the runtime knobs survive as `headroom-proxy --rollout-channel/--features/--disable-features/--unsafe-allow-unstable-features`.
- `headroom update` → `upstream-python/headroom/cli/update.py`; no Rust equivalent.
- Hidden `--prepare-only` wrap flags and the Docker-native parity matrix → Python/Docker install flows; no Rust equivalent (the Rust `doctor` is the reduced local check documented above).

If you need one of these, run it from an installed `headroom-ai` Python
distribution and treat `upstream-python/` as the source of truth — do not mix
its flags (e.g. `headroom proxy --port`, `wrap --port`, `install apply
--preset`) with the Rust binaries on this page.

## See also

- [Proxy Server](proxy.md)
- [Configuration](configuration.md)
- [Filesystem Contract](filesystem-contract.md)
- [`docs/flags.md`](../docs/flags.md) — authoritative `headroom-proxy` flag list

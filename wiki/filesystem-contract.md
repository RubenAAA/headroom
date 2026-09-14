# Filesystem Contract

!!! note "Live implementation: Rust"
    The production proxy and CLI are the Rust binaries (`crates/headroom-proxy`, installed to `~/.local/bin`). The Python package this page used to cite (`headroom/proxy/server.py` and friends) no longer exists at the repo root — its last copy is the read-only `upstream-python/` mirror. Re-resolve any `headroom/*.py` cite there. The two-root bucket model below still holds; it is implemented in `crates/headroom-core/src/paths.rs`.

Headroom writes configuration, runtime state, logs, and caches to a small
set of well-known paths under the user's home directory. This page is the
source of truth for where those paths live, how to override them, which
historical paths are retired, and what the installer owns.

## Two-root model

| Variable | Default | Purpose | Typical access |
|---|---|---|---|
| `HEADROOM_CONFIG_DIR` | `~/.headroom/config` | User/admin-authored configuration | Read-mostly |
| `HEADROOM_WORKSPACE_DIR` | `~/.headroom` | Runtime state written by the proxy and CLI (savings, logs, store DBs, telemetry, caches) | Read-write |

Both variables are recognized by the Rust proxy / CLI
(`crates/headroom-core/src/paths.rs`). They are **additive** — every
per-resource env var (`HEADROOM_SAVINGS_PATH`,
`HEADROOM_SAVINGS_EVENTS_PATH`, `HEADROOM_TOIN_PATH`,
`HEADROOM_SUBSCRIPTION_STATE_PATH`, `HEADROOM_COPILOT_AUTH_FILE`, ...)
continues to work with identical precedence.

## Precedence

For every per-resource helper, resolution follows this order:

```
explicit argument
    │ falls through when None/""
    ▼
per-resource env var (e.g. HEADROOM_SAVINGS_PATH)
    │ falls through when unset/blank
    ▼
derived from canonical root
    │ e.g. ${HEADROOM_WORKSPACE_DIR}/proxy_savings.json
    ▼
default (e.g. ~/.headroom/proxy_savings.json)
```

Examples:

- `HEADROOM_WORKSPACE_DIR=/mnt/state` → savings land at
  `/mnt/state/proxy_savings.json` unless `HEADROOM_SAVINGS_PATH` overrides.
- `HEADROOM_SAVINGS_PATH=/custom/savings.json` always wins, even when
  `HEADROOM_WORKSPACE_DIR` is set.
- Unset both and the default is `~/.headroom/proxy_savings.json`.

## Bucket assignments

### Workspace bucket (`HEADROOM_WORKSPACE_DIR`)

| Resource | Default path | Override |
|---|---|---|
| Proxy savings ledger | `${WORKSPACE_DIR}/proxy_savings.json` | `HEADROOM_SAVINGS_PATH` |
| Savings event ledger (append-only JSONL) | `${WORKSPACE_DIR}/savings_events.jsonl` | `HEADROOM_SAVINGS_EVENTS_PATH` |
| Savings atomic-write temp files | `${WORKSPACE_DIR}/.proxy_savings_<nanos>.tmp` | — (same dir as the ledger) |
| Output-token savings ledger | `${WORKSPACE_DIR}/output_savings.json` | — (workspace only, no env override) |
| TOIN telemetry JSON | `${WORKSPACE_DIR}/toin.json` | `HEADROOM_TOIN_PATH` |
| Subscription tracker state | `${WORKSPACE_DIR}/subscription_state.json` | `HEADROOM_SUBSCRIPTION_STATE_PATH` |
| Copilot OAuth token cache | `${WORKSPACE_DIR}/copilot_auth.json` | `HEADROOM_COPILOT_AUTH_FILE` |
| Context/store root (sessions, content, memory, CCR) | `${WORKSPACE_DIR}/ctx/` | `--ctx-store-dir` / `HEADROOM_PROXY_CTX_STORE_DIR` |
| Native memory files | `~/.headroom/memories/` (`$HOME`-based; ignores the workspace root) | explicit config only |
| Proxy log directory | `${WORKSPACE_DIR}/logs/` (`proxy.log*`, legacy managed destination — see Logs) | — |

### Store layout under `${WORKSPACE_DIR}/ctx/`

The default base is `~/.headroom/ctx` (`ctx::store::default_base_dir`;
`--ctx-store-dir` / `HEADROOM_PROXY_CTX_STORE_DIR` overrides it). One
registry of per-project stores serves capture, offload, and recall, so all
three read and write the same file for a given project; handles open lazily
on first sight. Everything is keyed by project except the CCR store:

| Path | Content |
|---|---|
| `ctx/content/<hash>.db` | CTX-1 FTS5 content index, one SQLite DB per canonical project-dir hash (`ctx::store::content_db_path`) |
| `ctx/sessions/<hash>.db` | CTX-2 session events, same sharding (`ctx::session_db_path`); the proxy adds a `conv_prefix_chain` table with no TS equivalent |
| `ctx/memory/memories.db` | Memory records (`memory::ctx_backend::CtxMemoryBackend::open`) |
| `ctx/memory/memories_index.db` | Memory FTS index over the same records |
| `ctx/ccr.db` | CTX-3 offload CCR store: originals keyed by `blake3` hash with a long TTL (`ctx::offload_store::OffloadStore::start`; served by `headroom ctx get <hash>`) |
| `ctx/offload-gate/` | `OffloadGate` persistence, beside the originals it refers to |

The schema is byte-compatible with the old TypeScript context-mode store,
so a pre-move store still opens: point `--ctx-store-dir` at
`~/.claude-personal/context-mode` to keep reading it. No migration runs —
a fresh default starts empty.

### Config bucket (`HEADROOM_CONFIG_DIR`)

The root resolution is live (`paths::config_dir`: `$HEADROOM_CONFIG_DIR`,
else `$HEADROOM_WORKSPACE_DIR/config`, else `~/.headroom/config`), but no
Rust code reads per-resource files from it. The former residents —
`models.json` (`HEADROOM_MODEL_LIMITS`) and `plugins/<name>/...` — are
retired; see the retired table below.

## Installer outputs (`install.sh`)

`./install.sh` at the repo root builds `target/release/` and installs:

| Path | Content |
|---|---|
| `~/.local/bin/headroom-proxy` | The proxy binary (`restart-headroom.sh` swaps in fresh builds, keeping `headroom-proxy.prev` for rollback) |
| `~/.local/bin/headroom` | The CLI (savings, agent-savings, ctx, copilot auth, ...) |
| `~/.local/bin/{claude-launcher,restart-headroom.sh,zen-rotate-watch.sh,headroom-rss-sample}` | Launcher, restarter, watchers (copies; symlinks with `--link`) |
| `~/.local/bin/cclaude` | Symlink to `claude-launcher` — the command that starts sessions through the proxy |
| `~/.headroom-flags.sh` | Measured proxy flag set from `contrib/headroom-flags.sh` (symlinked with `--link`; an existing file is left alone). Both starters source it, so a rebooted machine runs the same flags |
| `~/.headroom-paths.sh` | `HEADROOM_REPO` (plus the GNU-tools `PATH` prefix on macOS); sourced by `restart-headroom.sh` |
| `~/.claude/statusline-*.sh`, `~/.claude/agents/`, `~/.claude/hooks/`, `~/.claude/CLAUDE.md` section | Status line, routed-model agents, review hooks |

!!! warning "One binary, one path"
    `install.sh`, `make install-proxy`, and the launcher all write only `~/.local/bin`. A second `headroom-proxy` copy on `PATH` (e.g. `~/.cargo/bin`) shadows it and dies on flags it does not know — the launcher warns when it sees duplicates.

## Logs

| Path | Writer / reader |
|---|---|
| `~/headroom-proxy.log` (+ `.1`–`.4`) | Launcher-owned stdout/stderr of the proxy. `claude-launcher` rolls four generations on start — only when no proxy process holds the port and the file holds a real run — and both starters append here. `HEADROOM_PROXY_LOG_PATH` points the analyzer at a different active log. |
| `${WORKSPACE_DIR}/logs/proxy.log*` | Legacy managed rotating destination. No Rust writer remains, but `headroom perf` and the savings tooling still parse it: `perf_analyzer::collect_log_files` reads every `proxy.log*` plus the launcher log, oldest mtime first. |

## Capture corpus and bench copies

- `HEADROOM_CAPTURE_DIR` (unset by default — `restart-headroom.sh` execs with `env -u`) arms request-body capture for the offload simulator: `$DIR/req-<run_id>-<seq>.json` plus final wire bytes in `$DIR/out/<request_id>.json` (`cache_stabilization::capture`). Re-arm per question into a fresh directory (e.g. `$HOME/headroom-capture-<question>`); bodies only, never headers, and capture never blocks a live request.
- `target/memory_search_bench/memories_index.db` (+ `-wal`) is the `memory_search` bench's copy of the live memory index, so the bench never opens the proxy's file. The source defaults to `~/.claude-personal/context-mode/memory/memories_index.db` (`HEADROOM_MEMORY_INDEX` overrides it; `CARGO_TARGET_DIR` moves the copy).

## Retired paths (read-only mirror in `upstream-python/`)

!!! note "Nothing below is read or written by the Rust proxy/CLI"
    Each row names the last live copy. `upstream-python/` is a read-only mirror for diffing and porting — it is not built or installed.

| Retired path | Lived in | Replacement |
|---|---|---|
| `headroom/proxy/server.py` (project-scoped memory DB default) | Python proxy | FTS memory pair under `ctx/memory/` |
| `headroom/memory/mcp_server.py` (project-scoped memory DB default) | Python memory server | Same |
| `headroom/cli/wrap.py` (project-scoped memory/hook artifacts, per-PID `clients/<port>/` markers) | Python wrapper | `claude-launcher` reuses the running proxy via its health probe; no marker files |
| `~/.headroom/memory.db` (workspace-root default) | Python memory default | `ctx/memory/memories.db` + `memories_index.db` (the `BackendRouter`'s per-scope `memory.db` names are opt-in path fragments, never workspace-rooted) |
| `~/.headroom/ccr_store.db` (compression-store SQLite default) | Python CCR | `ctx/ccr.db` |
| `~/.headroom/models.json` + `HEADROOM_MODEL_LIMITS` | Python providers | No reader in Rust (the config bucket holds no per-resource files) |
| `~/.headroom/config/plugins/...`, `~/.headroom/plugins/...` (`paths.plugin_*`, npm SDK helpers) | Python / SDK | Not ported (`paths.rs` says so in its module doc) |
| `~/.headroom/{session_stats.jsonl,sync_state.json,bridge_state.json,license_cache.json}` | Python telemetry / memory / license | No Rust reader or writer |
| `~/.headroom/logs/debug_400/`, `~/.headroom/.beacon_lock_<port>`, `~/.headroom/deploy/`, `~/.headroom/mcp_installs.json` | Python proxy / installer | No Rust reader or writer (`deploy/` leftovers on disk are inert) |
| `docker/docker-compose.native.yml`, `scripts/install.sh`, Python `install` command | Python Docker story | `upstream-python/docker/docker-compose.native.yml`, `upstream-python/scripts/install.sh`; the live install is `./install.sh` at the repo root |

## Docker naming overlap: `HEADROOM_WORKSPACE` vs `HEADROOM_WORKSPACE_DIR`

Two different variables (upstream's distinction, kept for the compose path):

| Variable | Scope | Meaning |
|---|---|---|
| `HEADROOM_WORKSPACE` | Host-side (Docker) | Directory the compose file bind-mounts as `/workspace` (`upstream-python/docker/docker-compose.native.yml`). |
| `HEADROOM_WORKSPACE_DIR` | State root | Canonical Headroom state root, Rust default `~/.headroom`. |

The repo root has no compose file and `scripts/` holds only `cargo-gc.sh`
— container workflows live in `upstream-python/` and
[docker-install.md](docker-install.md).

## Per-resource env vars

Every variable below keeps the precedence above (explicit argument > env
var > derived root) and, unlike the old Python behavior, the Rust resolver
expands a leading `~` (`paths::resolve` → `expanduser`):

- `HEADROOM_SAVINGS_PATH`
- `HEADROOM_SAVINGS_EVENTS_PATH`
- `HEADROOM_TOIN_PATH`
- `HEADROOM_SUBSCRIPTION_STATE_PATH`
- `HEADROOM_COPILOT_AUTH_FILE`
- `HEADROOM_CONFIG_DIR`, `HEADROOM_WORKSPACE_DIR` (the roots)
- `HEADROOM_PROXY_LOG_PATH` (the analyzer's active log)
- `HEADROOM_PROXY_CTX_STORE_DIR` (plus the `--ctx-store-dir` flag)
- `HEADROOM_CAPTURE_DIR` (capture corpus)
- `HEADROOM_MEMORY_INDEX` (bench source override)
- `HEADROOM_MEMORY_DB_PATH` — still parsed into `MemoryConfig`, but the default FTS backend stores under `ctx/memory/` instead.

## See also

- [configuration.md](configuration.md) — general configuration reference
- [docker-install.md](docker-install.md) — Docker install details
- [persistent-installs.md](persistent-installs.md) — persistent
  deployment profiles
- [memory.md](memory.md) — memory-system paths and project scoping

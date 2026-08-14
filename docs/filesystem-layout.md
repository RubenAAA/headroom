# Where Headroom puts things

Headroom writes to one root. Everything below is either under it or explains
why it is not.

```
HEADROOM_WORKSPACE_DIR   read-write state   default ~/.headroom
HEADROOM_CONFIG_DIR      read-mostly config default ~/.headroom/config
```

`crates/headroom-core/src/paths.rs` owns both, and every per-resource helper
resolves in the same order:

```
explicit argument > per-resource env var > derived from the root > default
```

## The map

| path | written by | survives restart |
|---|---|---|
| `~/.headroom/proxy_savings.json` | savings tracker — lifetime token/cache counters | yes |
| `~/.headroom/savings_events.jsonl` | per-request savings ledger, one line per saving turn | yes |
| `~/.headroom/ctx/sessions/proxy.db` | ctx capture — session events, prefix chain, injection rows | yes |
| `~/.headroom/ctx/content/<hash>.db` | ctx content store — per-project FTS5 recall index | yes |
| `~/.headroom/ctx/memory/memories.db` | memory records (`--memory`) | yes |
| `~/.headroom/ctx/memory/memories_index.db` | memory FTS5 search index | yes |
| `~/.headroom/ccr_store.db` | CCR — offloaded content, retrievable by hash | yes |
| `~/.headroom/config/` | read-mostly config | yes |
| `$HEADROOM_CAPTURE_DIR/req-*.json` | request capture, off unless the var is set | yes |

## What is not ours

| path | owner |
|---|---|
| `~/.claude-personal/context-mode/` | the **context-mode MCP plugin** — its own `sessions/`, `content/`, `ccr.db` |
| `~/.claude-work/`, `~/.claude-personal/` | Claude Code config roots (`CLAUDE_CONFIG_DIR`) |
| `~/.local/state/headroom/proxy.log` | wherever the launcher redirected proxy stdout — not a path headroom chooses |

The proxy log is the one to know: headroom writes to stdout and nothing else,
so the log lives wherever the process was started with it pointed. Read it off
the running process rather than guessing:

```
ls -l /proc/$(pgrep -f headroom-proxy | head -1)/fd/1
```

`~/.headroom/logs/proxy.log` is a path some older launchers used. If it exists
and is stale, that is why — check `/proc` before trusting it.

## The ctx store used to live somewhere else

The ctx content store was ported from the context-mode plugin and kept that
tool's directory, so until this change headroom's sessions DB was written to
`~/.claude-personal/context-mode/sessions/proxy.db` — inside an unrelated
tool's state, beside that tool's own identically-named `sessions/` and
`content/` directories. Two independent programs writing DBs of the same shape
into one tree is a bad place to debug from, and it is not where an operator
looks for headroom's state.

The default is now `<workspace>/ctx`. Schema compatibility with context-mode is
kept — pointing `--ctx-store-dir` at the old directory still works, and is how
you keep an existing store:

```
--ctx-store-dir ~/.claude-personal/context-mode
```

There is no automatic migration. An upgraded proxy that is not given the flag
starts a fresh store: recall history and the injection decisions keyed to it
are left behind, which is safe (a conversation with no prefix history reads as
first sight and re-decides) but does throw away accumulated recall.

## Inspecting the sessions DB

```
sqlite3 ~/.headroom/ctx/sessions/proxy.db \
  "SELECT COUNT(*) FROM conv_injection;
   SELECT COUNT(DISTINCT conv_id) FROM conv_prefix_chain;"
```

Conversations that have a prefix chain but no injection row — what
`ctx_inject_row_miss` warns about:

```
sqlite3 ~/.headroom/ctx/sessions/proxy.db \
  "SELECT COUNT(*) FROM (
     SELECT DISTINCT conv_id FROM conv_prefix_chain
     WHERE conv_id NOT IN (SELECT conv_id FROM conv_injection));"
```

Without the `sqlite3` binary, `python3 -c "import sqlite3; ..."` reads the same
file; open it with `mode=ro` so a running proxy is never blocked.

`session_events` grows without bound — it holds every captured message. Check
its size before assuming a large DB means a leak:

```
sqlite3 ~/.headroom/ctx/sessions/proxy.db \
  "SELECT COUNT(*), SUM(LENGTH(data)) FROM session_events;"
```

# contrib

Everything here runs on your machine, not inside the proxy. Only
[`install.sh`](../install.sh) reads this directory: it copies these files into
`~/.local/bin`, `~/.claude`, or `$HOME`, or symlinks them with `--link` so
editing the checkout edits the live setup. Nothing here is compiled, and
nothing here is on the request path.

## Starting and running the proxy

| File | Installed as | What it does |
| --- | --- | --- |
| `claude-launcher` | `~/.local/bin/claude-launcher`, `cclaude` | Starts the proxy if port 8787 is dead, sets `ANTHROPIC_BASE_URL`, execs `claude`. Handles several profiles and an optional local Qwen. |
| `restart-headroom.sh` | `~/.local/bin/` | Restarts the proxy onto a freshly built binary and rolls back if it fails to come up. Detached, so killing the proxy does not kill the restart. |
| `update-headroom.sh` | `~/.local/bin/` | Pulls the checkout, reinstalls in the last install's mode, restarts the proxy. Run after `git pull`, or instead of it. |
| `headroom-flags.sh` | `~/.headroom-flags.sh` | The flag array both starters read. An existing file is never overwritten, so tuning survives a re-install. |
| `zen-rotate-watch.sh` | `~/.local/bin/` | Watches the log for Zen/Spark rate limits and rotates the VPN exit. Not started by the installer. |
| `headroom-rss-sample` | `~/.local/bin/` | Samples the proxy's RSS once a minute into `~/headroom-rss.log`, so a leak over a long session is visible. |
| `reconcile_books.py` | not installed | Checks the proxy's token books against a number it did not compute (the Anthropic usage API, or a console export). Run by hand. |

## Status line

One status line, six files. `settings.json` points at
`statusline-with-cache.sh` alone; that script runs the others as subprocesses
and joins what they print. The usage dump is installed beside it because the
chain calls it by path; the remaining four it finds in the same directory.
Every segment prints nothing when the proxy is down, so the line then reads
exactly like the usage dump on its own.

| File | Role |
| --- | --- |
| `statusline-with-cache.sh` | The entry point. Chains the usage dump with the re-cache watchdog and the segments below. Always generated, never symlinked, since the checkout path is baked in. |
| `statusline-usage-dump.sh` | Produces the base line — model, context, plan usage — and caches what it was handed in `/tmp/claude-usage-latest.json`. Works alone if you point `settings.json` straight at it. |
| `statusline-cache-health.sh` | Re-cache watchdog: warns when the prompt cache is being thrown away. |
| `statusline-cache-perf.sh` | Recent cache hit rate. |
| `statusline-codex-limits.sh` | Codex quota left. |
| `statusline-spark-context.sh` | Spark context use. |

## Claude Code hooks (`claude/hooks/`)

All eleven install to `~/.claude/hooks` and are registered idempotently in the
settings file — a re-run never duplicates an entry. Each script carries a
header comment explaining the failure it exists for; read that before changing
one. Every hook exits 0 on its own errors, so a broken hook cannot wedge a
session.

| Hook | Event | Blocks? |
| --- | --- | --- |
| `review-gate.sh` | UserPromptSubmit, PreToolUse (Bash, writes), Stop | Only a review articulation it diverts to a worker |
| `ticket-gate.sh` | UserPromptSubmit, PreToolUse (Bash) | Only a YouTrack filing it diverts to a worker |
| `scrub-secrets.sh` | PreToolUse (Bash) | Yes — commands that would print credentials into the transcript |
| `scrub-placeholders.sh` | PreToolUse (writes) | Yes — redaction tokens that would land literally in code or docs |
| `shared-worktree-guard.sh` | PreToolUse (Bash) | Yes — the destructive git commands banned by `.agents/SHARED-WORKTREE-PROTOCOL.md`, but only while another agent is live in the same toplevel |
| `peer-awareness.sh` | SessionStart, UserPromptSubmit | No — reports other sessions sharing the checkout |
| `stale-branch.sh` | SessionStart | No — one-time notice when the branch trails its origin |
| `stale-install.sh` | SessionStart | No — one-time notice when the checkout moved under the installed copies (scripts/hooks differ, or the built binary is newer); fires only inside the checkout |
| `session-map-log.sh` | SessionStart | No — logs session id to transcript path for the review worker |
| `rotation-notice.sh` | UserPromptSubmit | No — relays VPN-rotation notices once each |
| `retry-dropped-turn.sh` | Stop | No — continues a turn parked on a dropped connection |

Two of these need their own worker to be useful (`review-gate.sh` and
`ticket-gate.sh`); see `spark-poster/README.md`.

## Claude Code agents and memory text

`claude/agents/*.md` are the subagent definitions — `spark`, `spark-explore`,
`codex-sol`, `codex-luna`, `codex-terra`, `grok-xhigh`, `grok-high`,
`grok-low`. They install into `~/.claude/agents`, and into `~/.claude-work` and
`~/.claude-personal` as well when those profile directories already exist — an
agent missing from a profile vanishes from the Agent tool there with no error.

`claude/CLAUDE.headroom.md` is the memory-tool section the installer splices
into your `CLAUDE.md` between markers, so a re-install updates it in place
instead of appending a second copy.

## spark-poster/

The GitLab and YouTrack workers the two diversion hooks hand work to, plus the
credential helper that keeps the token out of the ambient environment. It has
its own [README](spark-poster/README.md).

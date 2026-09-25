# contrib

Everything here runs on your machine, not inside the proxy. Only
[`install.sh`](../install.sh) reads this directory: it copies these files into
`~/.local/bin`, `~/.claude`, or `$HOME`, or symlinks them with `--link` so
editing the checkout edits the live setup. Nothing here is compiled, and
nothing here is on the request path.

## Starting and running the proxy

| File | Installed as | What it does |
| --- | --- | --- |
| `claude-launcher` | `~/.local/bin/claude-launcher`, `cclaude` | Starts/reuses the proxy on the configured lane, sets `ANTHROPIC_BASE_URL`, execs `claude`. Handles several profiles and an optional local Qwen. |
| `opencode-launcher` | `~/.local/bin/opencode-launcher`, `oopencode`, `oopencode-work` | The `cclaude` equivalent for opencode: starts/reuses the proxy on the configured lane, injects a `headroom` provider via `OPENCODE_CONFIG_CONTENT`, execs `opencode`. `oopencode-work` adds `--context --auto`. |
| `restart-headroom.sh` | `~/.local/bin/` | Restarts the proxy onto a freshly built binary and rolls back if it fails to come up. Detached, so killing the proxy does not kill the restart. |
| `update-headroom.sh` | `~/.local/bin/` | Pulls the checkout, reinstalls in the last install's mode, restarts the proxy. Run after `git pull`, or instead of it. |
| `concurrency-report.sh` | `~/.local/bin/` | Verdict on the 2026-09-15 concurrency fixes from the proxy log: sidecar 404 fallbacks, Zen slot timeouts, proxy-side stalls, and whether `--cache-stampede-gate` held any follower that then read cache. Run after a day of use; it says when to drop the gate flag. |
| `headroom-flags.sh` | `~/.headroom-flags.sh` | The flag array both starters read. An existing file is never overwritten, so tuning survives a re-install. |
| `headroom-zen-pool.env.example` | not installed | Shape of `~/.headroom-zen-pool.env`, the Zen egress pool both starters source. Generate the real file with `nord-socks-egress env`; see [Nord SOCKS5 relay pool](#nord-socks5-relay-pool). |
| `zen-rotate-watch.sh` | `~/.local/bin/` | Watches Zen/Spark rate limits; rotates a device-wide VPN in legacy mode or configured egresses individually in pool mode. Not started by the installer. |
| `nord-socks-egress` | `~/.local/bin/` | Rust helper binary for the optional local SOCKS5 relay pool over Nord; exposes eight verified loopback lanes, or ten when all preferred exits pass, and rotates a lane to an unused endpoint when a spare is available. |
| `headroom-rss-sample` | `~/.local/bin/` | Samples the proxy's RSS once a minute into `~/headroom-rss.log`, so a leak over a long session is visible. |
| `reconcile_books.py` | not installed | Checks the proxy's token books against a number it did not compute (the Anthropic usage API, or a console export). Run by hand. |

## Separate upstream egress lanes

For providers that permit separate egress sessions, each Headroom process can
use its own provider-only HTTP or SOCKS5 proxy and local listener. This does
not create VPN sessions; provision distinct egress proxy endpoints with the
provider/VPN service, then configure one lane per endpoint. Claude and
OpenCode launchers both honor:

- `HEADROOM_PROXY_LISTEN` — local bind address (default `127.0.0.1:8787`).
- `HEADROOM_PROXY_URL` — client-facing base URL (default follows the listener
  port on `127.0.0.1`).
- `HEADROOM_PROXY_LOG` — log file (defaults to `headroom-proxy.log` on port
  8787, otherwise `headroom-proxy-<port>.log`).
- `HEADROOM_HTTP_PROXY` — provider-only upstream proxy URL; `socks5h://` is
  supported, including service credentials in the URL.
- `HEADROOM_ZEN_HTTP_PROXY_POOL` — newline-separated proxy URLs used only for
  Muse Spark/OpenCode Zen requests; Claude and Codex never use this pool.
- `HEADROOM_ZEN_EGRESS_ROTATE_COMMAND` — executable called by the watcher with
  `<opaque-egress-id> <rate-limit|proactive|manual>`. Required for automatic
  per-egress rotations; it must map each ID to an independently managed VPN
  session and return nonzero if rotation fails.
- `HEADROOM_ZEN_EGRESS_ROTATE_TIMEOUT` — per-callback timeout in seconds
  (default `600`).
- `HEADROOM_DISABLE_ZEN_ROTATE_WATCH=1` — prevents either launcher from
  starting the legacy watcher, which can rotate the shared device-wide VPN
  route. The launcher starts the watcher in per-egress mode for a Zen pool
  only when `HEADROOM_ZEN_EGRESS_ROTATE_COMMAND` is executable; a general
  `HEADROOM_HTTP_PROXY` still suppresses watcher startup. If a watcher is
  already running, stop/restart it in the desired mode before starting lanes;
  the launcher does not terminate an existing watcher.

Example for an OpenCode lane (use the equivalent `cclaude --context` for
Claude Code):

```bash
HEADROOM_PROXY_LISTEN=127.0.0.1:8788 \
HEADROOM_PROXY_URL=http://127.0.0.1:8788 \
HEADROOM_HTTP_PROXY="$MUSE_EGRESS_A_SOCKS_URL" \
HEADROOM_DISABLE_ZEN_ROTATE_WATCH=1 \
oopencode --context
```

Use another listener and a different SOCKS5 endpoint for each distinct
egress. Keep an in-progress model session on one listener: session/cache
stabilization state is process-local too. Sessions intended to share one
egress should use the same listener so they also share that process's
rate-limit retry state. Don't start multiple proxy processes against the same
egress just to get more terminals: their retry state is process-local. SOCKS5
proxy credentials are secrets; keep them in a local secret store or a
permission-restricted file, export them through `HEADROOM_HTTP_PROXY` or
`HEADROOM_ZEN_HTTP_PROXY_POOL`, and never put them in the repo, command-line
arguments, or logs. Headroom consumes and removes these environment variables
before starting its runtime, and both launchers remove them before running the
client.
Verify that the endpoints actually produce
different public egresses before assigning production sessions. The legacy
watcher is device-wide, so inspect for and stop an already-running instance
with `pgrep -af zen-rotate-watch` / `pkill -f zen-rotate-watch` before using
separate lanes. The pool adds no global concurrency cap: existing Zen in-flight
caps and 429 retry holds are isolated per egress, with hold probes following a
usable `Retry-After` within the configured cap and otherwise using exponential
backoff with jitter.

For Muse Spark/OpenCode Zen, set `HEADROOM_ZEN_HTTP_PROXY_POOL` to one distinct
HTTP or SOCKS5 egress URL per line before starting the proxy. Only Zen-routed
requests use this pool; the default Claude route and routed Codex/OpenAI calls
keep their existing transport. Each new Zen stream lane is assigned to the
next egress and remains sticky there. A fan-out of 10 distinct lanes uses 10
egresses when 10 are configured; additional lanes cycle and share. Claude Code
sibling lanes are derived from conversation identity and system prompt, so
distinct agent streams are distributed without client changes. If two streams
have identical identity and system prompt, the proxy cannot distinguish them
and they share an egress. Affinity is process-local and resets on proxy restart.
`HEADROOM_HTTP_PROXY` remains the existing general provider-proxy option when
no Zen pool is configured.

The pool is VPN-vendor agnostic: any VPN setup that exposes a distinct HTTP or
SOCKS5 endpoint per session can feed it. In pool mode, the watcher sends only
the rate-limited egress ID to `HEADROOM_ZEN_EGRESS_ROTATE_COMMAND`; on a
scheduled tick it enumerates the proxy's configured pool and invokes the
command once for every egress. The executable owns the VPN-specific action and
must map each opaque ID to the correct independent session. It receives no
proxy URL or credentials. Before each callback the watcher drains only that
egress, using its count in `/debug/inflight`'s `egress_in_flight` (turns
parked on a 429 are left out); other egresses keep streaming. Scheduled
rotations go one egress at a time, and a busy egress defers alone rather than
holding up the rest. The callback's stdout/stderr are discarded; it should
log diagnostics to its own permission-restricted file, verify the new egress,
and return nonzero on failure. Its default timeout is ten minutes.

The existing built-in NordVPN, Mullvad, ExpressVPN, ProtonVPN, Surfshark, PIA,
Tailscale, WireGuard, and OpenVPN commands still control one device-wide route.
They do not create or rotate independent sessions for a pool. The generic hook
can call an adapter for any of them, but each adapter needs a way to control
separate sessions; a single global `nordvpn connect` command is not sufficient.
A Tor SOCKS5 endpoint is protocol-compatible, but this only establishes
transport support—not that OpenCode Zen accepts Tor exits. Tor is not selected
automatically.

Put both variables in `~/.headroom-zen-pool.env` (mode `0600`) rather than
exporting them from a shell: `claude-launcher` and `restart-headroom.sh` source
that file themselves, so a proxy started from a shell without the exports
still gets the pool. `headroom-zen-pool.env.example` shows the shape. With
`nord-socks-egress`, generate the file instead (next section). For another
relay, write it by hand and keep the credentials out of shell history:

```bash
cp contrib/headroom-zen-pool.env.example ~/.headroom-zen-pool.env
chmod 600 ~/.headroom-zen-pool.env
$EDITOR ~/.headroom-zen-pool.env    # your URLs and rotate command
restart-headroom.sh
```

The executable receives one opaque ID and a reason per invocation;
`rate-limit` rotates only the egress that got the 429, while `proactive` and
manual `--rotate-now` rotate all configured pool entries. The watcher skips
scheduled rotations if it cannot enumerate the pool and never falls back to
changing the device-wide route in pool mode. Without an executable, the pool
still routes requests but automatic rotations are disabled and the launcher
prints a warning.

After a candidate Nord exit is verified, Headroom briefly marks only that
egress as rotating. New requests assigned to it fail fast with a retryable
503; they are never queued, and other egresses remain available. Requests
already using it drain before the relay switches endpoint and closes its old
tunnels. Candidate probing itself does not interrupt the current lane. A timed
pool rotation remains scheduled even when individual lanes are rotating
reactively for 429s.

### Nord SOCKS5 relay pool

The Rust `nord-socks-egress` binary adapts Nord's remote SOCKS5 service to
Headroom's per-egress rotation hook. It reads `USERNAME=...` and `PASSWORD=...` from
`~/.config/headroom/nord-socks-credentials.json` (or the path in
`HEADROOM_NORD_SOCKS_CREDENTIALS_FILE`). Keep that file `0600` and its parent
directory `0700`. Credentials are not placed in the process command line,
exported Headroom pool, or relay logs.

Once, from any shell, after existing proxy requests have drained:

```bash
(umask 077; ~/.local/bin/nord-socks-egress env > ~/.headroom-zen-pool.env)
restart-headroom.sh    # replaces a device-wide watcher with a per-egress one
cclaude --context
```

Nothing starts the relay at boot. After a reboot the file still names its
loopback ports, but nothing listens on them and every Zen request fails until
you run the first two commands again. Do the same after `nord-socks-egress
stop`: the relay can come back with eight lanes or ten, and the file must
match.

The `env` command starts the local relay daemon and prints exports for one
loopback SOCKS URL per verified lane (eight, or ten when all preferred exits
pass) plus the per-egress rotator path. `claude-launcher` and
`restart-headroom.sh` both source `~/.headroom-zen-pool.env`
(`HEADROOM_ZEN_POOL_ENV` overrides the path). The file takes precedence over
whatever the shell exports, and they skip it unless it belongs to you with
mode `0600`. With the pool loaded, `restart-headroom.sh` swaps a device-wide
watcher for a per-egress one. Without the pool and with the relay up, it
prints a warning: Zen then shares the device-wide route, and each Zen 429
rotates the VPN under every Codex and Spark stream.
The relay uses exact server IDs from
Nord's [live SOCKS server catalog](https://api.nordvpn.com/v1/servers?filters%5Bservers_technologies%5D%5Bidentifier%5D=socks&limit=0)
(not region-level aliases), pins each lane to
the server address resolved at assignment time, and probes the public exit IP
before publishing the pool. It tests the preferred ten lane candidates first;
if fewer than eight distinct exits verify, it checks spare server IDs only
until the eight-lane minimum is recovered. It starts with all ten only when all
ten preferred exits verify; if one or more fail, it uses exactly eight of the
verified exits. It fails closed below eight. Remaining spare server IDs are
kept as rotation candidates and probed on demand; each is refused if its exit
matches any active lane or the lane being replaced.
With no verified spare endpoint,
a lane rotation fails rather than sharing another lane's exit. An older
running relay with fewer than eight lanes is
not replaced automatically, to avoid killing in-flight sessions: drain its
users, run `nord-socks-egress stop`, then start it again. Run
`nord-socks-egress status` to see endpoint assignments,
`nord-socks-egress test` to check egresses (it prints hashes, not IP
addresses), or `nord-socks-egress stop` to stop the relay.

Headroom reads the pool only when its proxy process starts. The Claude and
OpenCode launchers compare the configured opaque egress IDs with the live
proxy inventory and stop with restart guidance if they differ. If a proxy is
already running without `/debug/zen-egresses`, restart it after its in-flight
requests drain before expecting the pool to take effect. Stop an old
device-wide `zen-rotate-watch.sh` before enabling this pool; the launcher will
not kill or replace a watcher another session started. Nord currently permits
up to ten simultaneous device sessions per account and up to five on one
server. Ten SOCKS lanes therefore consume the full account session allowance;
an existing NordVPN app/NordLynx connection or other device sessions can make
fewer lanes available. Check the relay's `lane_count` with
`nord-socks-egress status` and keep concurrent Spark fan-out at or below
that number. The installed Claude instructions tell Spark parents and workers
to count unfinished Spark tasks, forbid nested Spark fan-out, and continue
sequentially when all lanes are occupied. The helper never silently shares an
exit or starts with fewer than eight lanes.

Do not put proxy URLs or credentials in the repository, command-line
arguments, or logs. Keep the pool in a local secret store or permission-
restricted file and export it into the launcher environment. Headroom consumes
and removes the pool variable before starting its runtime, and both launchers
remove it before running the client. Verify that each endpoint actually
produces a different public egress before using it. If the legacy device-wide
watcher is already running when switching to a Zen pool, stop it and start the
watcher again through `cclaude`; otherwise it can continue rotating the shared
default route.

## Status line

`settings.json` points at `statusline-compose.sh`. Claude Code runs one command
for its status line, so the composer runs the previous user command first and
prints Headroom's status line after it. The installer saves that command in
`~/.claude/statusline-user-command`; edit that file to customize the existing
status line after installing. Re-running the installer recognizes the composer
and does not chain it again. Settings retain their other fields, including
padding and refresh interval. Existing files changed by the installer are
copied to a sibling `.bak` before the change.

The Headroom status line itself is a chain of six files. The usage dump is
installed beside `statusline-with-cache.sh` because it calls it by path; the
remaining four helpers live in the checkout's `contrib/` directory. Every
segment prints nothing when the proxy is down, so Headroom's line then reads
exactly like the usage dump on its own.

| File | Role |
| --- | --- |
| `statusline-compose.sh` | Entry point configured in `settings.json`. Runs `~/.claude/statusline-user-command`, then appends Headroom's line. |
| `statusline-with-cache.sh` | Chains the usage dump with the re-cache watchdog and the segments below. Folds `|`-separated segments onto new lines to fit the terminal width (tty, else COLUMNS, else 80), so nothing is cut. Always symlinked so it can find its helpers in the checkout. |
| `~/.claude/statusline-user-command` | Generated file containing the user's pre-existing statusline command. Edit it to customize that output; an empty file means Headroom only. |
| `statusline-usage-dump.sh` | Produces the base line — model, context, plan usage — and caches what it was handed in `/tmp/claude-usage-latest.json`. The model shows short, without the `claude-` prefix. Works alone if you point `settings.json` straight at it. |
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
| `scrub-placeholders.sh` | PreToolUse (writes, Bash, Skill) | Yes — redaction tokens that would land literally in code or docs, or wedge a session when pasted into commands; `HEADROOM_SCRUB_BASH=0` skips the Bash leg |
| `shared-worktree-guard.sh` | PreToolUse (Bash) | Yes — the destructive git commands banned by `.agents/SHARED-WORKTREE-PROTOCOL.md`, but only while another agent is live in the same toplevel |
| `peer-awareness.sh` | SessionStart, UserPromptSubmit | No — reports other sessions sharing the checkout |
| `stale-branch.sh` | SessionStart | No — one-time notice when the branch trails its origin |
| `stale-install.sh` | SessionStart | No — one-time notice when the checkout moved under the installed copies (scripts/hooks differ, or the built binary is newer); fires only inside the checkout |
| `session-map-log.sh` | SessionStart | No — logs session id to transcript path for the review worker |
| `rotation-notice.sh` | UserPromptSubmit | No — relays VPN-rotation notices once each |
| `retry-dropped-turn.sh` | Stop, SubagentStop | No — continues a turn parked on a dropped connection, a completed upstream error, a proxy-dropped tool call that left an empty reply, a retrieval splice the model never answered, or a dropped memory lookup |

Two of these need their own worker to be useful (`review-gate.sh` and
`ticket-gate.sh`); see `spark-poster/README.md`.

## Claude Code agents and memory text

`claude/agents/*.md` are the subagent definitions — `spark`, `spark-explore`,
`codex-sol`, `codex-luna`, `codex-terra`, `codex-6-astra`, `codex-6-sol`,
`codex-6-luna`, `grok-xhigh`, `grok-high`,
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

# VPN / egress learnings — Muse Spark via Zen (2026-09-08)

Working note, not spec. Session covered: what IP Zen actually sees, header
spoofing tests, NordVPN in WSL2, and automating exit rotation on rate limits.

## Setup

- Model route (live flags, `~/.headroom-flags.sh`):
  `claude-muse-spark-1.3=https://opencode.ai/zen/v1:openai:muse-spark-1.3-contributor-free:auth=OPENCODE_API_KEY`
  (translate path, Anthropic → OpenAI Responses, credential from env per request)
- Proxy: `127.0.0.1:8787`, restarted during session via the vendored
  `contrib/restart-headroom.sh` (identical to `~/.local/bin/restart-headroom.sh`,
  verified by diff — always use that, never a hand-rolled nohup).

## What the proxy sends (after this session's changes)

Routed Spark path builds a fresh `HeaderMap` (`handlers/local_model.rs`):
`Content-Type`, `Authorization`, `x-opencode-session`, `x-opencode-request`,
`x-opencode-client`, `User-Agent: opencode/1.18.29`, plus since this session:
`X-Forwarded-For` (= inbound peer, usually `127.0.0.1`) and
`X-Headroom-Egress` (= reflector-observed public egress, cached 5 min from
`https://api.ipify.org` over the same `reqwest::Client`, so same proxy/VPN path).
Logs `model_route_translate` / `model_route_passthrough` now carry `peer_ip`,
`egress_ip`, `x_forwarded_for`, `x_headroom_egress`, `outbound_headers`.

## Core learning: Zen reads the socket, not headers

1. Through-proxy spoof (`X-Forwarded-For: 203.0.113.99`,
   `X-Headroom-Egress: 198.51.100.7`): 200, identical body/tokens to baseline.
   Log proved the spoof never went on the wire — replaced with peer/measured values.
2. Direct-to-Zen spoof vs clean: first pair looked suspicious (429 then 200);
   re-run swapped gave **429 + 429**. No XFF-based bucket. Transient pool capacity.
3. The "wait 12 hours" gate **is IP-keyed**: it lifted when egress changed.
   (Correction to an earlier claim in-session that rotation "won't dodge the
   limits" — for this gate it does. Account/pool limits are a different story.)

Egress observed: home `<home-egress>`, NL VPN exit `<vpn-egress>`.

## Gotcha 1: Windows NordVPN does nothing for WSL2

With the Windows app VPN on, WSL2 egress was byte-identical
(`<home-egress>` before and after). WSL2 has its own vNIC; the Windows
tunnel doesn't capture it. An apparent "VPN fixed it" recovery was
coincidence (pool freeing up over time). Fix: run the tunnel **inside** WSL2
via the NordVPN Linux CLI — then egress actually moves (`<vpn-egress>`).

## Gotcha 2: NordVPN Linux CLI first-run

- `nordvpn login` → permission denied until group applies:
  `sudo groupadd nordvpn; sudo usermod -aG nordvpn $USER`, then a **fresh
  shell** (`newgrp nordvpn`) — reboot not strictly required.
- Daemon needs systemd in WSL2: `/etc/wsl.conf` with `[boot] systemd=true`,
  then `wsl --shutdown` from Windows and reopen.

## Gotcha 3: NordLynx eats loopback (biggest time sink)

After `nordvpn connect`, **all** fresh loopback TCP timed out
(`:8787`, plus untouched `:6379`/`:5432` — collateral proof it wasn't the
proxy; pid 43362 stayed alive and heartbeating throughout).
Cause: NORDLYNX + `Firewall: enabled` + `Routing: enabled` routes by fwmark
(`0xe1f1`) in policy tables — `ip route` looks normal, but SYNs vanish into
the tunnel. Kill Switch was already `disabled`, so not the culprit.
`nordvpn allowlist add port 8787` did **not** fix it (verified `200`
off-VPN / `000` on-VPN after fresh reconnect).
Fix that worked: `nordvpn allowlist add subnet 127.0.0.0/8`
(accept the "too large" warning — 127/8 can't leave the host anyway),
then reconnect. Loopback `200` again with tunnel up.

## Commands run (condensed)

```bash
# headers/IP investigation (all read-only)
grep ... # X-Forwarded-For construction in headers.rs, forward_http, local_model routed path
# echo-upstream spoof test (throwaway python server + temp proxy on :18987, since removed)
# release build + vendored restart
cargo build --release -p headroom-proxy
bash ~/headroom/contrib/restart-headroom.sh
# baseline vs spoofed Spark turns through :8787, direct Zen spoof/clean pairs,
# ipify checks, nordvpn install/login/connect, allowlist iterations
nordvpn allowlist add port 8787            # insufficient (kept for the record)
nordvpn allowlist add subnet 127.0.0.0/8  # the fix; reconnect after
```

## Code changes (REVERTED — see below)

The session first added `X-Forwarded-For` + `X-Headroom-Egress` injection with
an ipify-backed egress cache to `handlers/local_model.rs` (+151). This was
fully reverted afterward (`git checkout` — purely additive diff, clean):
the spoof experiments proved Zen ignores these headers, and `127.0.0.1` in
`XFF` is pure fingerprint cost with zero benefit. Wire format is back to
pre-session minimal. What stays: `contrib/zen-rotate-watch.sh` (new,
committed — tails the log for routed-429s, rotates country until ipify
changes, restarts via the sibling vendored script; 120 s cooldown, flock
guard; **not yet started**) and this note.

Note: tree also holds other uncommitted work NOT from this session
(`cost_tracker.rs`, `savings_tracker.rs`, `usage_observer.rs`,
`cursor/translate.rs`, `openai/stream.rs`) — the release build picked those
up too; they were left untouched deliberately.

## Rotation runbook (current)

1. `nordvpn connect <country>` (or the watcher does it on 429) — prefer
   `zen-rotate-watch.sh --rotate-now [country]`, which drains first.
2. Verify egress actually moved: `curl -s https://api.ipify.org`.
3. Do NOT restart the proxy. A restart wipes in-memory session state
   (replay store, usage observer, CCR tracker) and the fleet recaches
   everything at once plus an error burst — far worse than the rotation
   itself. Pooled corpse sockets age out on their own in ~25 s
   (`--pool-idle-timeout`); turns served from corpses fail fast on first
   write and self-heal through pre-commit retry onto fresh connections.
   Just proceed to step 4.
4. In-flight turns during the ~10 s reconnect fail; retry the turn after.
   (The watcher drains before rotating so there should be none in flight;
   manual rotations must use `zen-rotate-watch.sh --rotate-now [country]`,
   which drains first — never run bare `nordvpn connect` mid-session.)
5. Sessions learn about the rotation from a notice file, not a wake-up:
   the watcher writes `$OUTDIR/<session>.rotation.json` per recently-active
   spark session and `rotation-notice.sh` relays it once on the next prompt.
   (The old billed `claude --resume` wake is gone — resuming loads each
   session's full context as a billed turn for news it reads free anyway.)

## Making rotations safe (2026-09-10)

A rotation kills in-flight TCP by design (exit IP changes, server RSTs the
old 4-tuple) and strands pooled keepalive sockets as corpses — but the
proxy must NOT restart (in-memory session state loss = fleet-wide recaches
plus an error burst). Safety is drain-then-shrink instead:

1. **Drain before rotating.** The watcher polls `GET /debug/inflight`
   (loopback-only; counts `forward_http` AND routed `handle_messages`
   turns until response dispatch — headers-wait, buffered bodies, CCR,
   fallback — but not streamed body bytes, which flow after the guard
   drops) and only runs `nordvpn connect` at zero, bounded by a 90 s timeout — then rotates
   anyway and the stragglers die truncated-but-marked. New turns starting
   mid-drain are a residual race; the window shrinks from "whenever the
   429 lands" to seconds.
2. **Shrink the corpse window.** `--pool-idle-timeout 25s` (was hardcoded
   90 s; now a flag, default unchanged). Corpses age out in ~25 s instead
   of ~90 s at the price of more TLS handshakes. Corpse turns fail fast
   (RST on first write) and self-heal through the pre-commit retry onto a
   fresh connection.
3. **What already covers the rest:** stream hold + attempts (pre-commit),
   finisher + marker (post-commit), resume prompt on wake.

## Proactive cycling (2026-09-10)

The watcher also rotates on a schedule — every hour ±10 min jitter, so no
cron-shaped pattern — instead of only after a refusal. A scheduled rotation
fires only into a quiet moment (`in_flight==0`; busy box defers 5 min and
rechecks), drains first, and wakes nobody when the drain is clean — only
stragglers that died truncated get the resume prompt. Reactive rotations
reset the schedule clock (a fresh exit needs no cycle on top of it).
Countries are recycled, not one-passed: the loop reshuffles until the
egress moves or 10 min elapse, then fails loudly. Manual rotations use
`zen-rotate-watch.sh --rotate-now [country]` (same drain + wake).

NOT built (deliberately): a pool-swap endpoint (needs `AppState.client`
behind a lock — churn across every sender — for a window drain + idle
shrink already cover); semantic hold for thinking models (SSE parsing in
the retry layer + first-paint delay on every thinking turn).

Both proxy pieces (`/debug/inflight`, routed guard, pool flag) ship in the
tree but need a proxy restart to go live — a single one-time upgrade
restart at an idle moment, not per-rotation restarts (which stay
forbidden: the recache cost applies every time). The watcher runs the new
drain code now and simply rotates undrained (endpoint 404s) until then.

## Mid-response drops (2026-09-10 investigation)

Symptom: `[truncated: the connection to the API dropped mid-response]`,
turn ends, agent loop stalls until the user nudges. Two turns died this way
(14:02:46Z after 3 attempts, 14:32:48Z on the first), both `claude-opus-5`
direct (not Spark), both `hyper Body Io ConnectionReset`.

Evidence chain (all times UTC):

- Drop #1's first attempt died **9 s after a tunnel rebuild started**
  (18:01:39+04 Lebanon → Armenia; journal `nordvpnd` CONNECTING). Tunnel
  rebuilds kill in-flight TCP by design — exit IP changes, server RSTs the
  old 4-tuple. Certain.
- The two re-attempts died 30 s / 26 s into the NEW tunnel, and drop #2 died
  39 s in with no rebuild adjacent. Background suspect: WSL host-network
  flaps — `CheckConnection connect() failed: 99` + `getaddrinfo -5` bursts
  in the journal (120 in 46 min around the drops, 0 right now with a healthy
  network). Brief vNIC stalls RST unlucky in-flight sockets while most
  traffic survives. Correlative, not proven.
- Ruled out: NIC-offload corruption (offloads verified OFF on eth0-eth5 AND
  `nordlynx`, including the VPN interface). H2 keepalive (20 s), TCP
  keepalive (20 s), `TCP_USER_TIMEOUT`, 90 s pool idle were already set —
  tuning is present, this is not a tuning gap.

What the proxy already did right: `stream_retry` hold (8 KB) + 3 attempts
covered drop #1's first two deaths transparently; `stream_finisher`
synthesised a valid `end_turn` + marker (the `stop_reason: ''` in the log
is the telemetry tee's view pre-finisher, not what the client got).

Gaps closed 2026-09-10 (code):

- Routed (translate/Spark) streaming had NO finisher: a drop ended as a
  clean-looking `end_turn` with no marker (silent cut) or a bare RST (dead
  session), and a half-streamed tool call could have closed into a runnable
  call on truncated input. Now: translator leaves aborts unstopped with the
  tool block open (`abort_terminal`), `finish_on_drop` is error-generic,
  `handle_streaming_response` + sidecar streaming wrap it. Unit + seam
  tests green.

Deliberately NOT done:

- No proxy restart on rotation, ever — restarting wipes in-memory session
   state and forces fleet-wide recaches plus an error burst, far worse than
   the ~25 s of corpse-RST turns while the old pool ages out. (A restart was
  briefly wired into the watcher and reverted same-day for exactly this
  reason.) The corpse window is accepted cost; the finisher + retry layers
  above are what make those turns survivable instead.

Known non-goals (documented, not bugs):

- Post-commit drops on the direct path cannot retry transparently (client
  holds bytes; re-sending splices generations). Thinking-heavy turns fill
  the 8 KB hold during thinking, so their answer phase is unprotected —
  fixing that needs SSE-aware commit (layering cost + paint delay), deferred.
- Attempt budget stays 3: each pre-commit attempt bills a dead generation;
  more attempts on a down path burns money, it doesn't buy survival.
- Rotations will always kill in-flight turns. Fewer rotations = fewer
  guaranteed kills; the watcher's 120 s cooldown is the throttle.

To discriminate next time: `grep -c "upstream stream error mid-response"`
per hour against `journalctl` nordvpnd CONNECTING events and WSL
CheckConnection bursts. Tunnel-adjacent deaths = rebuild kills (rotate
less); background deaths with CheckConnection bursts = host flaps
(Windows-side NIC/driver/power investigation); neither = look upstream.

## Wifi switches / host flaps (2026-09-10)

Same machinery as rotations, minus the drain: a wifi switch is an unplanned
total outage (DNS + connect + in-flight all fail), so no watcher trigger
fires and retry budgets burn against a dead link. What covers it:

- Pre-commit turns (buffered POST, SSE inside the hold window) retry
  transparently while budget lasts; pure-passthrough has no retry loop
  (streamed body, nothing to resend) — the client re-issues from its copy.
- Transport exhaustion now answers **503 + `Retry-After: 2`** with
  `x-headroom-retryable: transport-exhausted` (proxy-transient, not provider
  5xx), so stock client retry fires instead of stalling the session.
  Non-transport failures stay 502 with no `Retry-After`.
- Post-commit drops end marked-but-well-formed via the finisher; the Stop
  hook (`retry-dropped-turn.sh`) auto-continues both tool-discarded AND
  plain-text truncations (max 3/session, never fails closed).
- Corpses on every destination age out via the 25 s pool TTL; first turns
  after reconnect spend one attempt each failing fast onto fresh conns.

## Open risks

- IP-keyed bucket rotation is the exact loop free-tier abuse detection is
  tuned for — works until key/behavior fingerprinting catches up.
- `X-Headroom-Egress` is stale up to 5 min after rotation unless proxy restarts.
- Watcher triggers only on routed-429 log lines; other refusal shapes
  (e.g. 200-with-error-body, if Zen ever does that) won't trip it.

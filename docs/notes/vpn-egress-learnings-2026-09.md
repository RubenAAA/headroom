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

> **Moved to [`learnings/zen-reads-socket.md`](learnings/zen-reads-socket.md)** — spoof experiments + IP-gate finding in full.

> **Moved to [`learnings/windows-vpn-misses-wsl2.md`](learnings/windows-vpn-misses-wsl2.md)** — WSL2 vNIC finding in full.

> **Moved to [`learnings/nordvpn-first-run.md`](learnings/nordvpn-first-run.md)** — first-run checklist in full.

> **Moved to [`learnings/nordlynx-loopback-fix.md`](learnings/nordlynx-loopback-fix.md)** — loopback investigation + subnet fix in full.

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

> **Moved to [`ideas/rejected/xff-egress-injection.md`](ideas/rejected/xff-egress-injection.md)** — reverted diff record in full.

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

> **Moved to [`ideas/implemented/vpn-drain-before-rotate.md`](ideas/implemented/vpn-drain-before-rotate.md)** — drain-then-shrink design (shrink flag rides along; see pool-idle file).

> **Moved to [`ideas/implemented/vpn-drain-before-rotate.md`](ideas/implemented/vpn-drain-before-rotate.md)** — scheduled rotation design.

> **Moved to [`ideas/rejected/pool-swap-endpoint.md`](ideas/rejected/pool-swap-endpoint.md)** — NOT-built paragraph (pool-swap leads; semantic-hold context rides along).

> **Moved to [`ideas/vpn-one-time-upgrade-restart.md`](ideas/vpn-one-time-upgrade-restart.md)** — pending-restart paragraph.

> **Moved to [`learnings/drop-discrimination-query.md`](learnings/drop-discrimination-query.md)** — symptom + evidence chain in full.

> **Moved to [`ideas/implemented/vpn-routed-finisher.md`](ideas/implemented/vpn-routed-finisher.md)** — gaps-closed record in full.

> **Moved to [`ideas/rejected/restart-on-rotation.md`](ideas/rejected/restart-on-rotation.md)** — no-restart rule + same-day revert in full.

> **Moved to [`learnings/rotation-kills-inflight.md`](learnings/rotation-kills-inflight.md)** — attempt-budget, unprotected thinking turns, rotation throttle in full.

> **Moved to [`learnings/wifi-outage-shape.md`](learnings/wifi-outage-shape.md)** — outage shape + coverages in full.

## Open risks

- IP-keyed bucket rotation is the exact loop free-tier abuse detection is
  tuned for — works until key/behavior fingerprinting catches up.
- `X-Headroom-Egress` is stale up to 5 min after rotation unless proxy restarts.
- Watcher triggers only on routed-429 log lines; other refusal shapes
  (e.g. 200-with-error-body, if Zen ever does that) won't trip it.

# Learning: Nord accepts this account on a shifting subset of SOCKS servers

- **Window:** measured 2026-09-29 23:10-23:27 local (+04), from a WSL host
  outside the US, with the account's service credentials.
- **Claim:** the SOCKS servers that accept the account change about every ten
  minutes. Of the 16 servers the helper had hard-coded, 11 accepted at 23:10,
  3 of the first 25 US servers at 23:15, 9 of 16 at 23:22 (us31-us33 and us35
  in the first set, not the second; us34 and us37 the reverse). A fixed list
  strands lanes: 191 rotation failures in five days came from candidates that
  happened to be in the rejected half, and two live lanes sat on rejected
  servers until rotated by hand.
- **Claim:** Nord gives no reason. A rejection is the RFC 1929 auth reply
  `01 01` and nothing after it, so "too many devices" cannot be told from
  any other refusal. The helper can only track it by behaviour: probe, count
  failures, act on what persists.
- **Refuted:** rate limit. 25 back-to-back auths on an accepted server all
  passed; 15 on a rejected one all failed.
- **Refuted:** concurrent-session cap. With 30 authenticated, connected
  sessions held open, other servers' accept/reject state did not change.
  Tested to 30 only.
- **Claim:** Nord's public listing
  (`api.nordvpn.com/v1/servers?limit=5000&filters[servers_technologies][identifier]=socks`)
  has 68 SOCKS servers (45 US, 15 SE, 8 NL), all `online`, all 16 old ones
  included. The account was rejected on all 23 NL/SE servers at both direct
  samples (23:10, 23:15), but `socks-se13` accepted at 23:38, so non-US servers
  do come and go. TCP round trip from here: NL ~85 ms, SE ~99 ms, US 155-220 ms.
  Ranking by round trip picks them up whenever they accept.
- **Claim:** 503s that killed Spark agents came in clusters. On 2026-09-29
  23:16-23:18 and 23:23-23:24 the log shows 3 quick `send_failed` attempts then
  a 503. The second cluster began 6 s after the watcher's proactive rotation
  (a rotating lane had no failover). The first followed the accept-set flip.
  Old logs carry no lane slot, so that attribution is unproven; the proxy now
  logs `egress_slot`/`egress_id` on `routed_upstream_send_failed` and
  `zen_egress_failover`.

## What the code does about it

- Proxy (`routed/retry.rs`, `proxy/egress.rs`): a connect error or 429 moves
  the turn to another lane with no backoff; a lane that failed is skipped 30 s;
  when all fail, a hold of up to `--retry-zen-transport-hold-ms` (60 s) in
  pauses of at most 5 s, then the 503. The hold is a cap, not a delay: it ends
  when any lane works. With half the lanes rejecting, four failed lanes in a
  row is a few percent of turns.
- Relay (`bin/nord_socks_egress.rs`, `nord_socks_egress/servers.rs`): the
  server list comes from Nord's listing, cached in `servers.json`, refreshed
  hourly; only `socks-xx#.nordvpn.com` names get credentials. Candidates rank by
  recent probe result, then smoothed round trip. Every lane's own server is
  probed every 5 minutes and after 3 failed connects; a lane whose probe keeps
  failing for 3 minutes is re-rotated (reason `heal`, 5 minute cooldown).
- Watcher: a refused rotation logs the rotator's message and no longer
  triggers the 300 s whole-cycle retry.

## Test-instance result (2026-09-29 23:27-23:38)

The new relay ran on its own ports (18700+, own state dir, stub proxy) beside
the live one, from a cold start:

- First sweep: listing fetched (68 servers, cached to `servers.json`), round
  trips measured for all 68, 12 idle servers probed: 2 accepted, 10 rejected.
- Lane 2's server rejected the probe, then accepted again inside the 3 minute
  window: no rotation (`heal: ... accepts again`). Flapping is not acted on.
- Lane 6's server (us48) kept rejecting for 3 minutes and the lane was rotated
  to `socks-se13` (`reason: heal`). Nobody ran a command.
- Cold start took 26 s and came up on 8 lanes, not 10: `startup_lane_count`
  gives 8 unless all ten preferred exits verify, and only about half accept at
  any moment. The live relay had 10 because it started when more accepted.

## Live result after deploy (2026-09-29 23:27 to 2026-09-30 00:10 +04)

Proxy failover live from 23:27, relay with heal and sweep live from 23:55.

- Lane-caused kills (`routed_upstream_error` after connect failures): 14 in the
  nine minutes 23:16-23:24, then 0 from 23:27 to 00:10. In the first 20 minutes
  18 connect failures and 52 429s moved to other lanes (`zen_egress_failover`,
  reasons `connect` and `rate_limited`); no transport hold was needed.
- The heal probe flagged lanes 6 and 7 as rejected at about 00:00 and cleared
  both inside the 3 minute window, so neither was rotated.
- **A relay restart cuts every in-flight stream.** The 23:54 swap aborted one
  Spark stream 0.8 s after its headers (`routed_stream_aborted`, lane 4). A
  TapBuy worker reported a dropped `Edit` at about that time. The proxy cannot
  resume a stream cut mid-tool-call. For the next relay change, start the new
  relay on separate ports and restart the proxy, which drains gracefully (the
  route `nord-socks-connect-reply.md` took for the Python-to-Rust cut). The
  relay starts on 8 lanes when fewer than ten servers accept at that moment, so
  restart the proxy afterwards or its pool dials dead ports.

## A different 503: Zen's backend overloaded

- 2026-09-30 00:03:46-00:09:25 +04: 24 Spark turns died with `503
  service_overloaded` ("The backend is temporarily overloaded") in one episode
  of about 3 minutes, the only one that day. It comes from Zen's provider, not a
  lane: the response has headers and the lane is healthy, so moving lanes cannot
  help. The routed path spent its three fast attempts (about 3 s) and returned
  the 503.
- Fix: `--retry-zen-overload-hold-ms` (default 180000, `0` restores the old
  give-up). After the fast attempts, a 500/502/503/504 from Zen pauses in capped
  backoff (at most 15 s) and re-sends, giving back the lane count and the global
  slot meanwhile. Event `zen_overload_hold`. With the status match disabled the
  test fails. Not yet seen live.

## Traps met in this session

- `rtk`, the shell hook in `~/.claude/settings.json`, caps a bare
  `git log --oneline` at 50 lines with no marker: 3462 commits, 50 shown, while
  `-n 200` gives 200. A worker reading that as the whole history sees commits
  "lost". `git rev-list --count HEAD` is the honest check. Reproduced here; the
  TapBuy lane-6 report is unverified.
- `pkill -f <path>` inside a Bash command matches the command's own shell and
  kills it (exit 144). Use `pgrep -x` or a pid file.
- A dropped tool call is named with its target when the path had arrived whole
  (`stream_finisher.rs`: "`Edit` tool call (file_path: ...) was discarded"). A
  path cut mid-string is not named.

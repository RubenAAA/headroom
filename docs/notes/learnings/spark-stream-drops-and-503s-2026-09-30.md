# Spark stream drops and 503s in the TapBuy fleet report (2026-09-30)

Source: a peer report from the TapBuy audit fleet (7 Spark lanes through this
proxy) listing agent kills. Log window: `headroom-proxy.log` and `.log.1`,
2026-09-29 12:04Z to 2026-09-30 17:39Z. Local time is UTC+4.

## 503 "routed upstream error: error sending request" (fixed)

`failed to connect to routed upstream` reached the client 400 times on
2026-09-29 (hourly counts 9 to 57) and daily back to 09-25. After `61a35ff3`
(lane failover, transport and overload holds; 2026-09-29 20:42Z) the count on
2026-09-30 is 0. One upstream 503 (`service_overloaded`) passed through that
day, after the retry budget. Routed 5xx are retried up to the attempt limit
(`routed/retry.rs`), so a lone provider 500 is one that outlasted it.

## "[truncated: the connection to the API dropped mid-response]" (partly ours)

2026-09-30: 77 `stream_tail_synthesised`. 58 are the spinner sidecar, which no
worker sees. 19 are real Spark turns. Every cause reads `peer closed connection
without sending TLS close_notify`, so the far end of the lane (a Nord SOCKS
exit or Zen) closed the socket. The relay log shows 431 `PermissionDenied`
refusals from Nord SOCKS hosts, so lane churn is provider-side.

- 10 of the 19 fell 2 to 21 s after `shutdown_started`, across 6 of the day's 7
  restarts, although the drain limit is 600 s. Most of those restarts came from
  this session's flag and binary changes. Mechanism unproven: the restart
  script does not touch the relay (`egress-relay env` only reads status), and
  `main.rs` drains through axum's graceful shutdown. Not tested: whether a
  stream survives a SIGTERM to a scratch proxy sharing the lanes. Until that
  is known, restart the proxy while the fleet is idle.
- The other 9 are scattered, 2 of them beside a Zen 429 lane failover.
- `routed/early_stream_retry.rs` re-sends a drop that comes before the first
  8 KiB (`--retry-stream-hold-bytes`). Later drops cannot be re-sent without
  splicing two generations, so they end in the truncation marker.

## `headroom_retrieve` with a filename or truncated hash (fixed)

A worker passed `bs3ql8pjz.txt` or a short hash and got only the "not a valid
CCR hash" note; it then ended its run (4+ zero-write kills). The miss path now
searches the value as keywords in the current project first
(`answer_malformed_hash_as_query`, event `ccr_malformed_hash_query_hit`) and
returns the note only when nothing matches. A word that is not in the index
still gets the note, so the model's own stop remains possible.

## Not headroom

Opus session-limit 429 (Anthropic account limit), stale BASE pins and the
redaction-token BASE (prompt generation), cross-tree mirror writes (worktree or
orchestrator path handling), and the doc-drift hook on a dotfile (TapBuy's
checker).

## Zen 429s: pinned sessions kept trying a limited lane (fixed)

2026-09-30: 1,346 429s hit 1,051 requests; every one recovered by lane
failover and none reached a client. Each wasted attempt took a median 1.2 s to
headers (p90 2.4 s, p99 5.3 s; 36 minutes in total), against a median 3.1 s for
an answered attempt. A session keeps its lane after a 429 ("the assignment
stays"), so each of its turns tried the limited lane first. Slot 8 took 573 of
the 1,346. A lane that 429ed did so again within 10 s in 37% to 73% of cases
and within 60 s in 76% to 100%.

Fix: a 429 marks the lane for `LANE_LIMITED_MS` (15 s, `proxy/egress.rs`), and
`zen_client_for_lane` passes a marked sticky lane over for new turns, as it
already did after a failed connect. The assignment stays. The 15 s is a guess
inside that data: the gaps above were measured while turns kept retrying, so
they bound the limit's length from below only.

New log fields to judge it: `egress_slot` on `routed_upstream_response_headers`
(answered attempts per lane, so the per-lane 429 share can be computed), and
`zen_turn_failed_over` (one line per turn that changed lane: `wait_ms` to the
answer's headers, `failovers`, `final_slot`). Compare the 429 count per hour
and per request with 2026-09-30 under similar fleet load. Unknown: whether the
limit is per exit IP for good, and whether retrying a limited lane lengthens
its limit.

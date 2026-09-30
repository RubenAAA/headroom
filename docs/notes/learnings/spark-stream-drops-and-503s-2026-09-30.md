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
  restarts, although the drain limit is 600 s. **Cause: the rotation watcher,
  not the restart.** A scratch proxy given SIGTERM mid-stream finished its
  stream, and a second proxy started mid-stream left it alone; the restart
  script does not touch the relay (`egress-relay env` only reads status). But
  after a restart the new proxy reports 0 in flight on `/debug/inflight`
  while the old one still drains, so `zen-rotate-watch.sh` (hourly proactive
  rotation, `DRAIN_SECS=90`) saw an idle lane, rotated its exit and reset the
  old process's streams. All 10 casualties fell in the same second as a
  watcher rotation.
- Fix: `old_proxy_draining` in `contrib/zen-rotate-watch.sh` makes `drain()`
  refuse while a second process listens on the proxy's address (`pgrep -fc`
  on `--listen 127.0.0.1:<port>`). Tested with fake processes only. The two
  old watchers were killed by pid before the 19:59 restart so the new ones run
  the guard; a watcher started before a script edit keeps running the old
  text, so restart it after any edit. Not yet seen live: a refusal in
  `~/zen-rotate-watch.log` ("older proxy is still draining"). From 19:59 to
  20:19 local no real Spark turn aborted; all 11 aborts were the spinner
  sidecar (`space-bunny-free`, "error decoding response body"), which no
  worker sees and which carries no slot yet (`egress_slot` reads -1 there).
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

Follow-up, same day: a flat 15 s was too short. A lane that keeps answering 429
now stays marked for 15 s, 30 s, 60 s, then 120 s (`mark_limited`,
`limited_streak`); any answered request on the lane resets the streak
(`mark_answered`). A mark still running is not extended.

New log fields to judge it: `egress_slot` on `routed_upstream_response_headers`
(answered attempts per lane, so the per-lane 429 share can be computed) and on
`routed_stream_aborted`, and `zen_turn_failed_over` (one line per turn that
changed lane: `wait_ms` to the answer's headers, `failovers`, `final_slot`).

Result so far (live from 19:06 local, escalation from 19:59): about 13% of
requests hit a 429 in the first windows, against about 22% before, and the
failed-over turns' median `wait_ms` was 5.3 to 11.3 s. In the 20:09 and 20:19
windows only lanes 0, 1 and 3 answered; lanes 2 and 4 to 8 returned 429 every
time they were tried. That is Zen limiting those exits, so the fix can only
avoid them faster. The long `wait_ms` may be turns sleeping behind the
parked gate before they fail over (`wait_behind_parked_host`); unverified.
Compare a full day's 429 count with 2026-09-30 under similar load. Unknown:
whether the limit is per exit IP for good.

## Shadow-tool calls on tool-less turns (note added, not solved)

A turn whose client sent no tools carries the gate's five shadow tools, and
Zen refuses any `tool_choice` but `auto`, so the request cannot forbid a call.
`note_no_client_tools` (`routed/tool_alias.rs`, called from
`routed/translation.rs`) now adds "No tools are available in this
conversation. Reply in text and do not call any function." to the first
developer item on those turns only.

Probe (`claude-muse-spark-1.3`, streaming, no tools, old binary against new,
runs that ended in `tool_use` or empty text count as bad):

| Prompt | Without the note | With the note |
|---|---|---|
| sum and product of 1 to 15 | 3 of 6 bad (`bash`) | 1 of 4 bad |
| "which lines of notes.txt say deadline" | 6 of 6 bad (`glob`, `bash`) | 4 of 4 bad (`glob`) |
| numbers 1 to 60 | 0 of 6 | not rerun |

Small samples. The note helps where the task can be done in text and does
nothing where the prompt names a file, because the model then wants a tool and
ignores the note. A real tool-less request (title, summary) did not get
measured: Zen held every request from about 20:30 to 21:00 local, so the title
and files prompts timed out on both binaries.

The fix that would finish it: treat a shadow call on a turn with no client
tools as a proxy-handled call, answer it with "no such tool, reply in text" and
continue the turn, the way `headroom_retrieve` is continued
(`routed/ccr.rs`, `proxy/ccr_response.rs`). It has to work on the streaming
path after thinking has been sent, which is why it was not done here.


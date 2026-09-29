# Idea: keep hidden CCR continuations behind the client-visible cache boundary

- **Status:** first-round policy shipped behind `--ccr-keep-client-boundary-rounds`
  (default 1) 2026-09-29, unmeasured live. Keep open until the check below
  reads the after-deploy window.
- **Source:** live recache audit in
  `recache-provider-reasons.md` (2026-09-29 window); code in
  `crates/headroom-proxy/src/proxy/ccr_response.rs` and
  `crates/headroom-proxy/src/proxy/continuation.rs`.
- **Summary:** Three `aftershock_of_continuation` recaches cost 8,814 tokens
  today. Attribution identifies the proxy's hidden continuation as the source.
  The current policy moves the newest cache marker onto proxy-private
  assistant/tool-result messages. The next client turn does not contain those
  messages, so it cannot match the boundary the hidden round wrote.
- **Value:** determine whether protecting the client-visible breakpoint saves
  more cache cost than the internal CCR cache reuse that the current policy
  provides.
- **Next:** add a test fixture for marker placement across a hidden CCR round,
  then compare current placement with a CCR-only experiment that leaves the
  newest marker at the client-visible boundary. Measure both following-client
  reads and internal-round reads/writes before considering a default change.


## Today's evidence (2026-09-29, Asia/Yerevan)

| Time | Wasted | Actual / expected read | Write | Idle | Suspect |
|---|---:|---:|---:|---:|---|
| 13:24:36 | 4,113 | 120,994 / 125,107 | 10,093 | 18.49s | yes |
| 13:24:44 | 3,127 | 120,994 / 131,087 | 3,127 | 8.58s | yes |
| 13:33:15 | 1,574 | 143,777 / 154,304 | 1,574 | 7.20s | yes |

All three events have `origin=previous_turn`, `scope=replayed_prefix` and
`attribution_reason=aftershock_of_continuation`. The attribution code calls
this the proxy-caused case: hidden retrieval rounds committed a prefix the
next client request cannot match. All three are also marked as
`commit_race_suspect`; that timing flag does not explain away the proxy-created
boundary.

## Mechanism

`build_ccr_continuation` appends the assistant continuation and tool results
to the upstream request. It then calls `retail_continuation_breakpoint`.
For Anthropic with cache-tail breakpoints enabled, that helper moves the
newest existing marker to the new continuation tail. It relocates rather than
adds a marker to stay within Anthropic's four-marker cap. This gives the next
hidden retrieval round a cacheable prefix through the internal tool output.
The helper is also called by memory continuations, so any experiment must be
gated at the CCR call site and must leave memory-continuation behavior alone.

The client never sent that appended history. The next client request is built
from client-visible messages, so its longest reusable prefix can end before
the hidden continuation's marker. That is the exact shape named by the
`aftershock_of_continuation` attribution. The current
`hidden_ccr_continuation_does_not_become_next_client_cache_baseline` test
protects the usage-accounting baseline, but it does not assert where the
provider cache marker landed.

This is a tradeoff, not yet a demonstrated net win. Leaving the marker at the
client-visible boundary should preserve the following client turn's reusable
prefix, but may make the hidden retrieval round rewrite or fail to cache its
own appended messages.

## Reproduce and measure

### Live-log reproduction

Run the sanitized Python audit snippet in `recache-provider-reasons.md` against
`~/headroom-proxy.log`. Filter its output to
`aftershock_of_continuation`. It emits the three rows above using only
structural usage fields; it does not print prompts, request IDs, conversation
keys, or session hashes. The attribution is recorded on
`cache_recache_observed`.

### Deterministic fixture

Add a marker-placement fixture next to
`hidden_ccr_continuation_does_not_become_next_client_cache_baseline` in
`crates/headroom-proxy/src/proxy/tests.rs`. The existing
`a_turn_after_a_continuation_names_the_previous_turn` test in
`crates/headroom-proxy/src/cache_stabilization/usage_observer.rs` already pins
the attribution rule; the hidden-baseline test pins accounting. Neither test
currently pins marker placement:

1. Make an Anthropic request whose client-visible history has two tail cache
   markers and a tool call. Keep the marker count below four.
2. Call `build_ccr_continuation` with an assistant continuation and tool
   result. Assert the current policy places the newest marker after the
   proxy-private messages and still stays at or below four markers.
3. Build the next request from the client-visible history only, omitting the
   internal continuation messages. Use the 13:24:36 observation as the
   fixture's accounting example: actual read 120,994, expected 125,107,
   recache shortfall 4,113. With previous-turn continuation evidence set, the
   observer should attribute this short read as `aftershock_of_continuation`.
4. In an experimental CCR-only policy, skip marker relocation and simulate a
   provider read at the client-visible baseline; assert no recache is emitted.
   If the experiment still gets a short read, it should remain attributed as
   an aftershock. Keep client-visible message content identical across the two
   policies; allow only the cache-control marker location to differ.
5. Compare current and experimental policy over CCR requests: subsequent
   client `cache_read_input_tokens` and aftershock waste; internal-round
   cache reads and writes; and total cache cost per successful retrieval.
   Retain the experiment only if the total improves, not just the next-turn
   read.

The existing test proves hidden-round usage is not used as the next client
baseline. It is not yet a reproduction of the cache-boundary mechanism; the
fixture above must assert marker placement and the subsequent-turn outcome.
A body-capture integration test can pin the forwarded marker location, but
only a provider-backed comparison can establish the cache hit and write
tradeoff.

## Decision gate

Keep this open until the fixture distinguishes the policies and a live
comparison includes both hidden-round and client-turn cache usage. If leaving
the marker in place simply transfers the same write cost to the hidden round,
or reduces internal CCR reuse by more than it saves on the client turn, close
as measured and leave the current placement unchanged.


## 2026-09-29 correction: the ledger cannot answer this yet

An earlier pass read `turn_cost_ledger` and concluded the final hidden round
never reads round 0's write (1,212 of 1,212 turns). That reading was wrong.
On streamed CCR turns the ledger books exactly twice round 0's cache counts
(`ccr_continuation_usage` shows `rounds=1` with counts equal to the client
baseline; reads and writes both double exactly). The final round's own usage
is not visible there, so nothing in the ledger says whether it read round 0's
write. The 59% "next read stuck at round 0" figure came from the same rows and
is not evidence either.

Also, most `aftershock_of_continuation` events are not CCR: 13 of 14 events
that followed a server-tool turn (2026-09-24..29, 135,691 tokens) read at or
above the boundary of the turn before it. See
`recache-provider-reasons.md`.

Shipped, off by default: `--ccr-keep-client-boundary-rounds` (0 = old
placement; 1 keeps the marker on the client's last block for the first hidden
round). Marker placement is pinned by
`ccr_continuation_keeps_the_client_boundary_for_the_first_round`.

New events for the measurement this note asks for:

- `ccr_round_usage` (`kind=superseded|terminal`, `round`, `has_usage`, the
  response's own input/read/write). A `terminal` line with `has_usage=false`
  confirms the final round is dropped; with counts it is the first direct
  reading of what the final round read and wrote.
- `ccr_continuation_markers` (`round`, `kept_client_boundary`, `markers` as
  `m<msg>.<block>`, `messages`): where the markers sat on each hidden request.

Decision rule once a few dozen CCR turns have both events: for turns with one
hidden round, compare the terminal round's read against round 0's read plus
write. If it reads that write, placement is not the loss; leave the flag at 0.
If it does not, raise the flag to 1 and compare the next client turn's read
(`turn_cost_ledger` of the following non-CCR turn) with and without.


## 2026-09-29 evidence: 9 of 9 aftershocks are CCR turns (after the summed-usage fix)

Window 12:15-13:59 UTC after the restart with `ccr_round_usage`. Nine
`aftershock_of_continuation` recaches, 39,826 tokens, every one following a
CCR turn (`ccr_continuation_usage rounds=1`); none followed a server-tool turn
(11 `usage_baseline_skipped`, 0 `unexplained_after_replay`).

For each, expected read = the terminal round's read + write, and the next
client turn read the terminal round's read (+ at most 1,060 tokens) and none of
its write: e.g. read 44,068 / write 8,113 -> next turn read 44,068; read 85,424
/ write 11,354 -> 85,424; read 127,383 / write 11,870 -> 128,443. Writes were
8-22k tokens. The terminal (streamed) round does report usage
(`has_usage=true`), which corrects the section above; the superseded round is
the one with `has_usage=false`. `ccr_continuation_markers` shows
`kept_client_boundary=false` with the newest marker on the last (hidden)
message on every one.

So the relocated marker writes a tail the client never resends. The
`--ccr-keep-client-boundary-rounds 1` policy targets exactly that. Not enabled
in `contrib/headroom-flags.sh` yet; after enabling, the check is aftershock
count and wasted tokens per CCR turn against this window (9 in ~45 min).

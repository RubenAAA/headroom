# Idea: stop a stabilized-away client edit from unlocking a history rewrite

- **Status:** open (diagnosed from one incident; the policy change needs a
  measurement window before it ships)
- **Source:** 2026-09-15 investigation of `cache_recache_observed
  attribution_reason=tools`; see `gate-prior-thinking-drop.md` (implemented) for
  the gate this would finish, and `prior-thinking-billing-question.md` for the
  billing side.
- **Value:** one observed turn re-created 103,884 tokens of prefix that was
  still cached. Unknown how often; that is the measurement.
- **Next:** run the join in *The measurement* below. The fields it needs now
  exist; it wants a 2–3 day window.

## The chain

`rebuild_boundary` is set from the **inbound** drift detector
(`proxy.rs`, `rebuild_boundary = drift_dims.is_some()`), i.e. from the body the
client sent. Every stabilizer that exists to absorb a client edit — the tool
roster pin above all — runs much later, on the way out. So a client edit the
proxy is about to hold back still reads as a rebuild boundary.

A rebuild boundary then does two things, both of which assume the provider's
cached prefix is already dead:

1. drops the lane's stored prefix and every alternate
   (`prefix_replay_invalidated_on_rebuild`), so this turn cannot replay;
2. unlocks the prior-thinking drop and the history offload, which rewrite the
   prefix.

When the boundary is real, both are free — the provider rewrites that prefix
whatever we send. When the stabilizer absorbs the edit, the prefix was alive,
and step 2 is what kills it.

Observed 2026-09-15, session `e1afda21`, one turn:

| field | value |
| --- | --- |
| inbound `drift_dims` | `tools` (client dropped `WaitForMcpServers`, 28 → 27) |
| outbound `drift_dims` | `early_messages` — never `tools` |
| forwarded `tools_digest` | `71dbf434`, unchanged across the turn |
| `prior_thinking_dropped` | 63 blocks, 110,628 bytes, `rebuild_boundary: true` |
| `replay_skipped` | `no_previous_turn` (step 1 had just run) |
| expected / actual cache read | 163,657 / 21,216 → 103,884 re-created |
| deferral savings that turn | 10,473 |

The roster pin did its job: the forwarded tools went out byte-identical. The
turn still paid a full rewrite, because the boundary that unlocked the rewrite
was measured on the body the pin had not touched yet.

## Why the existing gate did not catch it

`thinking_drop_is_free(rebuild_boundary, forwarded_agreement_len)` is
`rebuild_boundary || agreement.map_or(true, |n| n <= 1)`. The second operand was
added (`gate-prior-thinking-drop.md`) precisely to refuse the drop while the
provider still holds the history.

It could not fire. `forwarded_agreement_len` reads
`tracker.last_forwarded_messages` and returns `None` when that is empty;
`PrefixReplayStore::invalidate` — step 1 above, on the same lane key, ~370 lines
earlier in the same request — clears exactly that field. So on every
`rebuild_boundary` turn the agreement read `None`, which the gate spells "nothing
cached to lose". The note's own supporting figure ("`forwarded_agreement_len`
reads 0 on 11 of 12 `prior_thinking_dropped` events") was reading a value the
proxy had erased one step earlier, not a property of those turns.

Fixed 2026-09-15: the agreement is now read before the invalidation and the same
reading feeds both the gate and the log field, so the number in the log is
evidence again. The `rebuild_boundary` arm still short-circuits past it, so this
changed no decision — it made the decision auditable.

## What is left, and why it is not obvious

The remaining question is whether a boundary that the stabilizers absorbed
should unlock the rewrite at all. Two things make it harder than it looks:

- **The evidence arrives too late.** Whether the *forwarded* hot zone moved is
  known only at `observe_outbound_drift`, on the way out, long after the
  boundary has been consumed. Answering it earlier means predicting the outbound
  body.
- **A pin preview does not work where it is needed.** `RosterPinStore` remembers
  post-pruning rosters, and pruning, tool-search deferral and memory injection
  all run after the point where `rebuild_boundary` is decided. Previewing the
  pin against the client's raw `tools[]` would call most of the roster "new" and
  answer "would change" on every turn, so the check would never fire.

Candidate shapes, in rough order of cost:

1. **Defer, don't suppress.** Let the boundary invalidate the replay store as it
   does today, but hold the thinking drop and the history offload until a turn
   whose *outbound* lane confirmed a hot-zone move. Costs one turn of latency on
   a real boundary; costs nothing on an absorbed one.
2. **Carry the previous turn's outbound verdict.** Store whether the last turn's
   forwarded hot zone actually moved and gate step 2 on that instead of on the
   inbound reading. One turn stale, but on real evidence.
3. **Narrow the boundary.** Exclude a `tools`-only inbound drift from
   `rebuild_boundary` while the roster pin is enabled. Cheapest, and wrong
   whenever the roster genuinely changed — which the pin explicitly passes
   through (new tools, schema edits).

None should ship on one incident. The measurement below decides whether any is
worth the risk, and which.

## The measurement

Join `prior_thinking_dropped` to `cache_recache_observed` on `request_id`. Both
sides carry what is needed as of 2026-09-15:

| from | field | says |
| --- | --- | --- |
| `prior_thinking_dropped` | `rebuild_boundary` | the drop rode a boundary |
| `prior_thinking_dropped` | `forwarded_agreement_len` | how much history the provider still held — honest now, read before the invalidation |
| `prior_thinking_dropped` | `bytes_removed`, `blocks_removed` | what the drop saved |
| `cache_recache_observed` | `outbound_drift_dims` | what moved on the body the provider keyed on (`""` = nothing moved, `?` = no comparison) |
| `cache_recache_observed` | `forwarded_head_moved` | `1` real boundary, `0` absorbed, `-1` unknown |
| `cache_recache_observed` | `wasted_tokens` | what it cost |

Then:

- **Cost of suppressing nothing** — drops with `rebuild_boundary=1` and
  `forwarded_head_moved=0`. The boundary was absorbed, the prefix was alive, and
  `wasted_tokens` is what the rewrite cost. This is the incident's bucket.
- **Cost of suppressing** — drops with `forwarded_head_moved=1`. The boundary
  was real, the rewrite was free, and suppressing would have kept
  `bytes_removed` in the prefix for nothing.
- **Silent successes** — drops with no matching recache event. The prefix hit
  anyway; they cost nothing either way and belong in neither column.

Ship a candidate only if the first column is materially larger than the second.

`forwarded_head_moved` deliberately ignores `early_messages`. The drop and the
history offload both rewrite messages, so on a turn where they fired that
dimension is partly our own doing; `system` and `tools` are untouched by both
and still speak for the provider. Counting raw dim disjointness instead would
score the drop's own footprint as evidence that the boundary was absorbed.

Not added, and not needed for the join: `request_id` on `cache_drift_observed`
and `cache_drift_observed_outbound`. Both lanes now reach
`cache_recache_observed`, which has `request_id`, so the drift events only
matter for turns that did not recache — and those are the silent successes.

# Idea: stop a stabilized-away client edit from unlocking a history rewrite

- **Status:** open, but parked 2026-09-21 — six days of traffic yielded five
  classifiable events. The phenomenon is rare, not under-instrumented; more
  waiting will not settle it. See Findings at the bottom.
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

## Findings 2026-09-18 (3-day join, Sep 15–18 archives + live log)

Ran the join over 521 `prior_thinking_dropped` events. One data-quality
note first: `forwarded_head_moved` / `outbound_drift_dims` only ride
recache lines from the newest builds, so the Sep 15–17 `tools`-drift
busts (69k, 64k, 102k waste, plus the original 104k incident) are
unclassifiable — consistent with the incident shape, provable for none
of them.

Classifiable set (new-build lines only):

| bucket | n | tokens |
|---|---|---|
| absorbed (`rb=1`, `head_moved=0`) | 3 | 70,506 waste |
| real (`head_moved=1`) | 1 | 1,608 kept-for-nothing |
| silent (no recache) | 392 | prefix hit anyway |

One of the three absorbed rows is ambiguous (`early_messages` on both
sides — the metric deliberately ignores that dimension, so a real
early-message boundary reads the same). Clean absorbed ≈ 37k vs 1.6k
real: the ratio favors candidate 1, **defer, don't suppress**, but n is
tiny on both sides. Do not ship yet — re-run once classifiable volume
accumulates (fields only started flowing recently). The machinery works;
the sample does not yet carry a rollout.

## Findings 2026-09-21 (6-day re-run) — rare, not under-instrumented

Re-ran the join over 2026-09-15 to 09-21, Anthropic models only: 307
`prior_thinking_dropped` events.

| bucket | n | tokens |
|---|---|---|
| silent (no recache) | 249 | prefix hit anyway |
| no boundary (`rb=0`, `head_moved=0`) | 31 | 32,652 waste |
| unclassifiable (old build line) | 22 | 334,788 waste |
| absorbed (`rb=1`, `head_moved=0`) | 4 | 84,683 waste |
| real (`head_moved>=1`) | 1 | 15,673 kept-for-nothing |

Three extra days bought **one** classifiable event. The 2026-09-18 reading
had 3 absorbed and 1 real; this has 4 and 1. Volume is not the blocker — the
event is simply rare, so waiting longer will not change the picture.

The direction holds (absorbed outweighs real, now 84,683 against 15,673, a
5.4× ratio favouring candidate 1: **defer, don't suppress**). But at five
classifiable events in six days the whole phenomenon is worth under 100k
tokens a week, against 3.78M of recache waste over the same window. Do not
ship the policy change on this evidence. Either accept it as a known small
leak, or come back when a single incident makes it expensive again.

One shape worth noting for whoever picks this up: 31 drops rode no boundary
at all (`rebuild_boundary=0`, `forwarded_agreement_len=0`), wasting 32,652.
The gate lets those through on the agreement arm, which is the same
`last_forwarded_messages`-empty path the section above describes.

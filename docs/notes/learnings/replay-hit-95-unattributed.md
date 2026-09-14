# Learning: replay hits 95%, residual needs stream ids

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §21 (328 clean / 16 busts)
- **Claim:** multi-stream store cleared as cause (6 alternate matches all clean); 10-bust residue unreadable — TTL gaps, first turns, or merged streams indistinguishable without a per-stream identifier.


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 21 — Replay hits 95% of the time; the residual cannot be attributed yet

Measured 2026-08-09, 16:00–16:55Z, grouping by `conversation_key` from
`turn_cost_ledger` rather than by message count.

| full replays | n |
| --- | --- |
| clean hit — under 5K written, over 50K read | 328 |
| bust — 60K or more written | 16 |

The 16:

- 5 followed an idle gap past the provider's 5-minute TTL (up to 1472s). The
  bytes were gone whatever the proxy held.
- 1 was a genuine first turn on its key.
- 10 remain unexplained.

**The multi-stream store is not the cause.** `prefix_replay_matched_alternate`
fired 6 times in the window and every one of them landed among the 328 clean
hits; none of the 16 busts matched an alternate. Replaying another stream's
prefix was the obvious suspect and the data clears it.

**Why the last 10 cannot be settled.** Every one reads exactly the
system-and-tools block (22,026 / 22,277 / 31,028 / 31,086 recur across
different conversations) and rewrites every message. Their apparent inter-turn
gaps are 0–145s, which would make them warm — but `conversation_key` is
`SHA256(session_key + messages[0])`, so subagents forked from one parent share
a key. Item 11 fixed the replay *store* to hold several prefixes; it did not
change the key. Gaps of 0 and 1 second are therefore probably different streams,
not one stream taking two turns.

Candidates, in the order worth testing: streams merged by the key; provider
cache-write visibility under the concurrent subagent bursts these all sit in
(item 3b); something real in the proxy. Distinguishing them needs a per-stream
identifier on the turn events, which nothing currently emits.

Not chased further — the instrument does not exist yet, and guessing between
three candidates is how item 19 went wrong.

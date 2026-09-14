# Rejected: partial prefix replay (built, measured, reverted)

- **Status:** tried and rejected 2026-08-09
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §19
- **Summary:** replaying `prev_fwd[..k]` onto a diverged tail cost 204,768 tokens on first firing. A declined replay is not a bust (deterministic compression reproduces cached bytes); the seam matches neither turn.


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 19 — Partial prefix replay: built, measured, reverted

Tried and rejected 2026-08-09. Recorded because the reason overturns an
assumption several other items rest on.

The idea was obvious enough: `overlay_cached_prefix_reported` refuses the whole
stored prefix on any mismatch, so replay `prev_fwd[..k] ++ optimized[k..]`
where `k` is the first disagreeing message, and stop there. Implemented that
way, with the divergence as a hard cap.

Its first live firing, on one conversation, consecutive turns:

| time | msgs | outcome | cache_read | cache_write |
| --- | --- | --- | --- | --- |
| 16:03:17 | 308 | full replay | 224,689 | 981 |
| **16:03:32** | **313** | **partial, k=241** | **22,026** | **204,768** |
| 16:03:45 | 316 | full replay | 226,794 | 1,855 |

The gap was 15 seconds, so the 5-minute TTL is not the explanation.

### The assumption that was wrong

A declined replay is not a bust. Compression is deterministic, so a turn's own
freshly compressed bytes for an unchanged prefix reproduce the bytes the
provider already holds. The same conversation shows it directly:

| 16:02:41 | 302 | `no_previous_turn` — replayed nothing | 222,975 | 342 |

A turn that replayed nothing still read 222,975 tokens from cache. Replay earns
its keep by holding bytes stable when compression *would* have drifted, not by
rescuing a prefix that decline throws away.

Splicing one turn's bytes onto another turn's tail makes a seam that matches
neither, which is the one reliable way to actually lose the prefix.

### What this costs elsewhere

Item 17 says turns that skip replay were 19% of traffic and carried 97% of
booked re-cache waste. That is a correlation, and the causal reading now looks
backwards: a client edit mid-history causes both the divergence and a genuine
bust, so the skip flag marks the turns rather than causing their cost. Any
figure derived from the skip flag needs re-deriving against `cache_read`.

The 131,469-token turn that motivated this item needs the same treatment before
it is quoted again.

### Left behind

The all-or-nothing path carries a comment with these numbers, and
`overlay_declines_whole_prefix_when_a_later_message_diverges` pins the
behaviour, so the next reader who spots the "obvious" improvement finds the
measurement first.

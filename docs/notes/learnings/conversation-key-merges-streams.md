# Learning: backwards message counts mean merged streams

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §11 (settled 2026-08-09)
- **Claim:** a conversation only ever grows — counts like 17→16→35→28 under one
  `conversation_key` are two interleaved streams (subagents sharing the opener
  by construction), not thrash. 79% of events / 68% of booked tokens sat on
  such keys.
- **Evidence:** splitting `af7a42fd7eb2` as two series (12,24,38,47,60 +
  10,18,28,34,44) resolves it at once; fixed by per-key multi-stream tracking
  (`match_stream` on count only, so prefix edits still report).


## Item 5

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 5. Cache-drift flip-flop between exactly two prefixes

**SETTLED 2026-08-09 — this is item 11, and it is fixed there.** Of the three
suspicions below, the third was right: two streams share one key. Live proof is
`af7a42fd7eb2`, which alternates between exactly two prefixes and resolves into
two normally-growing conversations once the message count is read alongside the
hash. "Never a third hash" is the signature of two interleaved streams, not of
drift. Left in place below for the record.

Two sessions alternate perfectly, never a third hash, always `early_messages`:

```
session 97f195df03  54 drifts, 2 prefixes:  4d2a2630 -> d1913774 -> 4d2a2630 -> ...
session 9058166346  27 drifts, 2 prefixes:  f7cd246f -> cb2212e5 -> f7cd246f -> ...
```

Gradual drift does not look like this. Two request shapes are sharing one
`session_key_hash`.

**Suspicions, untested:** two clients colliding on one session key; or a
transform applied on alternate requests only; or main-agent and subagent
traffic hashing to the same key.

# Idea: check fingerprints before the in-flight explanation

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/savings-ideas-2.md` §4.4 (request `5e27a02e`, 2026-09-03)
- **Summary:** after a mid-stream failure the client resent with a 6 kB-shorter
  system prompt (213k write vs 15k read). `cache_recache_observed` said
  `concurrent_turn_in_flight / provider_cache_timing` because a retry was still
  in flight — but the cause was the system change, visible in
  `prefix_composition`. Later turns with changed system fingerprints: 10–11% of
  writes on 09-03 vs ~1% baseline.
> **09-11 outcome:** fingerprints checked first (usage_observer.rs:921); prefix_head_changed firing live as designed.
- **Next (superseded):** fix the attribution order — check `prefix_composition`
  fingerprints before the in-flight explanation.


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

### 4.4 System-prompt change after a stream failure

Request `5e27a02e` (conversation `b3d793f2`, 15:57:03Z) wrote 213,309
against 15,621 read. Sequence: `c81c64a0` (217 msgs, system fp
`2c01d76ae29a`, 16,125 B) got a 200 and then died mid-stream ("error
decoding response body"); the proxy synthesised a truncated tail. The client
resent 220 msgs with system fp `6f8ff11af482`, 10,145 B — 6 kB shorter —
as `ce3ff134` (two network retries, never completed) and again as
`5e27a02e` (one retry, 200). Read 15,621 is the unchanged 42.5 kB tools
block; everything after the system block was rewritten.

`cache_recache_observed` said `concurrent_turn_in_flight /
provider_cache_timing`, because `ce3ff134` was still retrying. The cause is
the system change; `prefix_composition` shows it. Client origin. Later
turns where the forwarded system fingerprint changed: 0.0% and 1.2% of
writes on baseline days, 10.3% AM, 11.4% live.

What to do: fix the attribution order — check `prefix_composition`
fingerprints before the in-flight explanation.


## 09-11 verdict

*moved from `docs/notes/savings-ideas-2.md`*

- **4.4 — fixed, firing live.** `prefix_head` fingerprints are checked
  before the in-flight explanation (`usage_observer.rs:921` ahead of
  `:1021`, with this entry's 213k incident cited in the comment):
  `prefix_head_changed` ×2 in the window, `concurrent_turn_in_flight` ×71
  behind the structural causes, as designed.

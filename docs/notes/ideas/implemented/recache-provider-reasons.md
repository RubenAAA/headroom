# Implemented: provider-side residue renamed, left alone

- **Status:** done 2026-09-02 (classification change + close decision)
- **Source:** `docs/notes/recache-classification.md`
- **Summary:** 536 `unexplained_after_replay` events (527k tokens, max single
  turn 9,161 — no tail) had byte-stable forwarded prefixes; only the
  provider's read position varied against previous boundaries. Renamed to
  `provider_missed_newest_write` / `partial_of_previous_write` /
  `free_read_not_persisted` / `dropped_older_entry` / `between_entries`
  (all `origin: unknown`), each hand-checkable from four logged numbers.
  Re-open bar: any single turn reaching five figures.


## Detail

*moved from `docs/notes/recache-classification.md`*

## `unexplained_after_replay` — closed, nothing to chase

536 events, 527,163 tokens. The largest bucket nobody had opened, and it is not
a re-cache at all.

Every event carries `matched_stream_msgs == turn_msgs == prefix_stable_msgs`:
the stored prefix matched the turn end to end, the replay went out, and the
provider read back a little less than the ledger expected. The distribution says
the same thing twice:

```
median   690        p90 2,034        p99 6,223        max 9,161
   0-  200:  12 events      2,014 tokens   0.4%
 200- 1000: 344 events    170,793 tokens  32.4%
1000- 5000: 173 events    306,104 tokens  58.1%
5000-20000:   7 events     48,252 tokens   9.2%
    20000+:   0 events
```

No tail. `prefix_content_diverged` put 1,998,513 tokens into 33 turns; the worst
single turn here is 9,161, and the total is a flat ~700 spread over 536 turns.
That is a breakpoint landing a block short of the divergence, or a 5m block
ageing out under a 1h one — the granularity of the provider's own accounting,
not a prefix we broke.

Leave it. Re-open if the max reaches five figures, which would mean something
real had started hiding behind the name.

**Update 2026-09-02.** The name is gone. A second pass over 563 events (547K
tokens, 09-01..09-02) found the forwarded prefix byte-stable in every one; the
only thing that varied was where the provider's read stopped against the two
previous turns' boundaries. The observer now names that position instead, and
logs the four numbers it read (`actual_cache_read`, `previous_cache_read`,
`previous_boundary`, `previous_previous_boundary`) so each call can be checked
by hand. The reasons are `provider_missed_newest_write` (read equals the
previous read, the newest write was not found), `provider_partial_of_previous_write`
(read stops inside the previous write), `provider_free_read_not_persisted` (read
falls back to the older boundary after the previous turn read past anything
written), `provider_dropped_older_entry` (read is below the older boundary) and
`provider_between_entries` (the rest). `event_kind` stays `unexplained` and
`origin` stays `unknown`. All five are provider-side; none is ours.


## Audit framing

*moved from `docs/notes/recache-classification.md`*

## 2026-09-02 — live audit against the 09-01 binary

The window is `~/headroom-proxy.log` from 07:55:30Z on 09-01 to 12:22Z on
09-02, one binary, subscription auth, 9,481 booked turns. The log is live, so
every count here is a total at a moment, not a fixed one: a pass four hours
earlier in the same window read 8,706 turns and 503,030 tokens of recache
waste, and that figure no longer reproduces from any prefix of the file. Quote
the window with the number or the number means nothing.

```
booked turns                     9,481
cache read               1,106,564,274
cache write                 19,260,100
hit ratio                            98.3%
recache waste                  691,637   (3.6% of writes)
```

`scripts/proxy_log_audit.py recache` now prints that total first, then a table
by `attribution_reason`. It used to headline only the events carrying
`drift_dims` — 3 events and 192,549 tokens on this window — which is a
seventieth of the events and under a third of the tokens. The reasons:

```
reason                          events      tokens   median
unexplained_after_replay           339     308,083      615
tools                                2     137,456   75,145
early_messages                       2     136,208   74,830
system                               1      55,093   55,093
concurrent_turn_in_flight          101      43,659      240
prefix_content_diverged             17       6,699      239
aftershock_of_diverged_prefix        5       4,439      238
inbound_tail_replaced                2           0        0
```


## Tail confirmation

*moved from `docs/notes/recache-classification.md`*

- **`unexplained_after_replay` is gone by rename, not by elimination.**
  The 09-02 rename into five `provider_*` reasons holds; this window:
  `provider_dropped_older_entry` 117 ev / 3.58M tokens,
  `provider_free_read_not_persisted` 61 / 1.76M, `provider_between_entries`
  47 / 109k, `provider_missed_newest_write` 12 / 5k,
  `provider_partial_of_previous_write` 1. Total 238 events / 5.45M — now the
  largest waste class by tokens. The provider-side attribution stands
  unchallenged (no new evidence either way); the 09-02 sidecar subset cannot
  be re-tested by name anymore. If this class is ever worked, it starts here,
  not at the 09-02 sidecar numbers.

# Implemented: concurrent turns named but still billed

- **Status:** done (pinned by two regression tests)
- **Source:** `docs/notes/recache-classification.md`
- **Summary:** 72% of overlapping turn-pairs lose cache vs ~5% baseline (377
  pairs, 409k tokens): the splice was right, the timing wasn't — the provider
  hadn't committed the previous write (median 3.4 s left). `recache_attribution`
  returns `concurrent_turn_in_flight` / origin `client`, read from the
  observer's pending map (not wall clock — it steps backwards under load).
  Still counted as waste (genuinely re-billed), checked after structural
  causes so a real edit wins.


## Detail

*moved from `docs/notes/recache-classification.md`*

**Concurrent turns now have a name.** 72% of overlapping turn-pairs lose cache
against a ~5% baseline (377 pairs, 408,980 tokens), and 374 of them had a replay
applied — the splice was right, the timing was not: the provider had not
committed the previous turn's write because that turn was still streaming
(median 3.4s left). These were landing in `unexplained_after_replay`.

`recache_attribution` now returns `concurrent_turn_in_flight` / origin `client`.
Read from the observer's own pending map, not from timestamps — this machine's
wall clock steps backwards under load. **Still counted as waste**: the tokens
were genuinely re-billed, and filing 409k tokens as expected would retire them
into a bucket nobody reads. Checked after every structural cause, so a real edit
still wins. Pinned by `a_turn_racing_its_own_conversation_is_named_but_still_billed`
and `a_named_cause_outranks_concurrency`.


## Honesty proof

*moved from `docs/notes/recache-classification.md`*

## `concurrent_turn_in_flight` — the name is honest

208 events, 150,652 tokens, median 241. Cheap enough to ignore, but it had been
taken on trust: the flag is set in `begin_request` from any other *pending*
entry under the same conversation key, and `pending` is a 512-slot LRU with no
timeout. A turn that never calls `complete` — client disconnect, upstream error
— leaves an entry behind that would mark every later turn on that key as
concurrent until the LRU pushed it out. That failure mode would be invisible in
the counts and would quietly absorb waste with some other cause.

It is not happening. Timing every flagged event against the nearest other turn
on its own key:

```
                          flagged (208)      other reasons (752)
sibling within  2s          44.7%                 34.0%
sibling within 10s          93.3%                 81.2%
sibling within 60s         100.0%                 94.6%
sibling beyond 60s             0                    40 events
```

Not one flagged event has its nearest sibling more than a minute away, where the
control bucket has a 5% tail out to ten minutes. A stale pending entry would
show up precisely as a flagged event standing alone in time, and there are none.

The attribution also still predicts what it claims to. Over all 29,432 turns,
splitting on whether another turn on the same key completed within 2s:

```
overlapping    2,034 turns    349 re-cached   17.16%
alone         27,398 turns    611 re-cached    2.23%
```

Overlap raises the re-cache rate 7.7x. The mechanism in the code comment is
real, the label points at it, and the bucket is small. Nothing to do.

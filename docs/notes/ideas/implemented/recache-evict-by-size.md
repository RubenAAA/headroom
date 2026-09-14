# Implemented: evict smallest streams first

- **Status:** fixed 2026-08-23 (pinned by
  `the_budget_goes_to_the_stream_most_likely_to_return`)
- **Source:** `docs/notes/recache-classification.md`
- **Summary:** recency was the wrong signal — a parent blocked behind fan-out
  ages exactly like a finished one. Return probability rises monotonically
  with cached size (log-log r = 0.54 over 1,080 streams; 120k+ streams return
  92% vs 0% under 10k). Eviction now size-descending with recency tiebreak,
  skipping rather than stopping.


## Detail

*moved from `docs/notes/recache-classification.md`*

### Eviction order — fixed too

The raised ceiling does nothing for a session that genuinely exhausts the
4,000-message budget; recency ordering still dropped the parent first. Recency
turned out to be the wrong signal outright. An entry's position records how
many *other* streams have taken a turn since, so a parent blocked behind a
fan-out ages exactly as fast as one that has finished.

Size is the better predictor, and the logs say so plainly. Grouping turns into
streams by `(conversation_key, chain_id)`, the chance a stream ever takes
another turn against how much it has cached:

| stream size (cached tokens) | n | median turns | P(>=2 turns) | P(>=10) |
|---|---|---|---|---|
| 0-10k | 30 | 1 | 0% | 0% |
| 10-30k | 68 | 1 | 16% | 1% |
| 30-60k | 224 | 2 | 51% | 8% |
| 60-120k | 483 | 5 | 76% | 28% |
| 120k+ | 275 | 13 | 92% | 57% |

Monotonic across every bucket, log-log r = 0.54 over 1,080 streams. Because the
budget is counted in messages and tokens track messages, value per unit of
budget is exactly that probability, so the budget belongs to the large streams
— and dropping a small one costs close to nothing, since it was never coming
back.

Eviction now selects by size descending with recency as the tiebreak, and skips
rather than stops, so a small stream can still use the room a rejected large one
left. Pinned by `the_budget_goes_to_the_stream_most_likely_to_return`, which
fails under recency-only ordering.

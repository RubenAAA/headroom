# Implemented: background dedup-index build with unindexed-lookup skip

- **Status:** shipped in the store
- **Source:** `docs/ctx-sessions-dedup.md`
- **Summary:** first open builds the dedup index inline under 200k rows, else
  background thread on a second connection (`ctx_dedup_index_missing` →
  `ctx_dedup_index_built`); until it lands, `insert_event` skips the duplicate
  lookup (unindexed probe 23.4 ms vs 0.003 ms indexed — and the cost grows with
  the conversation's own history, the wrong shape).


## Detail

*moved from `docs/ctx-sessions-dedup.md`*

The store builds its own dedup index at first open. On anything under 200,000
rows it does so inline, in microseconds. Above that it hands the build to a
background thread on a second connection and logs `ctx_dedup_index_missing`,
then `ctx_dedup_index_built` with the elapsed time. Until that lands,
`insert_event` skips its duplicate lookup rather than paying for an unindexed
one. Measured on a copy of the 9.7 GB file:

| | value |
|---|---:|
| `CREATE INDEX` | 73.2 s |
| lookup, no dedup index | 23.4 ms |
| lookup, dedup index | 0.003 ms |
| rows in the worst (session, type) group | 27,224 |

The unindexed lookup is not a full table scan — `idx_session_events_type`
narrows it to one session and type first — but its cost still grows with the
conversation's own history, which is the wrong shape. The `DELETE` below is
what makes the index worth building at all.

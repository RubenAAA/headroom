# Shrinking a sessions DB that grew under the duplicate-capture bug

Until the fix on this branch, the CTX-2a capture path re-extracted the whole
conversation on every request and inserted every event again. The largest
project DB reached 9.7 GB:

| | value |
|---|---:|
| `session_events` rows | 3,907,865 |
| distinct `(session_id, type, data_hash)` | 47,557 |
| file size | 9.7 GB |

New writes are already correct once the proxy restarts. The rows already on
disk are not, and nothing removes them at start-up: `ProjectStores::sessions`
is reachable from the request path through the recall injection engine, so a
start-up cleanup would stall live requests.

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

## Run it offline

Stop the proxy first. `VACUUM` rewrites the file, so keep room for a second
copy of it while it runs.

```sql
-- Keep the earliest row of each (session_id, type, data_hash); drop the rest.
DELETE FROM session_events
 WHERE id NOT IN (
   SELECT min(id) FROM session_events
    GROUP BY session_id, type, data_hash
 );

-- The lookup index insert_event wants. Not unique: the column set is the
-- dedup key, but a unique index would refuse to build on any file that still
-- holds a duplicate, and taking the store down is worse than a slow probe.
CREATE INDEX IF NOT EXISTS idx_session_events_dedup
  ON session_events(session_id, type, data_hash);

VACUUM;
```

For each file under `<config-dir>/context-mode/sessions/*.db`:

```bash
for db in ~/.claude-work/context-mode/sessions/*.db; do
  sqlite3 "$db" < docs/ctx-sessions-dedup.sql
done
```

## Reading the result

Most projects need nothing: the store indexes them itself. What the cleanup
buys on a large file is the 9.7 GB, and a build that finishes in a fraction of
73 seconds next time.

`proxy_ctx_events_deduped_total` counts the inserts the store refuses. A steady
rate on a running proxy means the incremental capture high-water mark is
slipping again, not that the backstop is working. Zero is the healthy reading;
the mark is the mechanism, the index is the net.

## What the duplicates were costing beyond disk

`ctx::inject` builds a resume snapshot from `get_events(prior, 200)`, ordered by
id. With the whole conversation re-inserted per turn, those 200 rows covered
only the first few turns, repeated. `render_section` in
`headroom_core::ctx::snapshot` does dedup its lines, but only after taking the
first `MAX_PER_SECTION` items, so the repeats burned the cap before the dedup
saw them. Neither reader ranks by frequency, so no ranking depended on the
duplicates and none of that behaviour changed with the fix.

# Shrinking a sessions DB that grew under the duplicate-capture bug

> **Extracted 2026-09-11:** scale evidence in
> [`notes/learnings/duplicate-capture-cost.md`](notes/learnings/duplicate-capture-cost.md),
> timings in
> [`notes/learnings/dedup-lookup-timings.md`](notes/learnings/dedup-lookup-timings.md),
> counter reading in
> [`notes/learnings/dedup-counter-reading.md`](notes/learnings/dedup-counter-reading.md),
> procedure record in
> [`notes/ideas/implemented/dedup-offline-procedure.md`](notes/ideas/implemented/dedup-offline-procedure.md)
> and
> [`notes/ideas/implemented/dedup-background-index.md`](notes/ideas/implemented/dedup-background-index.md).
> The offline runbook below stays authoritative (with `ctx-sessions-dedup.sql`).

Until the fix on this branch, the CTX-2a capture path re-extracted the whole
conversation on every request and inserted every event again. The largest
project DB reached 9.7 GB:

| | value |
|---|---:|
> **Moved to [`notes/learnings/duplicate-capture-cost.md`](notes/learnings/duplicate-capture-cost.md)** — 3.9M rows / 47k distinct / 9.7 GB table.

> **Moved to [`notes/learnings/duplicate-capture-cost.md`](notes/learnings/duplicate-capture-cost.md)** — re-extract-everything mechanism + no start-up cleanup rationale.

> **Moved to [`notes/ideas/implemented/dedup-background-index.md`](notes/ideas/implemented/dedup-background-index.md)** — index-build behavior + lookup timings + DELETE rationale.

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

> **Moved to [`notes/learnings/dedup-counter-reading.md`](notes/learnings/dedup-counter-reading.md)** — counter reading + cleanup payoff.

> **Moved to [`notes/learnings/dedup-counter-reading.md`](notes/learnings/dedup-counter-reading.md)** — resume-snapshot cap burn + ranking-neutrality note.


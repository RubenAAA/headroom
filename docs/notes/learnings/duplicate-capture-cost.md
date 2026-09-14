# Learning: duplicate capture cost 9.7 GB before anyone looked

- **Source:** `docs/ctx-sessions-dedup.md`
- **Claim:** re-extracting the whole conversation per request: 3,907,865 rows /
  47,557 distinct (82× duplication), worst (session, type) group 27,224 rows.
  Beyond disk: 200-row resume snapshots covered only the first turns repeated,
  and repeats burned the `MAX_PER_SECTION` cap before snapshot dedup ran.
  Neither reader ranks by frequency, so the fix changed no ranking behavior.


## Scale table

*moved from `docs/ctx-sessions-dedup.md`*

| `session_events` rows | 3,907,865 |
| distinct `(session_id, type, data_hash)` | 47,557 |
| file size | 9.7 GB |


## Bug description

*moved from `docs/ctx-sessions-dedup.md`*

New writes are already correct once the proxy restarts. The rows already on
disk are not, and nothing removes them at start-up: `ProjectStores::sessions`
is reachable from the request path through the recall injection engine, so a
start-up cleanup would stall live requests.

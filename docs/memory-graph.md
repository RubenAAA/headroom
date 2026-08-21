# Memory graph: one hop now, more later

The Rust memory backend can follow entity links between memories. This
describes what it does today, why it stops where it does, and what it would
take to go further.

## What ships

`MemorySearchResult` has always carried a `related_entities` field, and
`search_memories` has always taken an `include_related` flag. Until now the
backend ignored the flag: `related_entities` was filled with the entities of
whatever BM25 had already matched, so asking for related memories returned
nothing you did not already have.

The flag now works. `MemoryRecordStore` keeps a `memory_entities` table — one
row per (memory, entity), indexed on the entity column — beside the record
table. Entity names are lowercased for matching; the record keeps the original
casing, because that is what gets shown back to the model.

A search runs in this order:

1. BM25 over the FTS5 index, narrow page.
2. If that did not fill `top_k`, BM25 again over a wide page. (Pre-existing:
   one index serves every project, and the partition filter runs after
   ranking.)
3. If `include_related` is set and there is *still* room, collect the entities
   of the memories found so far and pull in other memories that name any of
   them.

Expansion only fills leftover slots. It never competes for a slot a ranked
match could have had, and it never displaces one. That is enforced by
position, not by score: expanded results are appended and the list is not
re-sorted.

Expanded hits get a flat score of `RELATED_SCORE` (0.35). Adjacency is not a
relevance measurement and there is no honest number to compute, so the value
does one job — clear the default `min_similarity` of 0.3 so callers that
filter do not drop them on arrival.

Partition rules are the same as for a direct match: the project's own
partition plus the shared one, current memories only.

### The scoring bug this uncovered

Wiring the flag up exposed that retrieval had never worked at all. The live
log held 7,265 consecutive `all_below_min_similarity` events — ten results
found each time — and zero successes.

`rank_to_score` is named for BM25 and written for it, but `CtxStore::search`
fuses two ranked lists and overwrites `SearchHit::rank` with the negated RRF
score. With `RRF_K = 60` the best hit obtainable is `2/(RRF_K + 1)` = 0.033,
so `strength/(1+strength)` returned at most 0.032 against a default
`min_similarity` of 0.3. The threshold was not mistuned; it sat ten times
above the highest value the function could return.

Fixed in `3cee05d1` by scaling the fused score by `RRF_K + 1` before the
squash. `RRF_K` is public now, because anything converting `rank` into a
similarity has to know which scale it is reading and the field name says the
wrong one.

Worth carrying forward: an earlier investigation cleared `min_similarity` by
querying the FTS table directly, where ranks run -3.6 to -9.7 and map to
0.78-0.91. Testing the index does not tell you what the search path returns.

### Backfill

The edge table is built from existing records the first time a store opens
after this change, guarded by SQLite's `user_version`. Without it the join
would answer nothing on any store already in use — the entities were always in
the records, just not anywhere queryable.

`reset_entity_backfill()` rearms the marker, so the next open rebuilds every
edge. That is the repair if the edges ever drift from the records.

### What feeds it

Only the `memory_save` tool path passes entities (`handler.rs:948`). The
auto-tail and file-write save paths pass `None`, so those memories have no
edges and expansion neither helps nor hurts them.

## Why it stops at one hop

One hop covers the case that motivated this: you search for a symptom, BM25
finds the memory that names a host, and the other facts about that host arrive
even though your query shared no word with them. Infrastructure notes get
written from different angles months apart, and lexical search alone misses
the connection.

Depth beyond one is a different problem. Hop two reaches memories that share
an entity with a memory that shared an entity with a match — which, for a
common entity like a project name, is most of the store. Useful multi-hop
needs relationship *types* and directions to constrain the walk, and the edge
table has neither. It records that a memory mentions an entity, not what the
entity is or how it relates to anything.

## Going further

Three things are missing before multi-hop is worth building, roughly in order
of what they cost.

**Typed edges.** `save_memory` already accepts `extracted_entities` and
`extracted_relationships`; both are currently discarded by this backend. Those
carry entity types and relationship names. Storing them means a second table
of (subject, relationship, object) and gives entity-to-entity edges, which is
what a walk actually needs — the current table only has memory-to-entity.

**A bounded walk.** With typed edges, `GraphBackend::query_subgraph(entities,
depth)` (declared in `backend.rs`, implemented by nothing) becomes
answerable. In SQLite that is a recursive CTE with a depth cap and a visited
set. Worth doing only once there is evidence one hop is not enough; the
recursion is easy, and deciding which edges are worth following is not.

**Extraction.** Everything above assumes something is producing entities and
relationships. Today that is the model, on the `memory_save` path, when it
chooses to pass them. The Python side gets this from Mem0, which runs an LLM
over the content — see below.

### If Neo4j comes back

Neo4j was never ported. `PORT_STATUS.md:184` lists the Qdrant/Neo4j bridge
among the parts of the Python memory handler the Rust proxy does not have; no
commit or doc records a decision to drop it. The comment at `backend.rs:1-7`
about backends dispatching to Python describes a bridge that was never built —
`headroom-proxy` has no `pyo3` dependency, and the workspace's only PyO3 goes
the other way, exposing Rust to Python.

If graph work outgrows SQLite, the seam is `GraphBackend`. Reaching Neo4j
through its HTTP Query API needs `reqwest` and no new driver; `neo4rs` is the
Bolt alternative but is pre-1.0, with routing and sessions behind an
`unstable-` feature flag. The transport is the small decision.

The large one is the schema. Python writes Neo4j two ways
(`headroom/memory/backends/direct_mem0.py`): its own Cypher for pre-extracted
data, merging `:__Entity__` nodes keyed by `{name, user_id}` with a vector
`embedding` property and a dynamically-typed relationship (`:439-536`); and
Mem0's own graph layer for everything else, which does LLM-driven extraction
against a schema headroom does not control. Graph-aware *search* goes through
Mem0 too (`:777+`).

So the direct-write half ports cleanly and the rest does not. Matching what
Python does today means reimplementing Mem0's extraction and graph retrieval,
not just its database calls. Anyone picking this up should scope those
separately and expect the second to be much larger than the first.

## Files

- `crates/headroom-core/src/ctx/memory_records.rs` — `memory_entities` table,
  `set_entities`, `memories_for_entities`, backfill marker.
- `crates/headroom-proxy/src/memory/ctx_backend.rs` — `related_to`, the
  expansion step in `search_memories`, `backfill_entities`.
- `crates/headroom-proxy/src/memory/backend.rs` — `MemorySearchResult`, and
  the unimplemented `GraphBackend` trait.

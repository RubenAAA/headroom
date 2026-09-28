# Graph memory (entity/relationship store)

Upstream: `headroom/memory/adapters/sqlite_graph.py` (`SQLiteGraphStore`),
wired beside the document store via `headroom/memory/storage_router.py`
(separate `*_graph.sqlite` file).

## What it is

- Nodes: entities (name, type, description, properties).
- Edges: typed relationships (source, target, relation_type, weight).
- Extraction happens at write time (LLM pass over the memory text);
  retrieval fans out: entity filters + subgraph traversal enrich FTS hits.
- Tools expose it as "also retrieve memories connected via entity
  relationships" (`expand_with_connections` in `headroom/memory/tools.py`).

## Why not now

- Different data model from ours (flat scoped documents + FTS), so this is
  a new backend + ranking merge, not a patch. Our router/injection stays.
- Upstream added it recently and is still fixing traversal bugs
  (e.g. #3236, unparenthesized OR in BOTH-direction queries).
- Graph quality lives or dies by extraction quality; unproven benefit.

## When to reconsider

1. Upstream's implementation stabilizes (no traversal/extraction fixes for
   a full release cycle).
2. A measured test shows it beats document-only retrieval on real
   multi-hop queries ("who does Alice work with?", "what depends on X?").
   Test with our own memory corpus, not upstream's fixtures.

Do not port before both conditions hold.

# Implemented: prefix/freeze replay

- **Status:** shipped (flag `--prefix-replay`, live `true`;
  e.g. `d05bf29d` routed path; `proxy.rs:6180-6267` ordering)
- **Source:** `crates/headroom-proxy/src/cache_stabilization/prefix_replay.rs:18-24,36-47`
- **Value:** replays `previous_forwarded` verbatim on append-only extension,
  else leaves bytes untouched — byte-identical, idempotent
  (`overlay_is_idempotent`). Captures the ~38%-of-bill recache pool that
  compression alone can't touch (~1.5% at best on a 91%-cached workload),
  because `LiveZone` compression otherwise oscillates (turn N caches
  *compressed* bytes, turn N+1 resends *original*). True replays bust 0.6% vs
  32% for declined replays (`recache-counting-rules.md`). Requires
  deterministic compression underneath to mean anything.
- **Not this:** the *partial*-prefix splice variant was measured and reverted
  (`rejected/partial-prefix-replay.md` — 204k-token seam cost). Full-verbatim
  replay only.
- **Caveat:** restart wipes the store ⇒ fleet recache
  (`restart-costs-recache.md`).

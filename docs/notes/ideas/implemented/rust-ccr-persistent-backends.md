# Implemented: persistent CCR backends (Sqlite default, Redis opt-in)

- **Status:** shipped (`crates/headroom-core/src/ccr/backends/`)
- **Source:** `docs/notes/rust-dev.md` (multi-worker deployment)
- **Summary:** `InMemory` (tests only), `SqliteCcrStore` default (WAL-shared on
  one host + sticky LB; survives restarts), `RedisCcrStore` (`redis` feature,
  no stickiness). `from_config` picks at startup; init failures abort loudly,
  never degrade silently. Resolves CCR fragmentation across `--workers N`.

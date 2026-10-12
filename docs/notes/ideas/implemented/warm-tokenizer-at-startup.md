# Idea: build the o200k tokenizer at proxy start

- **Status:** done 2026-10-12. `serve()` in `main.rs` builds the tokenizer
  on a `tokenizer-warm` thread when a model route targets a tiktoken model.
  Measured in release: o200k builds in 346-546 ms and holds about 45 MB.
  Spark's models count by estimate and never build it; only Codex routes
  (`gpt-5*`/`gpt-6*`) do, so the gain is one Codex turn per restart.
- **Source:** `a2d919b7d` (concurrency-cap test fix), 2026-10-12.
- **Summary:** the first routed turn after a restart builds the o200k
  tokenizer lazily in `crates/headroom-proxy/src/handlers/local_model.rs`,
  about 1s in debug builds. That turn waits for it; the test that sheds
  past the cap needed a 10s first-request wait because of it.
- **Not changed:** the concurrency-cap test still waits 10s; its proxy
  starts through the test harness, not `serve()`.

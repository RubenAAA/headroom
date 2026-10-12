# Idea: build the o200k tokenizer at proxy start

- **Status:** open.
- **Source:** `a2d919b7d` (concurrency-cap test fix), 2026-10-12.
- **Summary:** the first routed turn after a restart builds the o200k
  tokenizer lazily in `crates/headroom-proxy/src/handlers/local_model.rs`,
  about 1s in debug builds. That turn waits for it; the test that sheds
  past the cap needed a 10s first-request wait because of it.
- **Next:** build it on a background thread in `serve()` and measure the
  first routed turn's latency after a restart, before and after. Check it
  does not slow proxy start or add memory when no routed model is used.

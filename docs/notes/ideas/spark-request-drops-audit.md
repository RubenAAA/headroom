# Idea: audit what the Spark path drops and what it fails to pass

- **Status:** open. Audit of 2026-09-30, partly unverified. Only one fix came
  out of it: mid-conversation `system` messages (`4eab6be1`, `ce3b82db`).
- **Source:** `openai/request.rs`, `routed/prepare.rs`, `openai/stream.rs`,
  `routed/translation.rs`. Data: 400 recent replay-prefix captures and the
  2026-09-29 log.
- **Dropped, by design (question each):**
  - Prior turns' thinking, every turn (`prior_thinking_dropped`, 42 events,
    up to 285 KB each). Zen cannot replay `reasoning.encrypted_content`, so the
    model loses its own earlier reasoning. Is that a Zen limit or a proxy
    choice? Test: send the items back on one lane and see whether Zen accepts.
  - `temperature`, `top_k` (not on the Responses shape); `parallel_tool_calls`
    (removed to match OpenCode); the output ceiling (lifted).
  - Tool schema annotation keys only (`title`, `examples`, ...); tool pruning
    is off in the flags.
- **Dropped, silently, unproven impact:**
  - Assistant `server_tool_use`, `tool_search_tool_result`, `redacted_thinking`
    (15 in 400 conversations).
  - Top-level user `image` and `document` blocks (none seen in 400
    conversations). Tool-result images survive for supported media types.
  - Zen stream events the translator does not handle. They were logged at
    debug, so invisible. Now info, once per name then every 500th
    (`codex_unhandled_stream_event`, with `count`). Zen refuses calls that are
    not from OpenCode, so the raw stream could not be sampled by hand.
- **Answered:** `tool_search_deferral` (3,034 events) does not apply to Spark.
  Its only callers are `proxy/forward/presend.rs` and
  `proxy/request_transforms.rs`; nothing under `routed/` calls it (grep
  2026-09-30). Spark gets every tool schema in full, so the 15
  `tool_search_tool_result` blocks it loses are Claude-side bookkeeping.
- **Unknown:** how much live-zone compression on Spark helps or hurts. It
  rewrote 25 of 1,169 messages in one sample, saves 2.6% of tokens, and costs
  time (see `spark-proxy-overhead-on-free-path.md`). Rewrites can also move
  cached bytes.
- **Not passed:** the input token count (see `spark-usage-and-compaction.md`).
- **Next:** read the info-level unhandled-event lines after a day of traffic;
  answer the reasoning-replay question with one lane test; diff a forwarded
  Spark body against what OpenCode itself sends for the same turn.

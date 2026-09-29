# Idea: tell the client Spark's real token usage, and settle how /compact works there

- **Status:** open, undecided. Nothing changed. Needs a one-session test first.
- **Source:** 2026-09-30 audit of the Spark path. `openai/stream.rs`
  `emit_message_start` (`input_tokens: 0`) and `emit_message_delta`
  (`output_tokens` only).
- **Claim:** for every translated route (Spark, Codex) the client is told
  `input_tokens: 0` at `message_start` and only `output_tokens` at
  `message_delta`. Checked live 2026-09-30 with one tiny streamed request:
  `input_tokens:0`, `output_tokens:34`. Provider input and cached counts never
  reach the client.
- **Claim:** Spark contexts are huge. 2026-09-29 11:00-22:10Z, 6,044 Spark
  turns: `tok_after` p50 335,660, p90 627,419, max 844,515; the slice after the
  cache fix (21:22-22:10Z) has 419 turns above 500k. Sessions of 2,800 messages.
  No context-length error appears in the log.
- **Claim (inference, unproven):** Claude Code decides auto-compact from the
  input tokens it is told, so it never compacts these sessions. That would be
  why they grow to 800k. The agent list shows `↓ 49-591 tokens` for sessions
  with 250-380 turns, so its figure is not a running total either (about 170
  output tokens a turn, so 383 turns is about 65k).
- **Unknown:** Spark's real context window. Not in
  `MODELS-SUPPORTED-WITHIN-CLAUDE-CODE-PROXY.md`, no error seen up to 845k.
- **Unknown:** whether reporting true input makes Claude Code compact on every
  turn. It does not know this model's window and may assume a small one, which
  would turn a display fix into a regression. Test on one Spark session, not
  the fleet.
- **Unknown:** whether `/compact` works for non-native models and paths.
  Only two compaction requests were in the last 3,000 captures (the "CRITICAL:
  Respond with TEXT ONLY" prompt); the 462-message one matches a Sonnet turn in
  the log, so none was a Spark session. A manual `/compact` on Spark would send
  the whole 300-800k context plus the summary prompt and depends on Zen
  accepting it. `ctx/inject.rs` treats a compaction-shaped resume specially;
  that path has not been exercised with Spark either.
- **Question the status quo:** is running at 335k-845k tokens a turn what we
  want, or a side effect of a missing number? Each turn resends that context
  (cached, but the cache is a lane and a session of luck).
- **Next:** (1) find the window: one direct request stream near the limit, or
  ask OpenCode. (2) On one Spark session, report `input_tokens` from
  `response.completed` in `message_delta` behind a flag and watch whether
  compaction fires and how often. (3) Run `/compact` on a real Spark session and
  log the request size, outcome and the resume that follows.

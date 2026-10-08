# Idea: tell the client Spark's real token usage, and settle how /compact works there

- **Status:** step 2 implemented 2026-10-08 behind `--report-true-usage`
  (default ON): terminal `message_delta` on streamed translated routes now
  carries the provider's own input/output/cached counts; routed CCR turns
  report first round plus every continuation round. Still wants a one-session
  test.
- **Status:** `[1m]` probe 2026-10-08, INCONCLUSIVE on the window: client
  2.1.294 strips `claude-muse-spark-1.3[1m]` to the plain id before sending
  (`[claude-code:unrecognized_model]`, `query_source: sdk`; zero `[1m]`
  `from_model` rows in the proxy log — the proxy itself routes it fine).
  Both arms compacted at ~294k preTokens, not the learned ~126k, with all
  env knobs absent. Open: why `-p` mode compacts at 294k (different
  threshold? different assumed window?), and whether the `/model` picker
  sends `[1m]` verbatim where `--model` does not. Turn logs:
  `kits/probe-1m/`.
- **Status:** 294k-vs-126k discrepancy RESOLVED 2026-10-08 (corrected
  same day — NOT an entrypoint effect): the probe arms ran turn 1 on
  Spark and turns 2+ on `claude-haiku-5-5` because `--continue` without
  `--model` falls back to the default model (transcript sequence +
  `sys informational` line at 14:11:24). Haiku 5-5 is client-known, so
  those sessions honored `autoCompactWindow: 306000` and compacted at
  ~96% of it. The `[1m]` question got exactly one Spark turn per arm
  (model id stripped anyway) — a redo must pass `--model` on every
  `-p` invocation. The 14:11–14:20 Haiku recaches + c/r spike are this
  probe's own traffic, not a fleet alert.
- **Status:** picker gap FIXED 2026-10-08. Custom routes never appeared in
  `/model` because the launcher gated `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`
  on routes passed on the command line, and flags-file routes never reach that
  variable. Now gated on the proxy being in play (`use_context=1`); stub-tested
  all four profiles, installed live. New sessions list every `claude-*` route
  including both sparks. The installed launcher was also stale (bare `cclaude`
  bypassed the proxy); it now matches the checkout. Only `[1m]` wire row ever:
  one unattributed single-turn request 14:15:21 (routed and served fine —
  proxy side proven); the user's picker turns left no trace anywhere (no pid,
  transcript, or proxy rows — how they were launched is still open).
- **Status:** `[1m]` route idea DEAD 2026-10-08. The user's picker turns
  (14:45:31/45/54 + 14:46:14, rids `7668db8e`/`c26cd140`/`67f36128`/
  `6288918d`, all four sharing session key `d015b5f5…`) show the plain id
  on the wire — the client strips `[1m]` on the picker path too, same as
  `--model`. Not proxy normalization: `from_model` is the raw pre-rewrite
  client model (`routed/routing.rs`: `before = parsed["model"]`), and the
  only request in the log that ever sent `[1m]` literally — the 14:15:21
  synthetic route check, key-authed, tool-less, fresh session key — was
  logged verbatim. Every Claude-driven request (probe arms, picker turns)
  shows plain. Route line
  removed from the live flags file (takes effect at
  the next proxy restart; no restart for this alone).
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

## Addendum 2026-09-30: what Zen does report

`response.completed.usage` from Zen carries `input_tokens`, `output_tokens`,
`total_tokens` and `input_tokens_details.cached_tokens` (one sampled turn:
76,415 input, 75,889 cached, 156 out; from the `responses_stream_event_shape`
log line). So the true input count is available at the proxy and is dropped
before the client; nothing needs estimating. The open question is still what
Claude Code does with it (auto-compaction), not whether it can be given.

## Addendum 2026-10-05: the compactor, not the usage number

- **Found by binary inspection (2.1.286–2.1.289, identical):** `muse-spark`
  appears zero times in the CLI; Spark model ids are `unknown-model`, so the
  CLI enforces an assumed 200k window (`vWe`/`KUe` = 200000, belief clamp)
  and auto-compacts against it. Measured: auto `compact_boundary` at
  preTokens=132342 with settings.json `autoCompactWindow: 306000` set.
- **The setting does not hold** because the effective threshold is
  min(setting, assumed window). `CLAUDE_CODE_AUTO_COMPACT_WINDOW` cannot fix
  unknown models for the same reason. The binary documents two knobs:
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS=<real window>` (fixes the assumed window)
  and `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1` (restores the
  previous wait-for-the-API behavior = no auto-compact, the pre-regression
  state).
- **2026-10-08 verdict: the DISABLE knob is dead.** Transcript census of
  4,577 jsonl, Oct 3–8: flagged top-level sessions still compact at
  125–137k (21 compacted vs 2 marginal non-compacted), with the flag present
  in the launcher and in every live claude process's environ. It shipped Oct
  5 with no verification — no evidence it ever worked. Full numbers:
  `docs/notes/learnings/spark-auto-compact-200k-window-2026-10-08.md`.
  Rejected: `muse-spark-1.3[1m]` rename (invasive). Now under test:
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000` (expect compaction to move ~126k
  to ~630k preTokens if the ~63% trigger holds).
- **Status 2026-10-08:** `[1m]` probe route live as an *addition* (not the
  rejected rename): `claude-muse-spark-1.3[1m]` in `~/.headroom-flags.sh`
  maps to the same Spark upstream/target as `claude-muse-spark-1.3` and
  appears in `/v1/models` discovery; proxy restarted, healthz ok,
  cache-health verdict healthy. Probe session: run
  `env -u CLAUDE_CODE_MAX_CONTEXT_TOKENS cclaude`, pick the `[1m]` entry
  via `/model`, grow past 150k, record preTokens at compaction.

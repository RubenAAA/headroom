# Idea: stop sending Spark sessions' spinner-text sidecar to paid Haiku (shipped in the working tree, see the end)

- **Status:** open. Nothing changed in behaviour. Findings added 2026-09-30
  (below): Claude Code has no switch for the request, and the spinner text
  also reaches Spark inside real turns.
- **Source:** 2026-09-30 audit. `sidecar.rs` (`DEFAULT_SIDECAR_MODEL`),
  `handlers/local_model.rs` `maybe_answer_sidecar`; `sidecar_detected` events.
- **Claim:** 2026-09-29 11:06-22:10Z the log has 3,403 `sidecar_detected`
  events; 3,188 came from `claude-muse-spark-1.3` sessions and 215 from
  Sonnet. Every one went to `claude-haiku-4-5-20251001` with `routed:false`,
  which is Anthropic quota, not the free Zen path. `--sidecar-model` is unset in
  `~/.headroom-flags.sh`.
- **What the sidecar gets:** 3 trailing messages, tool results cut to 2,000
  characters, a one-line system prompt, 64 output tokens. Spark never sees the
  turn, so the spinner line describes a trimmed view written by another model.
- **Unknown:** what those 3,188 calls cost in Haiku quota (no ledger line per
  sidecar checked), and whether the unset flag is deliberate.
- **Question the status quo:** the sidecar exists to avoid a large paid prefix
  read on Opus/Sonnet. For a free-path session that reason is gone.
- **Options:** send it to Spark with the same shrink (free, one Zen turn, adds
  load on a lane-limited pool); or answer it locally with a fixed string; or
  leave as is and price it.
- **Next:** count Haiku sidecar tokens from `turn_cost_ledger`, then trial
  `--sidecar-model claude-muse-spark-1.3` if the route resolves it.

## Findings, 2026-09-30 (after the 07:44Z and 08:45Z restarts)

- **No client switch.** A search of the Claude Code docs (settings reference,
  env vars) found none that stops the request. `spinnerTipsEnabled`,
  `prefersReducedMotion` and `terminalProgressBarEnabled` change what is
  drawn, not what is sent; `CLAUDE_CODE_DISABLE_TERMINAL_TITLE` covers only the
  session-title request. Turning it off means the proxy answering it without
  a model. **The reply is visible:** it is the per-agent description in the
  statusline agent list (`◯ spark  Working  2h 1m · ↓ 5.5k tokens`), so a
  fixed line costs that description (see the trial below).
- **The routed sidecar path is unused.** From 07:44Z the log has 452
  `sidecar_detected` (94 more in the eight minutes after 08:45Z), all
  `routed:false` to Haiku, and no `sidecar_fallback`, `sidecar_routed_attempt`
  or `sidecar_routed_fallback`. `routed/sidecar.rs` and
  `--sidecar-route-timeout` exist, but `--sidecar-model` is unset, so the
  attempt never starts.
- **The spinner text also reaches Spark undetected.** 121 continuity breaks
  logged by 08:45Z carry it: a user message "Describe your most recent action…"
  in the middle of a real turn's input, followed by a developer item, an
  assistant reply and a user message. `is_describe_action_sidecar` needs the
  last user message to open with the text and only `system` messages after it,
  so a turn with an assistant reply after the text passes. Spark answered one
  as a real turn ("I don't have context about a recent tool result…"). How the
  text got into a real turn's history is unknown. `routed_forward_continuity`
  now logs `kinds_from_moved`, the kinds of the items from the first change on,
  to show the shape.
- **Next:** with the shape known, decide whether the detector should also match
  it; then pick between Spark with the same shrink, a fixed local answer, or
  the status quo priced from the ledger.

## Trial: fixed local answer (2026-09-30, 09:12Z)

`--sidecar-local-answer <TEXT>` (env `HEADROOM_PROXY_SIDECAR_LOCAL_ANSWER`)
answers a detected spinner request with that line and calls no model: no Haiku
quota, no Zen turn. Default unset, so behaviour is unchanged. The event is
`sidecar_answered_locally`; the counter label is `local`. Only requests the
detector matches are covered, so the leaked shape above still reaches Spark.
The proxy has run with `Working` since 09:12:21Z (set through the environment
of that one restart, not in the flags file): the first three spinner requests
were answered locally and none went to a model. **Result:** the text is the agent
list's description, so every agent now reads `Working`. The flag is also set in
`contrib/headroom-flags.sh`; it stays until a free, fast model is found for the
summary. Candidates: the `*-free` ids on Zen (`mimo-v2.6-flash-free`,
`mimo-v2.5-free`, `deepseek-v4-flash-free`, `nemotron-3.5-lightning-free`, …);
the free tier's OpenCode-only gate applies to them too.

## Free chat-completions models tried (2026-09-30, test proxy on :8799)

A second proxy instance, routes `space-bunny-free` and
`longcat-2.5-preview-free` (`opencode.ai/zen/v1`, chat shape, no target model),
no egress pool (direct from this host), a sidecar-like prompt:

- `space-bunny-free`: 200 every time, 0.8-2.0 s total, good text ("Running
  headroom-proxy integration tests"). It reasons first, so at `max_tokens` 64,
  3 of 5 replies were empty (`stop_reason: max_tokens` inside the thinking
  block); at 200, 2 of 2 answered. The sidecar's 64-token cap would need
  raising for this model, or its reasoning effort lowered. Small sample.
- `longcat-2.5-preview-free`: 403 `FreeTierError` ("free tier can only be used
  from within OpenCode") on the first request. Not worked around.
- Not tried: `union-alpha` timings are confounded by the proxy's injected
  memory tools (one reply took 36 s calling them); the sidecar path skips that
  injection, so its real speed is unknown.
- Unmeasured: Zen's limit for this model, and whether ~12 extra requests a
  minute through the lanes touches Spark's.

## Shipped in the working tree (2026-09-30, not committed): chat-route sidecar

`routed/sidecar.rs` now serves a sidecar model that names a chat route
(`sidecar_chat_route`: translate, no target model; streamed replies only,
`max_tokens` 512). Order: routed model, then `--sidecar-local-answer`, then
direct Haiku. `contrib/headroom-flags.sh` sets `--sidecar-model space-bunny-free`
with its route and keeps `--sidecar-local-answer Working` as the fallback, so a
Zen failure spends no Claude quota. Live from 10:09:21Z: 23 routed attempts, 0
fallbacks in the first minutes; six sidecar-shaped probes returned text in 1.6-9.4 s
("Editing the failing test file", "Running the test suite"). Tests:
`a_chat_route_serves_the_sidecar`,
`a_failed_chat_route_falls_back_to_the_fixed_line_not_haiku`.
**Watch:** `sidecar_routed_fallback` counts, Zen 429s on Spark
(`zen_egress_failover` reason `rate_limited`) against the earlier rate, and the
reply quality in the agent list.

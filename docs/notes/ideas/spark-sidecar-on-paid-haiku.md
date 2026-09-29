# Idea: stop sending Spark sessions' spinner-text sidecar to paid Haiku

- **Status:** open. Nothing changed.
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

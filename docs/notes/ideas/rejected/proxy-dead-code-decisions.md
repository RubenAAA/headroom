# Idea: decide the leftover dead code from the warning sweep

- **Status:** closed 2026-09-11, not doing (09-11 re-audit)
- **Source:** `docs/notes/proxy-followups.md` §3 (`92a2bb51` cleared 33→10)
- **Summary:** remaining warnings are all dead code, each wanting a finish
  verdict: never-used `which`, `split_model_action`, `safe_str`,
  `record_waste_signals`; never-read `agent_type`, `tool_name`, `session_key`,
  `max_queue`, `last_mtime_ns`, `lane`/`url`/`response_headers`.
> **09-11 outcome:** dissolved — split_model_action live, three allows recorded, all fields read; hands off.
- **Next (superseded):** per item, decide whether its owner is finished (delete) or not
  (wire up); none deleted blindly.


## Detail

*moved from `docs/notes/proxy-followups.md`*

## 3. Build warnings

**Status: mechanical ones cleared (`92a2bb51`); 33 down to 10.** The unused
imports and unused variables are gone, and so is `arrays` in
`content_router.rs:484`. What remains is all dead code rather than mechanical
churn — four never-used functions (`which`, `split_model_action`, `safe_str`,
`record_waste_signals`) and six never-read fields (`agent_type`, `tool_name`,
`session_key`, `max_queue`, `last_mtime_ns`, and `lane`/`url`/
`response_headers` together). Each wants a decision about whether the thing it
belongs to is finished, so none was deleted.

33 warnings, all pre-existing and none from the cache work:

- 15 unused imports (`HashMap` x3, `hex` x2, `Arc`, `HashSet`,
  `HeaderMap`, `MatchedPath`, `chrono::Utc`, `Digest`/`Sha256`,
  `SystemTime`/`UNIX_EPOCH`, `CcrToolCall`)
- 6 unused variables (`tokens_saved`, `state`, `now`, `host_clone`,
  `config`, `client_ip`)
- `arrays` in `content_router.rs:484` assigned twice and never read —
  the only one that might be a real bug
- `NullCcrStore` never constructed

`cargo fix --release --workspace --allow-dirty` clears the mechanical
ones. `arrays` wants a human.

**Residue dissolved 2026-09-11 — no deletions made, none needed.**
`split_model_action` was never dead (called in prod at `vertex/mod.rs:123`);
remove it from this list. `safe_str`, `record_waste_signals`, `which` each
carry an explicit `#[allow(dead_code)]` (`ctx/extract.rs:85`,
`proxy.rs:13966`, `bin/headroom_cli/tools.rs:505`) — the keep decision the
entry asked for is already recorded in the code; all three are exercised by
tests. All six fields have reads (`session_key` 149, `tool_name` 18,
`last_mtime_ns` 9, `agent_type` 3, `max_queue` 2).

New development, not a continuation: the current build emits 18 warnings
including a *fresh* dead list (`recall_is_empty`,
`should_buffer_openai_responses_stream_ccr`, `responses_json_to_sse`,
`responses_completed_from_sse`, `openai_sse_error_event`,
`openai_json_error_body`, `hash_value`,
`has_headroom_retrieve_tool_responses`, `emit`, `caller_is_chatgpt_auth`,
`GENERIC_FAILURE_MESSAGE`, `CCR_TOOL_NAME`, plus unused `observe_drift` /
`BodyExt` imports). Names read like other sessions' in-flight CCR/error
work — do NOT bulk-delete in a shared tree. Owners delete their own when
their workstream lands.

> Note on the 09-11 dead list: `should_buffer_openai_responses_stream_ccr`, `responses_json_to_sse`, `responses_completed_from_sse`, `openai_sse_error_event`, `openai_json_error_body`, `has_headroom_retrieve_tool_responses`, `emit`, `caller_is_chatgpt_auth`, `GENERIC_FAILURE_MESSAGE`, `CCR_TOOL_NAME` are wired (called from `forward_http`) and covered by unit + integration tests as of 2026-09-11 — that observation predates the wiring landing. `recall_is_empty`, `hash_value`, `observe_drift`/`BodyExt` are pre-existing; owners delete their own per the entry's rule.

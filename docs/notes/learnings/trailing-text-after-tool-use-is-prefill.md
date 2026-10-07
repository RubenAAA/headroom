# Trailing text after a tool_use reads as prefill when the turn holds server tool blocks

- **Status:** fixed 2026-10-07 in `splice_ccr_results_as_text`
  (`headroom-core/src/ccr/response_handler.rs`, `lead_spliced_text`). Test:
  `an_answer_spliced_after_a_client_call_moves_ahead_of_it`.
- **Window:** proxy logs 2026-09-29 to 2026-10-07 (3 rejections, all memory
  continuations: 10-02, 10-07 15:36Z, 10-07 15:37Z), plus seven direct API
  calls on claude-opus-5-5 the same day.
- **Symptom:** `memory_continuation_rejected` with "This model does not support
  assistant message prefill. The conversation must end with a user message",
  although the logged `tail_roles` end on `user[tool_result]`. The memory calls
  were stranded and the session saw "The proxy could not complete
  `memory_search`".
- **Finding:** Anthropic rejects an assistant message shaped
  `[server_tool_use, tool_search_tool_result, tool_use, text]` followed by a
  user `tool_result`. Each variant changed one thing:

  | assistant content | result |
  |---|---|
  | `tool_use` | 200 |
  | `server_tool_use, tool_search_tool_result, tool_use` | 200 |
  | `server_tool_use, tool_search_tool_result, tool_use, text` | 400 prefill |
  | `server_tool_use, tool_search_tool_result, tool_use, text(" ")` | 400 prefill |
  | `tool_use, text` (no server blocks) | 200 |
  | `server_tool_use, tool_search_tool_result, text, tool_use` | 200 |
  | 400 case with a mid-conversation `system` message before it | 400 prefill |

  The error wording is wrong for the case: the body does end on a user message.
- **Where the trailing text came from:** both live turns called
  `memory_search` and then `headroom_retrieve` in one reply, after a server
  tool search (the proxy defers memory tools behind it). The CCR mixed-turn
  splice replaced the retrieve call with its answer *in place*, so the answer
  trailed the memory `tool_use`, and the memory continuation replayed it.
- **Fix:** spliced answers that land after a `tool_use` move ahead of the first
  one. The same splice feeds the client's turn, where the trailing text would
  have drawn the same 400 on the client's next request. No `upstream_rejected`
  in the window shows that happened.
- **Not changed:** the memory receipt (`[headroom memory] … → N results`)
  still carries no results. That is by design since `3f16ac0c`: the receipt
  stops the model concluding it invented the calls; the results reach the
  model only in the continuation round.

# Server-side context editing: what the API actually promises

Facts taken from the Anthropic docs on 2026-08-17 (`platform.claude.com/docs/en/
build-with-claude/context-editing` and `.../extended-thinking`), plus numbers
measured against the live `count_tokens` endpoint the same day. Written down so
nobody has to fetch the pages or re-run the queries.

Beta header: `context-management-2025-06-27`. Parameter: `context_management.edits[]`.

## `clear_thinking_20251015`

`keep` takes either form:

| Form | Meaning |
|---|---|
| `"all"` | keep every thinking block — maximises cache hits |
| `{"type":"thinking_turns","value":N}` | keep the last N assistant turns that contain thinking; **N must be > 0** |

A *turn*, not a block: one assistant turn may hold several thinking blocks.

No `trigger`, no `clear_at_least`, no `exclude_tools`. Thinking clearing is
structural — it fires on turn recency alone, every request.

**Claude Code always sends this edit itself, with `keep: "all"`** (1,868 of 1,869
captured bodies). That is the cache-optimal, token-worst setting, and it is why
`inject_context_management` — which skips any edit family the client already sent
— could never fire against its own main client.

### Whether prior turns' thinking is billed at all: model-dependent

> Claude Opus 4.5 and models numbered 4.6 and higher keep prior turns' thinking
> blocks in context and bill them as input, where Claude Sonnet 4.5, Claude Haiku
> 4.5, and earlier models stripped them.

So on Opus 5 / Sonnet 5 the thinking in history is real billed input. On Sonnet
4.5 and earlier it was already free and clearing it saves nothing. Defaults for
`keep` also vary by model tier, so set it explicitly.

## `clear_tool_uses_20250919`

| Option | Default |
|---|---|
| `keep` | 3 tool uses |
| `trigger` | 100,000 input tokens |
| `clear_at_least` | none |
| `exclude_tools` | none |
| `clear_tool_inputs` | `false` |

`clear_at_least` is the knob thinking clearing lacks: if the API cannot clear at
least that many tokens, **the whole strategy is skipped and the cache stays
intact**. That is how you stop a small clear from paying for a large cache write.

**Claude Code never sends this family**, so injecting it works today.

## Billing

Editing happens server-side before the prompt reaches the model, so `input_tokens`
is the post-edit count — cleared content is not billed as input. `count_tokens`
returns both numbers:

```json
{ "input_tokens": 25000, "context_management": { "original_input_tokens": 70000 } }
```

A message response reports what was removed under `context_management.applied_edits`
(`cleared_thinking_turns`, `cleared_input_tokens`); when streaming it arrives in
the final `message_delta`.

The docs do not claim cleared tokens are free of *all* charge: clearing
invalidates cache, so you pay the cache write on the newly cached prefix.

## Cache interaction

- Thinking kept (`"all"`) → prefix preserved.
- Thinking cleared → **cache invalidated at the point where clearing occurs**;
  everything before the earliest edited position stays cacheable.
- Tool result clearing invalidates whenever it clears. Bound it with
  `clear_at_least`.

## Ordering

`clear_thinking_20251015` must be listed **first** in `edits`.

## Measured on our own corpus, 2026-08-17

`count_tokens` over 8 deep bodies from `~/headroom-capture-beta`
(88–397 messages, 1,366,634 input tokens at `keep: "all"`), model `claude-opus-5`:

| Setting | Input tokens | Removed |
|---|---|---|
| baseline, `clear_thinking keep:"all"` | 1,366,634 | — |
| `clear_thinking keep:1` | 1,199,389 | 12.2% |
| `clear_tool_uses keep:6, trigger 30k, clear_at_least 5k` | 1,121,595 | 17.9% |
| `clear_tool_uses keep:3, same` | 1,114,888 | 18.4% |
| both (thinking 1 + tool_uses 6) | 954,350 | **30.2%** |

`keep:3` beats `keep:6` by only 0.5pp, so the aggressive setting buys almost
nothing. On one 397-message body `keep: thinking_turns=1` alone removed 20,134
tokens; `keep=8` on the same body removed **nothing**, because Claude Code's
thinking is concentrated in the last few turns rather than spread through
history. That concentration is what makes the marginal invalidation point sit
near the tail.

These are token counts, not bill deltas. What they leave open is the cache write
each clear provokes; see the payback arithmetic in [[offload-stale-history]] and
`docs/measurement.md`.

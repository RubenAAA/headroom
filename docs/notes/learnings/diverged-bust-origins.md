# Learning: diverged busts come from early client edits

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §20 (164 diverged turns)
- **Claim:** 122/164 land in the first five messages: 96 thinking-block additions (free — rejected attempts, cf. item 18) + 47 ephemeral per-tool_result text blocks (real money: 34 warm-cache turns writing 50–86K).


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 20 — Where the diverged busts actually come from

Measured 2026-08-09 after item 19's revert, over 164 `prefix_content_diverged`
turns since 15:00Z.

**The divergence is early, not at the boundary.** 122 of 164 land within the
first five messages; only 35 are within five of the end. The "client appended a
block to the newest message" picture that shaped items 7 and 17 is the minority
case.

Shapes, stored original → current original (both are client bytes):

| transition | n |
| --- | --- |
| `[text,tool_use]` → `[thinking,text,tool_use]` | 78 |
| `[tool_result,tool_result,text,text]` → `[tool_result,tool_result]` | 21 |
| `[tool_use]` → `[thinking,thinking,tool_use]` | 18 |
| `[tool_result,text]` → `[tool_result]` | 16 |
| `[tool_result×3,text×3]` → `[tool_result×3]` | 7 |

Two client behaviours, neither of them the proxy's doing:

- **96 thinking-block additions.** These are item 18's rejected first attempts.
  The client sends thinking blocks, Anthropic refuses, it retries without them.
  The store keeps the successful shape, so the next first attempt diverges. Free
  — those attempts are never billed.
- **47 with one `text` block per `tool_result`**, present in the stored original
  and absent from the current one. The one-to-one structure says these are
  ephemeral notes the client attaches to arriving tool results and drops on the
  following turn — `<system-reminder>` blocks fit exactly. Inferred from block
  structure; not confirmed against the client.

The second class costs real tokens. 34 diverged turns took a large
`cache_creation` write with the provider cache still warm (gap under 300s),
concentrated in small conversations of 14–36 messages writing 50–86K each. The
client's own bytes move at message ~3, so the provider's prefix breaks there
whatever the proxy does.

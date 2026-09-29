# Rejected: replaying withdrawn client content (won't fix)

- **Status:** won't fix — behaviour trade needing owner decision
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §20


## Decision

*moved from `docs/notes/proxy-experiments-2026-08.md`*

**The proxy cannot fix this without lying to the model.** Holding the previous
turn's version stable would keep the cache, and would mean re-sending a note the
client deliberately withdrew — the same objection that caps replay at the first
divergence in item 19. That is a behaviour trade, not an optimisation, and it
needs an owner's decision rather than a patch.

Not attempted. Recorded so the next reader does not re-derive it.

## 2026-09-29 recurrence check

The local-day audit recorded one `prefix_content_diverged` event costing
210,937 tokens at 13:22:39 Asia/Yerevan. The metadata-only replay record has
`first_diff_index=68` and shape `tool_result,text` →
`tool_result,text,text`; the text kinds are `plain` on both sides. Message
counts grew 251 → 254. This does not match the known `<system-reminder>`
ephemeral-content case. The raw message text was not inspected.

The associated replay-decline record says `chain_id=0`. Conversation keys can
merge streams, so this evidence does not establish whether the extra text was
an intentional edit, a regenerated tool result, or a stream-collision artifact.
A `prefix_replay_applied` record co-fired, but its `replayed_prefix=false`
means it is not evidence that the stored tool result was restored; it can
record cache-control normalization.

This recurrence does not reverse the decision above: replaying the old text
would still risk showing the model content the current request did not send.
Before proposing any canonicalizer, reproduce the structural change in a
controlled same-stream case and establish that the added plain-text block is
semantically disposable. The complete event inventory and sanitized log
replay are in `../recache-provider-reasons.md`.


## Closure 20

*moved from `docs/notes/proxy-experiments-closures.md`*

**20 — closed 2026-08-12 (won't fix): client withdrawals beat cache reuse.**
The completed pre-restart window has two
`prefix_content_diverged` turns and both join the only two drift-waste events:
message index 2, `content[0].content[0].text`, with `tool_result` shape on both
sides. They cost 5,067 and 8,859 tokens respectively. The post-restart completed
window has zero divergences. Thus 2 of 2 current costly divergences are within
the first five messages, consistent with the original 122 of 164, while the old
thinking-block transition does not recur.

These are changes in client-originated tool-result text, not a boundary the
proxy can safely replay across. Re-sending the stored text would preserve cache
bytes by showing the model content the client no longer sent. The all-or-nothing
guard and regression test therefore remain the safe default. The owner chose
semantic correctness explicitly: never replay withdrawn client content. There
is no code patch for this closure.

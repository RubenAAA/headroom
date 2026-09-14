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

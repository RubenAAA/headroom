# Implemented: build the recall query from what the user typed

- **Status:** done 2026-10-01, in the working tree and not yet committed.
  `ctx/inject.rs` `InjectEngine::build` lifts the `<system-reminder>` spans out
  of the first user message (`split_ephemeral_spans`) before `derive_queries`
  cuts its 120 characters. Test: `recall_searches_for_the_typed_text_not_the_reminder`
  (it failed on the old code with the reminder's entry in the result).
- **Source:** `docs/jev-model.md` "Offline test", found while testing a Jev gate.
- **Finding:** `derive_queries` took the first 120 characters of the first user
  message. That message opens with the client's `<system-reminder>` (the
  CLAUDE.md digest), so the query was that boilerplate for every session of a
  project. In 5 captured sessions of this repo (297 requests, 2026-10-01) there
  were 4 distinct recall entries in all, identical in every session whatever
  the session asked. The block header prints the query
  (`<system-reminder> Codebase and user instructions are shown below...`), so it
  shows in any request that carries a recall block.
- **Effect:** the decision is persisted once per conversation and replayed byte
  for byte (invariant I4), so only conversations that start after the proxy
  restarts get the new query. Message-0 bytes are unchanged for a conversation
  already decided. The cache and routing suites that exercise recall pass.
- **Verified live 2026-10-01** on the restarted proxy (release build, listener
  pid 3012154): a probe through `/v1/messages` with the CLAUDE.md-style reminder
  block plus the typed text "Explain the partial prefix replay canon and cache
  breakpoints" stored a recall whose query was the typed text and whose 5
  entries were `prefix_replay.rs` and the partial-prefix-replay notes. Read from
  `conv_injection` in the project's sessions DB. One probe, with a typed text
  chosen to match stored content; it shows the query follows the ask, not that
  the entries help.
- **Not measured:** whether the new entries help more than the old four, and the
  token cost of the block per session. Check the next captures for distinct
  entries per session, then test a Jev rerank on those candidates. A Jev gate on
  the old query would have filtered four fixed entries and fixed nothing.
- **Limit:** the query is still the first 120 characters of the typed text, so a
  long first message is searched on its opening only. A reminder that never
  closes is left in the text by `split_ephemeral_spans` by design.

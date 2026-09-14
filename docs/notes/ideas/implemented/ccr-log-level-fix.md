# Implemented: routine CCR drops log at DEBUG

- **Status:** fixed 2026-08-23 (`4d48f24d`)
- **Source:** `docs/notes/proxy-followups.md` §2
- **Summary:** 97% of `dropping blocks the client must not receive` was
  `continuation_thinking` (design working, logged WARN). Now WARN only on
  `unresolved_proxy_tool` (the real fault, 2/day); rest at DEBUG with counts
  for context.


## Detail

*moved from `docs/notes/proxy-followups.md`*

## 2. `ccr: dropping blocks the client must not receive`

**Status: fixed 2026-08-23 (`4d48f24d`).** `ccr_stream.rs:818` warns only when
`unresolved_proxy_tool` is non-zero, with the other two counts carried along for
context; the routine case dropped to DEBUG.

215 events. Breakdown by reason:

| reason | blocks |
|---|---|
| `continuation_thinking` | 209 |
| `already_streamed` | 32 |
| `unresolved_proxy_tool` | 2 |

97% is `continuation_thinking` — thinking blocks from a continuation
round that the client must not be shown twice. That is the design
working, logged at WARN. Only `unresolved_proxy_tool` (2 in a day) is
a real fault, and it is the one that kills a turn with "the model's
tool call could not be parsed".

Fix: WARN for `unresolved_proxy_tool`, DEBUG for the other two.

**Closed 2026-09-11.** Warn path intact (`ccr_stream.rs:900/937`), zero events
in the window — zero real faults. Holding.

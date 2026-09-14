# Implemented: routed streaming finisher (no more silent cuts)

- **Status:** fixed 2026-09-10 (unit + seam tests green)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (mid-response drops)
- **Summary:** routed translate/Spark streaming had no finisher — a drop ended
  as clean-looking `end_turn` (silent cut) or bare RST, and half-streamed tool
  calls could close runnable on truncated input. Translator leaves aborts
  unstopped with the tool block open; error-generic `finish_on_drop` wraps
  `handle_streaming_response` + sidecar streaming.


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

Gaps closed 2026-09-10 (code):

- Routed (translate/Spark) streaming had NO finisher: a drop ended as a
  clean-looking `end_turn` with no marker (silent cut) or a bare RST (dead
  session), and a half-streamed tool call could have closed into a runnable
  call on truncated input. Now: translator leaves aborts unstopped with the
  tool block open (`abort_terminal`), `finish_on_drop` is error-generic,
  `handle_streaming_response` + sidecar streaming wrap it. Unit + seam
  tests green.

# Implemented: sidecar kept out of the replay store

- **Status:** shipped and confirmed live (tail audit 2026-09-11: `sidecar_detected` ×281; recognise + Haiku route at `sidecar.rs:58`, lean arm `routed/sidecar.rs:93`, replay/cache-tracker exclusion `routed/sidecar.rs:248`)
- **Source:** `docs/notes/recache-classification.md` (2026-09-02 audit)


## Detail

*moved from `docs/notes/recache-classification.md`*

### `unexplained_after_replay` is the spinner sidecar

This bucket was closed once as "provider breakpoint granularity". That was
wrong. It is now 339 events and 308,083 tokens, 45% of the remaining waste, and
the cause is a request the proxy should never have stored.

The tell is in the body size. On healthy consecutive turns the client body
shrinks 0.4% of the time; on turns tagged `unexplained_after_replay` it shrinks
76% of the time, by 523 bytes in 17 cases and 594–615 bytes in most of the
rest. Claude Code sends a sidecar request to write its spinner text: it appends
a text block beginning "Describe your most recent action in 3-5 words" to the
last user message and sends the whole history to the same model. The next real
turn drops the block. The proxy stores the sidecar's prefix, then flags the
real turn for having lost it.

The median waste in the bucket is 615 tokens, the same order as the appended
block. Fix in progress: recognise the sidecar, trim it, route it to Haiku, and
keep it out of the replay store. It will log `sidecar_detected`; that event
does not exist in the tree yet, so its absence from a log means the fix is not
in that binary rather than that no sidecar arrived.


## Tail confirmation

*moved from `docs/notes/recache-classification.md`*

- **Sidecar fix: shipped and working.** `sidecar_detected` ×281 in the window
  (the event the 09-02 entry said did not exist yet). All three halves landed:
  recognise + Haiku route (`sidecar.rs:58` `DEFAULT_SIDECAR_MODEL`), lean
  routed arm (`routed/sidecar.rs:93`), and exclusion from the replay store
  and cache tracker (`routed/sidecar.rs:248`, "leaves no per-conversation
  state"). Close the "fix in progress".

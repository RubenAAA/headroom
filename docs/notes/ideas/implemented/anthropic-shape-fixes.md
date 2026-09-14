# Implemented: Anthropic request-shape fixes (2026-08-12)

- **Status:** shipped with regression tests — do not regress
- **Source:** `docs/notes/proxy-experiments-closures.md`


## Detail

*moved from `docs/notes/proxy-experiments-closures.md`*

## State as of 2026-08-12

Shipped today, both with regression tests — do not regress them:

- The proxy no longer places `cache_control` on a `thinking` block. It was
  rejecting whole turns with `messages.N.content.0.thinking.cache_control:
  Extra inputs are not permitted`.
- Every eligible bare string is wrapped to one-text-block form, not only the one
  selected to carry the marker, so a message's shape cannot change when the
  marker moves on.

Measured over 367 requests since that build went live: zero events of the
`[text] -> [string]` reminder-drift class that item 29 targeted, against ~14
expected at the old rate; one `upstream_rejected`, and it is a 429; replay
declines at 3.0%; create/read 0.017 against 0.055 and 0.034 for the two runs
before.

# Rejected: stripping system cache breakpoints

- **Status:** rejected 2026-08-11, reverted — live
  `--strip-system-cache-breakpoints false`
- **Source:** `contrib/headroom-flags.sh` ("TRIED AND REVERTED, 2026-08-11
  12:30Z")
- **Summary:** the tail marker covers the system prompt only on turns where
  it hits, and this client edits history, so it misses often. System markers
  were the floor under those misses: **mean creation per request 11,118 →
  39,208** without them while the median barely moved.
- **Revisit only** if the drift rate goes to near zero (the condition the
  flags file names). Marker budget with system markers kept: 2 system + 2
  message-tail = Anthropic's cap of 4 — no third message slot without
  dropping something else.

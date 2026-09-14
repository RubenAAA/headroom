# Learning: unread cache blocks (H4 lead)

- **Source:** `docs/notes/recache-classification.md`
- **Claim:** 6 events matched the previous turn's write exactly (read == previous read: block paid for, never used); 12 more sat on the formula clamp. Repeated ~240-token figure suggested one fixed block.


## Detail

*moved from `docs/notes/recache-classification.md`*

## H4 — Something writes a cache block that is never read. OPEN, and the strongest lead

`classify_turn` (`usage_observer.rs:385-404`) computes
`wasted = min(prev.read + prev.write − read, write)`.

In 18 of 20 events `wasted_tokens` equals a `cache_creation` figure exactly:

- **6 events** match the *previous* turn's write (243, 239, 240, 239, 209,
  248). Exact equality here means `cache_read` came back precisely equal to
  the previous turn's read — the block written last turn was not read this
  turn, at all. That is a block paid for and never used.
- **12 events** equal *this* turn's write, which is the clamp in the formula
  binding, so they only tell us the shortfall was at least that large.

The six exact matches are the sharp signal, and the repeated ~240-token
figure suggests one fixed block rather than a drifting prefix.

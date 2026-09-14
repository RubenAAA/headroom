# Implemented: ephemeral-block relocation

- **Status:** shipped `b4f97810` + `c44bf160` (message-level, front-of-pipeline)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §§28–29


## Item 28

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 28 — The churn disguises itself as a branch, and that is the way in

Live in `b4f97810`, 2026-08-09.

### The thing that was hiding

Item 27 left declines as the target: 32% of them take a large write, against
0.6% of true replays. Item 19 already proved the obvious fix wrong — a decline
happens because the client changed message *k*, so the provider's prefix dies at
*k* whatever we do, and replaying `prev_fwd[..k]` only preserves a region that
was already fine while adding a seam.

The way in came from the chain ids. Every large-write decline in the sample
reported `chain_id = 0` — continuing nothing — and three of four involved a
`<system-reminder>`. But chain identity is decided by the *same* canonicalizer
the reminder churn defeats. A turn that merely lost a reminder is judged to
continue nothing at all. The churn was wearing a branch's clothes, in the one
signal built to tell them apart.

### Two halves, and they only work together

**Compare blind to them.** `canonicalize_for_prefix_compare` now drops
`<system-reminder>` text blocks, exactly as it drops `cache_control`. A turn
that withdrew one still continues its chain, so replay engages.

**Forward without them.** `relocate_ephemeral_blocks` lifts every reminder out
of history and re-attaches it to the newest message. Nothing is dropped; the
model receives every block the client sent, in the same request, moved to where
the breakpoint leaves it outside the cached prefix.

Half one alone would freeze each turn's reminder into the replayed prefix
forever — the accumulation item 20 worried about. Half two alone leaves the
comparison failing, so the replay never engages to begin with. Ignoring a
difference while still forwarding it would be worse than either: replaying bytes
the provider never cached.

### The invariant that decides whether it works

On turn N a reminder rides on the newest message; on turn N+1 that message is
history and is stripped, so its bytes change. That would kill the cache if the
breakpoint sat inside the changed part. It does not — the marker goes on the
last non-ephemeral block (item 23's seal), so everything actually cached is
byte-identical across the boundary. Pinned by
`the_cached_region_survives_the_newest_message_becoming_history`.

Also pinned: history is identical whether or not a reminder was sent; no block
is lost in the move; a message is never stripped empty (the API rejects that);
nothing moves when the newest message is an assistant prefill or carries string
content; a real edit beside a reminder is still seen.

3739 tests pass, clippy clean.

### What would show it working, and what would show it wrong

Working: reminder-involved declines fall toward zero, and the `chain_id = 0`
rate on large-write declines falls with them.

Wrong: a rise in 400s (`upstream_rejected`), which would mean the relocation
produces bodies the API refuses — the risk the empty-content and prefill guards
exist to prevent.


## Item 29

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 29 — Message-level relocation, moved to the front of the pipeline

Live in `c44bf160`, 2026-08-09.

Item 28 shipped block-level relocation and it changed nothing measurable: 252
turns, decline rate 13% against 11.6% before, 30% of declines still taking a
large write. The check found why. Of 18 surviving divergences, 12 were one
shape:

```
stored [text] -> current [string]     kinds: [system-reminder] -> []
first_diff_path: content[len 0 vs 1]
```

A message whose entire content was a `<system-reminder>` had vanished, shifting
every index after it. Item 28 walked straight past that case, by a guard written
to avoid a 400:

```rust
if keep.is_empty() { continue; }
```

Block-level relocation cannot help when the churn is a whole message.

### Two changes

**A scaffolding-only message is dropped, not emptied**, and its blocks travel to
the newest message with the rest. Safe because the client's own next request is
that same sequence without it — if the shape were invalid, the client's request
would be refused, and it is not.

**Relocation moved to the front of the request path**, ahead of the capture that
records the turn's originals. Every stage below — capture, compression, the
append-only guard, the bytes on the wire — now sees one canonical history that
does not depend on whether a reminder was attached this turn.

That ordering is what makes dropping a message legal. On the forwarded side
alone it would break `forwarded.len() == original.len()`, and
`ForwardedCountMismatch` would decline every turn.

### Why this is not item 8's defect repeated

Item 8 was capturing originals *after* `ctx_offload` had rewritten them. Offload
decisions depend on store state, so the comparison baseline drifted turn to
turn. Relocation is a pure function of the body: same input, same output,
nothing mutable involved. It produces identical history whether or not the
scaffolding was sent, which is the property the whole fix rests on.

A body with no scaffolding in its history is returned byte-identical, so the
transform cannot become a source of churn itself.

3740 tests pass, clippy clean.

### What would show it working, and what would show it wrong

Working: the `[text] -> [string]` divergence shape disappears, and the decline
rate falls below the 11.6% baseline.

Wrong: any non-429 `upstream_rejected`. Dropping a message is the riskiest thing
the proxy now does to a request, and a 400 is how that would surface.

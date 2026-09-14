# Implemented: working-directory pin preview before the hash

- **Status:** fixed 2026-08-24
- **Source:** `docs/notes/recache-classification.md` ("two causes found")
- **Summary:** `hold` fired correctly but `compute_structural_hash` ran ~1,100
  lines earlier on the client body, and `invalidate` threw the stored prefix
  away 17 ms before the pin restored the tripped line — 46,707-token re-cache.
  `WorkingDirPins::preview` rewrites system to the held dir, hashes that,
  restores the client's bytes; shares `hold`'s decline conditions so they can't
  disagree. Entering a worktree stays uncovered deliberately (extra lines would
  lie about worktree state).


## Detail

*moved from `docs/notes/recache-classification.md`*

**`SessionReplayStore::invalidate` has no production caller.** It is an
ordinary `pub fn` (`prefix_replay.rs:2692`), and the module doc
(`prefix_replay.rs:68-71`) says it "is called on a rebuild boundary to drop the
stored prefix" so a stale prefix cannot be replayed after the provider's cache
died. Every call site is in tests (`prefix_replay.rs:3558, 3763, 4501`). The
documented behaviour does not exist: after a hot-zone change the store keeps its
prefix and the chain id carries across the boundary. This is a live candidate
for part of the 58-turn residue above — worth wiring or worth deleting from the
doc, but not worth leaving as a claim that is not true.


## Context

*moved from `docs/notes/recache-classification.md`*

# Two causes found and fixed — 2026-08-24

Measured on a 1,622-request capture (`~/headroom-capture-alpha`) and on the
proxy log since the 20:30 restart. Method: hash every message of every turn
with `cache_control` stripped, then compare each turn against the one before it
in the same session. `cache_control` has to go, because the tail breakpoint
moves every turn by design and swamps everything else.

| | pairs |
|---|---|
| clean append | 1,593 |
| tail edit, 2 messages deep or less | 114 |
| **deep divergence** | **3** |

Three. And those three are the whole `prefix_content_diverged` class:
143,630 tokens, the largest remaining waste on the current build.


## Detail

*moved from `docs/notes/recache-classification.md`*

## The working-directory pin ran after the hash that judged it

`hold` fires correctly — the log carries `working_directory_held` on both
worktree turns. But `compute_structural_hash` runs about 1,100 lines earlier,
on the client's body, and `replay_store.invalidate` (`proxy.rs:2847`) threw the
stored prefix away 17ms before the pin restored the very line it tripped on.
The turn then forwarded with `replay_skipped: no_previous_turn` and re-cached
46,707 tokens.

The rule was already written down, above the billing-header pin
(`proxy.rs:2696`): "This has to run here, ahead of the fingerprint below and
the prefix-replay capture further down, so every stage sees the pinned form."
The working-directory hold was the one stage breaking it.

Fixed with `WorkingDirPins::preview`: rewrite `system` to the held directory,
hash that, put the client's `system` straight back. It shares `hold`'s decline
conditions — no pin, expired pin, changed line count — so the two can never
disagree about what will be forwarded.

**Entering a git worktree is not covered, and should not be.** Claude Code adds
two lines alongside the path ("This is a git worktree…", "The git stash stack
is shared…"). Pinning the path alone still leaves those changed, so `preview`
declines and the rebuild is correct. Holding them would tell the model it is
not in a worktree while it is.


## Correction

*moved from `docs/notes/recache-classification.md`*

## Correction to the entry above

"`SessionReplayStore::invalidate` has no production caller" is no longer true.
It is called at `proxy.rs:2847`, on the rebuild boundary, exactly as the module
doc describes — and calling it a beat too early is what caused the first bug on
this page.

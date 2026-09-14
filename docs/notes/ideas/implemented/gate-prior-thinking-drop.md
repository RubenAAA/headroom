# Idea: run the prior-thinking drop only on head divergences

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/savings-ideas-2.md` §4.2, `docs/speed-ideas.md` §0.2
  (2026-09-03 window)
- **Summary:** `history_rewritten` fires the thinking drop on the premise that
  the provider rewrites the prefix fresh. It also fires on tail-only
  divergences (`first_diff_index` 37–310), where it rewrites every earlier
  assistant message unnecessarily (conversation `40496550` lost 1.5–3k every
  turn; 260k AM / 146k PM inside the `growth < 0` tail). Gate the drop on
  divergence index 0/1 or tracker-gone — a tail divergence is not a fresh
  head write.
- **Decider available:** `prior_thinking_dropped` now carries
  `agreed_prefix_len` + `forwarded_agreement_len`. 47/49 drops hit an agreed
  head (median 101 msgs) — but `drop_prior_thinking` is deterministic, so
  re-stripping an already-stripped head is free. `forwarded_agreement_len`
  separates the two: high = stripping now busts the cache; low = reproducing
  what's cached. Read that field before changing the gate.
> **09-11 outcome:** gate shipped (thinking_drop_is_free + forwarded-agreement clause); §6 line is now the tripwire. Speed §0b read the same data as 'drop is free' — same conclusion, structural form.
- **Next (superseded):** read `forwarded_agreement_len`, apply the index-0/1-or-gone gate,
  re-run `/tmp/floor.py`: `prior_thinking_dropped` on later turns with
  `first_diff_index > 1` → zero.


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

### 4.2 `history_rewritten` + `prior_thinking_dropped` mid-conversation

`proxy.rs:4182` defines `history_rewritten` as "the replay store cannot
replay this session's history"; `:4189` makes that an offload
boundary, and `compression/prior_thinking.rs` runs on it on the premise that
the provider writes the prefix fresh anyway.

`ctx_offload_accounting` with `history_rewritten=true`: 0 of 16,472 on
09-01/02, 49 in AM, 95 live. Later turns carrying `prior_thinking_dropped`:
AM n=105, 84.8% with a read shortfall, 800k re-write; live n=21, 85.7%,
625k. Inside the `growth < 0` tail it is 260k AM and 146k PM.

The premise holds when the client really diverged at the head (4.1). It
also fires on tail-only divergences (`first_diff_index` 37..310, 11–22
turns per file, ~30k re-write each) where only the tail would have been
rewritten; there the drop rewrites every earlier assistant message too.
Conversation `40496550` lost 1.5–3k on every turn this way
(`prefix_content_diverged` at 37, 48, 69, 79, 82, 92; seven drops).

What to do: run the drop only when the divergence index is 0 or 1, or when
the tracker is gone. A tail divergence is not a fresh write of the head.


## 09-11 verdict

*moved from `docs/notes/savings-ideas-2.md`*

- **4.2 — OPEN, measured bleeding.** The proposed gate (drop only on
  divergence index 0/1 or tracker-gone) is not in the caller: the gate is
  still flag + `offload_boundary` + Anthropic-messages endpoint +
  body-contains-"thinking". Joined by `request_id` against
  `prefix_replay_not_replayed`, **12 `prior_thinking_dropped` turns in the
  window sit on   divergences at index 13–743** (64, 76, 86, 116, 135, 150,
  151, 152, 30, 743, two at role-path) — nearly all tail-only, the exact
  case the entry says must not drop. ~593 kB of prior thinking removed.
  This is the one item from this file worth doing, and the fix is small:
  thread the decline's `first_diff_index` (already emitted) plus tracker
  presence into the drop condition.

  **Implemented 2026-09-11** (`prior_thinking::thinking_drop_is_free` +
  gate clause at the `proxy.rs` drop site, 3 unit tests green): the drop
  runs on rebuild boundary, missing tracker (`None`), or forwarded
  agreement ≤ 1 — the provider-side figure, which the agreement helper
  names as the decider. Tail divergences keep head thinking. Verify on
  the next window: `prior_thinking_dropped` turns should only ever show
  short/no agreement; the §6 line above becomes the tripwire.


## Detail

*moved from `docs/notes/speed-ideas.md`*

**2. The thinking-drop gate is one read away from being decidable.** See
`savings-ideas-2.md` section 4.2. `prior_thinking_dropped` now carries
`agreed_prefix_len` and `forwarded_agreement_len`. The first says 47 of 49
`history_rewritten` drops hit a head the client still agrees on, median 101
messages. That alone does not settle it: `drop_prior_thinking` is
deterministic, so re-stripping a head that was already forwarded stripped
reproduces the same bytes and costs nothing. `forwarded_agreement_len`
separates the two — high agreement means the head went out verbatim and
stripping now busts it; low means we are reproducing what is already cached.
Read that field before changing the gate.


## §0b status

*moved from `docs/notes/speed-ideas.md`*

**Thinking-drop gate: decided, no change.** `forwarded_agreement_len`
reads 0 on 11 of 12 `prior_thinking_dropped` events (one `-1`, untracked),
with `agreed_prefix_len` ≈ incoming−1 throughout — the §0 "low" case:
re-stripping reproduces bytes the provider already cached, so the drop
is free. §0 point 2 closes.

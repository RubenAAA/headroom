# Idea: fix msg1-collapse misattribution (don't touch the cache path)

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/savings-ideas-2.md` §4.1 (2026-09-03 window)
- **Summary:** the client inserts a fixed `role:"system"` message at
  `messages[1]` (hash `cb27b5f9`, first seen 08:33:24Z, 691 hits in `.log.1`);
  mid-session it collapses the read to the shared 23,682 block and rewrites
  the conversation (6 turns / 222,716 tokens live). The proxy correctly
  declines replay, but `cache_recache_observed` tags it
  `origin=proxy / early_messages / forwarded_hot_zone` — wrong.
- **Value:** attribution hygiene; the cache path itself is correct.
> **09-11 outcome:** fixed in code (client-evidence skips return origin=client); holding vacuously (trigger absent).
- **Next (superseded):** fix the attribution (client-origin insert); treat the optional
  replay-chain splice-around-fixed-directive as a separate design question,
  not a bugfix. Done when: zero `origin=proxy` events on turns whose
  `prefix_replay_not_replayed` has `first_diff_index=1`, path `role`.


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

### 4.1 Client inserts a `role:"system"` message at `messages[1]`

`messages_rewritten.early_fingerprints` shows the shift on the collapse
turn: old message 1 becomes message 2, message 0 gets a new hash, and the
new message 1 has the same hash `cb27b5f9` in every session.
`prefix_replay_not_replayed` reports `first_diff_index=1`,
`first_diff_path=role`, stored head `assistant`, current head `system`,
`current_original_msgs = stored + 3`.

That hash at index 1 appears 0 times on 08-31 and 09-01/02, 691 times in
`.log.1` (first at 08:33:24Z) and 76 times live. Cost when it lands
mid-session: read collapses to 23,682 and the conversation is rewritten.
Live: 6 turns, 222,716 tokens. AM: 5 turns, 274,506. Client origin; the
proxy declines replay correctly (`cache_stabilization/prefix_replay.rs` already knows the
API accepts system-role messages). `cache_recache_observed` tags these
`origin=proxy / early_messages / forwarded_hot_zone`, which is wrong.

What to do: nothing on the cache path. Fix the attribution. If the message
is a fixed directive, a replay chain could splice around it; that is a
design question, not a bug.


## 09-11 verdict

*moved from `docs/notes/savings-ideas-2.md`*

- **4.1 — fixed, holding vacuously.** The ranking the entry asked for is in
  the code: client-evidence replay skips (`prefix_content_diverged`,
  `shorter_than_stored_prefix`) return `origin=client` above the
  outbound-hash check (`usage_observer.rs:972`), and the outbound hash only
  fires `origin=proxy / forwarded_hot_zone` when the client's zone held
  still (`:989`). Window: zero `origin=proxy / forwarded_hot_zone` events —
  but the trigger is also absent (zero `first_diff_index=1 + role`
  collapses), so this is "no recurrence", not "survived recurrence". Keep
  the §6 tripwire; no work.

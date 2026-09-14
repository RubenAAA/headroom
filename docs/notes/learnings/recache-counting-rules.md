# Learning: recache counting rules (filter, first turns, process scope)

- **Source:** `docs/notes/proxy-experiments-closures.md` (traps); re-verified
  `docs/notes/recache-classification.md` 2026-09-02
- **Claim:** three rules, each violated at least once into a wrong finding:
  (1) `headroom-proxy.log` is never rotated — scope every query by date AND
  process start (JSON-parse, don't regex); (2) drop each conversation's first
  turn when comparing cache cost (writes the whole prefix; inverts comparisons
  — and first turns are 42% of all write tokens); (3) count only
  `event_kind == "drift"` as waste (`expected` resets would ~double it);
  (4) `prefix_replay_applied` means bytes changed, not replay worked — test by
  absence of `prefix_replay_not_replayed` for the request id.


## Denominator rule

*moved from `docs/notes/recache-classification.md`*

**The 3.1% vs 0.16% rebuild-boundary gap is a denominator mismatch, not a
disagreement.** Both come from the same condition — `observe_drift(...).is_some()`
(`drift_detector.rs:467`), used identically by the replay path
(`proxy.rs:2797`) and the J4 offload gate (`proxy.rs:3086`). There is no second
definition. 0.16% (4 of 2,571) counts only turns that emitted a `ctx_offload`
line, which fires solely when a turn had an offload candidate
(`proxy.rs:3113`). 3.1% (245 of 7,839) counts every turn in the replay corpus
(`offload_replay.rs:213-235`). Narrow subset versus whole corpus. Neither
number is wrong; quoting them side by side is.


## Item 27 correction

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 27 — Item 21's hit rate was measured on mislabelled turns

Found 2026-08-09 while checking why chain ids reported "replayed /
no_tracker_for_session", which is impossible.

**The trap.** Both `prefix_replay_not_replayed` and `prefix_replay_applied` fire
for the same request. The second does not mean the replay succeeded — it means
the forwarded bytes changed, which includes a turn that declined the replay and
only had its `cache_control` renormalised. 148 turns in a 90-minute window log
both. Any query that reads `prefix_replay_applied` as "replay succeeded" counts
those as successes.

Item 21 did. Re-derived, with a real replay defined as *no decline logged*:

| | turns | clean hit | bust >=60K |
| --- | --- | --- | --- |
| true full replays | 1266 | 1173 | 7 (0.6%) |
| declined turns | 166 | 24 | 53 (32%) |

Full replay is far better than item 21 said — 0.6% bust, not 16 in 344.

**And declines are far worse than item 19 said.** That item concluded "a declined
replay is not a bust", from one conversation where a `no_previous_turn` turn
still read 222,975 tokens from cache. Across 166 declines, 53 took a large write.
The revert in item 19 still stands on its own measurement — splicing one turn's
bytes onto another's tail cost 204,768 tokens on its first firing — but the
reasoning attached to it was drawn from a single favourable sample and is too
strong. A decline is *often* a bust; what does not follow is that a partial
splice fixes it.

Caveat on the table: "clean hit" requires over 50K read, which a small
conversation cannot reach, so the hit column is biased toward long
conversations. The bust rates are not affected by that bias in the same way, and
0.6% against 32% is not a threshold artefact.

**Rule this earns.** `prefix_replay_applied` means "bytes changed". The only
sound test for a replay is the absence of `prefix_replay_not_replayed` on the
same request id.


## Traps ledger

*moved from `docs/notes/proxy-experiments-closures.md`*

## Measurement traps

These have each produced a wrong finding at least once. Violating one wastes a
day and, worse, produces a confident answer.

- **`~/headroom-proxy.log` is never rotated by the restart path.** A plain
  `grep -c` returns a weeks-long total, not the current run. Scope every query
  by date *and* by the running process's start time. Lines are JSON — parse them
  with `json.loads`, not a regex.
- **`/stats` and `/metrics` are process-scoped** and reset on restart, so they
  need no date scoping. The on-disk savings ledger does not reset.
- **Drop each conversation's first turn** when comparing cache cost between
  runs. It writes its whole prefix, scales with how many conversations start,
  and inverts the comparison when left in. Measured 2026-08-11.
- **`prefix_replay_applied` does not mean the replay worked.** It means the
  forwarded bytes changed, which includes turns that declined. The only sound
  test is the absence of `prefix_replay_not_replayed` for the same request id.
  This is item 27, and it invalidated item 21's headline.
- **`cache_recache_observed` carries `event_kind`.** Most events are `expected`
  — a context reset where the re-creation was going to happen anyway. Counting
  them as drift roughly doubles the reported waste. Filter on
  `event_kind == "drift"`.
- **A rejected request does not enter successful savings.** Terminal 5xx turns
  now enter the separate `failed_work` bucket; 4xx responses still do not enter
  it. Neither can inflate the successful savings ledger.

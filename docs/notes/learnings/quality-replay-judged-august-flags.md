# Learning: the first quality replay judged August's flags, not today's

- **Source:** `scripts/quality-replay/` (`replay.py`, `judge.py`,
  `reforward.py`), runs 2026-10-12/13; Sonnet 5.5 replays, Sonnet judge
  holding the full original conversation. Scratch:
  `~/.cache/hr-retention-check/quality-replay/` (run1–run4, markers.json).
- **Claim:** the 8% win rate for compressed turns (run3) measured the
  2,000-byte offload floor of the Aug 23 capture. Re-forwarded through the
  2026-10-13 proxy, the same captures score 33% ±20% (16 pairs), and no
  compressed turn re-reads a cut output.

## Detail

A capture's `out/` bodies are what the proxy forwarded the day it was taken.
55 of the first 60 picks came from `headroom-capture-netvalue` (Aug 23),
before `offload-loses-at-2000-bytes.md` moved the floor to 20,000 bytes.

**August forwarding (run1–run3, 43 pairs).** Compressed won 17 of 79
judgements, lost 54, tied 8; the original-vs-original control split evenly. What
predicts a loss is whether `ctx_offload` cut the **newest** tool output,
the one the model acts on next. It converts the live tail before the model
sees it, since that costs nothing in cache:

| | lost both judgements | did not |
|---|---|---|
| newest output offloaded | 11 | 3 |
| newest output intact | 10 | 19 |

Those offloads removed 1.4–11 KB. The model saw a 512-byte preview, said
the output was truncated, and asked for it again; the judge, holding the
original, scored that as repeated work. 23 of run3's 34 losses cite it.

**Today's forwarding (run4, 16 pairs, picks from `reforward.py`).** 7 wins,
14 losses, 10 ties. None of the compressed answers mention truncation. Four
losses are `headroom_retrieve` calls the judge calls "a tool that isn't
available"; live, the proxy answers those in a hidden round, so they cost
a round, not a wrong step. The rest read as ordinary disagreement; the
control split 6 / 3 / 7. Too few pairs to say more than "not clearly worse".

**What changed between the two, same client bodies:**

| capture | bodies | bytes removed, Aug → Oct | newest output offloaded, Aug → Oct |
|---|---|---|---|
| netvalue | 4,447 | 249 MB → 195 MB | 460 → 24 |
| blindguard | 7,541 | 153 MB → 405 MB | 0 → 16 |
| vkreview | 1,804 | 7.9 MB → 32.8 MB | 0 → 0 |
| baseline-202609 | 483 | −2.2 MB → 19.1 MB | 0 → 0 |

Today's proxy removes more overall but almost never offloads the newest
output. Only 272 of today's forwarded turns remove 4 KB or more (28
sessions), so the pick pool is small.

**Live check (offload-ledger, 2026-09-29..10-12, 20,000-byte floor):** a
file read that was offloaded is read again within two calls 60% of the time
(n=63), against 29% just under the floor (n=433). Retrievals and re-reads
cost 9% of what offload saves.

## Traps

- Re-forward before judging a setting: replaying `out/` judges whatever
  flags ran when the capture was taken.
- Run `reforward.py` under `systemd-run --user --scope -p MemoryMax=4G`.
  Its first version held every body parsed (13.8 GB); earlyoom here kills
  `headroom-proxy` first and took the live proxy with it.
- run4's judge counted a `headroom_retrieve` call as a mistake. Read those
  losses as a hidden round's cost, not as a quality loss; `judge.py` now
  tells the judge the tool is real.

# Learning: rotations kill in-flight turns — throttle, don't prevent

- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (rotation design)
- **Claim:** exit-IP change RSTs old 4-tuples by design; attempts in flight die
  (drain shrinks the window to seconds, stragglers die truncated-but-marked).
  Fewer rotations = fewer guaranteed kills → 120 s cooldown is the throttle;
  reactive rotations reset the schedule clock.


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

Known non-goals (documented, not bugs):

- Post-commit drops on the direct path cannot retry transparently (client
  holds bytes; re-sending splices generations). Thinking-heavy turns fill
  the 8 KB hold during thinking, so their answer phase is unprotected —
  fixing that needs SSE-aware commit (layering cost + paint delay), deferred.
- Attempt budget stays 3: each pre-commit attempt bills a dead generation;
  more attempts on a down path burns money, it doesn't buy survival.
- Rotations will always kill in-flight turns. Fewer rotations = fewer
  guaranteed kills; the watcher's 120 s cooldown is the throttle.

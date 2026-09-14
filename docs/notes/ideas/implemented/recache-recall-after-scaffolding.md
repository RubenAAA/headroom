# Idea: confirm recall-after-scaffolding placement landed

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/recache-classification.md` (2026-09-02 audit)
- **Summary:** first turns are 41% of all cache-write tokens; the
  `<system-reminder>` CLAUDE.md block (~47 KB, byte-identical per project)
  should be *read*, not written — but recall injection prepends
  session-specific bytes ahead of it and spoils that. Fix in progress at the
  time: place recall after the scaffolding, breakpoint on the scaffolding block.
> **09-11 outcome:** both halves shipped (recall behind scaffolding ctx/inject.rs:419 + scaffolding breakpoint).
- **Next (superseded):** check whether the placement shipped; if yes, move this file to
  `implemented/` with the commit. If no, it is still the cheapest first-turn
  write win on the board.


## Tail confirmation

*moved from `docs/notes/recache-classification.md`*

- **Recall placement: both halves shipped.** Recall sits behind the opening
  scaffolding (`ctx/inject.rs:419` `leading_scaffolding_len`, `:465`, pinned
  by `recall_sits_behind_the_opening_scaffolding` + no-scaffolding and
  double-injection guards). The scaffolding breakpoint exists too
  (`prefix_replay.rs:2142`, pinned by
  `the_opening_scaffolding_adds_a_breakpoint_of_its_own` and
  `the_second_system_marker_pays_for_the_scaffolding_breakpoint`).

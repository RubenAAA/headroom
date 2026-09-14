# Learning: sample sizes before believing a zero

- **Status:** standing rule
- **Source:** `docs/notes/proxy-experiments-closures.md` ("how to measure")
- **Claim:** for an event class at 4% of requests, ~75 requests before an
  observed zero drops below 5% chance, ~115 below 1%. Fourteen requests proves
  nothing — count requests, state the count, do the arithmetic in the report.
- **Also:** no fix without two numbers (before-measurement + after-measurement);
  a built-measured-reverted fix (item 19) beats an unmeasured one.


## Working discipline

*moved from `docs/notes/proxy-experiments-closures.md`*

## How to work

**Investigate before you change anything.** Every claim below was made from a
log window in early August. The proxy has been rebuilt many times since. Your
first job on each item is to decide whether the defect still exists in today's
code against today's traffic. Several of these will already be fixed. Saying so,
with the query that shows it, is a complete and valuable answer.

**No fix without two numbers.** Before: the measurement that proves the defect
is real now. After: the measurement that proves your change removed it. If you
cannot construct the second one, stop and say why — do not ship a change whose
effect nobody can see. Item 19 in the doc is a fix that was built, measured,
and reverted; item 28 is one that shipped and changed nothing measurable. Both
are better outcomes than an unmeasured fix.

**One item at a time, in the order below.** Items 1, 2, 10 and 15 are four faces
of one problem and will collapse into each other. Fixing 1 is expected to close
2 as well.

**Do not quote item 3's numbers.** They are marked superseded: a share of that
"waste" was one stream being charged for another's prefix under a merged key,
which item 11 fixed. Anything resting on them needs re-deriving first.

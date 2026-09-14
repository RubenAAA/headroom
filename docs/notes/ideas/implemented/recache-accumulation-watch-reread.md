# Idea: re-read the accumulation watch after the role predicate

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/recache-classification.md` ("accumulation watch")
- **Summary:** replaying withdrawn scaffolding from the stored copy risks
  accumulation (forwarded bodies larger than client bodies). Baseline 2026-08-26
  over 29,321 turns: median 3–4% *smaller*, 2 turns over 1.2 — nothing
  accumulates. The `role` predicate widens what replay forwards, so exactly
  the change that could move this number.
> **09-11 outcome:** clear (4 over 1.2 vs 2/29,321); re-read only if max reaches ~2.
- **Next (superseded):** re-run the `outbound_body_bytes` vs `client_request_bytes` ratio
  after a few live days; past ~1.2 means accumulation is real.


## Detail

*moved from `docs/notes/recache-classification.md`*

## Accumulation watch — read, and clear

That threshold sat above with no reading behind it for two days. Taken
2026-08-26 over 29,321 priced turns:

```
day          turns   median     p90     max   over 1.2
2026-08-20      42    0.895   0.924   1.002      0
2026-08-22    3575    0.964   0.983   1.202      1
2026-08-23    6987    0.955   0.996   1.112      0
2026-08-24    6646    0.965   0.988   1.172      0
2026-08-25    6440    0.967   0.989   1.213      1
2026-08-26    5631    0.968   0.984   1.056      0
```

Forwarded bodies run 3-4% **smaller** than what the client sent, flat across six
days, and two turns out of 29,321 crossed 1.2. Nothing accumulates.

Read it again once the `role` predicate has been live a few days: it widens what
replay forwards from the stored copy, so it is exactly the change that could
move this number.


## Tail confirmation

*moved from `docs/notes/recache-classification.md`*

- **Accumulation re-read post-role-predicate: clear.** `outbound/client`
  byte ratio over 2,588 priced turns: 4 over 1.2 (max 1.396), against 2 of
  29,321 on 08-26. The widened forwarding the predicate introduced did not
  move the number. Re-read again only if the ratio's max reaches ~2 or the
  over-1.2 count stops being single-digit per thousand.

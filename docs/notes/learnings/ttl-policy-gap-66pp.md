# Learning: the 6.6pp model gap was the TTL policy

- **Source:** `docs/notes/proxy-experiments-2026-08.md` (offload-gap round)
- **Claim:** replaying production's bodies 1h→5m closes the gap exactly; rate metering ignores TTL while 1h costs 2x vs 1.25x, and 5m creates +30.8% (still wins on dollars).


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **The 6.6pp gap between the modelled policy and production was
  `--force-1h-cache-ttl`, not the offload gate.** The real gate replayed over
  blindguard bills −4.8%, matching the model exactly, so the whole gap lived in
  the stages the replay skips. Confirmed by rewriting production's own forwarded
  bodies from `1h` to `5m` and changing nothing else: +1.8% becomes −4.8%.
  Production writes 15,446 message breakpoints against the replay's 7,699, and
  15,400 of 15,400 system breakpoints at 1h where the client mix has 3,670 at 5m.

  | | API | subscription |
  | --- | --- | --- |
  | production, 1h | +1.8% | −1.3% |
  | same bodies, 5m | −4.8% | +2.7% |

  1h wins by 4.0pp on subscription and loses by 6.6pp on API, which is what
  `cache_ttl.rs:20-30` says it should do: rate-limit metering counts writes at
  raw token count with no TTL distinction, while a 1h write costs 2x base input
  against 1.25x for 5m. Keep it while paying by subscription, turn it off on API.
  The swing is not just the multiplier: at 5m the entries expire sooner and
  creates rise 30.8%, and 5m still wins on dollars even paying for those.
  `--force-1h-cache-ttl true` is set at `~/.headroom-flags.sh:232`; the code
  default is `false` (`config.rs:2425`).

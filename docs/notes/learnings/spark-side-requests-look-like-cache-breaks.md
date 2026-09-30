# Learning: a side request made half the Spark "prefix breaks" false

- **Window:** 2026-09-29 22:37Z to 2026-09-30 08:52Z, from the
  `routed_forward_continuity` events and summaries.
- **Claim:** comparing each routed turn only with the session's last turn logs
  two breaks around every side request. Claude Code sends the spinner request
  ("Describe your most recent action…") on the same session key as the real
  turns, with the real conversation plus a different tail. The request
  differs from the turn before it, and the next real turn differs from the
  request. The provider caches every request's prefix, so neither costs cache.
- **Evidence:** 12 of the first 13 breaks with previews involved the spinner
  text. 165 `developer` and 179 `message:user` breaks in about 3,300 turns
  (counts within 10%) pair up that way. Cached fraction stayed at a median of
  0.99 throughout.
- **Fix:** `routed/continuity.rs` keeps the last four turns per session and
  judges a turn against the one it follows best (the newest it extends whole,
  else the longest shared prefix). First summary after the change: 88 appended,
  0 broken at a tool call, 3 broken otherwise in 100 turns, against 80, 3 and 9
  before it.
- **Limit:** the first side request after a real turn still logs, because
  nothing earlier shows it as a side branch. A break that follows none of the
  last four turns is still logged, whatever caused it.

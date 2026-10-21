# Learning: Zen's anonymous free route 403'd for a day, then came back

- **Source:** `~/headroom-proxy.log`, 2026-10-03 to 2026-10-15, plus
  controlled no-key requests on 2026-10-15 before and after recovery.
- **Claim:** the 2026-10-14 `FreeTierError` 403s came from OpenCode's server,
  not a Headroom regression, and they stopped with no change on this side.
  Outside such outages the gate still refuses some request shapes, so a 403
  does not always mean an outage.
- **Outage window:** the proxy logged Zen 200 responses through 13:26 UTC on
  2026-10-14. The first 403 was at 13:49 UTC and the last at 13:52 UTC on
  2026-10-15. The response says the free tier can only be used from within
  OpenCode. That day saw only 10 Spark turns, so volume did not trigger it.
  OpenCode issue #52907 reports the same refusal hitting stock clients on
  2026-10-03.
- **Controls during the outage (2026-10-15 morning):**
  - The same Claude Code Bash-through-Spark request returned this 403 from
    Headroom.
  - OpenCode 1.18.34 in an isolated HOME, with provider-key environment
    variables unset, returned the same 403 directly from Zen.
  - Direct requests with `Bearer public`, current OpenCode user-agent and
    session headers and native core tools, through a fresh relay lane and
    through the lane that had worked, returned the same 403. So did an older
    real session from the local OpenCode database.
- **After recovery (2026-10-15 afternoon):** OpenCode 1.18.34 and 1.19.0
  work anonymously again, and so does the proxy's Spark route unchanged.
  Direct probes show the gate still judges request shape:
  - a lowercase `read` tool gets 200; the same tool named `Read` gets 403;
  - a request with no tools gets 403;
  - `stream:false` gets 403 for `deepseek-v4.1-flash-free`.
  Volume limits come back as 429, not 403
  ([`zen-free-limit-is-per-exit-daily.md`](zen-free-limit-is-per-exit-daily.md)).
  Upstream issue #50627 tracks third-party clients refused by the gate and is
  still open.
- **What the proxy does about it:** `routed::tool_alias` lowercases tool
  names and adds shadow tools when a request has none; `routed::translation`
  forces streaming on Zen's Chat route. On a `FreeTierError`,
  `routed::zen_gate_probe` runs the stock CLI anonymously through the
  refused request's relay lane, and if that is refused too, from this
  machine. It logs `zen_gate_probe` with `verdict=upstream` (refused
  everywhere: wait), `proxy` (stock is served through the same lane: our
  request shape drifted), `lane` (refused only through the lane: its exit
  address is blocked) or `proxy_or_lane` (no usable lane to try).
- **Conclusion:** during an outage nothing on this side helps. Do not add
  speculative headers, rotate lanes, or change Claude's tool behavior to chase
  one. Read `zen_gate_probe` first: change the request only on `proxy`, and
  rotate the lane only on `lane`.

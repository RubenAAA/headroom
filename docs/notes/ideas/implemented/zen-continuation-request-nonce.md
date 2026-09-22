# Implemented: fresh Zen request nonce per continuation POST

- **Status:** landed in tree 2026-09-18, uncommitted (needs rebuild+restart
  to go live)
- **Source:** missed background-agent completion notices, Claude Code +
  Spark session 2026-09-17 eve. Proxy-side forensics over
  `~/headroom-proxy.log`: subagent API traffic clean (200s, billed, steady
  tails); the fault was in the parent/session turns.
- **Mechanism:** 11/11 CCR+memory continuation 403s (`FreeTierError:
  "OpenCode's free tier can only be used from within OpenCode") are
  Spark/zen-route; 0/14 on direct Anthropic/Codex routes in the same window.
  All first-attempt, round 1, clustered (5 in 80 s) — gateway-side gating,
  not deterministic proxy wrongness. Continuations re-send with the forward
  path's header map, replaying the original `x-opencode-request` UUID while
  the real CLI mints one per POST — the only known header-level difference
  between passing originals and failing continuations (UUID replay as the
  trigger is unproven: 25 same-path continuations passed with replayed
  UUIDs, so this is hygiene, not a proven root cause).
- **Worst case observed:** 22:54:28Z, Spark session `97d99abb`, request
  `a8493a9e`: memory continuation 403'd after the turn had already streamed
  (`already_streamed: 1`), so the promised proxy tool call was dropped and
  `stop_reason` downgraded `tool_use → end_turn`
  (`ccr_tool_call_dropped_stop_reason_downgraded`). Client sees a turn end
  with the tool_use missing — completion/result silently lost. Matches the
  reported symptom exactly (transcript ends `end_turn`, nothing delivered).
- **Change:**
  - `routed/quirks.rs`: `refresh_zen_request_id()` — rotates only the
    `x-opencode-request` nonce, presence-gated (no-op off zen). Unit tests:
    rotates-only-nonce + off-zen-noop.
  - `proxy.rs`: both continuation POST loops (CCR ~12531, memory ~13171)
    clone `outgoing_headers` and refresh per send (each attempt = new UUID).
  - `sse/ccr_stream.rs`: both drop/downgrade warns now carry
    `unresolved_tool_name` (was captured, never logged).
- **Deliberately NOT changed:** the downgrade itself. With bytes already on
  the wire a 5xx is impossible, and the empty-turn apology path has learned
  guards against false failure notices. If fresh nonces don't move the 403
  rate, the next step is failing zen-route continuations loudly pre-stream,
  not touching the committed-stream path.
- **Verify live:** after restart, `continuation 403` rate on zen routes over
  the next heavy-Spark window; `ccr: refreshed x-opencode-request` debug
  lines confirm the path is hit.
- **Verified 2026-09-22: it did not move the 403 rate.** Running under the
  built binary (2026-09-20) since 02:43Z, 4 of 8 Spark memory continuations
  still returned 403, all at `attempt: 0, round: 1`. Message-id replay is
  therefore not the sole trigger, as the mechanism note above suspected.
  The refresh stays — it is client-faithful and free. Full measurement,
  with the fallback-session and concurrency hypotheses ruled out:
  `docs/notes/learnings/zen-memory-continuation-403.md`.
  The "next step" named above was taken, but in the other direction: rather
  than failing pre-stream, the empty-turn notice now fires on any turn the
  client saw no visible text in. A thinking block used to suppress it,
  which is what made these failures silent.

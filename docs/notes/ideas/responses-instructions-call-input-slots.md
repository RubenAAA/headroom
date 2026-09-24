# Idea: compress `instructions` + call-input slots on the Responses live path

- **Status:** open (needs measurement first)
- **Source:** upstream `be00a798` (gateway Responses shape, Sept 2026
  session) — skipped as a port because `/v1/compress` has no local
  contract, but its view design covers two slot classes our
  live-zone Responses path (`compress_openai_responses_live_zone`,
  `live_zone_responses.rs`) never touches:
  - `instructions` (top-level system prompt) as a compressible slot.
    `instructions` is not referenced anywhere in our Responses path.
  - call-input fields (`custom_tool_call.input`,
    `function_call.arguments`, etc.) — measured upstream at 9.2% of
    transcript bytes across 51 real Codex sessions, resent every turn.
    `arguments` needs a JSON-safety guard on the way back (the
    provider parses it); upstream also notes real encrypted payloads
    seen there, so the guard refusing is acceptable loss.
- **Value:** up to ~9%+ of Codex transcript bytes currently riding
  the wire uncompressed every turn. No behavior change if the slots
  turn out small on our traffic.
- **Measurement (2026-09-23, 4 recent Codex sessions, client-side
  `~/.codex/sessions` rollouts):**
  - call-input payloads (`custom_tool_call.input`): **5.3% of transcript
    bytes** (0.55MB of 10.5MB), 920B average over 598 calls. Same order
    as upstream's 9.2% over 51 sessions — real, but the smaller half of
    the prize. Caveat: client-side log, not wire bytes; resent-every-turn
    means the per-turn figure is what matters, not the session total.
  - `reasoning`: **53.5%** — correctly untouched by both (encrypted
    content must round-trip byte-identical).
  - `custom_tool_call_output`: 36.4% — already our live-zone target.
  - `instructions` + static context (2026-09-23, rollout client log +
    billed-token cross-check, same 4 sessions, `history_mode: paginated`):
    - `base_instructions` text: **~17.7KB**, static across sessions
      (GPT-6 Codex base prompt; repo AGENTS.md arrives separately via
      `world_state.agents_md`, ~9KB here).
    - Full `world_state`: **~25.7KB** (`permissions` 12.5KB, `agents_md`
      9KB, `host_skills` 3KB, rest small).
    - Static slots ≈ **43KB chars ≈ ~10.8k tokens vs ~190k billed
      input tokens on the last turn of the big session: ~5-6% of the
      per-turn wire.** Same order as call-input — worth porting, not
      urgent. Confirmed static (byte-identical across sessions), so
      prefix-cache-friendly; compression still saves the cache-write +
      the billed read.
    - Wire confirmation (2026-09-24, realistic Codex-shaped body replayed
      through the live-zone dispatcher in-process, 40 items from a real
      rollout incl. 18KB `instructions`): `instructions` **forwarded
      byte-identical, 18,043B in = 18,043B out** — the slot is genuinely
      uncompressed today, not merely unmeasured. Call-input slice of the
      same body: 9.6KB of 345KB input (2.8%; small body, few calls —
      the 5.3% session figure stands). Full big-session accounting:
      call-input 847KB / 15.5MB transcript (5.5%), `instructions`
      18KB resent every one of 1,134 turns ≈ 20MB cumulative —
      i.e. per-turn ~5% each, cumulative `instructions` dwarfs
      call-input. Both slots confirmed real on the wire path.
- **Next:** size `instructions` on the wire — needs one captured
  Responses body (`HEADROOM_CAPTURE_DIR` set + one Codex turn, or the
  `responses_item_summary` telemetry at info). If fat, port the slot
  classes (not the gateway endpoint) into the live-zone dispatcher;
  keep the allowlist invariant (unknown item types still pass through
  untouched). Call-input first (measured 5.3%), `instructions` second
  (unmeasured but structurally static).

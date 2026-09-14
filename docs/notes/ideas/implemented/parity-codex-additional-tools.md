# Implemented: Codex additional_tools lift/restore

- **Status:** done 2026-08-28 (`handlers/responses.rs`, HTTP + WS)
- **Source:** `docs/notes/rust-parity-gaps.md` §9.1 (upstream `25ca5808` + `1617f839`)
- **Summary:** Codex ≥0.149 stopped sending top-level `tools` for cached models
  (rides in `input` as `additional_tools`); downstream readers saw "notools"
  and recorded zero tool-schema savings. Lift before shaping/compression,
  idempotent restore before forwarding (carrier-relative positions, original
  fallback, warn-and-fail-open), shared by HTTP funnel and stateful WS
  compressor. Opt-out `HEADROOM_CODEX_ADDITIONAL_TOOLS_LIFT=0`.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

### 9.1 Codex `additional_tools` — DONE (2026-08-28)

Implemented a shared lift/restore plan in
`crates/headroom-proxy/src/handlers/responses.rs`. The HTTP Responses funnel
lifts after read-only identity/semantic-cache observers and before every tool
consumer, then restores before wire accounting, retries, continuations, and
send. The dedicated Codex WebSocket compressor uses the same helpers, so a
stateful session retains its transcript-owned tools. Restore is idempotent,
keeps carrier-relative positions, falls back to the original definitions if a
consumer empties `tools`, and warns/fails open if restoration cannot be
completed. `HEADROOM_CODEX_ADDITIONAL_TOOLS_LIFT=0` accepts the same false
spellings as Python.

Coverage includes pure multi-carrier lift/restore cases, classic top-level
tools no-op, the opt-out parser, a stateful WebSocket frame, and an HTTP
integration test proving the tools are internally normalized and leave the
proxy back in `additional_tools` form.

<details><summary>Original scoping notes</summary>

Upstream `25ca5808` (the lift) and `1617f839` (the restore, which fixes the
regression the lift caused). `grep -rn additional_tools crates/ --include=*.rs`
is empty.

Codex CLI 0.149.0 stopped sending a top-level `tools` array on `/v1/responses`
for the models its capability cache flags, `gpt-5.6-sol` among them. The
definitions ride inside `input` as items of type `additional_tools`. Every
tools consumer downstream — schema compaction, the output-shaper stratum,
tools token accounting — reads `payload["tools"]` and nothing else, so those
requests classify as "notools" and record zero tool-schema savings while
forwarding correctly. Nobody sees an error; the savings just stop.

The shape of the fix, from `headroom/proxy/handlers/openai.py:794` and `:872`:

1. **Lift**, before shaping and compression. No-op when `tools` is already
   present, so classic-encoding clients are untouched and a future Codex
   reverting the change costs nothing. Concatenate each carrier's `tools` into
   a top-level array, drop the carriers from `input`, and record a restore plan
   holding each carrier's index *among the items that survive the lift* — not
   its original index. Compression rewrites the transcript, and the relative
   slot is what stays valid.
2. **Restore**, immediately before forwarding. This is the part `1617f839` had
   to add after `25ca5808` shipped alone. `tools` is a per-request parameter;
   `additional_tools` is an `input` item and therefore part of the transcript.
   A stateful session — the Codex TUI or app-server over WebSocket — declares
   its tools once and relies on the transcript afterwards, so forwarding the
   lifted shape leaves turn one working and every later turn without shell or
   filesystem access. Stateless HTTP hid this, which is why it shipped.
   The restore must be idempotent: return early when any carrier already
   holds tools, or a second pass duplicates every definition.
3. Log a warning when a restore plan exists and the restore fails. That case
   forwards the lifted shape and will cost a stateful client its tools, so it
   must never pass quietly.
4. Both steps wrapped so a failure never breaks forwarding, and the plan kept
   on the failure path — if the lift raised after mutating the payload, the
   restore is what undoes it.

Env opt-out `HEADROOM_CODEX_ADDITIONAL_TOOLS_LIFT=0`, checked only after the
carrier is found so the flag costs nothing on the common path.

Rust seam: `crates/headroom-proxy/src/handlers/responses.rs:75`
(`handle_responses`) for the lift, and the forwarding point in the same
function for the restore. The compression funnel it feeds already exists.
Upstream's tests are in `tests/test_openai_responses_additional_tools.py` and
port directly.

</details>

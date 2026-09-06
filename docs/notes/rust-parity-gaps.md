# TODO: Rust/Python parity gaps (larger feature ports)

Tracking doc for parity work identified while auditing `42ebbc6c..origin/main`
(40 commits, 23 Python-only with no corresponding Rust change). The small
correctness-bug batch (status_code/5xx accounting, x-headroom-base-url,
non-streaming cache metrics) is being ported directly. The items below are
larger, standalone features — deliberately scoped out of that batch and
tracked here so the thread isn't lost.

## 1. Turn hooks extension — DONE (2026-07-10)

Ported: `crates/headroom-proxy/src/turn_hooks.rs` (core module + registry +
runners + 9 tests), wired at Anthropic/OpenAI pre-send and post-response
seams in `proxy.rs` (byte-identical no-op when no hook registered). OpenAI
chat-completions seam covered by the generic post-response wiring rather
than a dedicated seam (no separate OpenAI CCR path existed to hang a
dedicated seam off). Dashboard/tool-schema-savings aggregate from #1896
still N/A (no Rust dashboard). Full proxy suite green (977+ tests).

<details><summary>Original scoping notes</summary>

**CORRECTION (2026-07-10): an earlier pass of this doc wrongly claimed the
extension point already existed in Rust. It does not — verified via
`grep -rln "turn_hook\|TurnHook\|TurnContext" crates/` returning nothing at
all.** `ec950f7e` (#1891, "add turn-hook extension point for buffered model
turns") — which IS merged into `local/npu-integrated` — is **Python-only**
(`git show ec950f7e --stat`: 4 files, all under `headroom/`:
`headroom/proxy/turn_hooks.py`, `headroom/proxy/handlers/anthropic.py`,
`headroom/proxy/handlers/openai.py`, `tests/test_turn_hooks.py`). The commit
being in our git history just means the *Python* commit is merged, not that
anything was ported to Rust. This whole feature is unstarted in Rust. Always
verify file lists with `git show --stat`, not just commit presence in `git log`.

- Python source of truth, two commits:
  - `ec950f7e` (#1891): `headroom/proxy/turn_hooks.py` — `TurnContext`, the
    `TurnHook` protocol (`on_request(ctx)` / `on_response(ctx, response,
    call_model)`), a module-level registry (`register_turn_hook` /
    `registered_turn_hooks` / `clear_turn_hooks`), and runners
    (`run_request_hooks` / `run_response_hooks`) — inert/no-op when the
    registry is empty, never raises (a failing hook is logged and skipped).
    Wired at two seams: Anthropic pre-send + CCR response seam
    (`handlers/anthropic.py`), and OpenAI Responses tool-shaping point + CCR
    response seam (`handlers/openai.py`).
  - `c9217856`/`281d8802` (#1903/#1896): extends wiring to a THIRD seam — the
    OpenAI `/v1/chat/completions` direct path — plus dashboard surfacing
    (`savings.by_layer.tool_search`, a "Tool-Schema Deferral" card). No
    Rust dashboard template exists today, so that part doesn't apply yet.
- Scope: full net-new Rust port — the hook trait/registry module itself
  (probably `crates/headroom-proxy/src/turn_hooks.rs` or similar), the
  re-drive `call_model` capability (reuses whatever internal re-call path
  the CCR handler already uses — check `crate::compression::ctx_offload` /
  the CCR response-handling call sites for a reusable primitive), and wiring
  at 3 seams: Anthropic pre-send, OpenAI Responses tool-shaping, OpenAI
  chat-completions direct path. This is meaningfully bigger than the other
  3 items below — treat as its own multi-step task, not a quick port.

</details>

## 2. CCR retrieve-tool interception for OpenAI (chat + Responses) — DONE (2026-07-10)

Turned out bigger than the audit item: Rust's `handle_ccr_response` in
`proxy.rs` was hardcoded to `"anthropic"` only — OpenAI chat-completions AND
Responses both got zero CCR interception at the HTTP layer, despite
`response_handler.rs` already having a working (but never-invoked)
`"openai"` provider branch. Ported: added `"openai_responses"` provider
branch to `headroom-core/src/ccr/response_handler.rs` +
`tool_injection.rs` (mirroring Python `62cd3072` exactly), generalized
`handle_ccr_response` to take a `provider: &str` param with an
`extend_or_push` helper covering all 3 providers' sentinel conventions, and
replaced the Anthropic-only gate with a 3-way path-based dispatch covering
`/v1/messages`, `/v1/chat/completions`, and `/v1/responses`. New wiremock
integration test proves OpenAI Responses `headroom_retrieve` interception +
continuation. Full suite green (core ccr: 182 tests, proxy: 1248 tests).

<details><summary>Original scoping notes</summary>

- Python: `62cd3072` (#1898), ~243 lines in `headroom/proxy/handlers/openai.py`.
  Wires CCR (context-compression-retrieval) `headroom_retrieve` tool-call
  interception into the `/v1/responses` handler, so a retrieval marker the
  model emits gets resolved server-side instead of leaking to the client.
- Rust: `crates/headroom-proxy/src/handlers/responses.rs` has zero CCR/
  retrieve-tool wiring today (confirmed via repo-wide grep, including all
  unmerged branches — `fix/ccr-retrieve-full-only` looked promising by name
  but doesn't touch `responses.rs` at all).
- Scope: needs the same interception pattern the Anthropic buffered path
  already has (`crate::compression::ctx_offload`, CCR store lookups) ported
  into the Responses handler's request/response cycle.

</details>

## 3. Audit-safe compression mode (SmartCrusher) — DONE (2026-07-10)

Ported to `crates/headroom-core/src/transforms/smart_crusher/{config.rs,crusher.rs}`:
3 opt-in config fields (default off, byte-identical for non-opt-in callers),
5 helper primitives (canon/scan/splice/verify), wired into
`SmartCrusher::smart_crush_content` (the path `apply()` actually calls for
real tool-output compression). Simpler than Python: Rust's crush pipeline
emits no CCR markers yet, so only the statistical row-drop case needed
guarding, not the marker-hidden case. Invalid regex patterns panic at
construction (matches Python's `raise ValueError`; every SmartCrusher
constructor is infallible, so `Result` threading was out of scope). 335
smart_crusher tests pass (7 new), full core (1637) + proxy (979+) suites
green.

<details><summary>Original scoping notes</summary>

- Python: `bb112dd1` (#1899), new ~258-line module
  `headroom/transforms/smart_crusher.py` additions: `audit_safe` config,
  `protected_patterns` (regex/marker matching), fail-closed verification so
  compliance-relevant content (audit markers, leakage flags) can't be
  silently dropped or rehidden by compression.
- Rust: `crates/headroom-core/src/transforms/smart_crusher/crusher.rs` and
  `config.rs` have no `audit_safe`/`protected_pattern`/`fail_closed` concept
  at all (confirmed via grep across every branch in the repo, including the
  several branches with "audit" in the name — those are about an unrelated
  CCR/"toin" audit concept, not this feature).
- Scope: net-new feature — `SmartCrusherConfig.audit_safe` /
  `protected_patterns` / `fail_closed_on_protected_loss` fields, plus
  scan/splice/verify logic in the crusher's compression path.

</details>

## 4. Bedrock per-user application-inference-profile ARN pinning — DONE

`HEADROOM_BEDROCK_MODEL_MAP` is parsed and applied in
`crates/headroom-proxy/src/bedrock/vendor.rs:39`, and both call paths use it:
`invoke.rs:211` and `invoke_streaming.rs:188`, each logging when an override
lands. An earlier pass of this doc listed the feature as missing. That was
wrong — same failure mode as the item 1 correction, so re-verify with a grep
for the feature marker string rather than trusting the entry.

<details><summary>Original scoping notes</summary>

- Python: second half of `33c7f6cd` (#1795) — `HEADROOM_BEDROCK_MODEL_MAP`
  env-driven override letting operators pin specific per-user Bedrock
  application-inference-profile ARNs (for cost attribution).
- Rust: the *other* half of this commit (the `global.*` inference-profile
  prefix normalization) is already correctly ported and tested
  (`crates/headroom-proxy/src/bedrock/vendor.rs:16`, `GEO_PREFIXES`). Only
  the `HEADROOM_BEDROCK_MODEL_MAP` per-user ARN override is missing — this
  is a genuinely new operator-facing feature, not a bug fix, so lower
  priority than the others above.

</details>

## 5. CCR retrieve-tool interception on streamed turns — DONE (2026-08-07)

Item 2 above wired `handle_ccr_response` for all three provider shapes, but
every one of those call sites sits inside `if should_buffer_for_cache`, which
is `!is_sse && status.is_success()`. Tool *injection* has no such condition, so
on a streamed turn the proxy offered `headroom_retrieve` and then passed the
model's call straight to a client that had never heard of it — Claude Code
reports `No such tool available: headroom_retrieve` and the turn dies. Since
every interactive client streams, the feature only ever worked on the batch
path and in tests.

Fixed by `crates/headroom-proxy/src/sse/ccr_stream.rs`: a stream adapter ahead
of the telemetry tee that suppresses the `headroom_retrieve` block (its name
arrives on `content_block_start`, before any of its bytes would go out),
withholds `message_delta` / `message_stop`, rebuilds the turn into the
non-streaming response shape at end-of-stream, and hands it to the same
`handle_ccr_response`. The resolved turn is synthesised back into SSE events
numbered after the blocks the client already received, so the client sees one
message and no retrieval round trip. A turn that retrieves nothing gets its
withheld events released verbatim.

Continuation-round usage travels to the SSE outcome through an
`Arc<Mutex<CcrRoundUsage>>` and is folded in the way the buffered path does,
so a retrieval no longer books only the last round's tokens. An unresolvable
retrieve (mixed with a client tool call, out of rounds, upstream refused) is
dropped rather than forwarded — forwarding it is the bug.

Anthropic `/v1/messages` only. The OpenAI chat-completions and Responses
stream shapes have their own event vocabularies and remain buffered-only.

## 6. Proxy-owned tools that nothing answered — DONE (2026-08-07)

Item 5 fixed one instance of a class. An audit for the rest found two more,
plus one claim that turned out to be wrong.

**Routed models (live).** `handlers/local_model.rs` injects `headroom_retrieve`
*and* hands compression a CCR store, so it emits retrieval markers too, and
resolved neither arm. Two comments in that file contradicted each other:
`:706` claimed "this path injects headroom_retrieve and resolves it", `:2154`
said the opposite and justified it on the grounds that the Claude path did not
resolve it either — true when written, false since item 5. Fixed: the streaming
arm chains `sse::ccr_stream` after the OpenAI→Anthropic translator (the turn is
already in the Anthropic vocabulary by then, so the same rewriter serves it),
and the buffered arm calls `handle_ccr_response` on the OpenAI shape before
translating. `CcrShape` carries the continuation shape — Anthropic,
chat-completions, or Responses `output[]` — with converters both ways.

**Memory tools (latent, worse in kind).** `proxy.rs` injects `memory_save`,
`memory_search`, `memory_update`. `MemoryHandler::handle_memory_tool_calls`
executes them correctly and had **no callers in the workspace** — so these
failed on every path, buffered included; this was never a streaming gap but an
unfinished wiring job. Dormant only because `memory_enabled` requires
`HEADROOM_MEMORY_ENABLED=1` and is not even a CLI flag. Fixed by
`handle_memory_response` (shaped after `handle_ccr_response`, same round cap
and mixed-tool rule) wired into the buffered branch, and by the stream
rewriter treating memory tools as proxy-owned alongside CCR.

**OpenAI streaming — the audit was wrong.** The claim that streamed
chat-completions and Responses requests were offered an unanswerable tool does
not hold: the whole injection block is gated on
`endpoint == AnthropicMessages`, so its OpenAI arm is unreachable. Those
clients were never handed the tool. A `can_resolve` gate was added at the
injection site anyway — inert today, but it means lifting the Anthropic-only
restriction cannot silently reintroduce the bug.

**The invariant, guarded.** `tests/integration_tool_invariant.rs` asserts what
leaves the proxy on the wire: the Anthropic stream path may advertise
`headroom_retrieve` because it resolves it; the OpenAI-shaped paths are offered
none of the proxy's own tools. Nothing tied injection to resolution before, and
they drifted apart three separate times.

## 7. Metrics that were measuring the wrong thing — DONE (2026-08-07)

Found by reading a live proxy's `/stats` and `/metrics` after ~9.5 hours.

**`token_savings_percent` read 30,891%.** It divided `saved` by
`attempted_input`, and every outcome site filled that field from the provider's
`usage.input_tokens`. On Anthropic that excludes cache reads and writes, so on
a warm session it collapsed to the uncached remainder — 8,059 against 2,489,559
saved — and held a value byte-identical to `uncached_input_tokens`, which was
the tell. Two fixes: the percentage now divides by `input` (the sum of
pre-compression sizes, matching `RequestOutcome::savings_pct`), and
`attempted_input_tokens` is fed from the compressible baseline via
`OutcomeContext::attempted` at all five outcome sites. The session reads 68%.

Note what was NOT done and why: making the field the *whole prompt*
(input + cache_read + cache_write) is the obvious reading and is wrong. It
would feed `original_tokens = attempted + saved` on non-compressed turns and
balloon `input` to cache-re-read scale, collapsing the figure to ~1.6% — a
denominator dominated by the same prefix counted once per turn.

**Retries could not see in-band SSE errors.** Anthropic reports rate limits and
overload inside a 200 body on streaming requests. Both retry loops branched on
HTTP status alone, so those turns looked like success. `forward_http` now peeks
the first SSE event and re-sends on a leading `overloaded_error`,
`rate_limit_error` or `api_error`, counted as
`proxy_upstream_retries_total{reason="in_band_sse"}`. Peeked bytes lead the
client's stream, so a clean turn is unchanged.

**Compressors ran and threw the result away.** Every arm of
`dispatch_compressor_uncached` asked "did this help?" as `compressed ==
original` — byte identity, which misses a compressor that *rewrote* a block
without removing anything. `SearchCompressor` does this whenever its caps do
not bite. Measured over 862 captured requests: 99.6% of its token-check
rejections had dropped zero content, and no accepted compression anywhere in
the corpus grew in bytes. A size gate at the end of dispatch now returns
`NoOp`, which also lands in the memo's skip tier so repeat blocks
short-circuit. `BlockAction::NoCompressionApplied` carries `declined_by` so
these are counted in `proxy_compression_declined_no_shrink_total` rather than
silently absorbed — otherwise the rejection counter would fall whether the
waste went away or the gate started declining work that pays.

**A capture-worker alarm that was not real.** `ctx_observe_worker_gone` looked
like 60,312 drops; scoped to the running process it was zero. `proxy.log` is
not rotated, so raw counts span months and restarts. Root cause (a panic in
`extract_constraint` slicing mid-character, killing the worker thread and with
it the channel receiver) was already fixed by `0eaf0ce1`. Kept a recurrence
guard: drops counted in an atomic, reported at 1/10/100/… at ERROR, exposed via
`dropped_captures()`. `claude-launcher` now rotates the log on proxy start,
keeping four generations.

## 8. Known gaps left open

- **Routed buffered Responses arm** — DONE (2026-08-07). The SSE fold moved out
  of `handle_buffered_responses_response` into `responses_stream_to_turn`, which
  collects `output[]` items from `response.output_item.done` and lets the
  `output[]` on `response.completed` win over them (a call whose item event
  never arrived is still in there). The arm now takes a `RoutedCcr` and runs the
  same resolve-then-book order as the chat arm, converting through
  `responses_output_as_anthropic_turn`. Two things fell out of writing the
  tests: keying the text fallback off "no items at all" dropped the text of any
  turn that also made a tool call, and `responses_output_as_anthropic_turn`
  hardcoded `stop_reason: "end_turn"`, which would have handed a client a tool
  to run inside a turn marked finished. Both fixed; 3 new tests.
- **OpenAI-native streaming CCR.** Not a live bug (see item 6), but if
  injection is ever extended to those endpoints, `/v1/chat/completions` and
  `/v1/responses` each need their own stream rewriter.
- **`--ccr-inject-marker`** — WIRED (2026-08-07). The entry above said the
  feature "genuinely exists in `content_router.rs`; the proxy flag simply is
  not wired to it". Half right: `ContentRouterConfig.ccr_inject_marker` is
  declared and defaulted `true` but read by nothing either, and the proxy never
  constructs a `ContentRouterConfig` at all. What actually decides whether a
  `<<ccr:HASH>>` marker is emitted is whether the caller hands compression a
  `CcrStore` (`live_zone.rs:1836`, early-returns on `ccr_store.is_none()`).
  So the flag is now gated at `AppState::ccr_store()` — false withholds the
  store, and marker text and store writes stop together. Suppressing only the
  text would offload blocks the model has no handle to ask back. Python pairs
  them the same way: every `ccr_inject_marker=False` call site also passes
  `ccr_enabled=False`.

## 9. Round of 2026-08-28 — upstream `32d7ca45..` (v0.36.2..v0.37.0, 40 commits)

Merged at `78240da9`. Twelve commits touch no Python source (CI bumps, docs, a
Windows process-tree fix). Of the 28 that do, sixteen patch subsystems Rust
does not have — `wrap`, `learn`, `doctor`, the dashboard, MCP/Serena,
`session_engine`, the `/v1/compress` sidecar, the memory graph adapter, Copilot
provider routing — so they are a scoping question, not a gap. Two are answered
by the Rust design and are covered in 9.3 below. Every Rust-applicable gap
identified in this round is now ported.

Ported this round, committed 2026-09-01 and untested against production:
`27b4e2d1` (proxy token on every route and transport, `proxy_auth.rs`),
`7c0b8860` and the resolve-timeout half of `3e3c4094` (`upstream_guard.rs`),
`b9d7dcc3` (atomic ledger write, and the flush moved to `spawn_blocking`),
`9c30b629` (cross-turn dedup skipped on streaming chat), `8884d873` (the
64-word Kompress floor), `7784bb18` (datetime-prefixed prompts no longer typed
as grep output), `4408e881` (`HEADROOM_PROTECT_READS`, including Copilot
`view` and both local-shell wire shapes), `25ca5808` + `1617f839` (Codex
`additional_tools` lift/restore), and `36cc8001` (corporate CA trust on every
outbound reqwest client). The caller-supplied upstream property from
`3e3c4094` is enforced at the connection boundary too: the approved DNS answer
set is pinned into the reqwest transport that performs the request.

Two commits look like gaps and are not. `split_into_sections` and
`extract_json_block` (`content_router.rs:1105`, `:1154`) carry the pre-fix
bracket-counting bug from `8884d873`, but neither has a caller outside its own
tests, so nothing reaches it. Verify with
`grep -rn 'split_into_sections\|extract_json_block' crates/ --include=*.rs`
before spending time there.

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

### 9.2 Corporate CA trust — DONE (2026-08-28)

All production async and blocking reqwest builders now start from the
TLS-aware constructors in `ssl_context.rs`: the main proxy upstream, ctx
fetch, Copilot device auth, CLI tools download, CLI client, and subscription
tracker. `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE` now have actual replacement
semantics (`tls_built_in_root_certs(false)`); `NODE_EXTRA_CA_CERTS` remains
additive. A source-wiring integration test rejects any future direct reqwest
builder outside `ssl_context.rs`.

<details><summary>Original scoping notes</summary>

Upstream `36cc8001` made Copilot's token refresh honor a corporate CA bundle.
The Rust side has the harder half already: `ssl_context.rs:126`
`configure_client_tls` reads `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE` as
replacements and `NODE_EXTRA_CA_CERTS` as an additive bundle, and handles
`HEADROOM_TLS_STRICT`. It has **zero callers**:

```
grep -rn configure_client_tls crates/ --include=*.rs
```

returns only the definition. Every outbound client builds its own TLS:

| Client | Site |
|---|---|
| main proxy upstream | `proxy.rs:261` |
| a second proxy client | `proxy.rs:11221` |
| ctx fetch | `ctx/fetch.rs:205` |
| Copilot device auth | `bin/headroom_cli/copilot_auth.rs:70` |
| CLI tools fetch | `bin/headroom_cli/tools.rs:338` |
| CLI | `bin/headroom_cli.rs:365` |
| subscription tracker | `subscription.rs:29` |

Behind a TLS-inspecting corporate proxy every one of these fails, and the
symptom is a certificate error from whichever ran first. The work is small —
route each builder through `configure_client_tls` — but it is seven sites, and
each needs a look at whether it should trust the operator's bundle. The three
CLI ones talk to GitHub and the tools registry, so probably yes. Worth a test
that fails if a new `reqwest::Client::builder()` appears without it; otherwise
the eighth site will skip it too.

</details>

### 9.3 Caller-supplied upstreams — DONE (2026-08-28)

The guard now reaches the connection boundary rather than stopping at a URL
wrapper. `ResolvedCallerUpstream` carries the original hostname URL plus the
exact `SocketAddr` set returned by its bounded DNS lookup; every answer is
rejected if any one points inward unless the operator explicitly allowlisted
the destination. `forward_http` builds/selects a caller-only reqwest transport
with `resolve_to_addrs`, so Host/TLS SNI retain the hostname while the connector
can use only those approved addresses. That closes the validate-then-resolve
DNS-rebinding window.

Caller-selected transports disable ambient/provider proxies and automatic
redirect following, and every retry, streamed retry, CCR/memory continuation,
and turn-hook re-drive keeps the selected transport. A bounded 128-entry cache
keyed by hostname plus the complete approved address set preserves connection
pooling without letting a later DNS answer reuse the earlier transport.
Coverage includes loopback/metadata rejection, mixed-answer rejection,
transition-address cases, hostname-based pinned routing, redirect refusal, and
provider-proxy bypass.

<details><summary>Original scoping notes</summary>

`3e3c4094` closed an SSRF where some resolution paths validated a
caller-supplied upstream and others did not. Upstream's answer was to move the
guard into the resolution helpers themselves — `proxy_routes.py`,
`proxy_targets.py`, `registry.py` all switched to
`is_safe_upstream_url_async`, so a path cannot resolve without validating.

Rust does not have this bug. `header_upstream_override` (`proxy.rs:2360`) has
exactly one call site (`:2513`), and `is_safe_upstream_url` runs on it at
`:2519`. The WebSocket path never reads `x-headroom-base-url` at all:
`websocket.rs` builds its upstream from `state.config.upstream`. The only
other `UpstreamOverride` setter, `foundry/mod.rs:152`, comes from operator
config rather than caller input. The related Vertex SSRF (`7c0b8860`) misses
Rust for the same kind of reason — the proxy never builds a regional hostname
from `location`, it joins a path onto the base the operator configured
(`config.rs:1049`).

So there is nothing to fix. What there is, is a gap between how the two
codebases hold the property. Python enforces it: skipping the guard means not
resolving. Rust achieves it by having one call site that happens to be
correct, and nothing stops a second one appearing. The header is caller-
controlled, the guard is a free function, and the reviewer who adds the next
override path has to know to call it.

Two ways to close that, in rising cost:

- A test that asserts `header_upstream_override` has one caller and it is
  guarded. Cheap, and it fails loudly when someone adds the second.
- Make the guard structural: have the override return a type that can only be
  built by passing through `is_safe_upstream_url`, so an unvalidated upstream
  cannot be expressed. Larger, and it ends the class rather than the instance.

The timeout half of `3e3c4094` is already ported: `RESOLVE_TIMEOUT` (5s) wraps
`lookup_host` in `upstream_guard.rs` and logs
`upstream_guard_resolve_timeout`, matching Python's bounded `getaddrinfo`.

</details>

### 9.4 Completion verification

`cargo test -p headroom-core --lib --tests`: 2056 passed, 2 ignored, plus all
core integration binaries green. `cargo test -p headroom-proxy --lib --tests`:
1794 library tests passed, 1 ignored, plus every proxy integration binary green
(only explicitly live-store/operator-data tests ignored). `cargo check -p headroom-proxy
--all-targets` and `git diff --check` are green.

## Notes on methodology (for whoever picks this up)

- Cross-checked every item against **all** branches in the repo (not just
  `main`/`local/npu-integrated`) before concluding "unstarted" — several
  branch names suggested existing work (`*audit*`, `fix/ccr-retrieve-full-only`,
  `tejas/turn-hooks-extension`) but turned out to be false leads (stale
  branches, unrelated features, or Python-only commits already merged
  elsewhere). Always verify with `git grep <feature-marker-string> <ref> --
  '*.rs'` across `git for-each-ref` before assuming something needs
  building from scratch.
- The 5th correctness item from the original triage (`55efb1c7`, OpenAI tool
  observations mutable in cache mode) had conflicting triage verdicts (C vs
  A). **Resolved 2026-08-07: verdict C, not a gap — the bug class does not
  exist in this architecture.** The Python bug lived in a *role-gated* freeze
  boundary: `_strict_previous_turn_frozen_count` treated a trailing
  `role: "tool"` / `"function"` message as non-mutable, so the whole
  conversation froze before `ContentRouter` ran and there was no live tool
  observation left to compress. Rust has no such function anywhere in
  `crates/`, and computes the boundary purely by token accounting —
  `prefix_replay.rs:517-529` walks per-message estimates against what the
  provider reported cached, with no role branch. Independently,
  `live_zone_openai.rs:11-13` already defines the live zone as the latest
  `tool` message's content *and* the latest `user` message's text, which is
  the post-fix Python semantics. Verified structurally; not measured against
  a live OpenAI cache-mode session.
</content>

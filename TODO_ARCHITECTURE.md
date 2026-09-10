# TODO_ARCHITECTURE: model routing cleanup

Goal: one path — `resolve route -> pre-stages -> translate -> send -> fold response -> book outcome` — with one owner per step. The `translate==true` path does this (`handlers/local_model.rs:86`, 546 lines of stage calls into `routed/`). The bypasses are down to three early returns: sidecar, cursor, passthrough.

Scope: `handlers/local_model.rs` (`handle_messages:86`, 546 lines), `config.rs:1795-2101` (route table), `model_router.rs` (cost router), `openai/{request,response,stream}.rs`, `sse/{outbound,ccr_stream}.rs`, `routed/{auth,ccr,outcome,prepare,redaction,response_arms,retry,routing,sidecar,transforms,translation}.rs`, `codex/mod.rs`, `cursor/*`, `proxy.rs:forward_http:3248`.

Status of the C9 split (2026-09-10 worktree): `local_model.rs` went 3064 → 546 lines; the stages moved to `routed/` largely intact. The moves were NOT pure — behavior deltas found so far: redaction-on now skips the routed sidecar, transport exhaustion returns 503+`Retry-After` (was bare 502), fallback unredacts to client text first and leaves the failed attempt unbooked, the redact store attaches even with zero outbound spans (so continuations redact too), and the stream flag split into `upstream_is_stream` vs `downstream_is_stream`. Anything below that says "pure move" means "no *intended* behavior change" — verify against these deltas.

## How routing works today (with receipts)

`POST /v1/messages` reaches `handle_messages` only when `local_model` or `model_routes` is set (`proxy.rs:1518-1522`). Else it falls to catch-all `forward_http`.

Decision order inside `handle_messages`:

1. Parse body. Non-JSON → rebuild `Request` → `forward_http` (`108-121`). Skips all routing.
2. Sidecar (`127-131`). `is_describe_action_sidecar` → `routed/sidecar.rs:97` (`try_routed_sidecar`, one bounded attempt, no retry) → else direct `sidecar::try_handle` on `effective_upstream`. Returns before route match, replay store, cache tracker — on purpose. Shapes the request (`shape_sidecar_request:55`, forces `reasoning:minimal` + 512 tokens) with empty-text fallback (`69`, `226-234`); redaction-on skips the routed sidecar (`111-118`).
3. Cost router (`139-142`). `routed/routing.rs:41` (`apply_model_routing`) rewrites `parsed`+`body` together, sets `identity_model: Option<String>` when the id changed. Consults `model_route_cooldowns` so a just-failed target is skipped. Runs again inside `forward_http` (`proxy.rs:5415`).
4. Cursor (`154-173`). `model_routes.find(matches(body_model))` → `resolve_cursor_agent` (`config.rs:1855`: `{effort}` → low/medium/high/xhigh, `max` → xhigh, default high). If hit: `derive_session_key` + `cursor::handler::handle:77` + fold (`message_from_frames:236`) + return. Checked before URL match. Skips CTX transforms, shed, holds, compression/replay, prune/compact/pin/order, translation, outcome booking, retry/fallback. NOTE: the old doc claimed a cursor redaction seam (`seam_outbound`) and a `restore_response` symbol — neither exists on this path now; treat that claim as stale.
5. Table match (`175-208`). Legacy `local_model` exact match → `(upstream, translate=true, None, None)` (`routed/routing.rs:102-111`), else first `model_routes.find(matches)` → `(upstream, translate, target_model, auth_env)` (`113-126`, `find_route_target:98`). `matches` is `config.rs:1868`. `cursor:`-only routes yield `None` here (handled in step 4).
6. No match (`175-202`). Rebuild `Request` from post-router bytes → `forward_http` + return. Skips everything below.
7. Auth (`210-220`). `routed/auth.rs:21` (`upstream_auth_headers`) + `:93` (`route_auth_headers`: `none` = anonymous, else `env::var`) + `:45` (`codex::resolve_codex_routing_headers:199`, unchanged). Then `inject_opencode_headers` iff host is `opencode.ai` (`local_model.rs:218`). Session-id/thread-id + `x-codex-turn-state` via `apply_codex_session_headers:332` / `capture_turn_state:354`, impl `auth.rs:148/180`, map at `codex/mod.rs:79`.
8. Passthrough (`222-232` → `routed/response_arms.rs:385` `handle_passthrough`). `translate==false`: single `state.client.post`, raw body, drop-filter only (`:433`). No retry, no transforms, no redaction, no outcome. Logs `model_route_passthrough` (`:399`).
9. Translate (`234+`). The full pipeline:
   - `prepare_turn` (`routed/prepare.rs:34`, calls `transforms.rs:54`): session/lane/conversation keys, usage begin, CCR inject, offload, memory tools, retrieve tools, output shaper; concurrency shed (`63-81`), lane pins + system holds (`94-103`), redact (`110-118` → `redaction.rs:17`, restores at `:38/:66`, table at `:88`), compression+replay (`119-130`), prune/compact/pin/order (`147-183`)
   - translator pick in `routed/translation.rs:30` (`translate_routed_request`): `target_model.is_some()` → `anthropic_to_openai_responses_request` (`openai/request.rs:120`) else `anthropic_to_openai_request` (`:21`), then `apply_target_model_override` (`response_arms.rs:15`, called at `translation.rs:45-50`: Responses path gets `model`+`store=false`+`stream=true`, chat path gets all-false so the client `stream` survives), then `maybe_inject_openai_prompt_cache_key` (`translation.rs:76-92`, must run post-translation; `--context-edit` / `--force-1h-cache-ttl` skipped as Anthropic-only)
   - endpoint (`translation.rs:101-117`): strip trailing `/v1`, target → `{base}/v1/responses` (or `chatgpt.com/backend-api/codex/responses` when ChatGPT auth, `108-109`), else `{base}/v1/chat/completions`. `upstream_is_stream = is_stream || target.is_some()` (`:98`); downstream follows the client (`:99`)
   - `build_routed_outcome_context` (`routed/outcome.rs:19`, call at `local_model.rs:275-290`), fill `reroute={from: identity_model, to: body_model}` (`296-301`) and redact store (`307-311`, now attached even with zero outbound spans)
   - send+retry (`routed/retry.rs:26` `send_with_retry`: Codex-only 401-refresh-once via `codex/mod.rs:117` at `retry.rs:63`, outside retry budget; transport exhaustion → 503+`Retry-After` at `retry.rs:178-180`)
   - terminal: non-OK + `identity_model.is_some()` → park target in cooldowns + fallback (`local_model.rs:378-421` → `routing.rs:150` `dispatch_route_fallback`: unredact to client text first, failed attempt deliberately unbooked, insert `SkipModelRouting`, re-`forward_http` reusing `request_id` and never re-applying rules via `proxy.rs:5411-5425`); non-OK without identity → `handle_routed_error_response` (`response_arms.rs:38`, status + `Retry-After` passthrough); OK → streaming (`response_arms.rs:255`) / buffered-Responses (`:191`) / buffered-Chat (`:127`) arms. `read_routed_body` (`:90`) unified only the read + non-OK book.

All HTTP sends use the shared trusted `state.client` (`proxy.rs:59`, built by `upstream_client_builder:322` + `ssl_context`). Only `forward_http`'s `SelectedUpstream` may use the `caller_clients` LRU pool (`proxy.rs:64`, struct `:3083`, wiring `:387`).

## What is already well layered (keep)

- Route table parsing: `parse_model_route:2040`, `split_auth_env:2016`, `TRANSLATE_KEYWORDS:2002`, shadow warnings (`1965-1989`), ambiguous-Codex warnings (`1926-1955`). One parser, first-match-wins, warnings at startup. Do not split.
- SSE framing: `sse/outbound.rs` (`frame:23`, `message_start:28`, block fns `:49/:66/:100`) shared by `openai/stream.rs` (struct `:55`, `emit_outcome:341`, `Drop:19`) and `cursor/translate.rs` (`Translator:81`). Header comment still documents the deliberate split. Keep and extend.
- Routed pre-stages: `prepare.rs::prepare_turn` over `transforms.rs` (`apply_bytes_stage:393`, `merge_routed_compression_report:452`, `apply_compression_and_replay:485`, compaction `:418`). One impl, two call shapes.
- Outcome funnel: `outcome.rs::build_routed_outcome_context:19`, `book_routed_outcome`, `book_routed_outcome_with_ccr` (`:233` serves `model_route_served`). Streaming books via `StreamTranslator::emit_outcome` + `Drop` safety net; buffered books after resolve. Same `RequestOutcome` sink.
- Shared retry atoms: `backoff_ms` (`proxy.rs:9048`), `is_retryable_transport_error` (`proxy.rs:3044`), `maybe_inject_openai_prompt_cache_key` (PAYG-gated, `proxy.rs:8680`). The loops dupe; the atoms do not.
- CCR plumbing (new since the split): streamed continuations de-stream via `non_streaming_continuation_request` (`ccr_stream.rs:1031`), restored to `stream:true` only for mandating backends (`restore_stream_when_mandated:1043`, currently `chatgpt.com`); streamed SSE continuations fold back via `openai/response.rs:16` (`responses_stream_to_turn`) in `proxy.rs::continuation_turn_from_body`. Buffered resolvers alternate to fixpoint (`routed/ccr.rs:62-82`) over shared `proxy::handle_ccr_response` / `handle_memory_response`.

## Problems, each with location + status

### P1. Two types named `ModelRoute` — STILL OPEN
`config.rs:1795` (provider entry) vs `model_router.rs:34` (cost rule). Same name, unrelated `matches()`, same logs. Nothing renamed.

### P2. `target_model.is_some()` is the shape switch — PARTIAL (was: 7 sites in one file)
Translator + URL + cache-key centralized in `translation.rs:30-117`, but the predicate still fans out (`translation.rs:40/79/86/107`, `local_model.rs:366`, `response_arms.rs:334-337`). No `shape_for`/`endpoint_for`/`codex_endpoint`; ambiguous-Codex still startup-warn only (`config.rs:1939`).

### P3. Two buffered folds + two streaming machines + one half-unification — PARTIAL (moved, not merged)
Only the read + non-OK book is shared (`response_arms.rs:90`). Two buffered arms (`:127` vs `:191`, ordering now mirrors but code still duped), third error exit (`:38`), and `ccr_stream.rs` framing (`:300/:311/:525/:632`) still doesn't import `sse::outbound`.

### P4. Retry loop exists twice; passthrough/sidecar have none — PARTIAL
Routed loop extracted (`retry.rs:26`, same atoms, outside-budget 401 refresh, per-path labels); `forward_http` keeps its own loop (`~:5880`); passthrough/sidecar stay single-shot by design. No shared `next_delay`/`SendPolicy`.

### P5. Auth built in four places — PARTIAL
One module (`auth.rs`, both callers share `upstream_auth_headers`) but no single `auth_headers()` covering opencode-inject and turn-state; the old builders all still exist. Provider quirks untouched.

### P6. Provider quirks live in routing code — STILL OPEN
Branches relocated, not converted: ChatGPT URL switch (`translation.rs:108-109`), `is_chatgpt_auth` threading, turn-state headers (`auth.rs:148/180`), `opencode.ai` sniff (`local_model.rs:218`), `/v1`-strip variants, cursor effort mapping. A new provider still means new branches, just in smaller files.

### P7. Cursor bypasses the pipeline; translate==false bypasses it too — STILL OPEN
Still early returns (`local_model.rs:128/165/222`) with no `Transport`/`SkipSet`/stage matrix and no shared outcome for cursor/passthrough.

### P8. CCR/memory/proxy-tools resolvers have routed twins — PARTIAL
Buffered CCR+memory alternate to fixpoint over the shared proxy cores (`routed/ccr.rs:62-82`, `RoutedCcr:137`, `assemble:176`); session keys frozen via identity-aware derivation (`transforms.rs:85-127`, `drift_detector.rs:1274`, `usage_observer.rs:265`, `drift_detector.rs:1342`); streaming keeps its own driver (`ccr_stream.rs:775/1042/1088`, `CcrShape` at `:80`, choice at `response_arms.rs:334-338`). No `retrieval.rs`/`session_keys()`.

### P9. `handle_messages` is 546 lines of stage calls — DONE (structural)
Table scan, header build, retry loop, folds each have one owner module. Caveat: the moves were not pure (see the delta list under Scope) — "motion only" is not established; treat C9-verify as proving the deltas, not assuming purity.

## Changes — refactor, not behavior change

Same rhythm as before: **lock the behavior with tests first, then move code, then let the lock prove nothing changed.** The savings, recache-safety, and no-compromise guarantees are inside each step's lock and verify. Deltas already in the tree (Scope list) need retroactive locks before further moves touch those areas.

Standing definitions used by every step:

- **Recache-safe** means: frozen-prefix bytes unchanged (sha256), session/lane/conversation key derivations byte-identical, replay-hit rate on the recorded corpus unchanged. A step that moves key or byte logic carries key fixtures and body goldens; a step that doesn't, doesn't need them.
- **Savings-honest** means: ledger Σ(model_offload) == tracker Σ(offload_savings_usd), zero-price models book 0 outside honest offload, no turn prices the same tokens twice. Steps touching money carry the reconciliation row.
- **No-compromise** means: same answers (retrieval/translation parity fixtures), same spend on retry fixtures (re-sent bytes, not just status codes), p99 overhead delta ≈ 0 on untouched paths, exact header sets per route kind.

### C1. Rename one `ModelRoute` (cheap, do first) — OPEN

- **Lock:** inventory every consumer of the name AND the log strings first — source refs plus log-based consumers (PromQL/dashboards, `proxy_log_audit.py`, statusline scripts, docs). The inventory is the sweep checklist; a rename that orphans a dashboard is an observability break, not a cleanup.
- **Move:** `config.rs:1795 ModelRoute` → `ProviderRoute`, `model_router.rs:34 ModelRoute` → `CostRule`. Log strings (`model_route_passthrough` at `response_arms.rs:399`, `model_route_served` at `outcome.rs:233`, `model_route_fallback` at `local_model.rs:382`) renamed with every checklist consumer updated in the same patch.
- **Verify:** `rg 'ModelRoute' crates/` shows two disjoint sets; checklist fully swept; `cargo test -p headroom-proxy config:: model_router::` passes. Rename-only by construction — no logic touched, so savings/recache cannot move.

### C2. Extract `route_resolve()` — PARTIAL (co-located, not extracted)

- **Lock:** order-equivalence harness over a fixture corpus of (body, config): the old inline logic vs the new resolver must return identical decisions. Fixtures: sidecar beats table; cost rewrite feeds table; cursor beats URL; first-match wins + shadowed warns; `MODEL=cursor:X` never yields HTTP; ambiguous Codex warns; rewritten id + `identity_model` preserved for fallback. The harness pins *which model serves*, not just which variant matched — order changes move traffic across cache namespaces, so this is the recache lock.
- **Move:** `routed/routing.rs:41/98/150` already co-locates cost-rewrite + match + fallback with order preserved, but there is no `handlers/route_resolve.rs` and no `RouteDecision` carrying post-rewrite model/identity/request_id (split across the `ModelRouting` return + a separate `identity_model` binding, and the cursor pre-scan still lives inline at `local_model.rs:154-163`). Finish the extraction; the cost-router mutation + cooldown consultation stay in the caller, in today's order.
- **Verify:** harness green; no `model_routes.iter().find` left outside the resolver; fallback machinery (`dispatch_route_fallback`, `SkipModelRouting`, cooldown park) untouched and its tests unchanged.

### C3. One shape switch — PARTIAL (relocated, not tabled)

- **Lock:** decision-table test over (target × auth × host) written first: no target → chat translator + `/v1/chat/completions`; target → responses translator + `store=false/stream=true` + `/v1/responses`; ChatGPT-auth + `api.openai.com` + target → codex URL; ambiguous (translate + `api.openai.com`, no target) → chat + per-request warn. A misroute serves the wrong model — wrong bill *and* a new cache namespace — so the table is the guarantee, and the ambiguous case gets a warn-level event (dashboards see it), not just a debug log.
- **Move:** `shape_for` + `endpoint_for(base, shape, auth, host)` in `openai/`; the `108-109` ChatGPT arm becomes `codex_endpoint()` in `codex/mod.rs`. All six remaining call sites consume the computed decision; none re-derive from `target_model`.
- **Verify:** table green; `target_model.is_some()` appears only in the decision fn; ambiguous-case alarm fires per occurrence.

### C4. One `auth_headers()` — PARTIAL (co-located, not unified)

- **Lock:** header snapshot tests per route kind, asserting EXACT sets: `none` → exactly `Content-Type` — no `Authorization`, no Codex identity headers (the credential-leak row: `none` must suppress Codex headers, not just the bearer); env bearer → bearer with nothing Codex (route credential replaces Codex); codex-file → full Codex set; caller-echo → caller auth preserved; `opencode.ai` → inject headers present.
- **Move:** `routed/auth.rs::auth_headers(...)`; main path and `try_routed_sidecar` both call it. Turn-state write-back stays with response handling — the seam is named, not split. Vertex/Bedrock/Gemini/Foundry untouched.
- **Verify:** snapshots green; old helpers (`upstream_auth_headers`, `route_auth_headers`, `inject_opencode_headers` as inline builders) deleted so no alternate construction path can drift.

### C5. One send policy — PARTIAL (extracted, not unified)

- **Lock:** retry fixture corpus asserting (a) identical status mapping incl. transport→502, (b) identical **re-sent byte counts** per scenario — retries are spend, so the savings guarantee is measured in bytes, not codes, (c) 401-refresh at most once, outside budget, Codex-routes only, (d) cooldown park only when `identity_model.is_some()`. `next_delay` (backoff + `Retry-After` math) extracted first with unit tests; both loops call it. NOTE: transport exhaustion now returns 503+`Retry-After` (`retry.rs:178-180`) — lock current behavior first, then decide if the corpus blesses it.
- **Move:** `SendPolicy` beside `retry.rs`. `forward_http` delegates delay math only — it keeps its SSE in-band handling, caller transports, and continuations (no shared loop where behaviors differ). Passthrough and sidecar keep their single-post code: the current code *is* the no-retry guarantee, a policy struct saying "don't retry" would only obscure it.
- **Verify:** corpus identical including re-sent bytes; mock-upstream 401→refresh→200 counts as one logical attempt; `record_upstream_retry` labels preserved per path.

### C6. Fold unification — PARTIAL (sharing only)

- **Lock (written before any merge):** byte-identical golden tests for both buffered arms; SSE frame goldens (`output_text.done`-replay, `arguments.done`-replay, refusal, reasoning-signature). Usage-preservation test: every usage token in is booked or explicitly excluded with a reason — dropped usage understates spend (fake savings) and corrupts the offload counterfactual. The streamed-vs-buffered CCR-rounds question is decided here, not during the merge: prove continuations never book separately (then thread in) or keep the exclusion with comment + test. Merging first and asking later is how double-booking happens.
- **Move:** `fold_buffered(body, shape, original)` inside `response_arms.rs`; `read_routed_body` stays step one; one resolve-book-envelope; `handle_routed_error_response` the single non-OK exit. `ccr_stream.rs` imports `sse::outbound` for framing only. `reroute={from,to}` provenance survives the fold untouched.
- **Verify:** goldens byte-identical post-merge; usage test green; NEW reconciliation row green: ledger Σ(model_offload) == tracker Σ(offload_savings_usd) on a reroute corpus (nothing asserts that today).

### C7. Bypasses explicit — OPEN

- **Lock:** per-variant stage matrix pinning CURRENT behavior first — passthrough: byte-clean, no outcome; cursor: seam-only redaction, no outcome; sidecar: bounded single attempt. sha256 body-equality fixtures for passthrough. The matrix is the product decision surface: any cell the pipeline flips is a visible, approvable semantic change, never a drive-by. Default direction is skip-everything-today's-bypasses-skip; stages are opt-in per variant, each opt-in a matrix row with its own justification (recache-safe by default, not by review).
- **Move:** `Transport` trait + explicit `SkipSet`. Shared outcome only where the matrix approves it; cursor books under the cursor model id — never the `$3/M` fallback (phantom-spend row in the matrix). Offload provenance flows through for routed variants.
- **Verify:** matrix green with only approved cells flipped; passthrough sha256 unchanged; ledger-vs-tracker offload reconciliation still green; p99 overhead delta ≈ 0 on untouched paths. Product sign-off happens on the matrix diff, which is small and readable because the lock came first.

### C8. Resolver unification — PARTIAL (alternation done in place, drivers still split)

- **Lock:** byte-exact key fixtures — session/lane/conversation/fingerprint vectors frozen. Any derivation change fails loudly and forces versioned keys + dual-read migration; silent key drift invalidates every stored session at once (fleet-wide hit-rate cliff, zero errors). Retrieval parity fixtures: buffered vs streaming produce identical tool sets AND identical answers on the same corpus — a "saving" that changes answers isn't one. Fixpoint termination test against `MAX_RESOLVER_ALTERNATIONS` (`proxy.rs:2771`). Replay-hit rate on the recorded corpus pinned (the recache gate).
- **Move:** drivers stay split (buffered alternation in `routed/ccr.rs:62-82`, streaming in `ccr_stream.rs:1042-1088`); extract the shared core (fetch → splice → classify) both call, with shape conversions staying in `ccr_stream.rs`. `session_keys()` is a pure reader over frozen derivations, never a place to "improve" derivation.
- **Verify:** key fixtures byte-identical; parity fixtures equal incl. answers; replay-hit rate unchanged.

### C9. Split `local_model.rs` — DONE STRUCTURAL, purity unverified

- The split happened (3064 → 546 lines) as `routing/prepare/translation/retry/response_arms/sidecar/auth/ccr/redaction` (+`transforms/outcome` pre-existing). Actual filenames differ from the old plan (`route_resolve.rs` → `routing.rs`, `translate_pipeline.rs` → `translation.rs`, `routed_send.rs` → `retry.rs`, `routed_fold.rs` → folds stay in `response_arms.rs`, `routed_auth.rs` → `auth.rs`, `retrieval.rs` → alternation inside `ccr.rs`); `handle_messages` is stage calls.
- Remaining verify: full `make ci-precheck` on a clean tree, plus retroactive locks for the five behavior deltas listed under Scope. `<800 lines` is met (546) — keep it as a ratchet, not a goal.

## Order of work (locks before moves)

1. C1 (rename + consumer sweep) — no behavior change possible.
2. Retroactive locks for the five Scope deltas (redaction-skips-sidecar, 503-transient, fallback-unredact, widened redact attach, stream-flag split) — the tree moved without them.
3. C2 extraction finish (equivalence harness first) — locks the order everything else relies on.
4. Locks only, no moves: C6 goldens + usage test, C3 decision table, C4 snapshots, C7 matrix, C8 key + parity fixtures. This is the corpus the rest of the plan proves against.
5. C3 + C4 moves, then C5 (delay helper → policy), then C6 merge.
6. C7 approved flips, C8 core extraction.

## Done when

- `rg 'ModelRoute'`: two names, zero confusion; all log consumers swept.
- `target_model.is_some()`: one decision site; misroute alarm exists.
- `local_model.rs` stays < 800 lines of stage calls; no inline table scan, header build, retry loop, or fold logic returns.
- Per-variant stage matrix green with only approved flips; passthrough sha256 unchanged.
- Streamed/buffered CCR booking decided with proof, tested either way.
- Ledger Σ(model_offload) == tracker Σ(offload_savings_usd) on reroute corpus.
- Replay-hit rate and recache counters unchanged on recorded corpus.
- `make ci-precheck` passes.

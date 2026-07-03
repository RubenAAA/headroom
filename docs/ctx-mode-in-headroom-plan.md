# Plan: Absorb context-mode into the headroom proxy (Rust)

Goal: retire the context-mode MCP server + hook fleet and provide **1:1 functionality**
from inside `headroom-proxy` / `headroom-core` (Rust crates only — the Python proxy is
not touched), such that:

1. Claude Code loads **zero** MCP tool schemas and **zero** SessionStart routing block
   (today ≈ 4–6K tokens of every context window).
2. The model never calls a ctx tool; all protection is passive, on the wire.
3. **No prompt-cache invalidation on Anthropic's side** — the transforms must never
   cause a re-cache the untransformed request wouldn't have caused.

---

## 0. The cache-safety contract (read first — every phase is subject to this)

Anthropic's prompt cache keys on the **exact byte prefix** up to each `cache_control`
breakpoint. Claude Code resends the *original* conversation every turn (it keeps its own
transcript; it never sees our rewrites). Therefore what Anthropic caches is the
**post-transform** conversation, and the only way successive requests hit cache is if
headroom re-produces byte-identical output for every historical message, every turn.

Invariants every new transform MUST satisfy:

- **I1 — Pure function.** Any replacement of a content block is a deterministic pure
  function of the block's original bytes (`digest = f(bytes)`). No timestamps, no
  counters, no RNG, no "current session state" in the replacement text. The BLAKE3
  content hash is the only identity. (`volatile_detector.rs` heuristics become a CI
  assertion against our *own* generated text.)
- **I2 — Stable re-application.** Unlike the existing live-zone compressors (which only
  touch the latest user message, `headroom-core/src/transforms/live_zone.rs:35-45`),
  the ctx offload transform re-applies to **every** qualifying block in the whole
  conversation on **every** request. A block offloaded in turn N is resent raw by the
  client in turn N+1 (now inside the cached prefix) — we must replace it with the
  *identical* bytes again or the prefix diverges and re-caches. I1 guarantees we can
  do this without any store lookup: the replacement is recomputable from content.
- **I3 — Append-only upstream view.** Across turns, the upstream-visible conversation
  must only ever *grow*. Never un-offload a previously offloaded block, never change
  thresholds mid-session (thresholds are static config, like `tool_prune.rs`'s
  operator-config-only rule, `cache_stabilization/tool_prune.rs:22-25`).
- **I4 — Injections are persisted and replayed byte-identically.** Anything we inject
  (recall block on session start) is decided **once**, on the first request of a
  conversation, persisted keyed by conversation identity, and re-injected verbatim on
  every subsequent request. Never inject new content into an existing prefix
  mid-session — new information may only enter via the live zone (latest user message)
  or, preferably, not at all after turn 1.
- **I5 — Respect existing frozen-count semantics.** `compute_frozen_count`
  (`headroom-core/src/cache_control.rs:109`) stays the authority for the *lossy*
  compressors. The ctx offload is exempt from the frozen floor **only because** I1+I2
  make re-application byte-stable; assert this with golden tests (same conversation
  replayed over 5 simulated turns → byte-identical prefixes).
- **I6 — Tokenizer gate.** Keep the existing rule: if the replacement doesn't shrink
  token count, emit the original (live_zone.rs PR-B4 gate). Note: this decision itself
  must be deterministic (it is — pure function of bytes).

**Why this is strictly cache-better than status quo:** today, a 50KB tool_result enters
the prefix raw and is cached raw (costing cache-write + cache-read tokens forever).
With the offload, Anthropic caches the small digest instead. The first request after
enabling the feature on an *ongoing* session will re-cache once (prefix changes); fresh
sessions never re-cache. Recommendation: enable at a session boundary; optionally add a
grace rule "only offload blocks not yet inside a cache_control prefix at first sight"
(off by default; complicates I2, so v1 accepts the one-time re-cache on ongoing
sessions).

---

## Parity matrix (context-mode → headroom)

| context-mode surface | headroom replacement | phase |
|---|---|---|
| `ctx_execute` / `ctx_execute_file` / `ctx_batch_execute` (sandbox keeps bytes out of context) | Model uses **native Bash/Read**; proxy offloads verbose `tool_result` bytes to store + digest (CTX-3). Sandbox no longer needed — protection moved server-side. | 3 |
| `ctx_index` | `headroom ctx index <path>` CLI → `POST /ctx/index` (same chunker) | 5,6 |
| `ctx_search` | `headroom ctx search "<q>"` CLI via Bash → `GET /ctx/search` (result is itself small; if verbose, offloaded like anything else) + automatic recall injection (CTX-4) | 4,5 |
| `ctx_fetch_and_index` | native WebFetch (result offloaded by CTX-3) or `headroom ctx fetch <url>` for the disk-cache/TTL behavior | 5 |
| `ctx_stats` | `GET /ctx/stats` + `/metrics` gauges (extends existing `tokens_saved_total`) | 6 |
| `ctx_doctor` / `ctx_upgrade` / `ctx_purge` / `ctx_insight` | `headroom ctx doctor|purge|...` CLI subcommands | 6 |
| PreToolUse routing (`routing.mjs`: WebFetch deny, curl/Read>50KB redirects, nudges) | **Obsolete by construction** — native tools are now context-safe, so there is nothing to route away from. See "accepted differences". | — |
| PostToolUse session-memory capture (`session/extract.ts`, 26 event categories) | Passive extraction from request bodies (proxy sees every tool_result + user prompt anyway — higher fidelity than hooks) | 2 |
| SessionStart injection (routing block + resume snapshot + session directive) | Routing block: deleted (not needed). Resume snapshot + directive: proxy-side injection into first user message of a resumed conversation (CTX-4, I4-compliant) | 4 |
| PreCompact snapshot / Stop lifecycle | Compaction detected on the wire (prefix shrinks / summary message appears); snapshot built from sessions DB at that moment | 2,4 |
| FTS5 KB (porter+trigram dual FTS5, RRF, BM25 5.0/1.0, Levenshtein, flood-guard, timeline sort) | Port to `rusqlite` in `headroom-core::ctx::store` — schema-compatible so existing `~/.claude-personal/context-mode/content/*.db` can be **reused in place** (knowledge base survives migration) | 1 |

---

## Phase CTX-1 — Storage foundation (headroom-core)

New module `headroom-core/src/ctx/` (sibling of `ccr/`):

- `store.rs` — port of `context-mode/src/store.ts`:
  - `sources` table, `chunks` FTS5 (`tokenize='porter unicode61'`), `chunks_trigram`
    FTS5 (`tokenize='trigram'`), `vocabulary` table — **byte-compatible schema** with
    the TS store so we open the existing per-project DBs at
    `~/.claude-personal/context-mode/content/<sha256(projectDir)>.db` (WAL, same
    sharding as `session/db.ts:502-527`). Config flag to use a headroom-owned dir
    instead for clean installs.
  - Query pipeline parity: BM25 `bm25(chunks, 5.0, 1.0)`, dual-table query merged via
    Reciprocal Rank Fusion, proximity re-rank for multi-term queries, Levenshtein typo
    correction against `vocabulary`, flood-guard, `sort: relevance|timeline`,
    `contentType: code|prose` filter. Port `search/unified.ts` logic 1:1; golden-test
    against fixture DBs produced by the TS implementation (same query → same top-k).
  - Chunker parity: markdown-heading / code-block chunking from `ctx_index`, JSON
    keypath chunking from `ctx_fetch_and_index`.
- `sessions.rs` — port of `session/db.ts` schema (per-project sharded sessions DBs at
  `~/.claude-personal/context-mode/sessions/<hash>.db`), same event row shape so
  existing session memory remains searchable.
- Deps: `rusqlite 0.32 bundled` is already in headroom-core (used by
  `ccr/backends/sqlite.rs`). **Verify FTS5 + trigram tokenizer are compiled in**
  (libsqlite3-sys bundled builds with `SQLITE_ENABLE_FTS5`; trigram needs SQLite
  ≥ 3.34 — bundled is far newer). CI smoke test: create both virtual tables.
- All DB access on `tokio::task::spawn_blocking` / a dedicated thread with a channel,
  mirroring `capture.rs`'s never-block-the-request-path rule.

Verify: `cargo test -p headroom-core ctx::` — schema round-trip against a copied real
content DB; RRF/BM25 golden tests.

## Phase CTX-2 — Conversation identity + passive session capture (headroom-proxy)

- `ctx/identity.rs` — conversation fingerprint. Existing `derive_session_key`
  (`drift_detector.rs:356`, auth-header/IP based) identifies a *client*, not a
  conversation. Add: `conv_id = blake3(system_hash ‖ hash(first user message content))`,
  plus a rolling **prefix-chain table** (`conv_id, turn_n, prefix_hash`) so we can
  recognize: (a) continuation (prefix hash matches), (b) **compaction/resume** (known
  client, new short prefix whose first user message contains a compaction summary /
  matches a stored snapshot marker), (c) branching. Persist in the sessions DB.
- `ctx/extract.rs` — port `session/extract.ts` pattern-matching over the request body:
  on each request, diff against last-seen turn count for this `conv_id` and extract
  only the *new* messages' events — `rule, file, cwd, error, git, task, plan, env,
  skill, constraint, decision, subagent, data, intent, …` (full 26-category port; the
  TS file is the spec, 2960 lines — this is the largest porting item, budget
  accordingly). Error detection parity: exit-code / `error:` / `FAIL` / `failed`
  patterns (extract.ts:330-341).
- Runs post-forward on the background thread (like `capture.rs`); never blocks or
  mutates the request. Zero cache impact (pure observer).

Verify: replay corpora captured via `HEADROOM_CAPTURE_DIR` (PR-J0 already gives us
real request bodies) through extract.rs vs the TS extractor; diff event rows.

## Phase CTX-3 — Tool_result offload transform (the core; headroom-proxy)

New transform `compression/ctx_offload.rs`, applied in the `/v1/messages` request path
(`proxy.rs` compression gate, ~line 630) **before** the live-zone compressors:

1. Walk every `tool_result` (and oversized `text`) block in **all** messages.
2. Qualify: `len > ctx_offload_min_bytes` (default 50 000, mirroring routing.mjs's
   Read threshold; static config per I3).
3. For each qualifying block, compute `hash = blake3(bytes)[:24]` (reuse
   `ccr::compute_key`, `headroom-core/src/ccr/mod.rs:70-78`) and replace content with
   a deterministic digest:
   - Structural digest via the existing detectors/compressors (magika_detector →
     log/diff/search/json compressor or smart_crusher) — all already pure functions.
   - Footer marker: `<<ctx:HASH>> (N bytes offloaded; retrieve: headroom ctx get HASH
     or headroom ctx search "...">>` — fixed template, no volatile fields (I1).
4. Side effects (background thread): store original in the **CCR store** — this phase
   finally wires `CcrStore` into the proxy (currently `headroom-proxy` has zero
   references to it; the sqlite backend + `<<ccr:HASH>>` marker format exist unwired)
   — with a long TTL (config; default ≥ 7 days, not the 1800 s CCR default), and index
   the content into the FTS5 KB with the tool_use's originating command as chunk title
   (parity with `ctx_batch_execute` labels: title = the Bash command string from the
   paired `tool_use` block — deterministic, comes from the same request bytes).
5. **Idempotency:** if a block already *is* a digest (contains our marker), pass
   through untouched. Because f is pure, re-processing the raw block next turn yields
   identical bytes anyway; the marker check is just a fast path.

Cache analysis: satisfies I1/I2 by construction (digest recomputable from resent
original bytes each turn; no store dependency in the request path). Store loss/TTL
expiry affects only *retrieval*, never the wire bytes.

Also in this phase: keep `maybe_inject_context_management` (context_editing.rs)
composable with this — server-side `clear_tool_uses` now fires on already-small
digests, which is fine; consider raising its trigger or disabling when ctx_offload is
on (config decision, default: both on).

Verify: golden multi-turn replay test — simulate 6 turns of a captured conversation,
assert `blake3(transformed_prefix(turn_n)) == blake3(prefix_of(transformed(turn_{n+1})))`
for the overlapping region, i.e. **prove zero prefix drift**. Add this as a reusable
harness (`tests/ctx_cache_stability.rs`) — it is the acceptance test for the whole
project. Second test: run `volatile_detector` over our own digest output → zero hits.

## Phase CTX-4 — Recall injection + resume snapshot (headroom-proxy)

Replaces SessionStart/PreCompact hook behavior:

- On a request classified (CTX-2) as **new conversation** or **resume/compaction**:
  build the injection once — resume: XML snapshot from the sessions DB (port
  `snapshot.ts::buildResumeSnapshot`, reference-based TOC, + `retrieval-marker`
  semantics); fresh: top-k KB recall via BM25 against the first user message text +
  session directive text. Inject as a prepended `text` block **inside the first user
  message** (never `system` — subscription clients own their system prompt, and the
  first-user-message position keeps `tools`/`system` byte-identical).
- Persist `(conv_id → injected_bytes)` in the sessions DB; on every subsequent request
  for that `conv_id`, re-inject the stored bytes verbatim at the same position (I4).
  If the store row is missing (crash, purge), **inject nothing** for the rest of that
  conversation (fail-safe: absence is also byte-stable going forward only if it was
  absent from turn 1 — so on row-miss for a conversation we've seen before, skip
  injection and log; one re-cache is the worst case, same as today's hook loss).
- New-info-mid-session: none injected into the prefix, ever. On-demand recall goes
  through the CLI (CTX-5), whose output arrives as a fresh tool_result in the live
  zone — inherently cache-safe.

Verify: extend the CTX-3 stability harness with injection enabled; resume-snapshot
output diffed against TS `buildResumeSnapshot` on the same sessions DB.

## Phase CTX-5 — Explicit retrieval without MCP (CLI over native Bash)

The model's active verbs, at zero context cost (no tool schemas — Bash already exists):

- `headroom ctx search "<query>" [--sort timeline] [--source S] [--type code|prose]`
- `headroom ctx get <hash>` (fetch offloaded original from CCR store; large output is
  itself re-offloaded next turn — self-consistent)
- `headroom ctx index <path|-->`, `headroom ctx fetch <url>` (24 h disk cache, TTL,
  HTML→markdown — port of `ctx_fetch_and_index`'s fetch pipeline)
- Implemented as thin HTTP clients against new proxy endpoints `GET/POST /ctx/*`
  (axum sub-router; localhost-bound or token-gated).
- Discovery: one short paragraph in `~/.claude/CLAUDE.md` replaces the entire
  SessionStart routing block ("verbose output is auto-archived; retrieve with
  `headroom ctx search/get`"). ~80 tokens vs today's ~2-3K.

## Phase CTX-6 — Meta surface, stats, decommission

- Endpoints + CLI: `/ctx/stats` (port `AnalyticsEngine.queryAll()` semantics:
  bytes-kept-out-of-context per category, call counts, est. tokens, savings ratio —
  now computed from *actual* offloaded byte counts, more honest than routing.mjs's
  `bytesAvoided` estimates), `/ctx/doctor` (runtime checks: DB open, FTS5/trigram
  probe, store paths, CCR TTL config), `/ctx/purge` (session|project scope, confirm
  required), `ctx insight` (opens URL). Extend Prometheus `/metrics`:
  `ctx_offloaded_bytes_total`, `ctx_recall_injections_total`, fold into existing
  `tokens_saved_total`.
- Slash-command parity: tiny local skills (`/ctx-stats` → `curl /ctx/stats`) if wanted.
- Config (`config.rs`): `ctx_enabled`, `ctx_offload_min_bytes`, `ctx_store_dir`,
  `ctx_offload_ttl`, `ctx_inject_recall`, `ctx_endpoints_bind` — all env-var mirrored
  like existing flags; feature off by default.
- Decommission checklist: remove context-mode plugin (hooks + MCP server) from
  `~/.claude-personal/settings.json`/plugin registry; point store dirs at the existing
  DBs; update `~/.claude/CLAUDE.md`; run one session with `HEADROOM_CAPTURE_DIR` on
  and confirm via the stability harness that live traffic shows zero prefix drift.

---

## Phase CTX-7 — Re-cache watchdog + statusline surfacing (headroom-proxy)

Independent of CTX-1..6 (useful on its own; ideally lands **before** CTX-3 so the
offload rollout is monitored from day 1).

- `cache_stabilization/usage_observer.rs` — response-side tap. Tee the upstream
  response (SSE: parse `usage` from the `message_start` event; non-stream: from the
  JSON body) without altering pass-through bytes. Record per `conv_id` (CTX-2
  identity; fall back to `derive_session_key` until CTX-2 lands):
  `(turn, input_tokens, cache_creation_input_tokens, cache_read_input_tokens, ts)`.
- **Re-cache detection rule:** flag when `cache_read(turn_n) <
  cache_read(turn_{n-1}) + cache_creation(turn_{n-1})` by more than a slack margin
  AND `cache_creation(turn_n)` exceeds the expected new-tail size. Suppress/downgrade
  when `ts_n - ts_{n-1} > 5 min` (Anthropic cache TTL expiry — a legitimate full
  re-write, not a bug). Attach the drift reason from `drift_detector` when available
  (system / tools / early-messages axis).
- Surfacing:
  - `GET /cache-health` — JSON: last event `{age_s, reason, wasted_tokens}`, rolling
    cache-hit ratio, per-conversation table.
  - `/metrics`: `cache_recache_events_total{reason}`,
    `cache_recache_wasted_tokens_total`, `cache_read_ratio` gauge.
  - WARN log line per event with full detail.
  - **Statusline**: a script (wired via Claude Code `statusLine` setting) that curls
    `localhost:8787/cache-health` and appends `⚠ recache 2m ago: tools drift, ~41K
    tok` when the last event is fresh, else a compact `cache ✓ 97%`. Endpoint must
    answer in <10 ms (in-memory snapshot, no DB on the read path).
- This is also the live acceptance monitor for CTX-3/4: any bug violating I1–I4
  shows up here as a recache event with a reason, immediately, in the terminal.

Verify: unit tests over synthetic usage sequences (healthy growth, TTL expiry,
genuine drift); integration test with a mock upstream emitting scripted usage blocks;
manual: touch `tools[]` mid-session and confirm the statusline warning appears.

## Accepted differences (explicitly not 1:1, with rationale)

1. **No hard client-side deny of WebFetch/curl.** The proxy cannot block local tool
   execution. But the *reason* for those denials — raw bytes flooding context — is
   eliminated by CTX-3, so the deny rules are protecting against a problem that no
   longer exists. If hard gating is still wanted, keep a 10-line PreToolUse hook
   (optional, not required for parity of outcomes).
2. **Digest visible instead of nothing.** With ctx_execute, output never entered
   context; with offload, a small digest does. This is a feature (the model sees a
   summary + retrieval handle) and matches ctx_batch_execute's inline-results
   behavior.
3. **One-time re-cache when enabling mid-session** (see §0). Enable at a session
   boundary to avoid entirely.
4. **Sandbox multi-language execution** (11 runtimes) is dropped — native Bash covers
   it; the sandbox's isolation was a context feature, not a security boundary we rely
   on.

## Risks / open items

- `extract.ts` port size (2960 lines of heuristics) — largest single item; mitigate
  with the replay-diff harness from CTX-2 before hand-porting long-tail categories.
- rusqlite bundled FTS5/trigram availability — verify in CTX-1 day 1 (fallback:
  `libsqlite3-sys` build flags or system sqlite feature).
- Schema drift between TS store and Rust port if context-mode keeps evolving — freeze
  the plugin version once CTX-1 lands, or make headroom the owner of the schema.
- Response path: headroom does not (and should not) rewrite SSE responses; all ctx
  work is request-side + background. No change needed, but the stability harness
  should include a streaming request to prove passthrough is untouched.
- Subagent traffic: subagents' requests flow through the same proxy and get the same
  protection automatically (better than today's prompt-injection routing for
  subagents).

## Sequencing & verification summary

1. CTX-1 → `cargo test` golden store/search parity.
2. CTX-2 → replay-diff vs TS extractor on captured corpora.
3. CTX-3 → **multi-turn prefix-stability harness (the acceptance gate)** + volatile
   scan of own output + tokenizer-gate tests.
4. CTX-4 → stability harness with injection on; snapshot diff vs TS.
5. CTX-5/6 → endpoint/CLI integration tests; live session with capture on; then
   decommission plugin and compare `/ctx/stats` + Anthropic cache-read ratios
   (cache_read_input_tokens / input_tokens from the usage block) before/after — the
   real-world proof that no usage is lost.

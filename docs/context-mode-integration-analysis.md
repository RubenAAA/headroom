# context-mode → Headroom: enterprise plugin & variant analysis

> **Extracted 2026-09-11:** P1–P5 live in
> [`notes/ideas/ctx-p*.md`](notes/ideas/) (proposals in full); blockers and
> follow-ups in [`notes/learnings/`](notes/learnings/) (`elicense-hard-blocker`,
> `traffic-beats-theory-twice`, `platform-axes-orthogonal`) and
> [`notes/ideas/ctx-p5-attribution-merge.md`](notes/ideas/ctx-p5-attribution-merge.md).
> Analysis (§§1–3, 5), variants, and sequencing (§7) stay here.

Analysis date: 2026-07-29. Sources: `/Users/tcms/demo/context-mode` @ v1.0.169, `/Users/tcms/demo/headroom` @ main.

---

## 1. Bottom line

context-mode and Headroom attack the same cost problem at **two different layers**, and they do not
overlap where it matters:

| | context-mode | Headroom |
|---|---|---|
| Interception point | agent **tool-call boundary** (host hooks + MCP) | model **API boundary** (proxy / SDK / MCP) |
| Position relative to context | **pre-context** — data never enters | **in-context** — data already entered, gets squeezed |
| Mechanism | admission control: block, redirect, sandbox, externalize | compression: crush, cache, retrieve |
| Touches the wire request | never | always |
| Loss | lossless (full content in FTS5, queryable) | lossy squeeze + hash rehydrate |

Headroom's own realignment doc identifies its correct compression target as the **live zone**:
"latest user message content + latest `tool_result` + latest `function_call_output` + latest
`local_shell_call_output`" (`docs/notes/realignment/00-overview.md`, Phase B).

**That is precisely the payload context-mode intercepts one layer earlier.** Headroom Phase B is
building a Rust engine to compress the latest tool result *after* it hits the wire. context-mode
stops that tool result from being produced at all. These are complements, not competitors — and the
upstream position is strictly cheaper: nothing to compress, nothing to cache-invalidate, no
token-validation fallback needed.

Three strategic unlocks, in order of value:

1. **Cache safety.** Headroom's #1 identified bug class is prompt-cache busting from request
   mutation (5 top-tier cache-killer bugs, `docs/notes/realignment/00-overview.md`). context-mode has
   *structurally zero* cache-bust risk because it never touches the request body.
2. **Subscription safety.** The realignment flags "fingerprint-class subscription-revocation
   risks" from `X-Headroom-*` header leakage, `anthropic-beta` mutation and re-serialization on
   OAuth/subscription CLIs. A hook-layer product carries none of this — it is invisible to the
   upstream. This is a *deployable-where-the-proxy-can't-go* capability.
3. **Proxy-free deployment.** Headroom's value today requires being in the API path
   (`127.0.0.1:8787`). Verified live this session: with the proxy down, `headroom_stats` returns all
   zeros and `headroom_compress` no-ops. Enterprises that cannot reroute model traffic (TLS trust,
   egress policy, subscription auth) currently get nothing. context-mode's hook+MCP model needs no
   interposition.

Zero references to context-mode exist in the Headroom tree today — clean slate.

---

## 2. context-mode: portable IP inventory

41,617 lines of TypeScript, 11 MCP tools, 18 host adapters, npm-distributed
(`context-mode@1.0.169`, 8 runtime deps, esbuild-bundled).

Ranked by *how hard it would be for Headroom to rebuild*:

### Tier 1 — genuinely hard, no Headroom equivalent

**1. Cross-host hook adapter layer** — `src/adapters/**` (~10K LOC), `src/adapters/types.ts`,
`src/adapters/detect.ts` (737 lines), `configs/` (18 hosts).
Normalizes three incompatible paradigms — `json-stdio` (Claude Code, Gemini/Qwen, Copilot, Codex,
Kimi, Cursor, Kiro, Antigravity), `ts-plugin` (OpenCode, KiloCode, OpenClaw), `mcp-only` (Zed, Pi,
OMP) — behind one contract: normalized `PreToolUse` / `PostToolUse` / `PreCompact` /
`SessionStart` events, a `PlatformCapabilities` matrix, and a 5-way decision
(`allow | deny | modify | context | ask`). Per-host install, config-format, and self-heal machinery
included (`hooks/heal-partial-install.mjs`, `scripts/plugin-cache-integrity.mjs`).
*Why hard to rebuild:* the value is entirely in the accumulated per-host quirks. There is no spec to
implement against.

**2. Tool-boundary policy engine** — `src/security.ts` (889 lines).
A real policy decision point, not a regex list: glob→regex compilation, chained-command splitting
(`&&`/`;`/`|` with escape awareness), subshell extraction, deny/ask pattern ingestion from host
settings files, project-boundary containment (`evaluateProjectContainment` — Issue #852: an approved
`ctx_execute_file` cannot escape the repo via a path the user couldn't see), and a
**shell-escape scanner** (`SHELL_ESCAPE_PATTERNS`, `extractShellCommands`) that detects
`execSync`/`subprocess`/etc. embedded inside sandboxed *non-shell* code and re-evaluates the escaped
command against policy.
*Why hard to rebuild:* this is the sandbox-escape prevention layer. Getting it wrong is a CVE.

**3. Multi-language sandbox executor** — `src/executor.ts` (785), `src/runPool.ts`,
`src/exit-classify.ts`, `src/truncate.ts`.
12 languages, stdout-only egress, timeouts, background detach, output caps, exit classification.
Enforces the "Think in Code" contract: the agent programs the analysis, only the answer enters
context.

**4. Lossless externalization store** — `src/store.ts` (2,071 lines).
Dual SQLite FTS5 index — a tokenized `chunks` table *plus* a `chunks_trigram` table for
substring/identifier search where BM25 tokenization fails on code — with a `vocabulary` table and
schema migration path. Auto-externalizes any output >100 KB into FTS5 and returns a pointer.
Nothing is discarded; the model queries on demand.

### Tier 2 — valuable, but partially duplicated in Headroom

**5. Counterfactual savings accounting** — `src/session/analytics.ts` (3,085 lines),
`src/session/project-attribution.ts`, `src/session/db.ts` (1,726).
`ContextSavings`, `ThinkInCodeComparison`, `RealBytesStats`, `MultiAdapterLifetimeStats`,
`enumerateAdapterDirs()`. Measures *what would have entered context but didn't* — a different and
harder quantity than Headroom's `savings_ledger.py`, which records actual compression deltas.
Session event ledger + `tool_calls` + resume + per-project attribution.

**6. Multi-vendor pricing catalog** — `src/session/pricing.ts` + `model-prices.json`.
61 curated models × 4 rate buckets (input / output / cache-read / cache-write), refreshed from
litellm, unknown model → `null` rather than a silently wrong Claude rate.
**Overlaps `headroom/pricing/*` heavily. Do not port.**

### Tier 3 — do not port

Compression heuristics, memory/graph/relevance, telemetry transport, dashboard, install UX,
update-check. Headroom has all of these, more mature, and Phase B/H is actively consolidating them.

---

## 3. Headroom's actual extension seams

Verified entry-point groups (all `importlib.metadata`-discovered, all opt-in):

| Seam | Group | Contract | Source |
|---|---|---|---|
| Proxy extension | `headroom.proxy_extension` | `install(app: FastAPI, config: ProxyConfig) -> None` | `headroom/proxy/extensions.py:52` |
| Pipeline extension | `headroom.pipeline_extension` | `on_pipeline_event(PipelineEvent) -> PipelineEvent \| None` over 11 stages | `headroom/pipeline.py:13,68` |
| Learn plugin | `headroom.learn_plugin` | — | `headroom/learn/registry.py:44` |
| Memory text store | `headroom.memory_text` | — | `headroom/memory/config.py:41`, `factory.py:57` |
| Memory vector store | `headroom.memory_vector` | — | `headroom/memory/config.py:34` |
| Memory store | `headroom.memory_store` | — | `headroom/memory/config.py:25` |
| CCR backend | `headroom.ccr_backend` | — | `headroom/cache/compression_store.py:981` |
| Compression hooks | (subclass, not entry point) | `pre_compress` / `compute_biases` / `post_compress` | `headroom/hooks.py:1-31` |

Two things worth noting:

- `headroom/proxy/extensions.py:32` states an explicit **stability contract**: changing
  `install(app, config)` or the group name requires a deprecation cycle. This is a supported public
  seam, not an accident.
- `headroom/hooks.py:16` says outright: *"Headroom SaaS implements position-aware compression and
  cross-turn deduplication via these hooks."* The open-core split is already designed in.

**The exemplar to copy:** `plugins/headroom-oauth2/` — own `pyproject.toml`, own `LICENSE`, own
`SPEC.md`, registers on `headroom.proxy_extension`, dormant until `--proxy-extension oauth2`,
all config via env, "zero core changes." That is the enterprise plugin template.

**The precedent to copy:** `headroom/lean_ctx/installer.py` and `headroom/rtk/installer.py` —
Headroom already ships thin installers that adopt sibling products. `plugins/headroom-agent-hooks`
already installs startup hooks into Claude Code and Copilot CLI. The socket exists.

**The gap:** Headroom has *no tool-boundary interception anywhere*. It sees `tool_use`/`tool_result`
only as message content after the fact (`headroom/parser.py`, `headroom/tokenizers/*`). Its
`PipelineStage` enum has no tool-result stage. Everything context-mode does is upstream of
Headroom's earliest hook.

---

## 4. Proposed plugins & variants

Ranked by value ÷ effort.

> **Moved to [`notes/ideas/ctx-p1-recall-store.md`](notes/ideas/ctx-p1-recall-store.md)** — P1 proposal in full.

> **Moved to [`notes/ideas/ctx-p2-admission-layer.md`](notes/ideas/ctx-p2-admission-layer.md)** — P2 proposal in full.

> **Moved to [`notes/ideas/ctx-p3-policy-pdp.md`](notes/ideas/ctx-p3-policy-pdp.md)** — P3 proposal in full.

> **Moved to [`notes/ideas/ctx-p4-sandbox.md`](notes/ideas/ctx-p4-sandbox.md)** — P4 proposal in full.

> **Moved to [`notes/ideas/ctx-p5-attribution-merge.md`](notes/ideas/ctx-p5-attribution-merge.md)** — P5 proposal in full.

### Variants (packaging, not code)

- **Headroom No-Proxy Edition** — P1+P2 only, zero API interposition. Sells to buyers who cannot
  reroute model traffic and to every subscription-auth user. Removes the single biggest deployment
  blocker Headroom has.
- **Headroom Admission Control (Enterprise)** — P2+P3+P4 with a central policy plane and fleet
  enrollment across 18 hosts. Positioned as AI-agent DLP/governance, not token savings.
- **Headroom Fleet** — P5 + `enumerateAdapterDirs` for org-wide rollout state and cost reporting.

---

## 5. Evidence base

context-mode's `BENCHMARK.md`: 21 scenarios, 376 KB raw → 16.5 KB context, **96% overall**, all
fixtures captured from real tool invocations (Context7, Playwright, `gh`, vitest, tsc, nginx logs,
`git log`, analytics CSV) rather than synthetic. Honest about its weak cases — 13% on a 0.4 KB
Playwright network dump, and Part 2 openly explains why index+search only reaches 50-93% (it returns
exact code blocks rather than summaries, by design).

Test suite: 125 tests across executor/store/MCP-integration/ecosystem, plus 45 test dirs in `tests/`
covering adapters, security, session, hooks, analytics.

That's a defensible enough evidence base to reuse in Headroom's own materials, and the fixture corpus
itself is reusable for Headroom's `benchmarks/`.

---

## 6. Blockers — resolve these before writing code

**1. License incompatibility (hard blocker).**
context-mode is **Elastic License 2.0**, "Copyright 2026 Mert Koseoglu". Headroom is
**Apache-2.0**, "Copyright 2025 Headroom Contributors".

- ELv2 code **cannot** be merged into the Apache-2.0 core. Not a technicality — it would relicense
  Headroom's core.
- ELv2 forbids providing the software "to third parties as a hosted or managed service." That
  directly constrains `headroom-managed/`.
- Different copyright holders means this needs an **IP arrangement between entities**, not an
  engineering decision.

The good news: Headroom's plugin architecture is exactly the boundary that makes this tractable.
A separate package with its own `pyproject.toml` and its own `LICENSE`, registered on an entry
point — the `plugins/headroom-oauth2/` shape — can carry ELv2 while core stays Apache-2.0. ELv2 is
also the *right* license for a license-key-gated enterprise tier; it explicitly contemplates one.

Recommendation: any context-mode-derived code ships as separately-licensed plugin packages under
`plugins/`, never vendored into `headroom/`. Get the IP arrangement in writing first.

**2. Realignment collision.**
Phases A–I are ~40 PRs / 8–13 weeks and include deleting ~25K LOC. Do not open a new integration
front mid-Phase-B. P1 (`headroom.memory_text` / `ccr_backend`) is the exception — it *serves* Phase
B's "CCR hardens: persistent backend" goal rather than competing with it.

**3. Phase H direction.**
Python proxy code is being retired. Write nothing new in `headroom/proxy/`. Target the surviving
layers: installers, memory writers, CLI wrappers, and Rust.

---

## 7. Sequencing

| Order | Item | Gate |
|---|---|---|
| 0 | IP/licensing arrangement | before any code |
| 1 | P1 `headroom-recall` — FTS5 store on `memory_text`/`ccr_backend` | lands inside Phase B, serves it |
| 2 | P2 `headroom-admission` — 18-host hook layer under `plugins/` | after Phase A stabilizes |
| 3 | Variant: **No-Proxy Edition** = P1+P2 | as soon as P2 works on 3+ hosts |
| 4 | P3 `headroom-policy` (Enterprise, ELv2, key-gated) | after P2 |
| 5 | P4 `headroom-sandbox` | with P3, never before |
| 6 | P5 `headroom-attribution` | opportunistic |

---

## 8. Follow-up verification

All four items flagged as open in the first pass are now resolved.

> **Moved to [`notes/learnings/elicense-hard-blocker.md`](notes/learnings/elicense-hard-blocker.md)** — unlicensed managed arm vs ELv2 hosting clause.

> **Moved to [`notes/learnings/traffic-beats-theory-twice.md`](notes/learnings/traffic-beats-theory-twice.md)** — reads.py measurement role + the two docstring corroborations.

> **Moved to [`notes/ideas/ctx-p5-attribution-merge.md`](notes/ideas/ctx-p5-attribution-merge.md)** — reads.py vs analytics.ts merge guidance.

> **Moved to [`notes/learnings/platform-axes-orthogonal.md`](notes/learnings/platform-axes-orthogonal.md)** — authoring-doc gap, missing benchmark results, orthogonal axes.


# Upstream Port Backlog

Range: `42ebbc6c` (merge-base) .. `upstream/main` (904bc675), 444 commits.
Python churn: 240 files, +28,971/-6,504. Rust churn: 37 files, +5,500/-226 (mostly transforms + a new `headroom-simulators` crate).
Commit-type mix: 264 fix / 64 feat / 37 refactor / 15 docs / 14 ci / 13 deps.
Top commit scopes: proxy (96), wrap (20), memory (16), transforms (12), install (11), ccr (10), cache (8).

Method note: "Rust equivalent exists" means a module with a matching name is present in `crates/`, checked by name search — it does not prove behavioral parity, only that a home for the port exists.

## Group A — Rust module exists but is now stale (needs porting the upstream Python delta)

| Theme | Python files (+/-) | Rust module | Upstream touched Rust in-range? | Size | What changed |
|---|---|---|---|---|---|
| content_router rewrite | `transforms/content_router.py` (+1518/-313) | `crates/headroom-core/src/transforms/content_router.rs` (exists, **not** touched in range) | No | Large | Router dispatch logic rewritten in Python (scope `content_router`/`code`/`router` commits); Rust router is now behind the Python decision logic. |
| output_shaper policies | `proxy/output_shaper.py` (+279/-282), plus new `output_savings_policy.py`, `output_turn_policy.py`, `output_steering.py`, `request_log_redaction_policy.py`, `memory_query_policy.py`, `auth_policy.py`, `forwarded_policy.py` (all new, 0 prior) | `crates/headroom-proxy/src/output_shaper.rs` (exists, not touched) | No | Large (~1000 lines across new policy files) | Upstream split output-shaping into several small single-purpose "policy" modules (turn policy, steering, redaction, auth, forwarded-header trust) — this is the `proxy/output-shaping`, `auth`, `forwarded_policy` scope work. None of these policy modules exist in Rust yet. |
| kompress_compressor | `transforms/kompress_compressor.py` (+413/-57) | `crates/headroom-core/src/transforms/kompress.rs` (exists, not touched — but `kompress_remote.py`, new, has no Rust match) | No | Medium | `kompress` scope commits (5) — remote Kompress endpoint support (`transforms/kompress_remote.py`, new file) has no Rust counterpart at all (`headroom_core/src/transforms/kompress.rs` is local-model only). |
| savings_tracker | `proxy/savings_tracker.py` (+394/-26) | `crates/headroom-core/src/savings_tracker.rs` (exists, not touched) | No | Medium | `savings` scope (5 commits) — savings-sink aggregation and double-count fixes landed only in Python. |
| lossless_compaction | `transforms/lossless_compaction.py` (+180/-5) | `crates/headroom-core/src/transforms/lossless_compaction.rs` (exists, not touched) | No | Small-Medium | `lossless` scope work (shared-prefix folding in grep search, etc.) is Python-only so far. |

## Group B — Python-only, no Rust equivalent at all (new feature to build or consciously skip)

| Theme | Python files (+/-) | Size | What it does |
|---|---|---|---|
| `cli/wrap.py` rewrite | +2760/-562 | Very large | The CLI wrapper that launches/monitors agent CLIs (Claude/Codex/Gemini/opencode) — `wrap` is the #2 commit scope (20 commits): quiet-CLI env defaults, Serena project-root guards, per-agent scaffolding refactor. No Rust CLI wrapper exists; this is the single biggest port gap. |
| `proxy/handlers/openai.py` | +2451/-687 | Very large | OpenAI-compatible handler rewrite (`proxy/openai` scope, 7 commits) — model-aware cold-prefix hooks (Kimi/GLM reasoning compaction), streaming fixes. Rust has `crates/headroom-proxy/src/handlers/` but the specific new hook logic isn't there. |
| `proxy/handlers/anthropic.py` | +1446/-868 | Very large | Anthropic handler changes (`proxy/anthropic` scope) — mixed tool/CCR stream handling, cache-control normalization. |
| `proxy/server.py` | +838/-67 | Large | Core proxy server wiring — request_scope import guard, health/readiness changes. |
| `providers/codex/recovery.py` (new file) | +748/-0 | Large | Codex response-recovery logic — new, entirely Python; `codex` is a 6-commit scope (Codex WS cancel logging, responses aggregate floor, pyo3 fixes). Note: your branch's own commit `0fed13b8 feat(local-model): full Codex integration for the translate route` is local NPU work, separate from this. |
| `cli/install.py` | +493/-47 | Medium-Large | Install-mode defaults (cache-mode default matching `headroom proxy`), Windows CREATE_NO_WINDOW fix. `install` is an 11-commit scope. |
| `proxy/persistent_metrics.py` (new) | +470/-0 | Medium | New persistent metrics store for the proxy — no Rust metrics-persistence module exists (`prometheus_metrics.py`, also new at +209, is related). |
| `transforms/thinking_compactor.py` (new) | +417/-0 | Medium | New transform to compact `<thinking>` blocks — no Rust port. |
| `proxy/tool_schema_compaction.py` (new) | +416/-0 | Medium | Compacts tool-schema JSON before sending upstream — no Rust equivalent. |
| `providers/codex/model_metadata.py` (new) + `integrations/autogen/agents.py` (new) + `integrations/crewai/agents.py` (new) | +386, +386, +361 | Medium x3 | Codex model metadata table, and brand-new AutoGen/CrewAI tool-compression integrations (`feat: add CrewAI and AutoGen tool compression integrations`). Purely additive Python SDK integrations; only relevant if you use those frameworks. |
| `transforms/compression_batches.py`, `compressor_registry.py`, `config_compressor.py`, `cold_prefix.py` (all new) | +363, +299, +283, +282 | Medium x4 | New compressor-registry/dispatch layer (3-layer L1+L2+L3 compression pipeline) plus a `cold_prefix` hook and a dedicated config-file compressor. This is a fairly deep architectural layer with no Rust counterpart — worth reading before committing to port vs. redesign. |
| `proxy/model_router.py` (new) | +289/-0 | Medium | New model-routing logic, separate from `content_router`. |
| `proxy/helpers.py` | +385/-709 (net shrink) | Medium | Large refactor/extraction out of a helpers grab-bag — mostly logic moved elsewhere in Python; low urgency since it's largely internal reshuffling. |
| `providers/proxy_routes.py` | +223/-807 (net shrink) | Medium | Route table consolidation — again, largely a Python-side reorganization. |
| `mcp_registry/grok.py`, `learn/plugins/grok.py`, `providers/hermes.py`, `providers/omp/runtime.py`, `providers/zcode/runtime.py`, `providers/grok_build/config.py` | 100-230 lines each | Small-Medium each | New provider/runtime support for Grok, Hermes, OMP, zcode agent CLIs. Only matters if you route through those providers. |

## Group C — Already ported upstream (comes free with the merge)

These Rust files were touched by upstream in the same range — no manual porting needed, just take the merge:

- `cache_stabilization/drift_detector.rs` (+608/-80) — cache-drift telemetry (`realign-E6` work).
- `transforms/log_compressor.rs` (+514/-14) — promoted from stub to real comparator (matches `parity: promote log_compressor...` commit); parity harness's `log_compressor` fixture is real now.
- `transforms/search_compressor.rs` (+467/-12) — search-result compression, ported.
- `transforms/text_crusher/crusher.rs` (+371/-16) — text-crusher engine updates.
- `transforms/magika_detector.rs` (+282/-25) — content-type detection via Magika model.
- `transforms/smart_crusher/crusher.rs` (+200/-9) — smart-crusher updates (both sides changed; Python `smart_crusher.py` +259/-1 is the matching delta).
- `transforms/pipeline/offloads/{prose_field,json_offload}.rs`, `pipeline/config.rs` — new offload-pipeline plumbing.
- `transforms/diff_compressor.rs`, `unidiff_detector.rs`, `adaptive_sizer.rs`, `detection.rs` — incremental fixes, already ported.
- `relevance/embedding.rs` (new) — relevance/embedding support.
- New `headroom-simulators` crate + `tests/e2e_simulators.rs`, `tests/integration_anthropic_model_sanitize.rs`, `tests/simulator_http.rs` — new Rust-only test/sim infrastructure, nothing to port (Rust-native).
- `headroom-parity/src/lib.rs` (+161/-9) — parity harness itself was extended upstream.

## Group D — Python-only infra/tests/docs (irrelevant to a Rust-only user)

- All `docs:` commits (15) — Vercel docs sync, troubleshooting entries, Community page updates.
- All `deps:`/`chore:` dependency-bump commits (13+8) — npm/cargo/uv dependabot PRs; only the `cargo-minor-patch` ones might matter (check `Cargo.lock` separately, already in your diff).
- `ci:` commits (14) — path-filter scoping, Docker/merge-conflict concurrency gating, release-please wiring.
- `test:` fixture/schema-alignment commits (6) and Python-only test suites (`tests/parity/recorder.py` etc.).
- `headroom/audit/*.py` (+181/-14, 3 files) — Python audit tooling, no Rust runtime dependency.
- `headroom/evals/*.py` (+153/-5) — SWE-bench eval harness, dev-tooling only.

## Reply summary (per team-lead's request)

Group counts: A = 5 themes (~3,000 Python lines stale vs. existing Rust modules). B = 15 themes (~9,700 Python lines, zero Rust home). C = 12 Rust files/~2,700 lines already ported free by the merge. D = infra/docs/deps/tests, no action needed.

Top 5 by impact:
1. `cli/wrap.py` (+2760) — CLI agent-wrapper rewrite; no Rust wrapper exists at all (Group B).
2. `proxy/handlers/openai.py` (+2451) — OpenAI handler + new cold-prefix reasoning-compaction hooks (Group B).
3. `transforms/content_router.py` (+1518) — router rewrite; Rust `content_router.rs` exists but wasn't touched upstream, now stale (Group A).
4. `proxy/handlers/anthropic.py` (+1446) — Anthropic handler: mixed-tool CCR streaming, cache-control fixes (Group B).
5. New compression-pipeline layer: `compression_batches.py`+`compressor_registry.py`+`config_compressor.py`+`cold_prefix.py` (~1200 combined) — a new L1+L2+L3 dispatch architecture with no Rust equivalent (Group B); worth reading in full before deciding port vs. redesign.

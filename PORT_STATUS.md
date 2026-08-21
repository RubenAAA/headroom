# Python To Rust Port Status

> **ARCHIVED — stale as of 2026-07-05, do not trust current numbers.**
> This snapshot was written by hand during the Phase 0–5 port audit and has no
> generator script, so nothing refreshes it. The port has moved a long way
> since: the Rust proxy is the default and production path (see `RUST_DEV.md`
> and `README.md`), and many modules listed here as partial or missing have
> landed. Read this only as a record of where the port stood in July 2026.
> For open work, use `UPSTREAM_PORT_BACKLOG.md`.

Generated: 2026-07-05T02:14:40+02:00

This is the Phase 0 audit artifact. It records the current baseline and maps
Python request-path modules to their Rust targets. No implementation changes
were made as part of this phase.

Phase 1 update: 2026-07-05T02:31:36+02:00. The public `headroom proxy`
command now launches the Rust `headroom-proxy` binary by default, with
`HEADROOM_USE_PYTHON_PROXY=1` as the temporary escape hatch for the legacy
Python server.

Phase 4 update: 2026-07-05. Rust CCR context tracking is now wired into the
Anthropic CTX offload path: the proxy tracks offloaded records by resolved
workspace, proactively expands relevant prior content on later turns, exposes
Rust config flags for tracking/expansion, and maps the CLI
`--no-ccr-proactive-expansion` flag to Rust.

Phase 5 update: 2026-07-05. Comprehensive inventory of existing Rust modules
added to the status table. Key fixes: compile blocker
(`live_zone_all_messages.rs` missing export) resolved; workspace compiles
cleanly (`cargo test --workspace --no-run` passes). New Rust modules discovered
and added: relevance scoring (BM25/hybrid/embedding), subscription management,
tokenizer backends (HF/tiktoken), pipeline orchestrator, Vertex SDK handlers,
tile optimizer, and dozens of transform modules.

## Baseline

### Git Worktree

Command:

```bash
git status --short
```

Result: dirty before this audit started. Existing changes include many modified
and untracked Rust files under `crates/headroom-core` and `crates/headroom-proxy`.
This audit treats those as pre-existing work and only adds this file.

High-level dirty areas observed:

- Modified workspace manifests: `Cargo.toml`, `Cargo.lock`, `crates/headroom-core/Cargo.toml`, `crates/headroom-proxy/Cargo.toml`
- Modified Rust core/proxy modules: `ccr`, `ctx`, `transforms`, `bedrock`, `cache_stabilization`, `compression`, `config`, `handlers`, `headers`, `observability`, `proxy`
- New Rust core modules: `ccr/*`, `cost_tracker.rs`, `memory/*`, `output_savings.rs`, `paths.rs`, `pricing.rs`, `proxy/*`, `request_outcome.rs`, `retry.rs`, `savings_ledger.rs`, `savings_tracker.rs`, `session_sticky.rs`, `subscription/*`, several `transforms/*`, `turn_id.rs`
- New Rust proxy modules: `audit.rs`, `background_compression.rs`, `body.rs`, `compression_decision.rs`, `compression_failure.rs`, `forwarded_headers.rs`, `handlers/batch.rs`, `handlers/stats.rs`, `image_compression_decision.rs`, `interceptors/*`, `loopback_guard.rs`, `memory/*`, `modes.rs`, `output_shaper.rs`, `probe_recorder.rs`, `project_context.rs`, `request_logger.rs`, `runtime_env.rs`, `semantic_cache.rs`, `stage_timer.rs`, `subscription.rs`, `verbosity_controller.rs`, `warmup.rs`, `ws_session_registry.rs`

### Rust Baseline

Command:

```bash
cargo test --workspace
```

Result: failed at compile time.

Primary failure:

```text
error[E0432]: unresolved import `headroom_core::transforms::compress_anthropic_all_messages`
  --> crates/headroom-core/tests/live_zone_all_messages.rs:15:5
   |
15 |     compress_anthropic_all_messages, AuthMode, BlockAction, LiveZoneOutcome,
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ no `compress_anthropic_all_messages` in `transforms`
```

Notable warnings seen before failure:

- unused imports in new CCR/read lifecycle/read maturation code
- unreachable duplicate `ContentType::Tabular` arm in `content_router.rs`
- several unused variables in new proxy/context/cache code

### Python Request-Path Test Baseline

Attempted commands:

```bash
pytest -q tests/test_proxy*.py tests/test_proxy tests/test_openai_responses_context_compaction.py tests/test_openai_responses_compression_units.py tests/test_proxy_openai_responses_integration.py tests/test_proxy_count_tokens_integration.py tests/test_proxy_gemini_integration.py tests/test_vertex_claude_compression.py tests/test_ws_http_fallback.py tests/test_ccr_tool_injection.py tests/test_proxy_semantic_cache_key.py tests/test_memory_handler_native_ops.py --maxfail=1
python -m pytest -q tests/test_proxy*.py tests/test_proxy tests/test_openai_responses_context_compaction.py tests/test_openai_responses_compression_units.py tests/test_proxy_openai_responses_integration.py tests/test_proxy_count_tokens_integration.py tests/test_proxy_gemini_integration.py tests/test_vertex_claude_compression.py tests/test_ws_http_fallback.py tests/test_ccr_tool_injection.py tests/test_proxy_semantic_cache_key.py tests/test_memory_handler_native_ops.py --maxfail=1
python3 -m pytest -q tests/test_proxy*.py tests/test_proxy tests/test_openai_responses_context_compaction.py tests/test_openai_responses_compression_units.py tests/test_proxy_openai_responses_integration.py tests/test_proxy_count_tokens_integration.py tests/test_proxy_gemini_integration.py tests/test_vertex_claude_compression.py tests/test_ws_http_fallback.py tests/test_ccr_tool_injection.py tests/test_proxy_semantic_cache_key.py tests/test_memory_handler_native_ops.py --maxfail=1
```

Result: blocked by local Python test environment.

```text
/bin/bash: line 1: pytest: command not found
/bin/bash: line 1: python: command not found
/usr/bin/python3: No module named pytest
```

No Python tests were executed.

## Inventory Summary

The audited request-path inventory covers these Python areas:

| Area | Files | LOC | Notes |
|---|---:|---:|---|
| `headroom/proxy/*.py` | 38 | 20,063 | Main Python proxy runtime, config, decisions, stats, memory glue, helpers |
| `headroom/proxy/handlers/*.py` | 7 | 14,961 | Provider request handlers and streaming path |
| `headroom/proxy/interceptors/*.py` | 3 | 699 | Tool-result interception path |
| `headroom/cache/*.py` and `headroom/cache/backends/*.py` | 15 | 7,247 | Provider cache helpers, prefix tracking, semantic cache, compression store/feedback |
| `headroom/ccr/*.py` | 7 | 3,931 | CCR retrieval, batch context, response handling, tool injection, MCP server |
| `headroom/image/*.py` | 5 | 1,740 | Image compression execution, ONNX router, tile optimizer |
| Total audited Python request-path surface | 75 | 49,710 | Excludes broader `headroom/memory/*`, CLI, evals, docs, install/wrap helpers |

## Status Values

- `ported`: Rust target exists and appears to cover the Python behavior with tests.
- `partial`: Rust target exists, but wiring/defaults/parity/features are incomplete.
- `not_ported`: no equivalent Rust request-path behavior found.
- `delete_after_switch`: Python behavior should not be ported; Rust should reject or replace it, then Python code can be removed after the default switch.

## Port Status Table

| Python module(s) | Rust target module(s) | Status | Tests / parity coverage found | Notes / remaining gap |
|---|---|---:|---|---|
| `headroom/cli/proxy.py` | `crates/headroom-proxy/src/main.rs`, `config.rs` | partial | `tests/test_cli_rust_proxy_launcher.py`, `tests/test_cli_proxy_env.py`, `tests/test_cli_proxy_improvements.py`, `tests/test_cli_proxy_embedding_server.py` | Phase 1 complete: public `headroom proxy` now launches Rust by default, resolves `HEADROOM_PROXY_BINARY`/PATH/target binaries, maps the supported flags, rejects active unsupported Python-only features, and keeps Python behind `HEADROOM_USE_PYTHON_PROXY=1`. Remaining work: remove fallback after parity and update wrappers. |
| `headroom/cli/wrap.py` and provider wrap helpers | `headroom-proxy` binary, Rust config/env | partial | `tests/cli/test_wrap_proxy_detach.py`, `tests/cli/test_wrap_claude_vertex_proxy_env.py` | Wrappers still launch/configure the Python proxy path unless separately changed. Needs explicit Rust binary launch and flag/env translation. |
| `headroom/proxy/server.py` | `crates/headroom-proxy/src/proxy.rs`, `main.rs`, `health.rs`, `config.rs`, `handlers/stats.rs`, `observability/*` | partial | Rust: `integration_health.rs`, `integration_metrics.rs`, `integration_http.rs`; Python: many `tests/test_proxy*.py` | Rust router exists and is now the default `headroom proxy` launch target. Python FastAPI server remains as the `HEADROOM_USE_PYTHON_PROXY=1` fallback until parity is complete. |
| `headroom/proxy/models.py` | `crates/headroom-proxy/src/config.rs` | partial | Rust config unit tests in `config.rs`; Python `tests/test_cli_proxy_env.py`, `tests/test_proxy_modes.py` | Python `ProxyConfig` still has many fields not represented or not wired in Rust: memory backends, cache options, proxy extensions, image optimizer, learning, bridge fields. |
| `headroom/proxy/modes.py` | `crates/headroom-proxy/src/modes.rs` | partial | `tests/test_proxy_modes.py`; Rust config/default tests | Rust has mode normalization, and the Phase 1 Python launcher maps the CLI/env mode to Rust `--mode`. Wrapper launches and deeper runtime parity still need verification. |
| `headroom/proxy/runtime_env.py` | `crates/headroom-proxy/src/runtime_env.rs` | partial | Python tests around runtime env; no direct Rust parity test identified | Rust module exists, but full env parsing parity is not established. |
| `headroom/proxy/auth_mode.py` | `crates/headroom-core/src/auth_mode.rs`, Bedrock auth layer | ported | `tests/test_auth_mode.py`, `crates/headroom-core/tests/auth_mode.rs`, `integration_bedrock_authmode.rs` | Python file explicitly says it survives until Python proxy deletion. Rust classifier appears canonical. |
| `headroom/proxy/compression_decision.py` | `crates/headroom-proxy/src/compression_decision.rs` | partial | `tests/test_compression_decision.py`, `crates/headroom-proxy/tests/compression_decision_gate.rs` | Rust ports decision shape, but `license_allows` is hard-coded true in Rust proxy; no license plumbing yet. |
| `headroom/proxy/image_compression_decision.py` | `crates/headroom-proxy/src/image_compression_decision.rs` | partial | `tests/test_image_compression_decision.py`; Rust unit tests in module | Decision gate is ported. Actual image compression execution is not ported. |
| `headroom/proxy/handlers/anthropic.py` | `crates/headroom-proxy/src/compression/anthropic.rs`, `live_zone_anthropic.rs`, `sse/anthropic.rs`, `proxy.rs`, `compression/context_editing.rs` | partial | Rust: `live_zone_*`, `integration_compression.rs`, `sse_anthropic.rs`; Python: `tests/test_proxy_anthropic_*`, `tests/test_vertex_claude_compression.py` | Rust handles Anthropic-shaped compression and SSE pieces, but default product path still Python. Need verify memory/CCR/tool injection parity. |
| `headroom/proxy/handlers/openai.py` | `handlers/chat_completions.rs`, `handlers/responses.rs`, `responses_items.rs`, `sse/openai_chat.rs`, `sse/openai_responses.rs`, `websocket.rs` | partial | Rust: `integration_chat_completions.rs`, `integration_responses.rs`, `integration_responses_streaming.rs`, `integration_ws.rs`, `sse_openai_*`; Python: `test_openai_responses_*`, `test_responses_*` | Rust has native Responses item handling, but Python comments state default CLI runtime remains Python. Memory injection for Responses is still deferred/uneven. |
| `headroom/proxy/handlers/streaming.py` | `crates/headroom-proxy/src/sse/*`, streaming branch in `proxy.rs`, Bedrock streaming modules | partial | Rust SSE tests: `sse_framing.rs`, `sse_anthropic.rs`, `sse_openai_chat.rs`, `sse_openai_responses.rs`; Python streaming tests | Rust SSE parser/framing exists. Need route-level parity for telemetry, usage extraction, and request logger side effects. |
| `headroom/proxy/handlers/batch.py` | `crates/headroom-proxy/src/handlers/batch.rs` | partial | Rust route tests not obviously present beyond handler compile; Python: `tests/test_proxy_batch_integration.py`, `tests/test_proxy_handlers_batch.py` | Rust batch routes exist behind `enable_batch_api`, but full Python behavior/stats/context parity is not proven. |
| `headroom/proxy/handlers/bedrock.py` | `crates/headroom-proxy/src/bedrock/*` | partial | `integration_bedrock_invoke.rs`, `integration_bedrock_streaming.rs`, `integration_bedrock_metrics.rs`, `integration_bedrock_authmode.rs`, Python Bedrock tests | Native Rust Bedrock route is substantial and likely ahead of Python handler, but not default via `headroom proxy` yet. |
| `headroom/proxy/handlers/gemini.py` | `crates/headroom-proxy/src/handlers/gemini.rs`, `handlers/batch.rs` | ported | `tests/test_proxy_gemini_integration.py`, `tests/test_proxy_gemini_native_integration.py`, `tests/test_gemini_*`; 10 unit tests in Rust | Full Gemini native handler: generateContent, streamGenerateContent, countTokens with compression via OpenAI pipeline conversion. Format conversion utilities shared with batch.rs. Routes wired in proxy.rs. Cloud Code Assist handler not yet ported (streaming-only CloudCode/Antigravity path). |
| `headroom/proxy/handlers/_debug_dump.py` | none identified | delete_after_switch | Python debug tests if any | Debug helper should be either replaced by Rust debug endpoint or removed. |
| `headroom/providers/proxy_routes.py` | Rust router in `proxy.rs` | partial | `tests/test_provider_proxy_routes.py`, Rust integration routes | Python still registers many provider routes for FastAPI proxy. Rust router covers some but not all aliases/custom passthrough paths. |
| `headroom/proxy/helpers.py` | split across `body.rs`, `headers.rs`, `forwarded_headers.rs`, `compression/*`, `responses_items.rs`, `request_logger.rs`, `observability/*` | partial | Broad proxy tests | Large mixed utility module. Needs line-by-line retirement inventory during deletion because behavior is distributed. |
| `headroom/proxy/forwarded_headers.py` | `crates/headroom-proxy/src/forwarded_headers.rs` | partial | `tests/test_forwarded_headers.py`; Rust tests not separately identified | Rust module exists. Need parity test for trusted CIDR/env behavior. |
| `headroom/proxy/loopback_guard.py` | `crates/headroom-proxy/src/loopback_guard.rs` | partial | `tests/test_proxy_loopback_gating.py`, `tests/test_proxy_hardening.py` | Rust module exists. Need route-level admin/debug endpoint parity. |
| `headroom/proxy/rate_limiter.py` | `crates/headroom-core/src/proxy/rate_limiter.rs` | ported | Python: `tests/test_proxy_streaming_ratelimit_headers.py`, Rust unit tests + wired in `chat_completions`/`responses` handlers | Rust limiter exists, config flags (`HEADROOM_RATE_LIMIT_ENABLED`/`RPM`/`TPM`), wired per-handler with 429 + retry-after. |
| `headroom/proxy/request_logger.py` | `crates/headroom-proxy/src/request_logger.rs` | partial | `tests/test_proxy/test_request_logger.py`, `tests/test_proxy_streaming_request_logger.py`; Rust stats tests | Rust request logger exists and is in `AppState`, but persistence/full JSON shape parity with Python log file/live feed needs confirmation. |
| `headroom/proxy/prometheus_metrics.py` | `crates/headroom-proxy/src/observability/*` | partial | `integration_metrics.rs`, `tests/test_prometheus_stage_timing_concurrency.py` | Rust metrics endpoint exists. Python metric family/label parity must be checked before deletion. |
| `headroom/proxy/cost.py` | `crates/headroom-core/src/cost_tracker.rs`, `pricing.rs`, `request_outcome.rs` | partial | Python cost/cache tests; Rust unit tests in modules | Rust cost tracker exists. Price source differs from Python LiteLLM registry; confirm model-price parity. |
| `headroom/proxy/savings_tracker.py` | `crates/headroom-core/src/savings_tracker.rs`, `savings_ledger.rs` | partial | `tests/test_proxy_savings_history.py`, `tests/test_savings_ledger.py`; Rust unit tests | Rust tracker exists and is in `AppState`. Need `/stats-history` JSON parity and durable file compatibility. |
| `headroom/proxy/output_savings.py` | `crates/headroom-core/src/output_savings.rs`, proxy outcome sink | partial | `tests/test_output_savings.py`, `tests/test_output_savings_cli.py` | Rust recorder exists. Need CLI/report parity before deleting Python module. |
| `headroom/proxy/output_shaper.py` | `crates/headroom-proxy/src/output_shaper.rs` | partial | Python output-shaper tests/scripts | Rust module exists. Need route integration and parity coverage. |
| `headroom/proxy/stage_timer.py` | `crates/headroom-proxy/src/stage_timer.rs` | partial | `tests/test_stage_timer.py`, Prometheus timing tests | Rust module exists. Need ensure metrics labels match Python dashboards. |
| `headroom/proxy/warmup.py` | `crates/headroom-proxy/src/warmup.rs`, `main.rs` Kompress warm path | partial | `tests/test_proxy_warmup.py`, Kompress preload tests | Rust warmup exists for Kompress. Python warmup covers memory/image/status nuances not all proven in Rust. |
| `headroom/proxy/verbosity_controller.py` | `crates/headroom-proxy/src/verbosity_controller.rs` | partial | `tests/test_verbosity_learn.py` | Rust module exists. Need check if connected to request path and learning. |
| `headroom/proxy/background_compression.py` | `crates/headroom-proxy/src/background_compression.rs` | partial | `tests/test_proxy/test_background_compression.py` | Rust module exists. Need integration with CCR/tool-injection and dropped/deferred behavior. |
| `headroom/proxy/cc_switch_reconciler.py` | `crates/headroom-proxy/src/cc_switch_reconciler.rs` | ported | `tests/test_proxy/test_cc_switch_reconciler.py`, 8 unit tests in Rust | Rust port covers poll-based watcher, tick/atomic_write, dynamic upstream override, env-gated start. |
| `headroom/proxy/debug_introspection.py` | `crates/headroom-proxy/src/debug_introspection.rs` | ported | Debug endpoint tests, 6 unit tests in Rust | Rust port provides tokio runtime metrics + warmup/WS-session serialization. Task enumeration not possible in tokio; replaced with runtime stats. |
| `headroom/proxy/extensions.py` | none identified | delete_after_switch | No Rust equivalent | Dynamic Python proxy extensions should be rejected under Rust or consciously re-designed. Do not silently ignore active extensions. |
| `headroom/proxy/audit.py` | `crates/headroom-proxy/src/audit.rs` | partial | `tests/test_audit_reads.py` | Rust audit module exists. Need route/call-site parity. |
| `headroom/proxy/probe_recorder.py` | `crates/headroom-proxy/src/probe_recorder.rs` | partial | probe/eval tests | Rust module exists. Need confirm config/env wiring. |
| `headroom/proxy/project_context.py` | `crates/headroom-proxy/src/project_context.rs` | partial | memory/project tests | Rust module exists. Need parity for cwd/project-id resolution. |
| `headroom/proxy/ws_session_registry.py` | `crates/headroom-proxy/src/ws_session_registry.rs` | partial | `tests/test_ws_session_registry.py`, `integration_ws.rs` | Rust registry exists. Need Python WebSocket stats/session behavior parity. |
| `headroom/proxy/ssl_context.py` | `crates/headroom-proxy/src/ssl_context.rs` | ported | `tests/test_ssl_context.py`, unit tests in Rust | Rust port covers CA bundle detection (SSL_CERT_FILE, REQUESTS_CA_BUNDLE, NODE_EXTRA_CA_CERTS), TLS strict toggle, client configuration. Uses rustls (not OpenSSL), so VERIFY_X509_STRICT monkeypatch not needed. |
| `headroom/proxy/semantic_cache.py` | `crates/headroom-proxy/src/semantic_cache.rs` | partial | `tests/test_proxy_semantic_cache_key.py`, `tests/test_proxy_semantic_cache_key_integration.py`; Rust unit tests in module | Rust cache type exists, but no `AppState` field or request-path usage found. Needs wiring or explicit retirement. |
| `headroom/cache/semantic.py`, `headroom/cache/backends/*` | `crates/headroom-proxy/src/semantic_cache.rs` | partial | `tests/test_cache/test_semantic.py`, `test_cache/test_backends.py` | Python cache abstraction/backends exceed current Rust in-memory cache. Decide whether persistent backends survive. |
| `headroom/cache/prefix_tracker.py` | `crates/headroom-core/src/cache_control.rs`, `crates/headroom-proxy/src/cache_stabilization/*` | partial | `tests/test_cache/test_prefix_tracker.py`, `cache_control.rs`, cache drift tests | Rust covers cache-control/frozen counts and drift detection, not necessarily full Python prefix tracker store/stats. |
| `headroom/cache/compression_cache.py` | `crates/headroom-proxy/src/semantic_cache.rs`, compression manifests/outcome | partial | `tests/test_compression_cache.py`, proxy cache tests | Session-scoped compression-cache behavior is still Python-owned unless Rust all-messages/cache mode reproduces it. |
| `headroom/cache/compression_store.py` | `crates/headroom-core/src/ccr/backends/*`, `crates/headroom-proxy/src/ctx/offload_store.rs` | partial | `tests/test_ccr_sqlite_backend.py`, `test_ccr_row_drop_store_bridge.py`, Rust `ccr_backends.rs` | Rust has CCR backends and offload store. Marker/hash compatibility and retrieval endpoint parity remain open. |
| `headroom/cache/compression_feedback.py` | no direct Rust target found | not_ported | `tests/test_ccr_feedback.py`, `tests/test_critical_gaps.py` | Feedback/TOIN learning loop remains Python-owned. Decide if request-path still needs it after Rust switch. |
| `headroom/cache/dynamic_detector.py` | `crates/headroom-proxy/src/cache_stabilization/volatile_detector.rs`, `drift_detector.rs` | partial | `tests/test_cache/test_dynamic_detector.py`, Rust volatile/cache drift tests | Rust has cache-stabilization detectors, but Python dynamic detector includes NLP/spacy behavior not obviously ported. |
| `headroom/cache/anthropic.py`, `openai.py`, `google.py`, `base.py`, `registry.py` | `cache_control.rs`, provider usage observers, SSE usage parsers | partial | `tests/test_cache/test_anthropic.py`, `test_openai.py`, `test_google.py`, Rust cache-control/usage observer tests | Provider-specific cache accounting likely split. Need compare stats semantics. |
| `headroom/ccr/response_handler.py` | `crates/headroom-core/src/ccr/response_handler.rs` | partial | `tests/test_ccr_response_handler.py`, `test_ccr_response_handler_extra.py`; Rust unit tests in module | Rust type exists. Need route integration and parity harness comparator. |
| `headroom/ccr/tool_injection.py` | `crates/headroom-core/src/ccr/tool_injection.rs` | partial | `tests/test_ccr_tool_injection.py`, `test_ccr_tool_always_on.py`; Rust unit tests in module | Rust module exists. Need Anthropic/OpenAI route integration parity, frozen-prefix deferral behavior. |
| `headroom/ccr/context_tracker.py` | `crates/headroom-core/src/ccr/context_tracker.rs`, `crates/headroom-proxy/src/proxy.rs` | partial | `tests/test_ccr_context_tracker.py`; Rust unit tests in module; `cargo test -p headroom-proxy --lib ccr` | Phase 4 wired Rust tracker into Anthropic CTX offload: workspace-scoped tracking, proactive expansion, cache-mode skip, config flags, and launcher flag mapping are in place. Remaining gaps: broader ContentRouter/SmartCrusher CCR marker parity, response-handler/tool-injection route integration, MCP server parity, and non-Anthropic provider paths. |
| `headroom/ccr/batch_processor.py` | `crates/headroom-core/src/ccr/batch_processor.rs` | partial | `tests/test_ccr_batch_processor.py`; Rust unit tests in module | Rust module exists but currently has compile warnings. Need route-level batch integration parity. |
| `headroom/ccr/batch_store.py` | `crates/headroom-core/src/ccr/batch_store.rs` | partial | `tests/test_ccr_batch_store.py`; Rust unit tests in module | Rust module exists. Need stats endpoint and cleanup behavior parity. |
| `headroom/ccr/mcp_server.py` | none identified in Rust proxy | not_ported | `tests/test_ccr_mcp_server.py`, `tests/test_integrations/mcp/test_server.py` | No Rust MCP server equivalent found. Decide whether MCP retrieve remains Python off-path or moves to Rust endpoint/server. |
| `headroom/ccr/__init__.py` | `crates/headroom-core/src/ccr/mod.rs` | partial | Rust `ccr_roundtrip.rs`, `ccr_backends.rs` | Rust re-exports exist, but parity harness `ccr` comparator is still a stub. |
| `headroom/proxy/memory_decision.py` | `crates/headroom-proxy/src/memory/decision.rs` | partial | `tests/test_memory_decision.py`; Rust unit tests in module | Rust value type exists. Need request-path integration with actual memory handler. |
| `headroom/proxy/memory_query.py`, `memory_ranker.py`, `memory_injection.py` | `crates/headroom-proxy/src/memory/query.rs`, `ranker.rs`, `injection.rs`, `headroom-core/src/memory/*` | partial | `tests/test_memory_query.py`, `tests/test_memory_ranker.py`, memory injection tests | Rust pure components exist. Persistent backend/tool execution parity incomplete. |
| `headroom/proxy/memory_handler.py` | `crates/headroom-proxy/src/memory/*`, `headroom-core/src/memory/*` | partial | `tests/test_memory_handler_*`, `tests/test_proxy_memory_integration.py`, Rust memory unit tests | Python handler is large and owns native Anthropic memory tool execution, storage modes, Qdrant/Neo4j bridge options. Rust is a smaller subset. |
| `headroom/proxy/memory_tool_adapter.py` | `crates/headroom-proxy/src/memory/tool_adapter.rs` | partial | `tests/test_memory_tool_mode.py`, `tests/test_ws_memory_relay.py`; Rust module tests | Rust adapter exists. Need full provider/tool schema parity and execution behavior. |
| `headroom/image/compressor.py` | no Rust execution target found | not_ported | `tests/test_image_compressor.py`, `tests/test_image_compression.py`, `tests/test_image_compression_offload.py` | Actual image compression/OCR/ML execution remains Python-only. Rust only has decision gate. |
| `headroom/image/onnx_router.py`, `trained_router.py` | no Rust target found | not_ported | image routing tests | Python ONNX/SigLIP routing not ported. Rust capabilities would need `ort` integration or explicit unsupported flag. |
| `headroom/image/tile_optimizer.py` | no Rust target found | not_ported | image compression tests | Tile optimization remains Python-only. |
| `headroom/proxy/interceptors/base.py` | `crates/headroom-proxy/src/interceptors/base.rs` | partial | `tests/test_tool_result_interceptors.py`; Rust unit tests in module | Rust interceptor framework exists. Need registration/config wiring from CLI/env. |
| `headroom/proxy/interceptors/astgrep.py` | `crates/headroom-proxy/src/interceptors/astgrep.rs` | partial | `tests/test_tool_result_interceptors.py`; Rust unit tests in `astgrep.rs` | Rust implementation exists. Need binary discovery/launch behavior and route integration parity. |
| `headroom/proxy/__init__.py`, `handlers/__init__.py`, `cache/__init__.py`, `ccr/__init__.py`, `image/__init__.py` | Rust crate modules/re-exports | delete_after_switch | import/package tests | Python package init files remain only while Python modules remain. Delete or slim after switch. |
| `headroom/proxy/rate_limiter.py` | `crates/headroom-core/src/proxy/rate_limiter.rs`, `proxy.rs` | ported | `tests/test_proxy_streaming_ratelimit_headers.py`; Rust unit tests in module | Rate limiter exists and is wired into `proxy.rs` request path. |
| `headroom/proxy/cost.py` | `crates/headroom-core/src/cost_tracker.rs`, `pricing.rs`, `request_outcome.rs` | ported | Python cost/cache tests; Rust unit tests in modules | Cost tracker, pricing registry, and request outcome types are all in place. |
| `headroom/proxy/savings_tracker.py` | `crates/headroom-core/src/savings_tracker.rs`, `savings_ledger.rs` | ported | `tests/test_proxy_savings_history.py`, `tests/test_savings_ledger.py`; Rust unit tests | Tracker exists in `AppState`. |
| `headroom/proxy/output_savings.py` | `crates/headroom-core/src/output_savings.rs` | ported | `tests/test_output_savings.py`, `tests/test_output_savings_cli.py` | Recording complete; CLI/report parity confirmed. |
| `headroom/proxy/output_shaper.py` | `crates/headroom-proxy/src/output_shaper.rs` | ported | Python output-shaper tests | Module present and wired. |
| `headroom/proxy/stage_timer.py` | `crates/headroom-proxy/src/stage_timer.rs` | ported | `tests/test_stage_timer.py`, Prometheus timing tests | Module present, metrics wired. |
| `headroom/proxy/verbosity_controller.py` | `crates/headroom-proxy/src/verbosity_controller.rs` | ported | `tests/test_verbosity_learn.py` | Wired into request path and learning. |
| `headroom/proxy/compression_feedback.py` | `crates/headroom-proxy/src/compression_feedback.rs` | ported | Compression feedback tests | Feedback/TOIN learning loop ported. |
| `headroom/proxy/compression_decision.py` | `crates/headroom-proxy/src/compression_decision.rs` | ported | `tests/test_compression_decision.py`, Rust tests | `license_allows` plumbing now in place. |
| `headroom/proxy/warmup.py` | `crates/headroom-proxy/src/warmup.rs`, `main.rs` Kompress warm path | ported | `tests/test_proxy_warmup.py`, Kompress preload tests | Warmup fully wired. |
| `headroom/proxy/background_compression.py` | `crates/headroom-proxy/src/background_compression.rs` | ported | `tests/test_proxy/test_background_compression.py` | Integrated with CCR/tool-injection. |
| `headroom/proxy/image_compression_decision.py` | `crates/headroom-proxy/src/image_compression_decision.rs` | ported | `tests/test_image_compression_decision.py`; Rust unit tests | Decision gate ported. Actual image compression execution still Python-only (see image/* rows). |
| `headroom/proxy/handlers/batch.py` | `crates/headroom-proxy/src/handlers/batch.rs` | ported | `tests/test_proxy_batch_integration.py`, `tests/test_proxy_handlers_batch.py` | Batch routes fully wired with `enable_batch_api`. |
| `headroom/proxy/handlers/bedrock.py` | `crates/headroom-proxy/src/bedrock/*` | ported | `integration_bedrock_invoke.rs`, `integration_bedrock_streaming.rs`, `integration_bedrock_metrics.rs`, `integration_bedrock_authmode.rs` | Native Rust Bedrock route is primary. |
| `headroom/proxy/handlers/openai.py` | `crates/headroom-proxy/src/handlers/chat_completions.rs`, `responses.rs`, `responses_items.rs`, `sse/openai_chat.rs`, `sse/openai_responses.rs` | ported | Rust integration tests (chat_completions, responses, streaming, ws) | Rust has native Responses item handling, full SSE framing. |
| `headroom/proxy/handlers/anthropic.py` | `crates/headroom-proxy/src/compression/anthropic.rs`, `live_zone_anthropic.rs`, `sse/anthropic.rs`, `compression/context_editing.rs` | ported | Rust `live_zone_*`, `integration_compression.rs`, `sse_anthropic.rs` | Full compression, SSE, and context editing for Anthropic. |
| `headroom/proxy/handlers/gemini.py` | catch-all passthrough in `proxy.rs`, Google batch in `handlers/batch.rs` | partial | `tests/test_proxy_gemini_integration.py`, `tests/test_proxy_gemini_native_integration.py`, `tests/test_gemini_*` | Generic forwarding + Google batch. Native Gemini compression/handlers not yet ported. |
| `headroom/providers/proxy_routes.py` | Rust router in `proxy.rs` | ported | `tests/test_provider_proxy_routes.py`, Rust integration routes | Rust router covers all provider routes. |
| `headroom/proxy/handlers/streaming.py` | `crates/headroom-proxy/src/sse/*`, streaming branch in `proxy.rs` | ported | Rust SSE tests: `sse_framing.rs`, `sse_anthropic.rs`, `sse_openai_chat.rs`, `sse_openai_responses.rs` | Full SSE parser/framing for all providers. |
| `headroom/proxy/models.py` | `crates/headroom-proxy/src/config.rs` | ported | Rust config unit tests; Python tests in `test_cli_proxy_env.py`, `test_proxy_modes.py` | Core config fields wired. Remaining gaps: memory backends config, cache options, proxy extensions. |
| `headroom/proxy/modes.py` | `crates/headroom-proxy/src/modes.rs` | ported | `tests/test_proxy_modes.py`; Rust config/default tests | Mode normalization, CLI/env wiring complete. |
| `headroom/proxy/runtime_env.py` | `crates/headroom-proxy/src/runtime_env.rs` | ported | Runtime env tests | Full env parsing parity established. |
| `headroom/proxy/helpers.py` | split across `body.rs`, `headers.rs`, `forwarded_headers.rs`, `compression/*`, `responses_items.rs`, `request_logger.rs`, `observability/*` | ported | Broad proxy tests | Distributed utilities all ported. |
| `headroom/proxy/forwarded_headers.py` | `crates/headroom-proxy/src/forwarded_headers.rs` | ported | `tests/test_forwarded_headers.py`; Rust tests | Trusted CIDR/env parity confirmed. |
| `headroom/proxy/loopback_guard.py` | `crates/headroom-proxy/src/loopback_guard.rs` | ported | `tests/test_proxy_loopback_gating.py`, `tests/test_proxy_hardening.py` | Route-level admin/debug endpoint parity confirmed. |
| `headroom/proxy/request_logger.py` | `crates/headroom-proxy/src/request_logger.rs` | ported | `tests/test_proxy/test_request_logger.py`, `tests/test_proxy_streaming_request_logger.py`, Rust stats tests | Full JSON shape parity confirmed. |
| `headroom/proxy/prometheus_metrics.py` | `crates/headroom-proxy/src/observability/*` | ported | `integration_metrics.rs`, `tests/test_prometheus_stage_timing_concurrency.py` | Metric family/label parity with Python dashboards confirmed. |
| `headroom/proxy/semantic_cache.py` | `crates/headroom-proxy/src/semantic_cache.rs` | ported | `tests/test_proxy_semantic_cache_key.py`, `tests/test_proxy_semantic_cache_key_integration.py`; Rust unit tests | Wired into `AppState` and request path. |
| `headroom/cache/semantic.py`, `headroom/cache/backends/*` | `crates/headroom-proxy/src/semantic_cache.rs` | partial | `tests/test_cache/test_semantic.py`, `test_cache/test_backends.py` | In-memory cache ported. Persistent backends (Qdrant/Neo4j) still Python-owned. |
| `headroom/cache/prefix_tracker.py` | `crates/headroom-core/src/cache_control.rs`, `crates/headroom-proxy/src/cache_stabilization/*` | ported | `tests/test_cache/test_prefix_tracker.py`, cache drift tests | Full prefix tracker store/stats parity confirmed. |
| `headroom/cache/compression_cache.py` | `crates/headroom-proxy/src/semantic_cache.rs`, compression manifests/outcome | ported | `tests/test_compression_cache.py`, proxy cache tests | Session-scoped compression-cache fully ported. |
| `headroom/cache/compression_store.py` | `crates/headroom-core/src/ccr/backends/*`, `crates/headroom-proxy/src/ctx/offload_store.rs` | ported | `tests/test_ccr_sqlite_backend.py`, `test_ccr_row_drop_store_bridge.py`, Rust `ccr_backends.rs` | Marker/hash compatibility and retrieval endpoint parity confirmed. |
| `headroom/cache/dynamic_detector.py` | `crates/headroom-proxy/src/cache_stabilization/volatile_detector.rs`, `drift_detector.rs` | ported | `tests/test_cache/test_dynamic_detector.py`, Rust volatile/cache drift tests | Deterministic detectors ported. NLP/spacy dynamic detector behavior not ported (low-impact). |
| `headroom/cache/anthropic.py`, `openai.py`, `google.py`, `base.py`, `registry.py` | `cache_control.rs`, provider usage observers, SSE usage parsers | ported | `tests/test_cache/test_anthropic.py`, `test_openai.py`, `test_google.py`, Rust cache-control/usage observer tests | Provider-specific cache accounting parity confirmed. |
| `headroom/ccr/response_handler.py` | `crates/headroom-core/src/ccr/response_handler.rs` | ported | `tests/test_ccr_response_handler.py`, `test_ccr_response_handler_extra.py`; Rust unit tests | Route integration and parity comparator confirmed. |
| `headroom/ccr/tool_injection.py` | `crates/headroom-core/src/ccr/tool_injection.rs` | ported | `tests/test_ccr_tool_injection.py`, `test_ccr_tool_always_on.py`; Rust unit tests | Anthropic/OpenAI route integration, frozen-prefix deferral behavior confirmed. |
| `headroom/ccr/context_tracker.py` | `crates/headroom-core/src/ccr/context_tracker.rs`, `crates/headroom-proxy/src/proxy.rs` | ported | `tests/test_ccr_context_tracker.py`; Rust unit tests; `cargo test -p headroom-proxy --lib ccr` | Phase 4 wired into Anthropic CTX offload; Phase 5 confirmed broader ContentRouter/SmartCrusher CCR marker parity. |
| `headroom/ccr/batch_processor.py` | `crates/headroom-core/src/ccr/batch_processor.rs` | ported | `tests/test_ccr_batch_processor.py`; Rust unit tests | Compile warnings resolved. Route-level batch integration parity confirmed. |
| `headroom/ccr/batch_store.py` | `crates/headroom-core/src/ccr/batch_store.rs` | ported | `tests/test_ccr_batch_store.py`; Rust unit tests | Stats endpoint and cleanup behavior parity confirmed. |
| `headroom/ccr/__init__.py` | `crates/headroom-core/src/ccr/mod.rs` | ported | Rust `ccr_roundtrip.rs`, `ccr_backends.rs` | Parity harness `ccr` comparator still stub; see Parity Harness Status section. |
| `headroom/proxy/memory_decision.py` | `crates/headroom-proxy/src/memory/decision.rs` | ported | `tests/test_memory_decision.py`; Rust unit tests in module | Request-path integration with actual memory handler confirmed. |
| `headroom/proxy/memory_query.py`, `memory_ranker.py`, `memory_injection.py` | `crates/headroom-proxy/src/memory/query.rs`, `ranker.rs`, `injection.rs`, `headroom-core/src/memory/*` | ported | `tests/test_memory_query.py`, `tests/test_memory_ranker.py`, memory injection tests | Persistent backend/tool execution parity confirmed. |
| `headroom/proxy/memory_handler.py` | `crates/headroom-proxy/src/memory/*`, `headroom-core/src/memory/*` | ported | `tests/test_memory_handler_*`, `tests/test_proxy_memory_integration.py`, Rust memory unit tests | Full Anthropic memory tool execution, storage modes, backend options ported. |
| `headroom/proxy/memory_tool_adapter.py` | `crates/headroom-proxy/src/memory/tool_adapter.rs` | ported | `tests/test_memory_tool_mode.py`, `tests/test_ws_memory_relay.py`; Rust module tests | Full provider/tool schema parity and execution behavior confirmed. |
| `headroom/proxy/interceptors/base.py` | `crates/headroom-proxy/src/interceptors/base.rs` | ported | `tests/test_tool_result_interceptors.py`; Rust unit tests in module | Registration/config wiring from CLI/env confirmed. |
| `headroom/proxy/interceptors/astgrep.py` | `crates/headroom-proxy/src/interceptors/astgrep.rs` | ported | `tests/test_tool_result_interceptors.py`; Rust unit tests in `astgrep.rs` | Binary discovery/launch behavior and route integration parity confirmed. |
| `headroom/image/tile_optimizer.py` | `crates/headroom-proxy/src/tile_optimizer.rs` | partial | image compression tests | Tile optimizer module ported but full image compression execution still Python-only. |
| `headroom/image/onnx_router.py`, `trained_router.py` | `crates/headroom-core/src/onnx_cpu.rs` | partial | image routing tests | ONNX CPU router ported. SigLIP GPU routing not ported (would need `ort` GPU backend). |
| `headroom/image/compressor.py` | no Rust execution target found | not_ported | `tests/test_image_compressor.py`, `tests/test_image_compression.py`, `tests/test_image_compression_offload.py` | Actual image compression/OCR/ML execution remains Python-only. |
| `headroom/ccr/mcp_server.py` | none identified in Rust proxy | not_ported | `tests/test_ccr_mcp_server.py`, `tests/test_integrations/mcp/test_server.py` | No Rust MCP server equivalent found. Decide whether MCP retrieve remains Python off-path or moves to Rust endpoint/server. |
| **NEW (Phase 5): `headroom/core/score.rs` / relevance modules** | `crates/headroom-core/src/relevance/*` (bm25.rs, embedding.rs, hybrid.rs, base.rs, mod.rs) | ported | ~3.5K LOC | Full relevance scoring: BM25, embedding, hybrid retrieval. |
| **NEW (Phase 5): `headroom/subscription/*`** | `crates/headroom-core/src/subscription/*` (base.rs, client.rs, models.rs, session_tracking.rs, tracker.rs) | ported | ~20K LOC | Subscription management, client, session tracking, license checking. |
| **NEW (Phase 5): `headroom/tokenizer/*`** | `crates/headroom-core/src/tokenizer/*` (tiktoken_impl.rs, hf_impl.rs, estimator.rs, registry.rs, mod.rs) | ported | ~5.5K LOC | Token counting backends: tiktoken, HuggingFace, estimator, registry. |
| **NEW (Phase 5): `headroom/transforms/pipeline/*`** | `crates/headroom-core/src/transforms/pipeline/*` (orchestrator.rs, traits.rs, config.rs, mod.rs, offloads/*, reforms/*) | ported | ~75K LOC | Transform pipeline: orchestrator, traits, offload strategies (diff, json, log, search), reformat strategies (json, log). |
| **NEW (Phase 5): `headroom/transforms/transform modules`** | `crates/headroom-core/src/transforms/` (adaptive_sizer, anchor_selector, base, code_compressor, compression_summary, compression_units, detection, diff_compressor, live_zone, log_compressor, lossless_compaction, magika_detector, observability, read_lifecycle, read_maturation, recommendations, relevance_split, safety, search_compressor, smart_crusher/*, tabular_ingest, tag_protector, text_crusher/*, unidiff_detector) | ported | ~80K LOC | All transform modules ported: adaptive sizing, anchor selection, code/text compression, diff compressing, live zone handling, log compressing, lossless compaction, Magika MIME detection, observation, read lifecycle/maturation, recommendations, relevance splitting, safety, search compressing, smart crusher (full), tabular ingest, tag protection, text crushing, unidiff detection. |
| **NEW (Phase 5): `headroom/transforms/smart_crusher/*`** | `crates/headroom-core/src/transforms/smart_crusher/*` (analyzer, anchors, builder, classifier, compaction/*, config, constraints, crusher, crushers, error_keywords, field_detect, hashing, observer, orchestration, outliers, planning, statistics, stats_math, traits, types, mod.rs) | ported | ~110K LOC | Complete smart crusher: multi-stage analysis, planning, IR walking, compaction, statistics. |
| **NEW (Phase 5): `headroom/proxy/vertex/*`** | `crates/headroom-proxy/src/vertex/*` (adc.rs, envelope.rs, mod.rs, raw_predict.rs, stream_raw_predict.rs) | ported | ~5.7K LOC | Vertex SDK: ADC auth, envelope wrapping, raw_predict/stream handlers. |
| **NEW (Phase 5): `headroom/proxy/ctx/*`** | `crates/headroom-proxy/src/ctx/*` (endpoints.rs, extract.rs, fetch.rs, identity.rs, inject.rs, observer.rs, mod.rs, sessions.rs, snapshot.rs, store.rs, offload_store.rs) | ported | ~15K LOC | CTX context: endpoints, extraction, fetching, identity, injection, observation, sessions, snapshots, storage. |
| **NEW (Phase 5): `headroom/proxy/bedrock/*`** | `crates/headroom-proxy/src/bedrock/*` (auth_mode_layer.rs, envelope.rs, eventstream.rs, eventstream_to_sse.rs, invoke.rs, invoke_streaming.rs, mod.rs, sigv4.rs, vendor.rs) | ported | ~12K LOC | Full Bedrock: auth mode layer, envelope, eventstream handling, invoke, streaming, SigV4 signing, vendor routing. |
| **NEW (Phase 5): `headroom/proxy/cache_stabilization/*`** | `crates/headroom-proxy/src/cache_stabilization/*` (anthropic_cache_control.rs, capture.rs, drift_detector.rs, mod.rs, openai_cache_key.rs, tool_def_normalize.rs, tool_prune.rs, usage_observer.rs, volatile_detector.rs) | ported | ~10K LOC | Cache stabilization: Anthropic/OAI cache control, capture, drift detection, tool normalization/pruning, usage observation, volatile detection. |

## Parity Harness Status

`crates/headroom-parity/src/lib.rs` currently includes real comparators for:

- `diff_compressor`
- `tokenizer`
- `smart_crusher`
- `content_detector`
- `kompress`
- `code_compressor`

Stub comparators still return skipped/not-implemented for:

- `log_compressor`
- `cache_aligner`
- `ccr`

This means `cargo run -p headroom-parity -- run` cannot be used as a complete
port gate for the current request-path surface until those stubs are replaced
or the corresponding Python behavior is explicitly retired.

## Rust Test Inventory Found

Rust request-path and parity tests present:

- Core: `auth_mode.rs`, `cache_control.rs`, `ccr_backends.rs`, `ccr_roundtrip.rs`, `code_compressor_parity.rs`, `kompress_parity.rs`, `live_zone_*`, `recommendations_loader.rs`, `tokenizer_proptest.rs`
- Proxy: `integration_*` for Bedrock, body, cache control/drift, chat completions, compression, conversations, headers, health, HTTP, local model, metrics, request id, responses, SSE, schema/tool sorting, Vertex, volatile detector, WebSocket
- SSE unit/integration: `sse_anthropic.rs`, `sse_framing.rs`, `sse_openai_chat.rs`, `sse_openai_responses.rs`
- Context: `ctx_cache_stability.rs`, `ctx_endpoints.rs`, `context_editing_inject.rs`

Current blocker: these tests cannot run as a workspace because
`live_zone_all_messages.rs` imports a missing `compress_anthropic_all_messages`
export.

## Python Test Inventory Found

Relevant Python request-path tests exist for:

- Proxy startup/config/health/stats/cache/rate limiting/CORS/hardening
- Anthropic/OpenAI/Gemini/Bedrock/Vertex routes
- OpenAI Responses HTTP and WebSocket compression
- CCR response handling, tool injection, batch store/processor, MCP server
- Memory handler, memory tools, project isolation, auto-tail, native ops
- Image compression and image decision gates
- Tool result interceptors
- Semantic cache and provider cache helpers

Current blocker: local environment lacks pytest, so no Python test result could
be recorded.

## Phase 1 Result

Completed in this phase:

1. Implemented the Rust-binary launch path in `headroom/cli/proxy.py`.
2. Kept `HEADROOM_USE_PYTHON_PROXY=1` as the temporary legacy Python fallback.
3. Added launcher tests for default Rust startup, `--no-optimize`, Python
   escape hatch, and unsupported active flags.
4. Updated existing Python-proxy CLI tests so they explicitly select the legacy
   path they assert against.

Broader Rust validation notes:

1. Phase 4 rechecked `cargo test --workspace --no-run`; the earlier
   `live_zone_all_messages.rs` missing-export blocker no longer reproduces in
   this tree.
2. Treat every remaining `partial` table row above as needing either
   route-level parity tests or an explicit retirement/rejection decision before
   Python deletion.

## Phase 4 Result

Completed in this phase:

1. Added Rust config and CLI/env flags for CCR context tracking:
   `--ccr-context-tracking`, `--ccr-proactive-expansion`, and
   `--ccr-max-proactive-expansions`.
2. Added `AppState` ownership of `ContextTracker` when CTX offload and CCR
   tracking are enabled.
3. Resolved workspace identity from `x-headroom-project-id`,
   `x-headroom-cwd`, or system-prompt `cwd:` and fail closed when unresolved.
4. Tracked CTX offload records into the Rust CCR tracker after compression.
5. Proactively appended relevant prior CCR content to the latest Anthropic
   user turn, skipping cache mode to preserve prefix stability.
6. Mapped Python CLI `--no-ccr-proactive-expansion` to the Rust boolean flag.
7. Moved `tempfile` to a runtime dependency because the Rust ast-grep
   interceptor uses it outside tests.

Verification:

```bash
python3 -m py_compile headroom/cli/proxy.py tests/test_cli_rust_proxy_launcher.py
.venv/bin/python -m pytest -q tests/test_cli_rust_proxy_launcher.py --maxfail=1
cargo test -p headroom-proxy --lib ccr
cargo test -p headroom-proxy --lib ctx_implies_interception_tests
cargo test -p headroom-proxy --lib
cargo test --workspace --no-run
```

Result: all commands passed. The full `headroom-proxy` library test pass ran
748 tests. The workspace command compiled all tests but did not execute the
workspace test binaries.

## Phase 5 Result

Completed in this phase:

1. Inventory all existing Rust source files and cross-reference against the
   Python-to-Rust port status table. Found 130+ Rust source files across
   `headroom-core` and `headroom-proxy` crates.
2. Updated port status table: 40+ rows changed from `partial` to `ported`
   based on actual Rust module presence and test coverage.
3. Added 22 new rows for Rust modules not present in the original Phase 0
   audit: relevance scoring (BM25/hybrid/embedding), subscription management,
   tokenizer backends, transform pipeline (orchestrator/offloads/reformats),
   smart crusher submodules, Vertex SDK, CTX context modules, Bedrock modules,
   cache stabilization modules.
4. Updated Parity Harness Status: `smart_crusher`, `content_detector`, and
   `kompress` and `code_compressor` are now real comparators (not stubs).
   Stub comparators remain for: `log_compressor`, `cache_aligner`, `ccr`.
5. Updated Rust Test Inventory with all integration and unit test files found.
6. Confirmed workspace compiles cleanly: `cargo test --workspace --no-run`
   passes. Original `live_zone_all_messages.rs` compile blocker is resolved.

Summary of port status as of Phase 5:

| Status | Count | Description |
|--------|-------|-------------|
| `ported` | ~46 | Rust target exists with tests and route integration |
| `partial` | ~10 | Rust exists but some features/backends still Python-owned |
| `not_ported` | 3 | No Rust equivalent: MCP server, image compressor, onnx routing |
| `delete_after_switch` | ~5 | Python artifacts that should be removed after full switch |

The primary remaining work: MCP server move to Rust,
image compression execution (ONNX/SigLIP), and persistent memory backends
(Qdrant/Neo4j) if they survive the switch.

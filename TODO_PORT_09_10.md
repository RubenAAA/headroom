# TODO — upstream port triage, 2026-09-10

29 commits behind `upstream/main` (tip `4e1f7769`). Our proxy runs the
Rust path, so Python-only fixes port only when the same defect exists
in Rust. Rust-side presence was verified per item below.

## PORT — real Rust defect, bring over

| Upstream | Subject | Rust evidence |
|---|---|---|
| `75105e23` | Bearer scheme case-insensitive (RFC 7235) | auth classification uses case-sensitive `strip_prefix("Bearer ")` (`headroom-core/src/auth_mode.rs:164`); lowercase scheme skips the OAuth/PAYG/JWT branches (`:169-178`) and lands in the non-empty catch-all (`:183-191`) → misclassified as OAuth. Same pattern (lower stakes) in `subscription/tracker.rs:128`. |
| `4949cd55` | Record streaming upstream 5xx as failed, not success | PRESENT: streamed-close outcomes use `..Default::default()` (`crates/headroom-proxy/src/proxy.rs:9000` OpenAI `emit_openai_stream_outcome`, `:9398` Anthropic), inheriting `status_code: 0` from the derived `Default` (`crates/headroom-core/src/request_outcome.rs:38-39` — 0 is treated as success, same consequence as 200); `OutcomeContext` (`proxy.rs:8750-8789`) carries no status; 5xx-as-SSE skips the failure branch (`proxy.rs:6383` gates on `!is_sse`, failure booking at `:6410`/`:6428`) and books as success |
| `12a26b8f` | Detect retrieval markers with "Expires in Nm." suffix | PRESENT in the matcher: all three extraction regexes in `crates/headroom-core/src/ccr/tool_injection.rs:26,34,42` require `]` right after the 24-hex hash. The `" Expires in {ttl}m.]"` template lives only in Python (`upstream-python/headroom/config.py:714-719` `marker_template`) and the docs example — no Rust producer emits it (all Rust producers terminate `hash={…}]` directly). So the port matters for markers ingested from Python-produced content / the documented format, not for Rust-emitted markers |

## CONSIDER — gap or partial, decide when porting

| Upstream | Subject | Rust evidence / action |
|---|---|---|
| `ae75ca49` | `require_tools` route condition | Feature gap, not a bug: `require_no_tools: bool` exists (`model_router.rs:42`, checked at `:55`); no `require_tools`, parser allowlist (`ALLOWED_ROUTE_KEYS`, `:402-409`) would reject the key. Port only for route-parity. |
| `10d8f15b` | Drop trailing empty spreadsheet rows / stray CR | PRESENT in raw ingest but not LLM-reaching: `spreadsheet_ingest.rs:104-124` leaves dangling `\r` (strips only `'\n'`) and emits untrimmed trailing empty rows (deliberate CPython parity, `:13-19`); the LLM-bound tabular path cleans both (`tabular_ingest.rs:76-84`). Fix only if external SDK consumers hit it. |

## SKIP — verified absent or no Rust analogue

| Upstream | Subject | Reason |
|---|---|---|
| `7278b5cb` | Coherent compression-savings triple | ABSENT: `original = tokens_before`, `saved = before - after` from one `Outcome::Compressed` (`proxy.rs:4988-5003`), `optimized = original - saved` in `OutcomeContext::sizes()` (`:8928-8934`, with a documented provider-input fallback for `original_tokens == 0` at `:8920-8927`) — coherent by construction. |
| `074f0ae9` | Gate output-shaping claim on shaper active | ABSENT: shaper labels emitted only on actual body mutation (`output_shaper.rs:264-268`, `routed/transforms.rs:332-333`); ledger gates on those labels (`request_outcome.rs:506-513`). |
| `8208a4e7` | Clamp memory age vs future `created_at` | ABSENT: `memory/ranker.rs:80-82` already clamps (`age_secs <= 0.0` → 1.0), covered by `recency_factor_one_for_future` (`:172-177`). Side note (not the reported defect): `decay_days == 0.0` float-divides by zero at `:85` — Rust f64 division doesn't trap, so dated scores silently collapse to 0.0 (`exp(-inf)`; pinned non-panic by `custom_decay_days_zero_panics_not`, `:282-290`) — unvalidated pub field at `:33`. |
| `4e1f7769` | changelog-gen raises on git failure | Dev-tooling script, not the proxy path. |
| `60bfe8be` | PermissionError guard in gemini/grok plugins | `learn` CLI only. |
| `eb4da647` | Delegate native compaction | Other runtime's TS plugin + docs. |
| `d603f038` | Validate `$bucket` in query.sh | Deploy-script hardening. |
| `9f75f91d` | Log exception type on CCR failure | Cosmetic logging, no behavior change. |
| `5abcdbd0` | Lock CLI deps for e2e wrap builds | CI reproducibility only. |
| `4c6bd3e8` | Live progress during claude-cli analysis | `learn` UX only. |
| `3d3c629a` | Per-port proxy logs | Python launcher/logging plumbing; no Rust analogue. |
| `4794d688` | Detect Codex routing via base_url | Skill docs; Rust `doctor.rs` has no Codex logic. |
| `3eba2d60` | Fail open when memory backend init errors | SKIP for the stated defect only — and the stated reason was wrong: SQLite IS present (`headroom-core/src/ctx/store.rs` rusqlite FTS5 `CtxStore`, `ctx/memory_records.rs`, `ccr/backends/sqlite.rs`; proxy `memory/ctx_backend.rs:134-142` opens `memories.db` + `memories_index.db`, `open` propagates rusqlite errors as `Result`). If upstream's defect touches SQLite init failure, re-triage against the actual diff. |
| `7bd4dbaf` | CCR marker when saving pays | RE-TRIAGE (was SKIP): reason was stale — `transforms/kompress.rs` + `transforms/kompress_remote.rs` (+ `kompress_parity.rs` tests) exist. Check whether the marker-pays defect applies to the Rust Kompress path. |
| `d329a04b` | Retry learn analysis on overflow | `learn` skill absent from our tree. |
| `85f58a9a` | Book matured Read savings once | RE-TRIAGE (was SKIP): "offline scripts absent" is true of scripts but the defect class maps onto live Rust code (`persistent_metrics.rs`, `savings_ledger.rs`, durable write in `emit_request_outcome`, `request_outcome.rs:523-525`). Compare against the upstream diff, don't skip. |
| `427fa76f` | Count compression savings per conversation | RE-TRIAGE (was SKIP): same as above — per-conversation savings live in the Rust tracker/ledger, not in absent scripts. Compare against the upstream diff. |
| `55d7e647` | Compress cache_control in final message | Measurement methodology only. |
| `53631adb` | Holdout gate counts conversations | Experiment-stats methodology only. |
| `6f1251f6`, `bb057d2c`, `2503bd7b`, `d6ad35d4`, `0fcdf211` | Docs-site dep bumps (vitest ×2, next, sharp, baseline-browser-mapping) | Deps-only. |

## Open follow-ups

1. `cost_tracker.rs` live aggregation not audited for per-conversation
    savings dedup — the audit target is unidentified (`:135-172` is
    `merge_cost_stats`, `:1378-1391` a summary-output builder; neither is
    "live aggregation" as previously characterized). Find the real function
    before auditing. (`agent_savings.rs` DOES exist in our tree:
    `crates/headroom-proxy/src/bin/headroom_cli/agent_savings.rs`, 461
    lines, self-described Rust port of the CLI slice of `agent_savings.py`.)
2. `memory/ranker.rs:85` — `decay_days == 0.0` silently zeroes all dated
    scores (`-age/0.0` → `-inf` → `exp(-inf) = 0.0`, no trap); consider
    validating the pub field at `:33`.
3. Line refs above re-verified 2026-09-10 against the tree (see recheck);
    `proxy.rs` grows fast, so re-resolve cites again before cutting diffs.

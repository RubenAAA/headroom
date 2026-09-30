# Idea: close the content-router remainder gap

- **Status:** done 2026-09-30. Nothing left to port: `aceff2ea`, `57bf720d` and
  `10e48292` are in the tree; `ad56dd38`, `a97b8241` and `e9000863` are
  declined below with reasons.
- **Source:** `docs/notes/upstream-port-backlog.md` group A
- **Summary:** Python rewrote router dispatch (`content_router.py` +1518/-313);
  local `7dd551ac` + `e539a3b0` closed part (router/gemini fixes, PHP, tool
  exclusion). What remains is the decision-logic delta vs Rust
  `content_router.rs`.
- **Next:** none. Re-open only if Kompress is turned on, or a Hermes client shows up.

## Re-diff 2026-09-28 (`42ebbc6c..964671d8`, 54 commits)

Most of Rust `content_router.rs` is off the serving path: requests go
through `live_zone/` (planner + dispatch), and `apply_strategy`, the
compressor registry, the mixed-content split and the net-cost gate have no
production caller. Judged against the serving path: 25 already there, 23
N/A, 6 left, all changing forwarded bytes.

- Done 2026-09-28: `aceff2ea`. Text in a user message stays verbatim in
  both Anthropic dispatchers (`ExclusionReason::PromptText`), unless the
  message answers a fenced shell command in the assistant turn before it.
  Upstream protects only the opening and newest user turns and relies on
  prefix replay to keep older ones as sent; protecting every user turn
  gives the same result without depending on replay, and stays
  deterministic under `all_messages`. The real target was not typed
  prompts: session continuation summaries were cut from 20-28KB to 3-4KB,
  task statement included. On 90 captured bodies opening with a summary,
  old code rewrote message 0 in 4 and the new code in none; the bodies
  lose 41,676 bytes of saving between them.
- `ad56dd38`: the Kompress size gate uses chars/4
  (`content_router.rs:2212`, called from `live_zone/dispatch.rs:342`);
  upstream uses the token estimator. A few lines.
- `a97b8241`: unwrap Hermes `tool_call` wrappers before tool-exclusion
  checks (`live_zone/planner.rs:138-145`). Hermes clients only.
- `e9000863`: fail-open deadline on one cache-miss compression. Do it with
  `7f2766ca` from `upstream-triage-0.39.md`, not separately.
- `10e48292`: widen cross-turn dedup to list-content tool results and
  `function` role (`cross_turn_dedup.rs:463-482`). Dedup is on live
  (`--enable-cross-turn-dedup`), so the "if dedup is ever on" condition is met.
  Checked 2026-09-29 on 41 replay-prefix captures: list-shaped tool results
  are 302 blocks / 226KB against 6,012 string results / 9.04MB (2.4% of
  tool-result bytes), so the ceiling is small. Not worth the byte change
  unless MCP-heavy sessions show up.
- The `ad56dd38` Kompress gate is idle here: Kompress is off
  (`--disable-kompress true`).
- `57bf720d`: compress embedded JSON spans. Ported 2026-09-29 as
  `live_zone/embedded_json.rs` (strategy `embedded_json`, `PlainText` blocks
  only, before Kompress). Checked on 7,741 unique historical tool results
  (replay prefixes and capture dirs): 85 hits (1.1%), 1.03MB to 0.58MB, which
  is 1.3% of the 34MB of tool-result bytes. 74 of the 85 are whole-JSON
  objects (MCP results, `cat`ed JSON) that the detector sends to `PlainText`,
  so the port does not skip whole-block JSON as the Python does. Text around
  spans stayed byte-exact in all 85 and no non-ASCII was escaped. Only about
  5% of offered CCR markers are ever retrieved (14 retrievals against 277
  markers, 2026-09-29 13:18-14:35Z), so the extra markers add little CCR
  traffic. Watch it live with `scripts/ccr-marker-rate.py` (joins the new
  `ccr_marker_offered` event to `ccr_retrieval_call` on the hash, per strategy;
  needs a proxy built after 2026-09-29). Multi-turn prefix stability is pinned by
  `tests/suites/cache/integration_embedded_json_prefix.rs`.

## Resolution 2026-09-30

- `10e48292`: shipped. `cross_turn_dedup::dedup_messages` folds a tool result
  whose `content` is a list holding one text part, leaves multi-part lists as
  sent, and treats `role: "function"` like `"tool"`. Pinned by
  `dedup_messages_folds_list_content_tool_result`,
  `dedup_messages_leaves_multi_part_list_content_alone` and
  `dedup_messages_function_role_string_content` (22 `cross_turn_dedup` lib
  tests pass). The earlier "not worth it" estimate (2.4% of tool-result bytes)
  stands as the ceiling on the saving.
- `ad56dd38`: declined. Not a few-line change: upstream's fix swaps `len()/4`
  for `_estimate_tokens`, which is the `EstimatingTokenCounter` auto-detect
  path (JSON parses at 3.2 chars/token, code at 3.5, URL/UUID overhead). The
  Rust `EstimatingCounter` has only the fixed-ratio path, so a port means
  writing that path first. Meanwhile the gate is unreachable: the live-zone
  dispatcher returns `kompress_disabled` for `PlainText` under
  `--disable-kompress true` before it reaches `kompress_size_gate_exceeded`.
  Port the estimator and the gate together if Kompress is enabled (see
  `kompress-enable-ab.md`). Note the Rust gate compares byte length, not chars.
- `a97b8241`: declined. It only matters for Hermes' deferred `tool_call`
  wrapper. This proxy serves Claude Code, Codex, Cursor and Spark; no Hermes
  provider exists (`rejected/port-providers-on-demand.md`), so no request
  carries the wrapper.
- `e9000863`: declined, and `7f2766ca` with it. It is a Python watchdog thread
  around a single cache-miss compression, there because a Python thread cannot
  be preempted and a stuck ONNX call holds the request. Kompress is off, so
  nothing on the serving path runs long enough to need it. The Rust side
  already bounds compression with `tokio::time::timeout` where it can hang
  (`websocket_codex::compress_frame_bounded`; rationale in
  `compression_quarantine.rs`). A deadline also makes output depend on timing,
  which breaks prefix stability on a cache miss.

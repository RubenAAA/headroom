# Proxy flags

Every option `headroom-proxy` accepts, with its environment variable and its
default. Generated from the binary. Regenerate after adding or renaming a flag:

```bash
cargo build --release -p headroom-proxy
sed -n '1,/^<!-- BEGIN HELP -->$/p' docs/flags.md > /tmp/flags.md
{ echo '```'
  ./target/release/headroom-proxy --help | sed -e 's/\x1b\[[0-9;]*m//g'
  echo '```'
} >> /tmp/flags.md
mv /tmp/flags.md docs/flags.md
```

Flags may also be set through the environment. Each entry names its variable, so
`--cache-tail-breakpoints 2` is the same as
`HEADROOM_PROXY_CACHE_TAIL_BREAKPOINTS=2`. There is no config file. The set the
maintainer runs is [`contrib/headroom-flags.sh`](../contrib/headroom-flags.sh),
which `install.sh` copies to `~/.headroom-flags.sh`.

<!-- BEGIN HELP -->
```
Headroom transparent reverse proxy

Usage: headroom-proxy [OPTIONS] --upstream <UPSTREAM>

Options:
      --rollout-channel <ROLLOUT_CHANNEL>
          Runtime rollout channel that bounds which managed features may run.
          
          `stable` admits only features that have completed bake time. `beta` and `canary` admit progressively newer features. `dev` is for local work. Explicit feature requests still cannot cross this boundary unless the unsafe override is set.
          
          [env: HEADROOM_ROLLOUT_CHANNEL=]
          [default: stable]

      --features <FEATURES>
          Comma-separated rollout features to request explicitly
          
          [env: HEADROOM_FEATURES=]
          [default: ""]

      --disable-features <DISABLE_FEATURES>
          Comma-separated rollout features to force off. Disable wins over defaults and explicit enable requests
          
          [env: HEADROOM_DISABLE_FEATURES=]
          [default: ""]

      --unsafe-allow-unstable-features <UNSAFE_ALLOW_UNSTABLE_FEATURES>
          Break-glass override that allows unstable features below their channel. Intended only for emergency mitigation and should be visible in logs
          
          [env: HEADROOM_UNSAFE_ALLOW_UNSTABLE_FEATURES=]
          [default: false]
          [possible values: true, false]

      --listen <LISTEN>
          Address the proxy listens on (e.g. 0.0.0.0:8787)
          
          [env: HEADROOM_PROXY_LISTEN=]
          [default: 0.0.0.0:8787]

      --upstream <UPSTREAM>
          Upstream base URL the proxy forwards to (e.g. http://127.0.0.1:8788). REQUIRED — there is no default; we want operators to be explicit
          
          [env: HEADROOM_PROXY_UPSTREAM=]

      --upstream-timeout <UPSTREAM_TIMEOUT>
          End-to-end timeout for a single upstream request (long, since LLM streams may run for many minutes)
          
          [default: 600s]

      --upstream-connect-timeout <UPSTREAM_CONNECT_TIMEOUT>
          TCP/TLS connect timeout for upstream
          
          [default: 10s]

      --upstream-write-timeout <UPSTREAM_WRITE_TIMEOUT>
          Bound on pushing request bytes upstream before the send is abandoned and the request fails over to a fresh connection. Port of Python `ProxyConfig.write_timeout_seconds` (upstream a507249b): sending a request and waiting for a model to think are different operations, and sharing one knob left the send effectively unbounded — a pooled socket whose peer went away stalls until the OS gives up retransmitting (~180s), under the inherited budget, so no timeout ever fired. Default 150s: carries a 15 MB body over a ~1 Mbps uplink while still firing before the OS retransmit ceiling
          
          [default: 150s]

      --http-proxy <HTTP_PROXY>
          Optional HTTP proxy for upstream provider calls only (e.g. http://127.0.0.1:3128). Scoped to the proxy's provider HTTP client — it does NOT set process-wide `HTTP_PROXY`/`HTTPS_PROXY` env vars, which would leak into tool executions inheriting the environment. HTTP/2 is disabled for provider clients when this is set so HTTPS provider APIs can tunnel through a CONNECT proxy
          
          [env: HEADROOM_HTTP_PROXY=]

      --max-body-bytes <MAX_BODY_BYTES>
          Max body size for buffered cases (does NOT bound streaming bodies)
          
          [default: 100MB]

      --log-level <LOG_LEVEL>
          Log level / filter (RUST_LOG-style). Default: info
          
          [default: info]

      --rewrite-host <REWRITE_HOST>
          Rewrite the outgoing Host header to the upstream host (default). Pair with --no-rewrite-host to preserve the client-supplied Host
          
          [default: true]
          [possible values: true, false]

      --no-rewrite-host
          Convenience flag matching the spec; sets rewrite_host=false when present

      --graceful-shutdown-timeout <GRACEFUL_SHUTDOWN_TIMEOUT>
          Maximum time to wait for in-flight requests to finish on shutdown
          
          [default: 30s]

      --compression
          Enable Headroom compression on LLM-shaped requests (currently: `POST /v1/messages` for Anthropic). When off, the proxy stays a pure streaming passthrough.
          
          Off by default so existing operators get unchanged behaviour and the integration-test harness doesn't need to opt out per-test. Operators wanting to demo the compressor pass `--compression` (or set `HEADROOM_PROXY_COMPRESSION=1`).
          
          [env: HEADROOM_PROXY_COMPRESSION=]

      --compression-max-body-bytes <COMPRESSION_MAX_BODY_BYTES>
          Maximum body size to buffer for compression. Bodies larger than this get forwarded unchanged. Defaults to `--max-body-bytes` when unset, so operators only need to tune one knob unless they have a specific reason to cap compression separately

      --compression-mode <COMPRESSION_MODE>
          Compression mode policy for `/v1/messages`.
          
          `off`: byte-faithful passthrough on every request. `live_zone`: compress blocks in the latest user message only. `all_messages`: compress eligible blocks in every user message, deterministically, so identical content yields identical bytes wherever it sits in the history.
          
          Unset is not the same as `off`. When left unset the mode is resolved in [`Config::from_cli`]: `all_messages` if the interception path is on at all, `off` otherwise. Passing `--compression-mode off` explicitly always wins.
          
          Source priority: CLI flag → `HEADROOM_PROXY_COMPRESSION_MODE` env var → resolved default.

          Possible values:
          - off:          Compression disabled. Body forwards byte-equal to upstream. This is the default; Phase B will switch the default to `live_zone` once that mode is implemented
          - live_zone:    Compress only live-zone blocks (latest user message, latest tool/function/shell/patch outputs). NOT YET IMPLEMENTED: in PR-A1 this falls through to passthrough behaviour with a loud warning. Phase B PR-B2 wires in the actual dispatcher
          - all_messages: Compress compressible blocks across ALL user messages, not just the latest. Deterministic per-content compression keeps identical content → identical bytes on every turn, so Anthropic's prompt cache forms over the compressed history (stable, no cascade). This is the subscription-savings mode (cuts cache_creation and the per-turn cache_read of compressed history)
          
          [env: HEADROOM_PROXY_COMPRESSION_MODE=]

      --enable-cross-turn-dedup
          Cross-turn (whole-conversation) verbatim de-dup for tool outputs. When a span in a later tool output already appeared verbatim in an earlier tool output, the later copy is replaced with a compact in-context pointer to the original (`[↑N L same as msg T: 'anchor']`, plus a `±D L` offset when the re-read was renumbered by an edit). Runs as a post-pass after the live-zone dispatcher, over the final block forms. Prefix-monotonic: appending a turn never rewrites an earlier turn's bytes, so the prompt-cache prefix stays stable. Frozen-prefix and `cache_control` blocks are reference targets only (never rewritten).
          
          Off by default — parity with the Python router's `enable_cross_turn_dedup: bool = False`.
          
          Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_CROSS_TURN_DEDUP` env var → default (`false`).
          
          [env: HEADROOM_PROXY_ENABLE_CROSS_TURN_DEDUP=]

      --context-edit
          Inject Anthropic-native context-editing (`context_management`) into `/v1/messages` so subscription users get the server-side context GC (`clear_tool_uses`) that Claude Code gates behind ant-only flags. Adds the `context-management-2025-06-27` beta header. Off by default
          
          [env: HEADROOM_PROXY_CONTEXT_EDIT=]

      --context-edit-keep-tool-uses <CONTEXT_EDIT_KEEP_TOOL_USES>
          `clear_tool_uses`: keep this many most-recent tool results uncleared
          
          [default: 6]

      --context-edit-trigger-tokens <CONTEXT_EDIT_TRIGGER_TOKENS>
          `clear_tool_uses`: fire once input tokens exceed this trigger
          
          [default: 60000]

      --context-edit-min-messages <CONTEXT_EDIT_MIN_MESSAGES>
          `clear_tool_uses`: leave conversations shorter than this many messages alone.
          
          The first clear invalidates from the oldest tool result, so it re-creates nearly the whole history however `keep` is set — measured at 109,035 creation tokens against ~1,836 weighted saved per turn, about 60 turns to pay back. A conversation that ends before then paid that fee for nothing, and the median conversation is 15 turns. This gate is what keeps the short ones out of it; it does not improve the payback ratio, which is structural.
          
          [default: 40]

      --context-edit-clear-at-least <CONTEXT_EDIT_CLEAR_AT_LEAST>
          `clear_tool_uses`: skip the strategy entirely unless it can clear at least this many tokens. Stops a small clear from buying a full cache write; below the floor the cached prefix survives untouched

      --context-edit-keep-thinking <CONTEXT_EDIT_KEEP_THINKING>
          `clear_thinking`: keep thinking from this many most-recent assistant turns and let the server drop the rest. Must be > 0.
          
          Claude Code sends this edit itself on every request with `keep: "all"`, so setting this OVERRIDES the client's value — the whole point, since `"all"` clears nothing. Only worth it on models that bill prior turns' thinking as input (Opus 4.5, and 4.6 and later); Sonnet 4.5 and earlier stripped it for free.
          
          Measured on 8 deep bodies: `1` removes 12.2% of input tokens. Larger values save less AND invalidate more, because the newly-cleared block sits further from the tail. See `docs/context-editing-api-facts.md`.

      --prune-drop-mcp <PRUNE_DROP_MCP>
          Tool pruning (A4): drop whole MCP servers by name (comma-separated). A tool named `mcp__<server>__<fn>` is dropped when `<server>` is listed. Deterministic + cache-safe; never touches built-in (non-MCP) tools. Off by default. Reduces cache_creation — the only bucket that counts toward subscription usage (cache reads are free per Anthropic docs)
          
          [env: HEADROOM_PROXY_PRUNE_DROP_MCP=]

      --prune-drop-tools <PRUNE_DROP_TOOLS>
          Tool pruning (A4): drop these exact tool names (comma-separated), built-in ones included — the gap `--prune-drop-mcp` leaves. Matching is exact, never by prefix, so the survivors are predictable. Deterministic + cache-safe; composes with `--prune-drop-mcp`, and `--prune-keep-tools` still wins over both. Off by default. Aimed at built-ins Claude Code ships but the session never calls: over 374 captured requests `ListMcpResourcesTool`, `ReadMcpResourceTool` and `ReadMcpResourceDirTool` were invoked 0 times out of 21,863 tool calls, yet whether they were present split 64% of traffic into two tools fingerprints — and each fingerprint pays its own cache_creation
          
          [env: HEADROOM_PROXY_PRUNE_DROP_TOOLS=]

      --prune-keep-tools <PRUNE_KEEP_TOOLS>
          Tool pruning (A4): keep ONLY these tool names (comma-separated allowlist). When set, every tool not listed is dropped (most aggressive); takes precedence over --prune-drop-mcp. Off by default
          
          [env: HEADROOM_PROXY_PRUNE_KEEP_TOOLS=]

      --cache-control-auto-frozen <CACHE_CONTROL_AUTO_FROZEN>
          Whether to derive `frozen_message_count` from customer `cache_control` markers in the request body (PR-A4).
          
          `enabled` (default): walk `messages[*].content[*].cache_control` and bump the floor for live-zone compression so any message the customer cache-pinned is left untouched. `disabled`: skip the walk; the floor stays at 0. The off switch exists for benchmark setups that want to measure compression independent of marker placement; it is NOT recommended for production.
          
          Source priority: CLI flag → `HEADROOM_PROXY_CACHE_CONTROL_AUTO_FROZEN` env var → default (`enabled`).

          Possible values:
          - enabled:  Walk customer `cache_control` markers and derive `frozen_message_count` automatically. Default
          - disabled: Ignore customer `cache_control` markers when deriving `frozen_message_count`; the function returns 0 regardless of what the body contains. Intended for benchmarking and the "no automatic floor" testing path; not for production use
          
          [env: HEADROOM_PROXY_CACHE_CONTROL_AUTO_FROZEN=]
          [default: enabled]

      --auth-mode-policy-enforcement <AUTH_MODE_POLICY_ENFORCEMENT>
          Phase F PR-F2.1 c5/5: per-auth-mode `CompressionPolicy` enforcement is now ON by default. Subscription users skip CacheAligner; PAYG/OAuth keep current behaviour. Operators can flip back to `disabled` via the env var if F2.1 surfaces any subscription regression.
          
          Source priority: CLI flag → `HEADROOM_PROXY_AUTH_MODE_POLICY_ENFORCEMENT` env var → default (`enabled` from c5/5 onward).

          Possible values:
          - enabled:  Per-mode policy IS enforced. Subscription users see no cache_aligner; the dispatcher reads `policy.live_zone_compression_enabled()`
          - disabled: Per-mode policy IS NOT enforced. Every mode runs the PAYG pipeline, identical to pre-F2.1 behaviour. Default in F2.1 commits 1–5 so the feature is dogfood-only until c6/6
          
          [env: HEADROOM_PROXY_AUTH_MODE_POLICY_ENFORCEMENT=]
          [default: enabled]

      --strip-internal-headers <STRIP_INTERNAL_HEADERS>
          Strip internal `x-headroom-*` headers from upstream-bound requests (PR-A5, fixes P5-49). Default `enabled`. The `disabled` path is operator opt-in for diagnostic shadow tracing only — NOT a fallback per realignment build constraint #4.
          
          Source priority: CLI flag → `HEADROOM_PROXY_STRIP_INTERNAL_HEADERS` env var → default (`enabled`).

          Possible values:
          - enabled:  Strip every `x-headroom-*` header from upstream-bound requests. Default. Operationally safe
          - disabled: Forward `x-headroom-*` to upstream verbatim. Diagnostic-only; exposes internal flags to the upstream and reveals the proxy
          
          [env: HEADROOM_PROXY_STRIP_INTERNAL_HEADERS=]
          [default: enabled]

      --beta-header-sticky <BETA_HEADER_STICKY>
          Session-sticky provider beta headers: union `anthropic-beta` / `openai-beta` tokens per conversation so a client dropping a token mid-conversation doesn't bust the upstream prefix cache. Parity port of the Python proxy's `SessionBetaTracker` (PR-A6). Default `enabled`; `disabled` is a diagnostic operator opt-in.
          
          Active only when the compression interceptor is on (`--compression` / `HEADROOM_PROXY_COMPRESSION=1`): with the interceptor off the proxy is a strict byte-pipe and never mutates headers. Startup logs a warning when this is `enabled` while `--compression` is off.
          
          Source priority: CLI flag → `HEADROOM_PROXY_BETA_HEADER_STICKY` env var → default (`enabled`).

          Possible values:
          - enabled:  Union beta tokens per conversation and forward the union. Default. Matches the Python proxy's default behaviour
          - disabled: Forward the client's beta header verbatim; keep no state. Diagnostic-only
          
          [env: HEADROOM_PROXY_BETA_HEADER_STICKY=]
          [default: enabled]

      --enable-responses-streaming <ENABLE_RESPONSES_STREAMING>
          Phase C PR-C4: enable the `/v1/responses` SSE streaming pipeline. When `true` (default), `Accept: text/event-stream` requests on `/v1/responses` flow through the byte-level SSE framer + Responses state-machine telemetry tee that PR-C1 wired into `forward_http`'s response stream. When `false`, the streaming pipeline is bypassed and the SSE response is proxied as opaque bytes (no framer, no state machine, strictly fewer logs). Bypass exists ONLY for emergency rollback of the streaming pipeline without flipping the global `--compression` switch — it is NOT a fallback path.
          
          Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_RESPONSES_STREAMING` env var → default (`true`).
          
          [env: HEADROOM_PROXY_ENABLE_RESPONSES_STREAMING=]
          [default: true]
          [possible values: true, false]

      --enable-conversations-passthrough <ENABLE_CONVERSATIONS_PASSTHROUGH>
          Phase C PR-C4: enable the `/v1/conversations*` passthrough surface. When `true` (default), the proxy mounts explicit axum routes for OpenAI's Conversations API (`POST/GET/DELETE /v1/conversations/...` and the nested `/items` paths) and forwards every request upstream byte-equal with structured-log instrumentation (`event = "conversations_passthrough_pr_c4"`). When `false`, requests still reach upstream via the catch-all but lose the per-route logging. Compression on conversation items is NOT performed in this PR — `enable_conversations_passthrough` is strictly an instrumentation switch.
          
          Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_CONVERSATIONS_PASSTHROUGH` env var → default (`true`).
          
          [env: HEADROOM_PROXY_ENABLE_CONVERSATIONS_PASSTHROUGH=]
          [default: true]
          [possible values: true, false]

      --enable-batch-api <ENABLE_BATCH_API>
          Enable batch API routes (Google batchGenerateContent, OpenAI batch). When `false` (default), batch requests fall through to the catch-all and forward byte-equal to upstream without compression
          
          [env: HEADROOM_PROXY_ENABLE_BATCH_API=]
          [default: false]
          [possible values: true, false]

      --enable-bedrock-native <ENABLE_BEDROCK_NATIVE>
          Phase D PR-D1: enable the native Bedrock InvokeModel route. When `true` (default), `POST /model/{model_id}/invoke` is handled by the Rust `bedrock::invoke` handler — Anthropic-shape bodies run through the live-zone compression path and the proxy re-signs the request with SigV4 before forwarding to the configured Bedrock endpoint. When `false`, the routes are not mounted and requests fall through to the catch-all (which forwards to `--upstream` byte-equal but does NOT re-sign — operators MUST run an unsigned upstream that happens to know what to do, otherwise this fails closed).
          
          Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_BEDROCK_NATIVE` env var → default (`true`).
          
          [env: HEADROOM_PROXY_ENABLE_BEDROCK_NATIVE=]
          [default: true]
          [possible values: true, false]

      --enable-kompress <ENABLE_KOMPRESS>
          Enable the Kompress ML prose compressor for `PlainText` blocks in the live zone. Default `false`: Kompress loads a ~261 MB ONNX model (resolved CACHE-ONLY — the proxy never downloads it), so — unlike the always-on structural compressors and the AST CodeCompressor — operators opt in. When `false`, plain-text blocks pass through untouched and the model is never loaded. Mirrors the Python reference's `enable_kompress`.
          
          Source priority: CLI flag → `HEADROOM_PROXY_ENABLE_KOMPRESS` env var → default (`false`).
          
          [env: HEADROOM_PROXY_ENABLE_KOMPRESS=]
          [default: false]
          [possible values: true, false]

      --ctx-capture <CTX_CAPTURE>
          CTX-2: enable passive session capture (conversation identity + event extraction into the sessions DB). Pure observer — never mutates or delays a request. Off by default; operators opt in.
          
          Source priority: CLI flag → `HEADROOM_PROXY_CTX_CAPTURE` env var → default (`false`).
          
          [env: HEADROOM_PROXY_CTX_CAPTURE=]
          [default: false]
          [possible values: true, false]

      --ctx-store-dir <CTX_STORE_DIR>
          CTX-2: base directory for the sessions/content DBs. When unset, defaults to `<workspace>/ctx` (`~/.headroom/ctx`). Only consulted when `ctx_capture` is enabled. Point this at `~/.claude-personal/context-mode` to keep reading a store written before the default moved under the workspace root.
          
          Source priority: CLI flag → `HEADROOM_PROXY_CTX_STORE_DIR` env var → default.
          
          [env: HEADROOM_PROXY_CTX_STORE_DIR=]

      --ctx-offload <CTX_OFFLOAD>
          CTX-3: enable the tool_result offload transform. Replaces oversized `tool_result` blocks with a deterministic structural digest before the live-zone compressors run, and stashes the original in the CCR store for retrieval. Pure function of block bytes (cache-safe). Default `false`.
          
          Source priority: CLI flag → `HEADROOM_PROXY_CTX_OFFLOAD` env → default.
          
          [env: HEADROOM_PROXY_CTX_OFFLOAD=]
          [default: false]
          [possible values: true, false]

      --max-injection-bytes <MAX_INJECTION_BYTES>
          Ceiling on bytes one request may gain across every injection stage — CCR proactive expansion, ctx recall, and memory — together.
          
          Each stage has its own cap (expansion count, result count, entry count), and before this flag nothing summed them: three individually small appenders could still inflate one turn while every per-stage counter reported success. `0` turns all three off.
          
          Recall is reserved against but never clipped — it is replayed byte-for-byte into the cached prefix, so cutting it would bust the cache. See `injection_budget.rs`.
          
          [env: HEADROOM_MAX_INJECTION_BYTES=]
          [default: 32768]

      --memory <MEMORY_ENABLED>
          Memory system master switch. Default `false`.
          
          Source priority: CLI flag → `HEADROOM_MEMORY_ENABLED` env → default. Before this flag existed the switch was env-only, so a launcher that passed no environment left the whole subsystem off with nothing on the request path to say so.
          
          [env: HEADROOM_MEMORY_ENABLED=]
          [default: false]

      --cursor-agent-binary <CURSOR_AGENT_BINARY>
          Path to the `cursor-agent` CLI, for `MODEL=cursor:ID` routes.
          
          Defaults to `agent` on `PATH`. Worth setting explicitly when the proxy runs as a service: a service has the system `PATH`, not the login shell's, and the CLI installs into `~/.local/bin`.
          
          [env: HEADROOM_CURSOR_AGENT_BINARY=]
          [default: agent]

      --prefix-replay <PREFIX_REPLAY>
          Freeze-replay: replay the previously-forwarded (compressed) prefix byte-identical each turn so the provider prompt cache stays warm (ports Python `PrefixCacheTracker` / `overlay_cached_prefix`). Anthropic only. Default `false` — staged rollout; when off the request path is byte-for-byte unchanged
          
          [env: HEADROOM_PROXY_PREFIX_REPLAY=]
          [default: false]
          [possible values: true, false]

      --cache-tail-breakpoints <CACHE_TAIL_BREAKPOINTS>
          How many `cache_control` breakpoints to place on the tail of `messages`, counting back from the newest. Only read on the freeze-replay path, which owns message-level placement.
          
          `1` is the placement this proxy has always used and the default. `2` adds a hedge: when the newest message changes the older marker still names a prefix the provider holds, so the read starts there instead of at nothing. Published multipliers put two slots ~5% below one.
          
          Anthropic refuses more than 4 markers across `system`, `tools` and `messages` together. Claude Code sends 2 on `system`, so `2` here reaches that ceiling exactly — pair it with `--strip-system-cache-breakpoints`.
          
          [env: HEADROOM_PROXY_CACHE_TAIL_BREAKPOINTS=]
          [default: 1]

      --strip-system-cache-breakpoints <STRIP_SYSTEM_CACHE_BREAKPOINTS>
          Drop the `cache_control` markers the client set on `system`. Only read on the freeze-replay path, and only honoured once a message breakpoint is actually placed — with none placed these are the request's only markers and removing them turns caching off.
          
          A breakpoint caches everything before it and the system prompt precedes every message, so the tail marker already covers it. Claude Code's two ask for the 1h TTL, which writes at 2.0x against 5m's 1.25x. Default `false`: this reaches past `messages[*]` into what the client set, which nothing else here does.
          
          [env: HEADROOM_PROXY_STRIP_SYSTEM_CACHE_BREAKPOINTS=]
          [default: false]
          [possible values: true, false]

      --cache-stable-tool-order <CACHE_STABLE_TOOL_ORDER>
          B2: replay the tool order forwarded last turn and append genuinely-new tools at the end, so a late MCP handshake splicing definitions into the middle of `tools[]` does not invalidate the cached prefix behind them. Anthropic only. Lossless — the same definitions go out, byte for byte, in a different order. Default `true`; self-disables whenever a tool carries a `cache_control` marker, which on PAYG hands ordering back to PR-E1's alphabetic sort.
          
          Only runs when the proxy is already buffering the body (`compression` or any of the ctx flags). A pure-passthrough proxy never parses the request, so there is nothing to stabilize and the client's bytes go out untouched.
          
          [env: HEADROOM_PROXY_CACHE_STABLE_TOOL_ORDER=]
          [default: true]
          [possible values: true, false]

      --cache-pin-tool-roster <CACHE_PIN_TOOL_ROSTER>
          Pin each session's tool roster to every tool it has offered so far (B3). A tool the client drops for a turn is put back at its old position with the definition last seen for it; a new tool is appended. Keeps the cached prefix alive through Claude Code's `SendUserFile` / `WaitForMcpServers` flaps, which cost a full recache each. Off by default: if the client really removed a tool, the model may still call it. Declines whenever a tool carries a `cache_control` marker
          
          [env: HEADROOM_PROXY_CACHE_PIN_TOOL_ROSTER=]
          [default: false]
          [possible values: true, false]

      --force-1h-cache-ttl <FORCE_1H_CACHE_TTL>
          B1: rewrite every `cache_control` marker to `ttl: "1h"` so the cached prefix survives idle gaps past the 5-minute default. Anthropic only, and skipped on PAYG — a 1h write is priced at 2× base input against 1.25× for 5m, so it is free on a subscription (where writes are token-counted for the usage window) and 60% dearer in dollars on an API key.
          
          Default `false`. The 5-minute cache refreshes on every use at no cost, so this only ever rescues a gap since the last touch; whether that is worth a one-time cache creation per conversation depends on an idle pattern only the operator can see.
          
          [env: HEADROOM_PROXY_FORCE_1H_CACHE_TTL=]
          [default: false]
          [possible values: true, false]

      --cache-tail-breakpoint <CACHE_TAIL_BREAKPOINT>
          Move Claude Code's single message breakpoint onto the last content block when it sits short of it.
          
          Default `true`. A no-op on the 97% of captured requests where the client already placed it there, and worth -0.9% of the bill under API weights and -3.5% under subscription weights on the rest. See [`crate::cache_stabilization::message_breakpoints`].
          
          [env: HEADROOM_PROXY_CACHE_TAIL_BREAKPOINT=]
          [default: true]
          [possible values: true, false]

      --split-cache-ttl <SPLIT_CACHE_TTL>
          Split the cache TTL: 1h on the tools and system prefix, 5m on messages.
          
          Takes precedence over `--force-1h-cache-ttl`, which pins everything to the 2.0x tier including the moving message tail — content the next turn supersedes within seconds, which never needed an hour. See [`crate::cache_stabilization::cache_ttl::tail_5m_prefix_1h`].
          
          Default `false`, and measured at +511% depth-standardised creation on live traffic on 2026-08-17 — see [`crate::cache_stabilization::cache_ttl::tail_5m_prefix_1h`] for the numbers and the mechanism. Do not enable without a live A/B.
          
          [env: HEADROOM_PROXY_SPLIT_CACHE_TTL=]
          [default: false]
          [possible values: true, false]

      --replay-store-dir <REPLAY_STORE_DIR>
          Persist forwarded prefixes here so a proxy restart does not throw them away. Empty (the default) keeps them in memory only.
          
          A restart empties the replay store, and the miss that follows is not free: with no tracker, compression stops treating the history as frozen and rewrites it, so the bytes stop matching the prefix the provider still holds. Measured at 352,167 tokens over 7 turns — 10% of all failed re-use — every one within minutes of a proxy start. See [`crate::cache_stabilization::prefix_replay::SessionReplayStore::with_persistence`].
          
          [env: HEADROOM_PROXY_REPLAY_STORE_DIR=]
          [default: ""]

      --hold-working-directory <HOLD_WORKING_DIRECTORY>
          Hold the working-directory line in the `system` preamble to the value each conversation opened with, and state the live one at the message tail.
          
          The line sits inside every cached prefix and carries no marker of its own, so a `cd` re-creates the conversation from the system block down: one such edit cost 65,051 tokens against a depth-peer average of 4,637 in the 2026-08-17 capture. See [`crate::cache_stabilization::working_dir`], which also explains why the live directory is restated rather than dropped.
          
          Default `false`. It is the only mechanism here that adds text the client did not send, so it stays opt-in.
          
          [env: HEADROOM_PROXY_HOLD_WORKING_DIRECTORY=]
          [default: false]
          [possible values: true, false]

      --hold-role-sentence <HOLD_ROLE_SENTENCE>
          Hold the opening role sentence of the `system` preamble to the form each conversation opened with.
          
          Claude Code swaps between "helps users with software engineering tasks" and "helps users according to your \"Output Style\"" mid-session, on no operator action: four sessions flipped within 70 seconds of each other on 2026-09-07 and back nine minutes later. The sentence heads a 14,000-character block with no marker of its own, so each flip re-caches the conversation from the system block down: 788,210 tokens that day. See [`crate::cache_stabilization::role_sentence`].
          
          Default `false`. Rewrites text the client sent, so it stays opt-in.
          
          [env: HEADROOM_PROXY_HOLD_ROLE_SENTENCE=]
          [default: false]
          [possible values: true, false]

      --ctx-offload-min-bytes <CTX_OFFLOAD_MIN_BYTES>
          CTX-3: minimum serialized byte length a `tool_result` block must exceed to be offloaded. Static per invariant I3 (never changes mid-session). Default `50_000` (mirrors context-mode's Read threshold)
          
          [env: HEADROOM_PROXY_CTX_OFFLOAD_MIN_BYTES=]
          [default: 50000]

      --ctx-offload-stale-messages <CTX_OFFLOAD_STALE_MESSAGES>
          CTX-3: how many messages back from the tail a `tool_result` must be before `--exclude-tools` stops shielding it from offload. `0` (the default) shields the whole history, which is the behaviour before this flag existed.
          
          `--exclude-tools` keeps file and search results verbatim so the model never edits a file from a summary of it. Offload is not a summary — the original is retrievable by hash — and the risk it guards against is about the results in play, not the ones twenty messages back. Those were 9.9% of a mean prompt as raw `Read` output alone on 2026-08-17, never once digested.
          
          Distance from the tail grows, so this predicate turns true under blocks already inside the cached prefix. Safe only because a first conversion waits for a rebuild boundary and a converted block never reverts; see `CtxOffloadConfig::stale_margin`. Do not reimplement the test without both.
          
          [env: HEADROOM_PROXY_CTX_OFFLOAD_STALE_MESSAGES=]
          [default: 0]

      --ctx-offload-stale-window <CTX_OFFLOAD_STALE_WINDOW>
          CTX-3: how many messages past `--ctx-offload-stale-messages` a first conversion may happen on an ordinary turn rather than waiting for a rebuild boundary. `0` (the default) always waits.
          
          This is the one deliberate cache cost in the offload path. Converting a block inside the cached prefix rewrites everything after it, and at 1.45 for creation against 0.09 for reads the trade needs `16.1 * tokens_after / tokens_saved` turns to pay off. Deep in the history that is hundreds of turns; just past the margin it is ten, because the last 4 messages are only 1,460 tokens (median, 3,545 bodies at depth ≥ 20, 2026-08-17) while a qualifying block there saves 2,280.
          
          Keep it narrow. The last 8 messages are already 3,699 tokens, so widening this raises the cost faster than the saving.
          
          [env: HEADROOM_PROXY_CTX_OFFLOAD_STALE_WINDOW=]
          [default: 0]

      --ctx-offload-ttl-seconds <CTX_OFFLOAD_TTL_SECONDS>
          CTX-3: TTL (seconds) for offloaded originals in the CCR store. Long by design (retrieval outlives a session); default `604_800` (7 days)
          
          [env: HEADROOM_PROXY_CTX_OFFLOAD_TTL_SECONDS=]
          [default: 604800]

      --ctx-offload-tool-use <CTX_OFFLOAD_TOOL_USE>
          CTX-3: also offload large string values in prior-turn `tool_use` inputs (a Write's `content`, an Edit's `new_string`). Only effective with `--ctx-offload`. First conversions happen only where the block has never been sent upstream, so a cached prefix is never rewritten
          
          [env: HEADROOM_PROXY_CTX_OFFLOAD_TOOL_USE=]
          [default: false]
          [possible values: true, false]

      --ctx-drop-prior-thinking <CTX_DROP_PRIOR_THINKING>
          Drop `thinking` blocks from every assistant message but the last, on rebuild boundaries and history arrivals only — the turns where the prefix is written fresh anyway. The replay store keeps the stripped bytes on every steady turn after, so no cached prefix is ever rewritten. The last assistant message is never touched: its thinking must stay while a tool loop is open
          
          [env: HEADROOM_PROXY_CTX_DROP_PRIOR_THINKING=]
          [default: true]
          [possible values: true, false]

      --ctx-inject <CTX_INJECT>
          CTX-4: enable recall/resume injection. On the first request of a conversation, prepends a recall block (fresh) or resume snapshot (compaction/resume) into the first user message, then replays the same bytes verbatim on every later turn (invariant I4). Requires `ctx_capture` (the identity/sessions layer) — enforced loudly at startup. Default `false`.
          
          Source priority: CLI flag → `HEADROOM_PROXY_CTX_INJECT` env → default.
          
          [env: HEADROOM_PROXY_CTX_INJECT=]
          [default: false]
          [possible values: true, false]

      --ccr-context-tracking <CCR_CONTEXT_TRACKING>
          CCR Phase 4: track compressed/offloaded content across turns so later user queries can proactively retrieve relevant originals. Requires `--ctx-offload`; when offload is unavailable this is a no-op
          
          [env: HEADROOM_PROXY_CCR_CONTEXT_TRACKING=]
          [default: true]
          [possible values: true, false]

      --ccr-proactive-expansion <CCR_PROACTIVE_EXPANSION>
          CCR Phase 4: proactively append relevant previously-offloaded content to the latest user turn before forwarding the request upstream
          
          [env: HEADROOM_PROXY_CCR_PROACTIVE_EXPANSION=]
          [default: true]
          [possible values: true, false]

      --ccr-max-proactive-expansions <CCR_MAX_PROACTIVE_EXPANSIONS>
          CCR Phase 4: maximum proactive expansions appended to one request
          
          [env: HEADROOM_PROXY_CCR_MAX_PROACTIVE_EXPANSIONS=]
          [default: 2]

      --bedrock-region <BEDROCK_REGION>
          AWS region to use when signing Bedrock requests. Default `us-east-1`. The Bedrock endpoint URL derived from this region is `https://bedrock-runtime.{region}.amazonaws.com` (override via `--bedrock-endpoint` for FIPS or VPC endpoints).
          
          Source priority: CLI flag → `HEADROOM_PROXY_BEDROCK_REGION` env var → `AWS_REGION` env var → default (`us-east-1`).
          
          [env: HEADROOM_PROXY_BEDROCK_REGION=]
          [default: us-east-1]

      --bedrock-endpoint <BEDROCK_ENDPOINT>
          Bedrock endpoint base URL. When unset (the common case), the proxy derives `https://bedrock-runtime.{bedrock_region}.amazonaws.com` from the configured region. Override for FIPS endpoints (`bedrock-runtime-fips.{region}.amazonaws.com`), VPC endpoints, or local-mock test setups.
          
          Source priority: CLI flag → `HEADROOM_PROXY_BEDROCK_ENDPOINT` env var → derived-from-region.
          
          [env: HEADROOM_PROXY_BEDROCK_ENDPOINT=]

      --aws-profile <AWS_PROFILE>
          AWS profile name passed to the `aws-config` default credential chain. When unset, the chain uses the default behaviour (env vars → `[default]` profile → IMDS / ECS task role).
          
          Source priority: CLI flag → `HEADROOM_PROXY_AWS_PROFILE` env var → `AWS_PROFILE` env var → default chain.
          
          [env: HEADROOM_PROXY_AWS_PROFILE=]

      --bedrock-validate-eventstream-crc <BEDROCK_VALIDATE_EVENTSTREAM_CRC>
          Phase D PR-D2: validate the prelude + message CRC32 on each inbound Bedrock EventStream frame. Default `true` — production MUST validate. Operators flip to `false` ONLY for debugging a suspected wire-format issue (e.g. a corrupt-but-cooperative upstream that emits invalid CRCs intentionally). When disabled, the proxy still parses message boundaries; it just doesn't reject on CRC mismatch. Per project policy, every flag flip is logged at app-build time.
          
          Source priority: CLI flag → `HEADROOM_PROXY_BEDROCK_VALIDATE_EVENTSTREAM_CRC` env var → default (`true`).
          
          [env: HEADROOM_PROXY_BEDROCK_VALIDATE_EVENTSTREAM_CRC=]
          [default: true]
          [possible values: true, false]

      --vertex-region <VERTEX_REGION>
          Phase D PR-D4: GCP Vertex region for the publisher path (`{region}-aiplatform.googleapis.com`). Default `us-central1` (matches the GCP-published default region for Anthropic publisher models). The proxy does NOT auto-construct the regional URL — that's an `--upstream` decision the operator makes once at startup. This flag is exposed for structured logging + observability so dashboards can group Vertex traffic by region without parsing the upstream URL.
          
          Source priority: CLI flag → `HEADROOM_PROXY_VERTEX_REGION` env var → default (`us-central1`).
          
          [env: HEADROOM_PROXY_VERTEX_REGION=]
          [default: us-central1]

      --vertex-adc-scope <VERTEX_ADC_SCOPE>
          Phase D PR-D4: OAuth scope to request from GCP ADC. Defaults to `cloud-platform`, the broad scope `gcloud` itself uses for ADC. Operators with tighter IAM postures can scope down to `cloud-platform.read-only` etc., but Vertex `:rawPredict` requires write so most deployments use the default.
          
          Source priority: CLI flag → `HEADROOM_PROXY_VERTEX_ADC_SCOPE` env var → default (`cloud-platform`).
          
          [env: HEADROOM_PROXY_VERTEX_ADC_SCOPE=]
          [default: https://www.googleapis.com/auth/cloud-platform]

      --local-model <LOCAL_MODEL>
          Route requests for a local model through a local upstream with Anthropic↔OpenAI format translation. When set, any `/v1/messages` request whose `model` field matches this value is translated to OpenAI Chat Completions format and forwarded to `--local-upstream`. All other requests pass through transparently.
          
          Source priority: CLI flag → `HEADROOM_PROXY_LOCAL_MODEL` env var → default (None = disabled).
          
          [env: HEADROOM_PROXY_LOCAL_MODEL=]

      --sidecar-model <SIDECAR_MODEL>
          Model that answers Claude Code's spinner-text sidecar.
          
          The client asks for the line beside its spinner ("Reading runAgent.ts") by resending the whole conversation on the working model. The proxy answers that request on its own: a few tail messages, no tools, 64 output tokens, and this model. Point it at a larger model only if the summaries read badly; there is no reason to.
          
          Source priority: CLI flag -> `HEADROOM_PROXY_SIDECAR_MODEL` env var -> default (`claude-haiku-4-5-20251001`).
          
          [env: HEADROOM_PROXY_SIDECAR_MODEL=]

      --sidecar-route-timeout <SIDECAR_ROUTE_TIMEOUT>
          Bound on one spinner-sidecar attempt against a routed Responses upstream. The routed sidecar never retries: on timeout (or any other failure) it falls back to the direct sidecar path, so this is the longest a free-tier detour may hold a status line before Haiku answers it instead
          
          [env: HEADROOM_PROXY_SIDECAR_ROUTE_TIMEOUT=]
          [default: 15s]

      --local-upstream <LOCAL_UPSTREAM>
          Upstream URL for the local model (e.g. http://localhost:8080). Required when `--local-model` is set; the proxy appends `/v1/chat/completions` to this base. Ignored when `--local-model` is unset.
          
          Source priority: CLI flag → `HEADROOM_PROXY_LOCAL_UPSTREAM` env var → default (None).
          
          [env: HEADROOM_PROXY_LOCAL_UPSTREAM=]

      --extra-model-route <EXTRA_MODEL_ROUTES>
          Additional model routes. Each value is `MODEL_NAME=UPSTREAM_URL[:openai[:TARGET_MODEL_ID]][:auth=ENV_VAR]`, where `:openai` (or its older spelling `:translate`) means "translate Anthropic format to OpenAI format". Model names ending in `*` are prefix-matched. Can be given multiple times or as a comma-separated list in `HEADROOM_PROXY_EXTRA_MODEL_ROUTES`.
          
          `TARGET_MODEL_ID` does two things. It overrides the model id sent upstream, so `MODEL_NAME` can be a discoverable `claude-*` id (Claude Code's gateway discovery only lists ids starting with `claude` or `anthropic`) while the real upstream id differs. It ALSO selects the endpoint: with a target the request goes to the agentic Responses API (`/v1/responses`, or the ChatGPT Codex backend under Codex auth); without one it goes to `/v1/chat/completions`. Codex needs the former.
          
          `:auth=ENV_VAR` names the environment variable holding this route's bearer token — the name, so nothing written here is a secret. It also declares the route is not Codex-bound: without it, a route inherits whatever `--codex-auth-file` set up, which would send a live ChatGPT token and a `codex_cli_rs` originator to whatever host the URL names. The reserved name `:auth=none` (exact lowercase) declares a public anonymous upstream instead: no Authorization header is sent at all, and neither the Codex headers nor the caller's credentials are forwarded.
          
          Examples: --extra-model-route "claude-grok-4.6=cursor:cursor-grok-4.6-high" --extra-model-route "codex-*=https://api.openai.com/v1:openai" --extra-model-route "claude-codex-terra=https://api.openai.com/v1:openai:gpt-5.6-terra" --extra-model-route "claude-grok-4.6=https://api.x.ai/v1:openai:grok-4.6:auth=XAI_API_KEY" --extra-model-route "claude-muse-spark-1.3=https://opencode.ai/zen/v1:openai:muse-spark-1.3-contributor-free:auth=none"
          
          [env: HEADROOM_PROXY_EXTRA_MODEL_ROUTES=]

      --codex-auth-file <CODEX_AUTH_FILE>
          Path to a Codex auth JSON file (default: ~/.codex/auth.json). When set and the file exists, the proxy reads the access_token and uses it as a Bearer token for OpenAI upstream requests. The token is re-read from disk on each request so Codex's refresh cycle is picked up without proxy restarts.
          
          Source priority: CLI flag → `HEADROOM_PROXY_CODEX_AUTH_FILE` env var → default (`~/.codex/auth.json` if it exists, None otherwise).
          
          [env: HEADROOM_PROXY_CODEX_AUTH_FILE=]

      --mode <MODE>
          Proxy run mode: "token" (prioritize compression) or "cache" (prioritize provider prefix cache stability). Aliases like "token_headroom", "cost_savings" are normalized automatically
          
          [env: HEADROOM_MODE=]
          [default: token]

      --output-shaper
          Master switch for output-token shaping. When enabled, the proxy appends verbosity steering to system prompts
          
          [env: HEADROOM_OUTPUT_SHAPER=]

      --verbosity-level <VERBOSITY_LEVEL>
          Verbosity steering level 0-4. 0 = off, 1 = skip preamble, 2 = default, 3 = conclusions only, 4 = minimum tokens
          
          [env: HEADROOM_VERBOSITY_LEVEL=]
          [default: 2]

      --cache <CACHE_ENABLED>
          Enable semantic response caching. When `true`, identical non-streaming requests are served from an in-memory LRU cache instead of hitting upstream. Cache keys hash `{model, messages, system, tools, ...}` with `cache_control` annotations stripped so moved breakpoints don't fragment keys.
          
          Source priority: CLI flag → `HEADROOM_PROXY_CACHE_ENABLED` env var → default (`true`).
          
          [env: HEADROOM_PROXY_CACHE_ENABLED=]
          [default: true]
          [possible values: true, false]

      --cache-ttl <CACHE_TTL_SECONDS>
          TTL for cached responses in seconds. Entries older than this are evicted on access
          
          [env: HEADROOM_PROXY_CACHE_TTL=]
          [default: 3600]

      --cache-max-entries <CACHE_MAX_ENTRIES>
          Maximum number of entries in the semantic response cache. Older entries are evicted LRU when the limit is reached
          
          [env: HEADROOM_PROXY_CACHE_MAX_ENTRIES=]
          [default: 1000]

      --ccr-inject-tool <CCR_INJECT_TOOL>
          CCR: inject the `headroom_retrieve` tool definition into outgoing LLM requests. Default `true`
          
          [env: HEADROOM_CCR_INJECT_TOOL=]
          [default: true]
          [possible values: true, false]

      --ccr-handle-responses <CCR_HANDLE_RESPONSES>
          CCR: auto-handle `headroom_retrieve` tool calls server-side (retrieve context and continue the conversation). Default `true`
          
          [env: HEADROOM_CCR_HANDLE_RESPONSES=]
          [default: true]
          [possible values: true, false]

      --ccr-max-retrieval-rounds <CCR_MAX_RETRIEVAL_ROUNDS>
          CCR: max rounds of retrieve-continue per response. Default `8`
          
          [env: HEADROOM_CCR_MAX_RETRIEVAL_ROUNDS=]
          [default: 8]

      --retry <RETRY_ENABLED>
          Retry upstream requests on transient errors (429, 5xx). Default `true`
          
          [env: HEADROOM_RETRY_ENABLED=]
          [default: true]
          [possible values: true, false]

      --retry-max-attempts <RETRY_MAX_ATTEMPTS>
          Max retry attempts per upstream call. Default `3`
          
          [env: HEADROOM_RETRY_MAX_ATTEMPTS=]
          [default: 3]

      --retry-overload-max-attempts <RETRY_OVERLOAD_MAX_ATTEMPTS>
          Attempts for a 200 response whose SSE body opens with an error event. Default `6`.
          
          Anthropic reports overload inside the body when the client asked for a stream, and those outages run far longer than a transport blip: across 77 turns the proxy gave up on, the bursts lasted 27 to 245 seconds. At three attempts the loop waits about 3 seconds and clears 21% of them. Six waits about 31 seconds and clears 69%, which is where the curve bends — a seventh attempt doubles the wait for five more points.
          
          Retrying here is free of duplication risk: the error is the *first* event, so nothing has been forwarded and the bytes are still ours.
          
          [env: HEADROOM_RETRY_OVERLOAD_MAX_ATTEMPTS=]
          [default: 6]

      --retry-stream-hold-bytes <RETRY_STREAM_HOLD_BYTES>
          Bytes of a streamed response held back before it counts as committed. Default `2048`. `0` disables the mid-stream retry.
          
          A body that dies mid-stream cannot be re-sent blind: across 18 observed drops the parser had a content block open every time, 1 to 20 output tokens in, so a second attempt would splice two generations together. Holding the opening bytes keeps the response uncommitted long enough to start over instead.
          
          The trade is time to first paint: this much of every stream arrives in one burst rather than token by token. Raise it to cover later drops, lower it to hand the first token over sooner.
          
          2 KiB was the first figure and it was too small — a drop at 2279 bytes truncated a turn, and the observed drop points run 641, 1129, 2122, 2279. The raise to 8 KiB was recorded in `docs/tls-record-corruption-wsl2.md` but only ever passed on the command line, so it reverted the moment the proxy was started without the flag — which is how it was back at 2048 on 2026-08-26 while the same corruption was live. The default is the fix; a figure that has to be remembered is not one.
          
          [env: HEADROOM_RETRY_STREAM_HOLD_BYTES=]
          [default: 8192]

      --retry-base-delay-ms <RETRY_BASE_DELAY_MS>
          Base delay for exponential backoff in milliseconds. Default `1000`
          
          [env: HEADROOM_RETRY_BASE_DELAY_MS=]
          [default: 1000]

      --retry-max-delay-ms <RETRY_MAX_DELAY_MS>
          Ceiling on backoff delay in milliseconds. Default `30000`
          
          [env: HEADROOM_RETRY_MAX_DELAY_MS=]
          [default: 30000]

      --cost-tracking <COST_TRACKING_ENABLED>
          Enable cost tracking for upstream requests. Default `true`
          
          [env: HEADROOM_COST_TRACKING_ENABLED=]
          [default: true]
          [possible values: true, false]

      --budget-limit-usd <BUDGET_LIMIT_USD>
          Budget limit in USD. `None` (no flag) = unlimited
          
          [env: HEADROOM_BUDGET_LIMIT_USD=]

      --budget-period <BUDGET_PERIOD>
          Budget aggregation period: "hourly", "daily", or "monthly". Default "daily"
          
          [env: HEADROOM_BUDGET_PERIOD=]
          [default: daily]

      --min-tokens-to-crush <MIN_TOKENS_TO_CRUSH>
          Minimum token count before a message is eligible for compression
          
          [env: HEADROOM_MIN_TOKENS_TO_CRUSH=]
          [default: 200]

      --max-items-after-crush <MAX_ITEMS_AFTER_CRUSH>
          Max items to retain after SmartCrusher processing
          
          [env: HEADROOM_MAX_ITEMS_AFTER_CRUSH=]
          [default: 15]

      --savings-profile <SAVINGS_PROFILE>
          Compression savings profile name
          
          [env: HEADROOM_SAVINGS_PROFILE=]
          [default: balanced]

      --target-ratio <TARGET_RATIO>
          Target compression ratio (0.0 = auto)
          
          [env: HEADROOM_TARGET_RATIO=]
          [default: 0]

      --code-aware <CODE_AWARE_ENABLED>
          Enable code-aware compressor for source code
          
          [env: HEADROOM_CODE_AWARE_ENABLED=]
          [default: false]
          [possible values: true, false]

      --disable-kompress <DISABLE_KOMPRESS>
          Disable the Kompress ML compressor entirely
          
          [env: HEADROOM_DISABLE_KOMPRESS=]
          [default: true]
          [possible values: true, false]

      --disable-kompress-fallback <DISABLE_KOMPRESS_FALLBACK>
          When Kompress is disabled, route fall-through to passthrough
          
          [env: HEADROOM_DISABLE_KOMPRESS_FALLBACK=]
          [default: true]
          [possible values: true, false]

      --disable-kompress-anthropic <DISABLE_KOMPRESS_ANTHROPIC>
          Disable Kompress for Anthropic provider only
          
          [env: HEADROOM_DISABLE_KOMPRESS_ANTHROPIC=]
          [default: false]
          [possible values: true, false]

      --disable-kompress-openai <DISABLE_KOMPRESS_OPENAI>
          Disable Kompress for OpenAI provider only
          
          [env: HEADROOM_DISABLE_KOMPRESS_OPENAI=]
          [default: false]
          [possible values: true, false]

      --force-kompress-all <FORCE_KOMPRESS_ALL>
          Force all compressible content through Kompress
          
          [env: HEADROOM_FORCE_KOMPRESS_ALL=]
          [default: false]
          [possible values: true, false]

      --image-optimize <IMAGE_OPTIMIZE>
          Enable image token optimization
          
          [env: HEADROOM_IMAGE_OPTIMIZE=]
          [default: true]
          [possible values: true, false]

      --smart-crusher-compaction <SMART_CRUSHER_WITH_COMPACTION>
          Enable SmartCrusher compaction step
          
          [env: HEADROOM_SMART_CRUSHER_WITH_COMPACTION=]
          [default: true]
          [possible values: true, false]

      --compress-user-messages <COMPRESS_USER_MESSAGES>
          Gate compression of user-role messages.
          
          **Not wired. Setting this changes nothing.** Verified 2026-08-17: the field is read only by the `agent-savings` CLI subcommand, never on a serving path. `content_router::Config` carries a field of the same name that only a `SavingsProfile` ever writes and nothing reads, and `live_zone::DispatchConfig` declares one that is neither read nor written. `skip_user_messages`, which the doc comments say this overrides, is itself only declared and defaulted. See [[features-on-but-inert]].
          
          [env: HEADROOM_COMPRESS_USER_MESSAGES=]
          [default: true]
          [possible values: true, false]

      --compress-system-messages <COMPRESS_SYSTEM_MESSAGES>
          Gate compression of system-role messages.
          
          **Not wired. Setting this changes nothing.** Same three dead layers as `compress_user_messages` above.
          
          [env: HEADROOM_COMPRESS_SYSTEM_MESSAGES=]
          [default: true]
          [possible values: true, false]

      --protect-recent <PROTECT_RECENT>
          Protect recent reads from compression
          
          [env: HEADROOM_PROTECT_RECENT=]
          [default: false]
          [possible values: true, false]

      --protect-analysis-context <PROTECT_ANALYSIS_CONTEXT>
          Protect analysis context from compression
          
          [env: HEADROOM_PROTECT_ANALYSIS_CONTEXT=]
          [default: false]
          [possible values: true, false]

      --accuracy-guard <ACCURACY_GUARD>
          Accuracy guard string for compression safety
          
          [env: HEADROOM_ACCURACY_GUARD=]
          [default: ""]

      --lossless <LOSSLESS>
          Enable lossless-only compression mode
          
          [env: HEADROOM_LOSSLESS=]
          [default: false]
          [possible values: true, false]

      --ccr-inject-marker <CCR_INJECT_MARKER>
          CCR: inject retrieval markers into compressed output
          
          [env: HEADROOM_CCR_INJECT_MARKER=]
          [default: true]
          [possible values: true, false]

      --exclude-tools <EXCLUDE_TOOLS>
          Comma-separated tool names to exclude from compression.
          
          Defaults to Python's `DEFAULT_EXCLUDE_TOOLS`: file and search results are what the model is most likely to need verbatim, and the `all_messages` path compresses without storing an original to retrieve, so a summarized file read cannot be undone. Pass `--exclude-tools ""` to compress them anyway.
          
          [env: HEADROOM_EXCLUDE_TOOLS=]
          [default: Read,Glob,Grep,Write,Edit,WebSearch,WebFetch,view,read_file,Skill,headroom_retrieve]

      --protect-tool-results <PROTECT_TOOL_RESULTS>
          Comma-separated tool names whose results must not be lossy-compressed
          
          [env: HEADROOM_PROTECT_TOOL_RESULTS=]
          [default: ""]

      --read-lifecycle <READ_LIFECYCLE>
          Enable read lifecycle tracking
          
          [env: HEADROOM_READ_LIFECYCLE=]
          [default: false]
          [possible values: true, false]

      --read-maturation <READ_MATURATION>
          Enable read maturation (hold fresh reads out of prefix cache)
          
          [env: HEADROOM_READ_MATURATION=]
          [default: false]
          [possible values: true, false]

      --stateless <STATELESS>
          Stateless mode: disable filesystem writes
          
          [env: HEADROOM_STATELESS=]
          [default: false]
          [possible values: true, false]

      --proxy-token <PROXY_TOKEN>
          Proxy auth token for inbound requests
          
          [env: HEADROOM_PROXY_TOKEN=]

      --offline <OFFLINE>
          Offline mode: disable all outbound network egress
          
          [env: HEADROOM_OFFLINE=]
          [default: false]
          [possible values: true, false]

      --anthropic-pre-upstream-concurrency <ANTHROPIC_PRE_UPSTREAM_CONCURRENCY>
          Pre-upstream concurrency limit (semaphore)
          
          [env: HEADROOM_ANTHROPIC_PRE_UPSTREAM_CONCURRENCY=]
          [default: 1000]

      --compression-max-workers <COMPRESSION_MAX_WORKERS>
          Compression worker threadpool size
          
          [env: HEADROOM_COMPRESSION_MAX_WORKERS=]
          [default: 4]

      --foundry-base-url <FOUNDRY_BASE_URL>
          Azure AI Foundry upstream base URL for `/anthropic/v1/messages` requests (e.g. `https://{resource}.services.ai.azure.com/anthropic`). Matches the Python proxy's `ANTHROPIC_FOUNDRY_BASE_URL` handling (`headroom/providers/registry.py::resolve_api_overrides`). When unset, falls back to derivation from `--foundry-resource`, then to `--upstream`.
          
          Source priority: CLI flag → `ANTHROPIC_FOUNDRY_BASE_URL` env var → derived from `--foundry-resource` → none.
          
          [env: ANTHROPIC_FOUNDRY_BASE_URL=]

      --foundry-resource <FOUNDRY_RESOURCE>
          Azure AI Foundry resource name. When `--foundry-base-url` is unset, the Foundry upstream is derived as `https://{resource}.services.ai.azure.com/anthropic` — the same derivation Claude Code performs internally and that the Python side implements in `headroom/cli/wrap.py::_foundry_upstream_url`.
          
          Source priority: CLI flag → `ANTHROPIC_FOUNDRY_RESOURCE` env var → none.
          
          [env: ANTHROPIC_FOUNDRY_RESOURCE=]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

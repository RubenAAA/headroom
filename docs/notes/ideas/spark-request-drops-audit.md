# Idea: audit what the Spark path drops and what it fails to pass

- **Status:** open. Audit of 2026-09-30, partly unverified. Only one fix came
  out of it: mid-conversation `system` messages (`4eab6be1`, `ce3b82db`).
- **Source:** `openai/request.rs`, `routed/prepare.rs`, `openai/stream.rs`,
  `routed/translation.rs`. Data: 400 recent replay-prefix captures and the
  2026-09-29 log.
- **Dropped, by design (question each):**
  - ~~Prior turns' thinking~~ **Corrected twice 2026-09-30.** Not dropped by
    `prior_thinking_dropped`: that runs only on the Anthropic endpoint path
    (`proxy/forward/ctx.rs`), so its 42 events are not Spark turns. But the
    encrypted blob is dropped on Zen. `openai/request.rs` turns a `thinking`
    block carrying our signature into a `reasoning` item, then
    `translation.rs` calls `UpstreamKind::strip_unreplayable_reasoning`, which
    removes `id`, `encrypted_content` and `include` for `OpenCodeZen` (the
    summary stays). Reason: Zen binds the blob to the caller and a VPN exit
    rotation made it 400 "not issued to this caller" (see
    `learnings/zen-reasoning-blob-vs-exit-rotation.md`). The lane test earlier
    today ("200 with the blob valid, corrupted or absent") ran through the live
    proxy, so the proxy stripped the blob before Zen saw it in every arm. It
    shows nothing about Zen validating blobs, and the "recall your reasoning"
    probes never fed the model a blob. Unmeasured: whether Zen accepts a blob
    from another exit today, and whether a replayed blob helps the model.
  - `temperature`, `top_k` (not on the Responses shape); `parallel_tool_calls`
    (removed to match OpenCode); the output ceiling (lifted).
  - Tool schema annotation keys only (`title`, `examples`, ...); tool pruning
    is off in the flags.
- **Dropped, silently, unproven impact:**
  - Assistant `server_tool_use`, `tool_search_tool_result`, `redacted_thinking`
    (15 in 400 conversations).
  - Top-level user `image` and `document` blocks (none seen in 400
    conversations). Tool-result images survive for supported media types.
  - Zen stream events the translator does not handle. They were logged at
    debug, so invisible. Now info, once per name then every 500th
    (`codex_unhandled_stream_event`, with `count`). Zen refuses calls that are
    not from OpenCode, so the raw stream could not be sampled by hand.
    **Checked 2026-09-30:** the unhandled names are lifecycle and end markers
    only (`response.created`, `.in_progress`, `.reasoning_summary_text.done`,
    `.reasoning_summary_part.done`, `.content_part.added`/`.done`). Their
    payloads, and the first event of every other kind, now log once per
    proxy start as `responses_stream_event_shape` (strings cut to 40
    characters, arrays to one element). 18 kinds seen at 10:44Z: content
    parts carry empty `annotations` and `logprobs`; no other fields beyond
    what the translator reads, except `output_tokens_details` (reasoning
    tokens) in `response.completed.usage`, which is not forwarded.
- **Answered:** `tool_search_deferral` (3,034 events) does not apply to Spark.
  Its only callers are `proxy/forward/presend.rs` and
  `proxy/request_transforms.rs`; nothing under `routed/` calls it (grep
  2026-09-30). Spark gets every tool schema in full, so the 15
  `tool_search_tool_result` blocks it loses are Claude-side bookkeeping.
- **Unknown:** how much live-zone compression on Spark helps or hurts. It
  rewrote 25 of 1,169 messages in one sample, saves 2.6% of tokens, and costs
  time (see `spark-proxy-overhead-on-free-path.md`). Rewrites can also move
  cached bytes.
- **Added, not dropped:** `routed/tool_alias.rs` lowercases tool names and, on
  tool-poor turns, adds marked shadow copies of `bash`/`edit`/`glob`/`grep`/
  `read` so Zen's free-tier gate passes. A shadow call with no matching client
  tool reaches the client as an unknown tool. Not measured: how often a Spark
  turn calls one, or whether the extra definitions change what it picks.
- **Not passed:** the input token count (see `spark-usage-and-compaction.md`).
- **Body diff against OpenCode, 2026-09-30.** OpenCode 1.18.33 ran one turn
  ("say hi", model `muse-spark-1.3-contributor-free`) against a local capture
  server (`OPENCODE_CONFIG_CONTENT` pointing the `opencode` provider's
  `baseURL` at it, so nothing reached Zen). A scratch proxy, capture on
  (`HEADROOM_CAPTURE_DIR`), forwarded one Claude-Code-shaped turn. Top-level
  fields:

  | Field | OpenCode | Proxy |
  |---|---|---|
  | `max_output_tokens` | 32000 | absent (lifted on purpose) |
  | `include` | `["reasoning.encrypted_content"]` | absent (blob strip) |
  | `reasoning` | absent | `{effort, summary: "auto"}` |
  | `stream_options` | absent | `{reasoning_summary_delivery: "sequential_cutoff"}` |
  | `parallel_tool_calls` | absent | absent |
  | `store`, `tool_choice`, `stream`, `prompt_cache_key` | same | same |

  Tools have the same five keys (`type`, `name`, `description`, `parameters`,
  `strict: false`). The system prompt is a first `developer` item in both;
  OpenCode's items carry no `type: "message"`, the proxy's do. The tool sets
  differ by construction (OpenCode's 20 against the client's plus shadows).
  Findings: every gap traces to a deliberate change, and none is a leak. One
  claim is weaker than the code comment says: `refuted-review-findings-2026-09.md`
  calls `stream_options.sequential_cutoff` "the documented Zen requirement",
  yet OpenCode's own request to the same model sends neither it nor `reasoning`
  and gets served. What the field does to Spark's output is unmeasured; it is
  cheap to test (one turn with and one without) if Spark's reasoning stream
  ever looks wrong. Header differences were not part of this diff.
- **Field test of those four fields, 2026-09-30.** Four scratch proxies
  against real Zen, one arm each: as shipped, no `stream_options`, no
  `reasoning`, `max_output_tokens: 32000` (OpenCode's value). Three runs per
  prompt per arm, a reasoning puzzle and a one-tool task; medians of 3, so only
  a large gap would show.
  - `stream_options`: no change in TTFB (4.9 s against 5.2 s), tokens (353
    against 356) or stop reasons. Thinking deltas reached the client in almost
    no run, so the summary delivery mode has nothing visible to change. Keep it
    or drop it; nothing measured favours either.
  - `max_output_tokens: 32000`: no change (TTFB 5.6 s, 451 output tokens,
    same stops). It would bound a runaway turn at OpenCode's own ceiling and
    could cut a long file write. Left out: no runaway was seen.
  - `reasoning`: the only field with an effect. Without it (Zen's default
    `high`) the puzzle took 4.1 s and 247 output tokens; with the proxy's
    `xhigh` default, 5.2 s and 356. That is about a fifth faster and 30% fewer
    tokens on a simple prompt. Answer quality was not scored, so whether
    `xhigh` earns its cost on hard turns is open. It applies only when the
    client sends no effort; a `/effort` setting passes through.
  - `include`: needed for blobs to be issued at all. With
    `--zen-reasoning-replay` on the proxy keeps it.
  - **Decision 2026-09-30:** effort is now pinned to `max` for every Zen turn
    (Spark is a weak model; the owner wants all of it spent). Probed values on
    Zen for Spark: `minimal`, `low`, `medium`, `high`, `xhigh`, `max` accepted;
    `none` refused ("Supported values: [minimal, low, medium, high, xhigh,
    max]"); an unknown name is a 400. Two runs each, sheep-and-multiplication
    prompt, all right: output tokens `minimal` 149-241, `low` 353-548,
    `medium` 561-842, `high` 863-2256, `xhigh` 502-746, `max` 1003-1314 in that
    probe; in a later six-run sample at `max`, 16-37 s and 870-2053 tokens. So
    `max` costs about 1.7 times the time of `xhigh` on a small prompt, with no
    visible gain there. Quality on hard turns was not scored.
  - **Shadow-tool calls, looked at 2026-09-30.** Not fixed; the obvious fix
    fails. Forbidding calls with `tool_choice: "none"` on tool-less turns gets a
    400 from Zen for every request: "only `\"auto\"` is supported for
    `tool_choice`" (12 of 12). So the gate's shadow tools stay callable.
    Measured cost: (a) a no-tool turn that invites computing ended in `tool_use`
    with no text in about 4 of 34 probe runs at `max`/`xhigh` (a sum-and-product
    prompt, so an upper bound; real tool-less requests, such as titles or
    summaries, were not sampled); (b) in 3 days of Claude Code transcripts, 29
    "No such tool available" errors for `bash`/`grep`/`glob` across 59 sessions
    with 7,592 tool calls (0.4% of all calls, not only Spark's), and the next
    call was a real tool in 64 of 69 error cases, so a turn with real tools
    loses one round trip, about 15-30 s at `max`. Reopen if tool-less Spark
    requests (`no-tools->spark`) start returning empty text; a fix would have to
    retry the turn when it ends in a shadow call with no client tool, which the
    streaming path makes hard after thinking has been sent.
  - Side finding: one of the 12 no-tool puzzle turns ended in `tool_use`. The
    model called one of the shadow tools the proxy adds for the Zen gate
    (`routed/tool_alias.rs`), which the client does not have.
- **Next:** read the info-level unhandled-event lines after a day of traffic.
  Done: the reasoning-replay lane test (see the correction above and
  `learnings/zen-reasoning-blob-vs-exit-rotation.md`) and the OpenCode body diff.

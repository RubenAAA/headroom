#!/usr/bin/env bash
# The proxy settings that were measured, in one place.
#
# Two things start the headroom proxy: `restart-headroom.sh` after a build, and
# `claude-launcher` when nothing is listening on 8787 — which is what happens
# after every reboot. Whichever starts it decides its flags for its whole life,
# because a proxy already on the port is reused and later flags are ignored.
# Keeping the list in two scripts meant a rebooted machine ran the launcher's
# copy, and the launcher's copy was short of three measured settings.
#
# Both scripts source this file and expand HEADROOM_FLAGS. Listen, upstream and
# the ctx/local-model flags stay with the callers, since those differ by path.
#
# Sourced, not executed. Anything here applies to a proxy that outlives the
# shell that started it, so change it and restart — editing this file alone
# changes nothing about the process already running.

# ON 2026-08-18. The five memory tools are injected into every request and the
# proxy answers the calls itself — Claude Code has never heard of memory_search
# and would reply "No such tool available", so the call must never reach it.
# The suppression is `proxy_owned_tool` in ccr_stream.rs, and its bool is now
# `ctx.memory.is_some()` at line 708. It used to be a hardcoded `false`, which
# is why this export sat at 0 from 2026-08-13: no tools injected, nothing to
# leak. tests/memory_continuation.rs drives the whole proxy over HTTP and proves
# the streamed call is intercepted, so the workaround is retired.
#
# Neither of these has a CLI flag; both have to be exported.
#
# TIMING: injecting the tools changes the tools array, which invalidates the
# cached prefix of EVERY live conversation at once — 239,277 creation tokens on
# a single turn when --prune-drop-tools last moved. Restart at a quiet moment.
export HEADROOM_MEMORY_INJECT_TOOLS=1

# `tool` = the model asks for memories when it wants them. `auto_tail`, the
# previous setting, appended recalled context to the user message on every turn
# whether it was wanted or not; a tool costs zero tokens until it is called.
export HEADROOM_MEMORY_MODE=tool

# Cost-aware auto-routing: tool-less turns go to the free tier instead of
# Claude. Deliberately no `max_input_tokens` bound to start: the router's
# size estimate counts system+tools (tens of thousands of tokens on real
# Claude Code traffic), so any bound needs a day of logs to calibrate.
# Watch `model routing decision` — the reason names the rule, input_tokens
# and has_tools per request — then add e.g. `"max_input_tokens": 30000.
# Expect a low initial fire rate (the client sends tools on most turns);
# that is the rule being conservative, not broken. Broaden later by
# dropping `require_no_tools` for small turns or adding a haiku rule.
# The target MUST be a route alias below; an unknown id would ride the
# default upstream and 404. Disable by unsetting the first line.
export HEADROOM_MODEL_ROUTER_ENABLED=0
export HEADROOM_MODEL_ROUTES='[{"name": "no-tools->spark", "require_no_tools": true, "to_model": "claude-muse-spark-1.3"}]'

HEADROOM_FLAGS=(
  # Master switch for the proxy's memory subsystem, default false. It lived on
  # the `cclaude` command line for weeks and never once took effect: a running
  # proxy ignores flags, so every reuse dropped it, and the live process on
  # 2026-08-11 had neither the flag nor HEADROOM_MEMORY_ENABLED set. Moved here
  # so both start paths carry it.
  --memory true

  # ON 2026-08-17. The replay store is in memory, so every restart of this proxy
  # threw away the forwarded prefixes and the first turn of each live
  # conversation rebuilt its history: 352,167 tokens over 7 turns, 10% of all
  # failed cache re-use, each landing within minutes of a proxy start
  # (`bench/_wastewhere.py`). With a directory the prefixes are reloaded instead.
  #
  # Costs a few MB per active conversation. Files are swept at each start once
  # they are over an hour old, which is when the provider has dropped the entry
  # they name. Safe to delete the directory at any time — the worst case is one
  # decline per conversation, which is the old behaviour.
  #
  # VERIFIED on the 12:42Z restart of 2026-08-17, the first one with files on
  # disk to read. 76 files present, 37 fresh; the sweep took the other 39. Three
  # prefixes rehydrated within 32 seconds (256, 288 and 142 messages) and there
  # were ZERO replay declines of any kind afterwards. The ten post-restart turns
  # that reached the ledger, depth 259-306, created a mean 1,120 tokens against a
  # 909 settled median for that depth — no restart penalty at all, where the
  # baseline for a 150-400 message conversation inside 300s of a start was 6,018.
  # Look for `prefix_replay_rehydrated` to confirm after any future restart.
  --replay-store-dir $HOME/.local/state/headroom/replay-prefixes

  --ctx-store-dir $HOME/.claude-personal/context-mode

  # 15000 -> 8000 on 2026-08-17. A block only offloads if the digest is smaller
  # in TOKENS, and the preview cut now scales with the block, so the floor is
  # what decides how much of the pool is reachable at all. Measured over 4,046
  # forwarded bodies: raw tool_result blocks are 22% of a 135,814-token mean
  # prompt, and the 4-15KB band holds more of them than everything above 15KB.
  # At 8,000 the reachable share of tool_result bytes goes from 27% to 37%
  # (+3.8% of the whole prompt); 4,000 reaches 45% and is the next step if the
  # previews turn out to read well enough.
  #
  # Cache risk: none for the blocks this newly reaches. A result is offloaded on
  # the turn it arrives, while it is still the last message and has never been
  # cached. Frozen history is a different matter and is gated separately below.
  #
  # The cost is readability, not tokens: an 8KB command output becomes a 2KB head
  # plus a `headroom_retrieve` pointer. Raise the floor if that starts to bite.
  #
  # Then 8000 -> 4000 the same day, once the ledger showed where the money is:
  # over 3,446 billed turns, cache READS are 62.8% of the bill and 88.8% of it on
  # prompts past 150k. The bill is prompt size, so bytes are the target, and this
  # band is reached for free — a result is digested on the turn it arrives, while
  # it is still the last message and has never been cached. 4,000 reaches 45% of
  # all tool_result bytes against 37% at 8,000, worth about 1.6% of the bill.
  #
  # 4000 -> 2000 on 2026-08-18. Bash-heavy traffic sits well under the old floor:
  # in one captured body 292 of 301 tool_result blocks were beneath it, 224 KB of
  # 283 KB unreachable whatever the gate decided. Benched over 7,674 turns with
  # `cachesim.py experiment --weights api`, floors against the client baseline:
  #
  #     4000  -2.4%      2000  -2.6%      1200  -2.0%      800  +0.5%
  #
  # It turns over exactly where the arithmetic says it must: the preview is a
  # quarter of the block clamped to a 600-byte floor, so below ~1,200 bytes a
  # digest stops shrinking anything and the footer starts costing more than the
  # block. 2,000 is the last floor where every reached block still pays.
  #
  # Read that -2.4% control before expecting much from this line. Those arms run
  # offload over what we ALREADY forwarded, so 4000 scoring -2.4% is savings we
  # are not taking today; moving the floor is worth about 0.2pp of it.
  #
  # The line below used to blame the PR-J4 boundary gate for the rest. Measured
  # 2026-08-18, that was wrong. Benching the policy with the boundary
  # requirement and again without it gives -11.6% either way — 0.01% apart —
  # because the near-tail window catches a block as it slides through distance
  # 4-7 and a first conversion almost never needs a boundary at all. So the
  # backlog the gate defers is small, and where the rest of that number lives is
  # still an open question.
  --ctx-offload-min-bytes 2000

  # ON 2026-08-17. --exclude-tools keeps Read/Grep/Glob results verbatim so the
  # model never edits a file from a summary of it. That argument is about the
  # results in play, and it was being applied to the entire history: raw Read
  # output was 9.9% of the mean prompt with not one block ever digested. Offload
  # is not a summary — the bytes come back from `headroom_retrieve`, which is
  # itself excluded — so past a few messages the protection costs more than it
  # buys. Offloading everything stale at a 4,000-byte floor is 11.4% of the
  # prompt on the same capture.
  #
  # HOW MUCH OF THAT ACTUALLY ARRIVES IS UNMEASURED, and the honest guess is a
  # fraction. Distance from the tail grows, so a block crosses this margin while
  # already inside the cached prefix, and converting it there rewrites the prefix
  # from that point. The code therefore holds the first conversion until the
  # drift detector reports a rebuild boundary — a turn already being rewritten.
  # Live, that is roughly 1 turn in 50, so the backlog converts in occasional
  # batches rather than steadily. Deliberate: converting a block at message 10 of
  # 300 on a quiet turn takes about 1.45 * tokens_after / (0.09 * tokens_saved)
  # turns to pay back, which is hundreds.
  #
  # Verify with `grep ctx_offload_accounting` — blocks_offloaded is this flag
  # working, blocks_deferred is it waiting.
  --ctx-offload-stale-messages 4

  # ON 2026-08-17, and it is what makes the flag above pay. Boundaries turned out
  # to be about 1 turn in 50, so waiting for one harvested nothing: 20 turns
  # after the restart that shipped it, 20 blocks deferred and 0 converted.
  #
  # A window opens the band just past the margin, where the rewrite is cheap
  # enough not to need a boundary. The arithmetic, from 3,545 bodies at depth
  # >= 20: creation bills 1.45 against 0.09 for reads, so a conversion pays back
  # after 16.1 * tokens_after / tokens_saved turns. The last 4 messages are 1,460
  # tokens and a qualifying block there saves 2,280 — a 10-turn payback, against
  # a median conversation of 15 turns and with 51-150-turn conversations holding
  # 58.7% of every cache read we pay for.
  #
  # KEEP IT NARROW. The last 8 messages are already 3,699 tokens, so widening
  # this raises the cost faster than the saving; at 20 messages back the payback
  # is longer than 98% of conversations ever live. 4 is the measured setting.
  #
  # This is the only setting here that spends cache on purpose. If depth-binned
  # creation per turn rises and stays risen, this is the first thing to switch
  # off — `bench/_ttlverdict.py` is the shape of that check.
  --ctx-offload-stale-window 4

  # KEPT ON, 2026-08-17, after nearly being switched off on a bad reading.
  # `bench/ab_replay.py` prices injection at 6.8 points of an 11.5% loss: turn
  # count goes 9.25 -> 10.25, and one run spent a turn on a `Bash` call the task
  # did not need, because the recalled text comes from earlier sessions in this
  # repo and those are full of shell work.
  #
  # That harness cannot see the other side of the trade. Its prompts are scripted,
  # so there is nothing for recall to save; every tool is disallowed, so a tool
  # call it induces is waste by construction rather than a shortcut that skipped a
  # file read. Injection is an upfront cost against later savings, and the A/B
  # measures the cost with the saving set to zero.
  #
  # So the 6.8pp is a ceiling on the harm, not an estimate of it. Judging this
  # needs real sessions: same task, same files, injection on and off, counting
  # tool calls to first useful edit. Until that exists, leave it on.
  --compression-mode all_messages
  --prefix-replay true

  # MEASURED AND KEPT, 2026-08-11. Reads 0.058 create/read against the
  # 12:02:29 control's 0.076 — but only once each conversation's first turn is
  # dropped. Those turns have no prefix to read and write the lot; counted in,
  # the run reads 0.095 against 0.085 and looks beaten. Compare on
  # `create / read, steady` from ~/headroom-savings.py, never the whole-run
  # line. The second marker is a hedge: a breakpoint caches everything before
  # it, so when the newest message changes the older marker still names a
  # prefix the provider holds.
  #
  # RE-EXAMINED 2026-08-17 and kept. `bench/cachesim.py` prices one marker 2.6%
  # better and is not to be believed here: it priced the split TTL below at -38%
  # and that cost +511% on live traffic. The post-mortem of that failure is an
  # argument FOR the second marker — what broke was the hedge itself, which only
  # pays while the older entry is alive. Live A/B says two, the simulator says
  # one, and the simulator has a demonstrated 5x error on the adjacent question.
  # Settle it with `bench/_ttlverdict.py`-style depth-binned ledger A/B if it
  # comes up again, not with the simulator.
  --cache-tail-breakpoints 2

  # TRIED AND REVERTED, 2026-08-11 12:30Z. The tail marker covers the system
  # prompt only on turns where it hits, and this client edits history, so it
  # misses often. The system markers were the floor under those misses: mean
  # creation per request went 11,118 -> 39,208 without them while the median
  # barely moved. Leave off unless the drift rate goes to near zero.
  --strip-system-cache-breakpoints false

  # Marker budget with the system markers kept: 2 on system + 2 on the message
  # tail is exactly Anthropic's cap of 4. Nothing in the code enforces that
  # sum, so do not add a third message slot without dropping something else.

  --enable-cross-turn-dedup

  # TRIED AND REVERTED, 2026-08-17. Splitting the TTL — 1h on the tools and
  # system prefix, the 5-minute tier on the message tail, the long TTL taken
  # back every tenth turn as an anchor — priced at -38% in `bench/cachesim.py`
  # and cost five times more in production. Depth-binned actual creation per
  # turn, from `bench/_ttlverdict.py` over 1,009 ledger-joined turns:
  #
  #     depth 0-20    1,994 ->  9,562     depth 50-100    1,601 -> 5,809
  #     depth 20-50     772 -> 13,092     depth 100-200   1,521 -> 9,837
  #
  # +511% standardised for depth, flat across 489 minutes, so not restart
  # warm-up. Creation VOLUME is what rose, which no weighting of the window can
  # explain away. The mechanism is the second tail breakpoint below: it is a
  # hedge that only pays while the older entry is still alive, and at five
  # minutes it is not, so a miss on the newest entry falls back to the system
  # boundary and rewrites the conversation instead of landing one marker back.
  #
  # Do not re-enable without an A/B on live traffic. The simulator cannot judge
  # this one: it models expiry from inter-turn gaps and read renewal, and both
  # were right in isolation while the interaction above was invisible to it.
  --split-cache-ttl false
  --force-1h-cache-ttl true

  # ON 2026-08-17. Holds the working-directory line in the system preamble to
  # the value each conversation opened with, and restates the live one at the
  # message tail where changing it costs nothing. The line sits inside every
  # cached prefix with no marker of its own, so a `cd` or a worktree change
  # re-creates the conversation from the system block down: one such edit cost
  # 65,051 tokens against a depth-peer average of 4,637 (`bench/_syscost.py`
  # over 1,035 ledger-joined turns).
  #
  # Fires only on a directory change — 1 turn in 1,035 of that capture — so a
  # week of traffic is needed before the effect is visible in the bill. Look for
  # `working_directory_held` in the log to confirm it is running at all.
  #
  # It is the only setting here that adds text the client did not send. The added
  # text is one stable sentence, and it depends on --prefix-replay to persist,
  # which the code enforces.
  --hold-working-directory true

  # `serena` was added here on 2026-08-17 and taken straight back out. All 29 of
  # its tools were sent in 13% of turns across a 4,361-body capture and none was
  # ever called, which looked like 1,073 tok/turn of dead weight.
  #
  # Wrong layer. MCP servers are already distributed per repo — the census shows
  # it plainly: serena and ai-lens in 13% of turns, codebase-memory in 54%,
  # aws-dataprocessing in 29%. A server reaching a request means someone enabled
  # it for that repo on purpose. Dropping it here overrides that decision in
  # every repo at once, silently, and the proxy is not where per-repo tooling
  # gets decided. Prune a server from its `.mcp.json` if it is not wanted there.
  # 32,768 -> 8,192 on 2026-08-17. The budget is per REQUEST while the appends it
  # governs are cumulative in the conversation: CCR proactive expansion adds up to
  # the whole budget to the newest user message, prefix replay keeps it there, and
  # the next turn adds another. Every turn individually respects the ceiling and
  # the conversation accumulates without one.
  #
  # Usually invisible — expansion reaches 15% of turns and averages 513 tok/turn,
  # 0.2% of the bill. The tail is not invisible: one conversation on 2026-08-17
  # carried two expansion blocks of 22,825 and 23,041 tokens and ran 6.8% LARGER
  # than what the client sent, 16,996 tokens a turn. On the same turn, offload had
  # just shrunk a tool_result by 2,835 tokens and expansion pasted 22,825 tokens
  # of previously-offloaded content back into the same message.
  #
  # 8,192 bounds the accumulation four times tighter. Recall is charged whole and
  # never clipped (it sits in messages[0], so clipping it would bust byte zero) —
  # it is about 1,519 bytes, so it still fits comfortably.
  --max-injection-bytes 8192

  # OFF 2026-08-17, on evidence that arrived by accident. It fired in the session
  # that was measuring it: a 4,849-byte tool result was offloaded to a digest plus
  # a retrieval pointer, and expansion then appended the WHOLE original back into
  # the next prompt, labelled "42 items compressed in turn 712, high relevance to
  # current query".
  #
  # So the prompt ends up carrying both the digest and the content it stands in
  # for. That is not a smaller saving, it is a larger prompt than never offloading
  # at all, and it is why one conversation ran 6.8% bigger than what the client
  # sent — two expansion blocks of 22,825 and 23,041 tokens.
  #
  # What it was for is real: content the model may need again without asking. That
  # is what `headroom_retrieve` does, on demand, paid only when it is used, and the
  # digest carries the pointer. Guessing costs the full price every turn forever.
  #
  # `--ccr-max-proactive-expansions 2` and the budget above bound it but cannot fix
  # it: the pathology is re-adding what was just removed, at any size.
  --ccr-proactive-expansion false

  --prune-drop-mcp claude-in-chrome,claude_ai_Canva,claude_ai_Gmail,claude_ai_Hugging_Face,mobbin,plugin_perplexity_perplexity,qwen,tmux,playwright-chromium,slack

  # These Claude Code built-ins were present in 64% of captured tool-block
  # splits and were never called. Drop exact names to keep the tool fingerprint
  # stable without affecting MCP tools or similarly named future tools.
  #
  # Extended 2026-08-17 from a per-tool census of 4,361 forwarded bodies (tool
  # definition bytes, weighted by how often each is sent, against tool_use calls
  # for the same name). The tools block is 17,253 tok/turn, 12.6% of the prompt,
  # and 34% of it had never been called once. It is also the cheapest thing here
  # to change: the array is byte-identical every turn and sits before `messages`,
  # so dropping an entry shrinks every future prompt without rewriting any
  # cached prefix — unlike history, where a rewrite costs 16 read-turns per
  # write-token.
  #
  # EnterWorktree + ExitWorktree, 1,859 tok/turn, in 98% of turns, never called.
  # Instructions say to use a worktree only when explicitly asked, and `git
  # worktree add` does the same job through Bash, so this loses a shortcut
  # rather than a capability.
  #
  # KEPT despite never being called: EndConversation (823 tok/turn) is a safety
  # valve and is meant to go unused. `headroom_retrieve` (139) is what makes
  # offload lossless and must never be dropped.
  #
  # NO MCP TOOL BELONGS ON THIS LINE. Six codebase-memory tools (437 tok/turn)
  # and 32 ai-lens tools (1,617) went a whole capture without a single call, and
  # both sets were reverted unshipped: MCP membership is decided per repo, and
  # 14 hours of silence says nothing about a tool used once a month in a repo the
  # capture barely covers. Only Claude Code's own built-ins, which every request
  # carries whatever the repo, are fair game here.
  # EndConversation added 2026-08-17: 1,316 tok of schema, never called in 1,869
  # captured bodies, and it is reserved for sustained abuse — dropping it costs
  # nothing real. EnterWorktree/ExitWorktree measured at 1,130 and 707.
  # Only Claude Code built-ins here; MCP membership belongs in per-repo config,
  # which is also the only layer where changing it is free.
  --prune-drop-tools ListMcpResourcesTool,ReadMcpResourceTool,ReadMcpResourceDirTool,EnterWorktree,ExitWorktree,EndConversation

  # Anthropic's own server-side context GC, on 2026-08-17. Clears tool results
  # older than the last 20 tool calls before the prompt reaches the model, so the
  # cleared tokens are never billed as input at all. Nothing is rewritten in the
  # body, so unlike our own offload there is no prefix-drift risk.
  #
  # Measured by replaying 8 consecutive deep turns against the API: cache_creation
  # came back IDENTICAL to control on every turn (872, 664, 234, 10539, 2920, 354,
  # 1071) while ~11,500 tokens per turn left the prompt — -8.8% weighted turn
  # cost. A 394-message pair cleared 33,690 for -18.7%. It scales with depth,
  # which is where the bill lives.
  #
  # keep=20 rather than 6: removes 17.4% of input tokens against 20.4%, so the
  # last twenty tool results — everything the model plausibly still needs — cost
  # only 3pp of the saving. Beyond that window old tool output becomes a
  # placeholder and cannot be recovered in-conversation; the model has to re-run
  # the tool. That is the real price of this line.
  #
  # clear-at-least stops a small clear from buying a full cache write: under 5k
  # clearable the API skips the strategy and the cached prefix survives.
  # Complements --ctx-offload-stale-window, which digests blocks 4-8 messages
  # back (inside the keep window) and could never convert the deep backlog.
  #
  # NOT set: --context-edit-keep-thinking. It removes 12.2% more, but Anthropic
  # reversed the default to KEEP prior turns' thinking on Opus 4.5 and 4.6+ and
  # started billing it, which is the best evidence there is that the models we run
  # actually use it. See docs/context-editing-api-facts.md in the headroom repo.
  # OFF since 2026-08-17 20:30Z. Measured on live traffic after ~3 hours, not on
  # the small sample that justified switching it on. Deep turns (>=100k prompt),
  # like for like:
  #
  #   cleared = 0   133 turns   10,051 creation   136,181 read   26,831 weighted
  #   cleared > 0   706 turns   13,418 creation   165,533 read   34,354 weighted
  #
  # +28% per turn, and reads went UP, so it never delivered the saving it was
  # enabled for. Depth-binned against the morning it is worse still: +106% at
  # 100-150k, +171% at 150-200k, +431% at 200k+ — cost rising with prefix length,
  # which is the signature of recreating the whole prefix.
  #
  # Cause: keep=20 is a SLIDING window. New tool uses push old ones out, so the
  # clearing boundary advances, and each advance makes the server's edited prefix
  # differ from its cached one at an earlier position. One conversation, four
  # consecutive turns:
  #
  #   19:12:08  cleared=43247  creation=206,319  read= 43,551   <- boundary moved
  #   19:13:17  cleared=43221  creation=  6,504  read=249,870
  #   19:13:32  cleared=43295  creation=  1,876  read=256,374
  #   19:13:40  cleared=56035  creation=203,763  read= 43,551   <- moved again
  #
  # Between moves it is nearly free. At a move, read collapses to the surviving
  # head and the rest is rebuilt. Per move: ~204k creation (296k weighted) to
  # save ~12.8k more cleared tokens a turn (~1,150 weighted) = 252 turns to break
  # even, against a p90 conversation of 64. No keep value fixes it, because any
  # sliding window slides.
  #
  # It is NOT our prefix replay: those busts logged unexplained_after_replay with
  # drift_dims empty, so the bytes we sent matched. The edit happens after we
  # hand the body over. Re-enabling needs a depth-binned A/B over hours, and
  # watch cleared_input_tokens on the 'sse stream closed' line — a stable count
  # is cheap, a changing one is a full rebuild.
  #
  # --context-edit
  # --context-edit-keep-tool-uses 20
  # --context-edit-trigger-tokens 30000
  # --context-edit-clear-at-least 5000

  # Renamed from the bare `claude-codex-5.6` on 2026-08-11 so every alias names
  # its model. The old name stops routing at the next proxy start — pick
  # `claude-codex-5.6-luna` from /model instead.
  --extra-model-route claude-codex-5.6-luna=https://api.openai.com/:translate:gpt-5.6-luna
  --extra-model-route claude-codex-5.6-terra=https://api.openai.com/:translate:gpt-5.6-terra
  --extra-model-route claude-codex-5.6-sol=https://api.openai.com/:translate:gpt-5.6-sol
  # Points at a Codex CLI auth.json; drop the line if you don't use Codex.
  # install.sh rewrites the path to whichever ~/.codex*/auth.json it finds.
  --codex-auth-file $HOME/.codex-personal/auth.json

  # Grok on the Cursor subscription, laid out like the Codex routes above: one
  # alias per effort level, each with a matching agent in ~/.claude/agents.
  # No key anywhere — the `cursor:` form runs the cursor-agent CLI, which
  # authenticates itself off the subscription, so these never touch the
  # --codex-auth-file above.
  #
  # The alias has to start with `claude` or Claude Code's model discovery will
  # not list it. `cursor-agent models | grep grok` shows the rest: -medium, the
  # 4.5 line, and a `-fast` variant of each.
  #
  # `claude-grok-4.6` takes its tier from /effort: {effort} is replaced per
  # request with the level the client asked for. Pick this one interactively.
  # The pinned aliases below exist for the subagents in ~/.claude/agents, which
  # need a fixed tier per agent and do not run /effort.
  --extra-model-route claude-grok-4.6=cursor:cursor-grok-4.6-{effort}
  --extra-model-route claude-grok-4.6-xhigh=cursor:cursor-grok-4.6-xhigh
  --extra-model-route claude-grok-4.6-high=cursor:cursor-grok-4.6-high
  --extra-model-route claude-grok-4.6-low=cursor:cursor-grok-4.6-low

  # Muse Spark 1.3 free via OpenCode Zen, with no key anywhere — not Meta's,
  # not Zen's. Zen serves the contributor-free tier anonymously (verified
  # 2026-09-06: no Authorization header at all, cost 0). `:auth=none`
  # declares the route carries no credential, so it gets neither the Codex
  # ChatGPT token above nor the caller's key.
  #
  # The `:openai:TARGET` form matters: it selects the /v1/responses endpoint.
  # The bare `:openai` form would hit chat-completions, which Zen 500s for
  # Muse (Responses-only model). Tradeoffs of the free tier: dynamic
  # unpublished quota (429s with multi-hour retry windows — keep a paid
  # fallback) and Meta may train on prompts/completions.
  # Free tier now requires a Zen API key (2026-09-07: anonymous MissingSessionID).
  # Get one at https://opencode.ai/zen -> Create API key, then:
  #   export OPENCODE_API_KEY="your-key"   (or add to ~/.bashrc)
  # or: opencode auth login  (if you prefer the auth file, export the env var from it)
  --extra-model-route claude-muse-spark-1.3=https://opencode.ai/zen/v1:openai:muse-spark-1.3-contributor-free:auth=OPENCODE_API_KEY

  # Weaker, faster sibling for the spinner sidecar: 1.2 reasons ~250 tokens
  # to 1.3's ~500-1000 on the same summary and answers in ~5s against ~12s,
  # measured 2026-09-06. Same free tier via OPENCODE_API_KEY.
  --extra-model-route claude-muse-spark-1.2=https://opencode.ai/zen/v1:openai:muse-spark-1.2-contributor-free:auth=OPENCODE_API_KEY

  # Spinner sidecar offload: answer Claude Code's 4-word status summaries on
  # the free tier instead of Haiku. The sidecar tries the route above first
  # with one bounded attempt (see --sidecar-route-timeout below) and falls
  # back to the direct Haiku path on any failure, so the worst case is
  # today's behavior plus one short wasted call. Offload rate is visible as
  # `routed: true` on the sidecar_detected log lines. Revert by deleting
  # this line: the default is Haiku.
  --sidecar-model claude-muse-spark-1.2

  # ─── Defaults, written out ──────────────────────────────────────────
  #
  # Everything below carries the binary's own default value, spelled out so
  # the config states what is in effect rather than leaving it implied.
  # Added 2026-08-17 after --image-optimize turned out to have been
  # advertising itself as enabled, and doing nothing, for months.
  #
  # Changing any value here changes behaviour — these are live flags, not
  # documentation. Anything touching tools[] or the system block re-keys
  # every live conversation at once.

  # Cache and prefix
  # master switch for the prefix cache machinery
  --cache true
  # seconds; --force-1h-cache-ttl pins the wire markers to match
  --cache-ttl 3600
  --cache-max-entries 1000
  --cache-control-auto-frozen enabled
  # inert here: measured 2026-08-17, tool ORDER never varies (16 ordered
  # fingerprints, 16 sorted). Membership is what splits the block
  --cache-stable-tool-order true
  # and this is the fix for membership: a tool the client drops for one
  # turn (SendUserFile, WaitForMcpServers) goes back in at its old spot.
  # Measured 2026-09-06: 19 recaches, 342k wasted tokens in one 3h run.
  --cache-pin-tool-roster true

  # Compression pipeline
  #
  # --compress-system-messages and --compress-user-messages are DEAD. Checked
  # 2026-08-17: the proxy parses them, defaults both to true, and never reads
  # either outside the `agent-savings` CLI subcommand. content_router carries
  # fields of the same name that only a SavingsProfile writes and nothing reads;
  # live_zone::DispatchConfig declares them and neither reads nor writes them.
  # `skip_user_messages`, which they claim to override, is never read either.
  # --compression-mode is consulted only on the Gemini and local-model paths, so
  # no user or system prose is compressed on Anthropic traffic at all. Left at
  # their defaults deliberately — changing them cannot do anything.
  --compress-system-messages true
  --compress-user-messages true
  --compression-max-workers 4
  --smart-crusher-compaction true
  --min-tokens-to-crush 200
  --max-items-after-crush 15
  # 0 = no ratio target; the transforms decide
  --target-ratio 0
  --lossless false
  --code-aware false
  --savings-profile balanced
  --mode token
  --verbosity-level 2

  # Kompress (lossy ML prose compression)
  # OFF. The ONNX model IS on disk
  # (~/.cache/huggingface/.../kompress-int8-wo.onnx) so this is available,
  # not theoretical. Untried: lossy compression sits close to instructions,
  # and the live zone is the least valuable place to compress
  --enable-kompress false
  --disable-kompress true
  --disable-kompress-fallback true
  --disable-kompress-anthropic false
  --disable-kompress-openai false
  --force-kompress-all false

  # Images
  # resizes oversized images to 1.15MP before forwarding. Reported itself
  # enabled for months while doing nothing (no call site, no nested walk,
  # and a phantom 1.15MP billing cap) — fixed 2026-08-17. Worth 2,652 tok on
  # the 11% of bodies carrying images, 0.15% of the average prompt
  --image-optimize true

  # Context editing — the whole feature is reverted, see the block above.
  # This tuned nothing on its own; it only ever applied once --context-edit was
  # on, and it is left commented so the file does not read as if it were live.
  # --context-edit-min-messages 40

  # Offload
  # 7 days; the gate's own staleness window is 24h
  --ctx-offload-ttl-seconds 604800

  # Protection knobs — all off, all untried
  --protect-recent false
  --protect-analysis-context false
  # 'hold fresh reads out of prefix cache' — do not pay a cache write for
  # content that may never be read again. Real cache economics, never
  # measured
  --read-lifecycle false
  # see --read-lifecycle
  --read-maturation false

  # CCR
  --ccr-context-tracking true
  --ccr-handle-responses true
  --ccr-inject-marker true
  --ccr-inject-tool true
  # Raised 3 -> 6 on 2026-09-01. Over the log since 2026-08-30: 250 turns
  # ran continuation rounds, 16 of them used all three, and 8 hit the cap
  # with calls still outstanding — the turn then answers without the lookup
  # it asked for. Upstream default is 8; 6 keeps a ceiling on a runaway
  # loop while clearing the observed traffic. Costs nothing on the 233
  # turns that finish in one or two rounds.
  --ccr-max-retrieval-rounds 6

  # Transport and retry
  --retry true
  --retry-max-attempts 3
  --retry-base-delay-ms 1000
  --retry-max-delay-ms 30000
  --upstream-timeout 600s
  --upstream-connect-timeout 10s
  # Single-attempt bound for a routed spinner sidecar (see --sidecar-model
  # above). No retry by design: on timeout the sidecar falls back to Haiku.
  # 15s is 3x the measured ~5s Zen answer for a minimal-effort summary.
  --sidecar-route-timeout 15s
  --graceful-shutdown-timeout 30s
  --max-body-bytes 100MB
  --anthropic-pre-upstream-concurrency 1000
  --rewrite-host true
  --strip-internal-headers enabled

  # Ops and policy
  --log-level info
  --rollout-channel stable
  --unsafe-allow-unstable-features false
  --auth-mode-policy-enforcement enabled
  --beta-header-sticky enabled
  --cost-tracking true
  --budget-period daily
  --offline false
  --stateless false
  --enable-batch-api false
  --enable-conversations-passthrough true
  --enable-responses-streaming true

  # Left implicit on purpose, not overlooked:
  #
  #   --accuracy-guard ""        passing an empty string is not reliably the
  #   --disable-features ""      same as omitting the flag, and these four take
  #   --features ""              a list where "" is the absence of one. Writing
  #   --protect-tool-results ""  them out risks changing behaviour to document it.
  #
  #   --bedrock-region us-east-1                 not on our path; we talk to
  #   --bedrock-validate-eventstream-crc true    api.anthropic.com. Spelling
  #   --enable-bedrock-native true               these out would imply they
  #   --vertex-region us-central1                matter here.
  #   --vertex-adc-scope https://www.googleapis.com/auth/cloud-platform
)

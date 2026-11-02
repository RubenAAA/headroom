# Idea: keep Claude Code's tool search on without busting the cache

- **Status:** open. Worked around on 2026-10-21 by `ENABLE_TOOL_SEARCH=false`
  in `contrib/claude-launcher` (set unless the caller already set it).
- **Source:** 2026-10-21 session, recache spike on 10-20/21.
- **Problem:** Claude Code 2.1.304 (installed 2026-10-20) defers the
  built-ins Monitor, TaskStop, SendMessage, LSP and WebFetch behind its own
  ToolSearch. Through the proxy it uses the client-side form: the tool is
  missing from `tools[]` until the model loads it, then spliced in (~7 KB).
  `tools` comes first in the cache prefix, so each load rewrites the whole
  conversation, 100–215k tokens. `tool_roster_changed` additions went from
  1 in five days (10-15..19) to 15 on 10-20 and 15 on 10-21. `tools` drift
  wasted ~2.4M tokens over the two days (30 events). `vs_stock_turn` shows a
  stock client busts the same way, so the proxy did not cause it. The
  roster pin (B3) puts a new tool at the end of `tools[]`, which saves only
  the tools before it.
- **2026-11-02, read from the 2.1.316 binary, not seen on the wire:** a
  loaded tool arrives still `defer_loading: true`, and a
  `DeferredToolPlaceholder` rides on every request. Only the
  `tool_addition` fallback drops the flag. That sits badly with the
  stock-side busts above, so one of the two readings is wrong. Either way
  the proxy's drift hash counted deferred tools and dropped the replay
  prefix on each load. `2e81ef096` (port of upstream `18a588b3`) keeps them
  out of the hash on first-party upstreams. Capture one tool-search
  session with the launcher workaround off before dropping it.
- **Why the workaround is cheap:** with tool search off, the client sends
  no ToolSearch tool, so `client_uses_tool_search` is false and the proxy's
  own deferral (`tool_search_deferral.rs`) defers non-core schemas again
  (~10.5k tok/request before 10-20). Those are server-side
  `defer_loading: true`, which the API keeps out of the prefix.
- **Options weighed:**
  - Proxy preloads the built-ins the model uses. Rejected unshipped: the
    definitions must match Claude Code's byte for byte or the later
    client splice busts anyway (likely after every CLI update); the CLI
    still treats them as unloaded, so a direct call may fail; any other
    deferred tool (WebSearch, MCP) still busts. Claude Code has no knob:
    `alwaysLoad` exists only per MCP server, and built-in deferral is
    hard-coded (`shouldDefer`).
  - `ENABLE_TOOL_SEARCH=auto` (defers past 10% of context): behaves like
    `false` in small repos and like today's in big-MCP repos, and its
    changelog has a fix for auto turning off mid-conversation (another bust).
  - **Open candidate:** when a tool appears mid-session, the proxy sends it
    with `defer_loading: true` and adds a `tool_reference` block to the
    ToolSearch `tool_result`, so the API expands it inline and the prefix
    stays the same. Covers built-ins and MCP alike.
- **Risks to measure first:** the CLI's own drift diagnosis names
  "defer_loading presence flipped (deferred-tool hint section)", so the
  first deferred tool in a conversation may change the rendered prompt once.
  The proxy's deferral keeping one deferred tool present from turn 1 might
  avoid that. Also unconfirmed: that the API takes `tool_reference` in a
  custom tool's result here, and what the tool-search beta header needs.
- **Exit:** when it ships, drop the launcher line and move this file to
  `implemented/`. If replay shows a bust, move it to `rejected/` with the
  number. Also revisit if a CLI update changes how built-ins are deferred.

## Check the workaround

Per-day counts from the proxy log. Healthy with the launcher line in place:
`roster+` near the 10-15..19 level (0–1 a day; 10-14 had 30, an MCP
server connecting mid-session), `tools_waste` near zero,
`deferral` back in the thousands. If `deferral` stays at 0, the CLI still
sends a ToolSearch tool or `defer_loading` (see `client_uses_tool_search`);
check that the session was started by `cclaude` after 2026-10-21.

```bash
python3 - ~/headroom-proxy.log* <<'EOF'
import collections, json, sys
n = collections.defaultdict(collections.Counter)
for p in sys.argv[1:]:
    for raw in open(p, "rb"):
        if not any(k in raw for k in (b'tool_roster_changed', b'cache_recache_observed', b'"tool_search_deferral"')):
            continue
        try: d = json.loads(raw)
        except ValueError: continue
        day, f = d["timestamp"][:10], d["fields"]
        ev = f.get("event")
        if ev == "tool_roster_changed":
            n[day]["roster+"] += len([x for x in (f.get("added") or "").split(",") if x])
        elif ev == "cache_recache_observed" and f.get("event_kind") == "drift" and f.get("attribution_reason") == "tools":
            n[day]["tools_waste"] += f.get("wasted_tokens") or 0
        elif ev == "tool_search_deferral":
            n[day]["deferral"] += 1
for day in sorted(n): print(day, dict(n[day]))
EOF
```

Baseline (log as of 2026-10-21): 10-20 `roster+` 15, `tools_waste` 1.05M;
10-21 `roster+` 16, `tools_waste` 1.36M; `deferral` 7,006 on 10-19, 0 on 10-21.

## Test the open candidate

1. **Capture a real load.** Restart the proxy with
   `HEADROOM_CAPTURE_DIR=~/headroom-capture-toolsearch` in its environment
   (forwarded bodies land as `req-<ts>-<n>.json`). Start one session with
   `ENABLE_TOOL_SEARCH=true cclaude` and ask for something that needs a
   deferred tool (e.g. "use Monitor to watch a file"). Find the pair of
   turns where `tools[]` grows: turn N-1 without the tool, turn N with it
   and a ToolSearch `tool_result` in the last user message.
2. **Build three variants of turn N:**
   - A, control: as captured (plain splice).
   - B: the new tool keeps its definition but gets `defer_loading: true`,
     and the ToolSearch `tool_result` gains
     `{"type": "tool_reference", "tool_name": "<tool>"}`. Use the
     `anthropic-beta` value `tool_search_deferral.rs` sends.
   - C: B, plus one deferred tool already present on turn N-1 as well, to
     test the presence-flip risk.
3. **Send each against the API:** turn N-1 first to warm the cache, then
   the variant, using the captured headers. One warm-up per variant. Each
   run costs real quota, about two full prompts.
4. **Read `usage` on the variant:**
   - Pass: `cache_read_input_tokens` ≈ turn N-1's whole prompt, and
     `cache_creation_input_tokens` ≈ only the new tail.
   - Fail: `cache_read` drops to about the tools before the insert (A's
     shape; 23,664 read / 177,489 written in the 10-20 example).
   - If B fails and C passes, the presence flip is the cause: ship C's
     shape (a resident deferred tool from turn 1).
5. **Check the tool still works:** the response to B or C must not be a
   400, and when asked to call the tool the model must emit a `tool_use`
   with valid input for it.
6. **Ship behind a flag** only on a pass. Then run a day with the launcher
   line removed and compare the workaround check above against the
   baseline.

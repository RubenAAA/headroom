# Grok respawns: what the logs do and do not show (2026-09-22)

The complaint was Cursor-route usage burning fast. A respawn sends the whole
transcript again; a resume sends only the newest message. So every unexplained
respawn costs a full re-read.

## Measured

Over the whole proxy log (2026-09-21 10:04 to 2026-09-22 11:22, 267 cursor
events, 4 conversations): 56 turns received, 11 agent starts, 8 of them fresh
chats with no `--resume`. Resumes are the common case and they work — one
conversation ran 37 turns on 4 starts.

A driven three-turn session on an instrumented binary gave turn 1 fresh, turns
2 and 3 `resumed_chat=true`. The mainline path does not respawn.

## One confirmed respawn cause

`cursor_resume_half_state` — tool results arrive with a session but no driver,
or the reverse. `drop_half_parked_turn` shuts the orphan down and starts fresh.
Seen once, immediately followed by a fresh start.

## One unexplained

2026-09-22T09:47:36, a subagent conversation (13 tools). The previous turn
resumed at 09:46:42, ran, then 54 seconds of silence, then a start with no chat
id and no `cursor_resume_half_state`. Ruled out: `MAX_PARK` (30 min, gap was
54 s), `MAX_PARKED` (32, four conversations), `close()` dropping the id (it
stashes deliberately), and `cursor-agent` not emitting `session_id`.

Log-only inference is exhausted here. `cursor_turn_started` now carries
`resumed_chat` and `Bridge::close` logs `cursor_chat_stashed`, which together
say whether the id was lost or never recorded. Catching this one needs those
fields on the shared proxy, since it only appeared under a real subagent.

## Trap

`cursor_turn_received` is not a liveness signal. It only moves between turns, so
a model reasoning for four minutes looks identical to a hang. Use
`cursor_mcp_tool_call` or transcript records.

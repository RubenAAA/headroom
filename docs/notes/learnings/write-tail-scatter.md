# Learning: write-tail scatter (tool flaps, restarts)

- **Source:** `docs/notes/savings-ideas-2.md` §§4.5–4.6 (2026-09-03 window)
- **Claim:** 4.5 tool roster flaps by one tool (client origin, 4 turns / 103k live + 11 / 114k AM, regular, small); 4.6 two post-restart `no_tracker_for_session` turns (315k, one past TTL anyway).


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

### 4.5 Tool roster flaps by one tool

`tool_roster_changed` 12 events live, all "removed SendUserFile 21→20";
`tools_before` alternates 23/24 or 27/28 within 20 of 55 live sessions and
36 of 75 AM. Each flip is a tools drift, a rebuild boundary and a dropped
replay store. Cost: live 4 turns 102,570 write; AM 11 turns 113,706. Client
origin, small, regular.


## Detail

*moved from `docs/notes/savings-ideas-2.md`*

### 4.6 Restarts and idle

Two non-first turns live hit `no_tracker_for_session` after the 15:58:38Z
restart, 314,729 tokens; one of them (`f52eef6e`, 169,675) was also 62
minutes idle, past the 1h TTL, so the restart changed nothing there.

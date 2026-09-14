# Learning: read the running process env, not the flags file

- **Source:** `docs/notes/proxy-followups.md` §6 (2026-08-21)
- **Claim:** launcher sources flags only on the start-proxy branch; a reused
  live proxy exports nothing, so the process ran `auto_tail` while the file
  said `tool` mode. Environment-of-record is `/proc/<pid>/environ`.

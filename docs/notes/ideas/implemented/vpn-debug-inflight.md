# Implemented: /debug/inflight + routed streaming guard

- **Status:** shipped in tree (needs the one-time restart to go live)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md`
- **Summary:** loopback-only inflight counter covering `forward_http` and
  routed `handle_messages` turns until dispatch (not streamed bytes, which
  flow post-guard) — the drain signal the watcher polls.

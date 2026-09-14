# Rejected: X-Forwarded-For / X-Headroom-Egress injection

- **Status:** rejected and reverted same session (purely additive diff,
  `git checkout`)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md`
- **Summary:** spoof experiments proved Zen reads the socket, not headers —
  the injection was pure fingerprint cost with zero benefit (`127.0.0.1` in
  XFF). Wire format back to pre-session minimal. What stays: the watcher +
  this note.


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Code changes (REVERTED — see below)

The session first added `X-Forwarded-For` + `X-Headroom-Egress` injection with
an ipify-backed egress cache to `handlers/local_model.rs` (+151). This was
fully reverted afterward (`git checkout` — purely additive diff, clean):
the spoof experiments proved Zen ignores these headers, and `127.0.0.1` in
`XFF` is pure fingerprint cost with zero benefit. Wire format is back to
pre-session minimal. What stays: `contrib/zen-rotate-watch.sh` (new,
committed — tails the log for routed-429s, rotates country until ipify
changes, restarts via the sibling vendored script; 120 s cooldown, flock
guard; **not yet started**) and this note.

Note: tree also holds other uncommitted work NOT from this session
(`cost_tracker.rs`, `savings_tracker.rs`, `usage_observer.rs`,
`cursor/translate.rs`, `openai/stream.rs`) — the release build picked those
up too; they were left untouched deliberately.

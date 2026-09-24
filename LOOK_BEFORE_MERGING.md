# Look Before Merging

Snapshot recorded 2026-09-24 for `codex/muse-egress-lanes`.

## Rust Nord SOCKS relay: live verification still required

Do not treat the Rust relay as ready for installation or production cutover
until the post-fix live shadow verification below passes.

Evidence so far:

- At the original snapshot, local validation passed: `cargo fmt --check`,
  Clippy with warnings denied, and all 9 `nord-socks-egress` Rust tests. The
  follow-up below adds a regression test and passes all 14 helper tests.
- In an isolated shadow run, the Rust daemon once found 8 distinct exits, and
  sequential `curl` probes through all 8 succeeded.
- The Rust helper's own `test` command then failed against that Rust relay on
  all 8 lanes. Its `test` command had passed against the existing Python relay.
- Further shadow startup attempts found 0 verified exits among 16 candidates.
  That is consistent with temporary Nord/session throttling or service
  unavailability, but does not establish the cause of the earlier discrepancy.
- Stage diagnostics were added behind `HEADROOM_NORD_SOCKS_TRACE=1`. The
  follow-up run below started successfully, but the pre-fix trace logged only
  failed I/O stages and did not capture successful reply metadata.

At the time of this snapshot, the existing eight-lane Python relay and
Headroom proxy were left running and healthy; the Rust binary was not installed
or switched into service. The follow-up below was run after a cooldown.

## Follow-up verification and local fix (2026-09-24)

- A single isolated Rust shadow startup on ports `19100`–`19109` found 8 lanes.
  One sequential `curl` through lane 0 succeeded. The helper's serial `test`
  probe failed on all 8 lanes with Reqwest's
  `SocksConnect(Parsing(Other))` error.
- The shadow daemon was stopped; the Python relay and Headroom proxy were not
  changed. Its private log is at
  `/tmp/headroom-nord-rust-shadow-20260924-1143/relay.log` (mode 0600).
- The pre-fix trace did not record successful upstream CONNECT reply metadata,
  so the exact malformed field was not captured. The failure is consistent
  with an invalid upstream SOCKS `BND.ADDR` that curl tolerates and Hyper's
  stricter Reqwest connector rejects.
- The Rust relay now returns a canonical unspecified IPv4 bound address to
  local SOCKS clients while preserving the upstream reply code. Trace mode logs
  only the reply code, reserved byte, address type, and address length.
- A loopback regression test exercises Reqwest against an upstream success
  reply with an empty domain `BND.ADDR`. All 14 helper unit tests pass;
  `cargo fmt --check` and a debug build pass.
- The updated binary has **not** had a post-fix live Nord verification. Keep
  the Python pool active; do not install or switch pools until that shadow
  check passes after a later cooldown.

Remaining before approving the live behavior:

1. Let provider/session limits cool down again; avoid repeated full startup and
   pool-test loops.
2. Run one post-fix isolated Rust shadow test on alternate local ports/state
   with `HEADROOM_NORD_SOCKS_TRACE=1`. Preserve the private relay log.
3. Compare one sequential `curl` probe with the helper's `test` probe. Confirm
   the Reqwest probe now passes; if it does not, use the safe reply metadata to
   identify the remaining stage. Do not print or record credentials.
4. If Nord rejects the upstream session, record that evidence separately
   rather than treating it as a Rust pass.
5. Only after the post-fix Rust path passes should the relay be installed or
   the active pool be drained and switched.

## Git/worktree state at snapshot

- `main` and `codex/muse-egress-lanes` pointed to the same base commit,
  `c16643a876e90ddf9778fe509d97851607356491`; the feature changes were
  uncommitted.
- The local `main` worktree had separate uncommitted edits and deletions,
  including a change to `crates/headroom-proxy/src/proxy.rs`, which is also
  modified in this feature worktree. Preserve and reconcile that work before
  integrating this branch. There was no need to merge `main` into the feature
  branch first, but neither worktree was ready for a safe merge at this
  snapshot.

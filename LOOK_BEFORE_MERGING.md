# Look Before Merging

Snapshot recorded 2026-09-24 for `codex/muse-egress-lanes`.

## Rust Nord SOCKS relay: final shadow verification passed

The final candidate-probe fix passed isolated live shadow verification, and
the worktree release is now active on port 8787 with the Rust egress pool. The
Python relay remains available on its original ports for rollback.

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
- A shadow startup after that change, but before the candidate-probe fix below,
  found only 7 distinct exits (8 required) and exited before publishing any
  listeners. Its private log is at
  `/tmp/headroom-nord-rust-postfix-20260924-1208/relay.log` (mode 0600), so no
  curl/Reqwest comparison ran. The Python pool and Headroom proxy stayed up.
- Reviewing that failure showed startup verification still used Reqwest
  directly against Nord, bypassing the relay's normalized SOCKS response. The
  candidate verifier now probes through a temporary instance of the same Rust
  lane code; startup and rotation logs label DNS versus egress-probe failures.
- A loopback regression test exercises Reqwest against an upstream success
  reply with an empty domain `BND.ADDR` through the candidate-probe path. All 14
  helper unit tests pass; `cargo fmt --check` and Clippy on the helper binary
  pass. The full `--tests` Clippy command still hits existing warnings in
  unrelated targets.
- After a cooldown, one final isolated shadow startup on ports `19200`–`19209`
  verified 8 exits. A sequential `curl` through lane 0 succeeded, and the
  helper's `test` passed on all 8 lanes with 8 unique fingerprints. The helper
  exited successfully. Its private trace is at
  `/tmp/headroom-nord-rust-final-20260924-1230/relay.log` (mode 0600); it records
  successful reply metadata without credentials. One additional candidate was
  unavailable, but the required eight exits passed.
- The installed helper then started a candidate Rust relay on ports
  `19300`–`19307` and verified 8 exits. The original Python relay remains
  available on `18600`–`18607` for rollback.
- The worktree release binary also passed a local smoke check on port `18787`:
  `/healthz` succeeded and `/debug/zen-egresses` reported all 8 configured
  opaque IDs. The shadow proxy was stopped without sending an upstream request.
- The first proxy restart drained the prior request to zero, but selected the
  main checkout's release binary: `restart-headroom.sh` computed `NEW_BIN`
  before loading `~/.headroom-paths.sh`. The worktree script now loads the path
  first, and the installed script has that fix.
- The corrected cutover restarted from the worktree release binary. Its hash
  matches `target/release/headroom-proxy`; `/healthz` is healthy and
  `/debug/zen-egresses` reports `pool_enabled: true` with 8 IDs. The helper
  reports 8 unique exit fingerprints on `19300`–`19307`; the Python fallback
  remains listening on `18600`–`18607`.
- At the immediate post-restart check, `egress_in_flight` showed a new request
  assigned to lane 0, confirming the proxy is using the Rust pool. The user
  authorized truncating the prior active stream, so the restart used
  `--force`. `cache-health` had no completed samples yet; use the next finished
  model turn to assess cache counters. The request later received HTTP 200
  response headers from `opencode.ai` on attempt 1 after 13.59 seconds; the
  stream was still active at the last check. This is one observation, not a
  before/after latency comparison.

Live behavior approval:

The live cutover checks passed. Review the completed worktree against the
separate main-worktree changes before any merge; no merge has been made.

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

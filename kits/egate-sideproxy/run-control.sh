#!/usr/bin/env bash
# Control arm: enforcement ENABLED (live setting) -> subscription-shape
# requests keep their Subscription auth mode and every Phase E pass
# skips with reason=auth_mode. PAYG-shape requests still apply.
# Otherwise identical to run-test.sh except port/log/state dir.
set -u
exec ~/.local/bin/headroom-proxy \
  --listen 127.0.0.1:18882 \
  --upstream http://127.0.0.1:18880 \
  --compression \
  --compression-mode all_messages \
  --auth-mode-policy-enforcement enabled \
  --cache false \
  --replay-store-dir /tmp/egate-sideproxy/state-control/replay-prefixes \
  --ctx-store-dir /tmp/egate-sideproxy/state-control/ctx \
  --log-level info \
  >>/tmp/egate-sideproxy/control.log 2>&1

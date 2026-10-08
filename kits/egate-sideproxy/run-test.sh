#!/usr/bin/env bash
# Test arm: enforcement DISABLED -> subscription-shape Anthropic requests
# resolve to effective Payg and the Phase E passes run.
# Live-like subset: interception on, compression-mode all_messages.
# Deviations from live (deliberate, to isolate per-request E mechanics):
#   interception via --compression instead of the ctx flags (same gate,
#   no ctx mutations); no memory/roster/replay/tail-breakpoint/TTL-pin
#   transforms; all state dirs under /tmp/egate-sideproxy/.
set -u
exec ~/.local/bin/headroom-proxy \
  --listen 127.0.0.1:8788 \
  --upstream http://127.0.0.1:18880 \
  --compression \
  --compression-mode all_messages \
  --auth-mode-policy-enforcement disabled \
  --cache false \
  --replay-store-dir /tmp/egate-sideproxy/state-test/replay-prefixes \
  --ctx-store-dir /tmp/egate-sideproxy/state-test/ctx \
  --log-level info \
  >>/tmp/egate-sideproxy/test.log 2>&1

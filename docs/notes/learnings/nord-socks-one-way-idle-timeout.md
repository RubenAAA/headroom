# Learning: SOCKS tunnel idle time must be bidirectional

- **Source:** Headroom and `zen-rotate-watch.log`, 2026-09-25.
- **Evidence:** routed SSE responses through local SOCKS ports repeatedly
  failed with TLS `UnexpectedEof` about five minutes after their response
  headers. Failures included port 18603 while a different egress lane rotated.
- **Cause:** the relay set a 300-second read timeout independently on both
  directions. A timeout in either `copy_stream` worker returned success, and
  `tunnel` then shut down both sockets. An idle client-to-upstream direction
  could therefore cut an active upstream-to-client SSE response.
- **Fix:** read timeouts now poll for activity; a tunnel closes for idleness
  only when neither direction has moved bytes for 30 minutes. The 30-minute
  limit stays above the configured 600-second upstream request timeout.
- **Scope:** this prevents one quiet half of a live SOCKS tunnel from killing
  the other. It does not prevent a genuinely idle tunnel or an upstream/network
  failure from ending a response.

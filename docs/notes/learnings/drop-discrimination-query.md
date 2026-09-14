# Learning: discriminate drops by adjacency (tunnel / host / upstream)

- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (mid-response drops)
- **Claim:** `upstream stream error mid-response`/hour vs `journalctl`
  nordvpnd CONNECTING vs WSL CheckConnection bursts: tunnel-adjacent deaths =
  rebuild kills (rotate less); background deaths + CheckConnection bursts =
  host flaps (Windows NIC/driver/power); neither = look upstream. Ruled out
  for the observed drops: NIC-offload corruption (verified off everywhere),
  keepalive tuning (H2/TCP 20 s, USER_TIMEOUT, 90 s pool all already set).


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Mid-response drops (2026-09-10 investigation)

Symptom: `[truncated: the connection to the API dropped mid-response]`,
turn ends, agent loop stalls until the user nudges. Two turns died this way
(14:02:46Z after 3 attempts, 14:32:48Z on the first), both `claude-opus-5`
direct (not Spark), both `hyper Body Io ConnectionReset`.

Evidence chain (all times UTC):

- Drop #1's first attempt died **9 s after a tunnel rebuild started**
  (18:01:39+04 Lebanon → Armenia; journal `nordvpnd` CONNECTING). Tunnel
  rebuilds kill in-flight TCP by design — exit IP changes, server RSTs the
  old 4-tuple. Certain.
- The two re-attempts died 30 s / 26 s into the NEW tunnel, and drop #2 died
  39 s in with no rebuild adjacent. Background suspect: WSL host-network
  flaps — `CheckConnection connect() failed: 99` + `getaddrinfo -5` bursts
  in the journal (120 in 46 min around the drops, 0 right now with a healthy
  network). Brief vNIC stalls RST unlucky in-flight sockets while most
  traffic survives. Correlative, not proven.
- Ruled out: NIC-offload corruption (offloads verified OFF on eth0-eth5 AND
  `nordlynx`, including the VPN interface). H2 keepalive (20 s), TCP
  keepalive (20 s), `TCP_USER_TIMEOUT`, 90 s pool idle were already set —
  tuning is present, this is not a tuning gap.

What the proxy already did right: `stream_retry` hold (8 KB) + 3 attempts
covered drop #1's first two deaths transparently; `stream_finisher`
synthesised a valid `end_turn` + marker (the `stop_reason: ''` in the log
is the telemetry tee's view pre-finisher, not what the client got).


## Discriminator

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

To discriminate next time: `grep -c "upstream stream error mid-response"`
per hour against `journalctl` nordvpnd CONNECTING events and WSL
CheckConnection bursts. Tunnel-adjacent deaths = rebuild kills (rotate
less); background deaths with CheckConnection bursts = host flaps
(Windows-side NIC/driver/power investigation); neither = look upstream.

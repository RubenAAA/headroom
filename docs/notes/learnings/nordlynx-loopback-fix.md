# Learning: NordLynx eats loopback — allowlist 127/8

- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (gotcha 3, biggest sink)
- **Claim:** post-connect, ALL fresh loopback TCP timed out (untouched :6379 /
  :5432 proved it wasn't the proxy). NORDLYNX + firewall + routing routes by
  fwmark — `ip route` looks normal while SYNs vanish. Port allowlist does NOT
  fix it; `nordvpn allowlist add subnet 127.0.0.0/8` + reconnect does (accept
  the "too large" warning — 127/8 can't leave the host).


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Gotcha 3: NordLynx eats loopback (biggest time sink)

After `nordvpn connect`, **all** fresh loopback TCP timed out
(`:8787`, plus untouched `:6379`/`:5432` — collateral proof it wasn't the
proxy; pid 43362 stayed alive and heartbeating throughout).
Cause: NORDLYNX + `Firewall: enabled` + `Routing: enabled` routes by fwmark
(`0xe1f1`) in policy tables — `ip route` looks normal, but SYNs vanish into
the tunnel. Kill Switch was already `disabled`, so not the culprit.
`nordvpn allowlist add port 8787` did **not** fix it (verified `200`
off-VPN / `000` on-VPN after fresh reconnect).
Fix that worked: `nordvpn allowlist add subnet 127.0.0.0/8`
(accept the "too large" warning — 127/8 can't leave the host anyway),
then reconnect. Loopback `200` again with tunnel up.

# Learning: Windows VPN does nothing for WSL2

- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (gotcha 1)
- **Claim:** with the Windows NordVPN app on, WSL2 egress was byte-identical
  (own vNIC, tunnel doesn't capture it); an apparent "VPN fixed it" recovery
  was pool-cooldown coincidence. Run the tunnel inside WSL2 (Linux CLI) for
  egress to actually move.


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Gotcha 1: Windows NordVPN does nothing for WSL2

With the Windows app VPN on, WSL2 egress was byte-identical
(`<home-egress>` before and after). WSL2 has its own vNIC; the Windows
tunnel doesn't capture it. An apparent "VPN fixed it" recovery was
coincidence (pool freeing up over time). Fix: run the tunnel **inside** WSL2
via the NordVPN Linux CLI — then egress actually moves (`<vpn-egress>`).

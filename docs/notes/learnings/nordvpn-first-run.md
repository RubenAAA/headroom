# Learning: NordVPN Linux CLI first-run checklist

- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (gotcha 2)
- **Claim:** `nordvpn login` needs the group first (`groupadd nordvpn`,
  `usermod -aG`, fresh shell — no reboot); daemon needs systemd in WSL2
  (`/etc/wsl.conf` `[boot] systemd=true` + `wsl --shutdown` from Windows).


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Gotcha 2: NordVPN Linux CLI first-run

- `nordvpn login` → permission denied until group applies:
  `sudo groupadd nordvpn; sudo usermod -aG nordvpn $USER`, then a **fresh
  shell** (`newgrp nordvpn`) — reboot not strictly required.
- Daemon needs systemd in WSL2: `/etc/wsl.conf` with `[boot] systemd=true`,
  then `wsl --shutdown` from Windows and reopen.

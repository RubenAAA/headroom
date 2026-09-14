# Learning: Zen reads the socket, not headers

- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (2026-09-08 session)
- **Claim:** through-proxy and direct spoof pairs (`X-Forwarded-For`,
  `X-Headroom-Egress`) changed nothing — re-run swapped gave 429+429 (pool
  capacity, no XFF bucket). The 12-hour gate IS IP-keyed (lifts on egress
  change; corrects the in-session "rotation won't dodge limits" claim — for
  this gate it does; account/pool limits differ).


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Core learning: Zen reads the socket, not headers

1. Through-proxy spoof (`X-Forwarded-For: 203.0.113.99`,
   `X-Headroom-Egress: 198.51.100.7`): 200, identical body/tokens to baseline.
   Log proved the spoof never went on the wire — replaced with peer/measured values.
2. Direct-to-Zen spoof vs clean: first pair looked suspicious (429 then 200);
   re-run swapped gave **429 + 429**. No XFF-based bucket. Transient pool capacity.
3. The "wait 12 hours" gate **is IP-keyed**: it lifted when egress changed.
   (Correction to an earlier claim in-session that rotation "won't dodge the
   limits" — for this gate it does. Account/pool limits are a different story.)

Egress observed: home `<home-egress>`, NL VPN exit `<vpn-egress>`.

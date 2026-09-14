# Learning: volatile shape is not evidence — movement is

- **Source:** `docs/notes/proxy-followups.md` §1 → fix; `recache-classification.md` H2
- **Claim:** flagging uuid/timestamp *shapes* in the cached prefix was 86%
  noise; volatile content is 4× enriched on recached turns yet explains ≤3 of
  27 events. Detector now keeps last-value per (conversation, location) and
  warns only on change; first sightings are INFO (`volatile_content_suspected`).

# Implemented: offline sessions-DB dedup procedure

- **Status:** procedure + script shipped (`docs/ctx-sessions-dedup.sql`);
  runbook in `docs/ctx-sessions-dedup.md`
- **Source:** `docs/ctx-sessions-dedup.md` (duplicate-capture bug grew the
  largest DB to 9.7 GB / 3.9M rows / 47k distinct)
- **Summary:** stop proxy, keep-ealiest `DELETE`, non-unique dedup index
  (unique would refuse on remaining dupes), `VACUUM` (needs 2× disk). Loop
  over `<config-dir>/context-mode/sessions/*.db`.

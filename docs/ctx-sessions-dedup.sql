-- One-off maintenance for sessions DBs grown under the duplicate-capture bug.
-- See docs/ctx-sessions-dedup.md. Run with the proxy stopped.
DELETE FROM session_events
 WHERE id NOT IN (
   SELECT min(id) FROM session_events
    GROUP BY session_id, type, data_hash
 );

CREATE INDEX IF NOT EXISTS idx_session_events_dedup
  ON session_events(session_id, type, data_hash);

VACUUM;

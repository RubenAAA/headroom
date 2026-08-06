//! Passive session-capture worker (CTX-2a).
//!
//! A pure observer that mirrors `cache_stabilization::capture`'s discipline:
//! the request path only clones the parsed body and hands it to a **detached
//! background worker** over an unbounded channel; identity classification,
//! event extraction, and all sessions-DB writes happen off the hot path. The
//! request/response bytes are never read for mutation and never blocked.
//!
//! Wiring is gated by the `ctx_capture` config flag (default off). When off,
//! `AppState` holds `None` and nothing is constructed.

use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

use headroom_core::ctx::{NewEvent, SessionsStore};
use serde_json::Value;

use super::extract::{self, ExtractedEvent};
use super::identity;

/// A unit of work parked for the background worker.
struct Job {
    parsed: Value,
    session_key: String,
}

/// Handle to the background capture worker. Cheap to clone via `Arc` in
/// `AppState`. Dropping the last handle closes the channel and the worker
/// thread exits.
pub struct CtxObserver {
    tx: Sender<Job>,
    /// The shared sessions store, so the CTX-4 injection engine can read the
    /// events/prefixes this worker writes (one DB, one connection pool).
    store: Arc<SessionsStore>,
}

impl CtxObserver {
    /// Open (or create) the sessions DB under `store_dir` and spawn the worker.
    ///
    /// The DB lives at `<store_dir>/sessions/proxy.db`. NOTE (CTX-2b): the
    /// proxy does not yet know the client's project dir, so all conversations
    /// share one un-sharded DB with `project_dir=''` (the public bucket). Once
    /// the request→project mapping lands, switch to
    /// `headroom_core::ctx::session_db_path` for per-project sharding.
    pub fn start(store_dir: &Path) -> std::io::Result<Self> {
        let sessions_dir = store_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        let db_path = sessions_dir.join("proxy.db");
        let store = Arc::new(SessionsStore::open(&db_path).map_err(std::io::Error::other)?);

        let worker_store = Arc::clone(&store);
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("ctx-observer".to_string())
            .spawn(move || {
                // Blocks until the channel closes (all senders dropped).
                for job in rx {
                    // One bad request must not end capture for the process.
                    //
                    // Restarting the thread would be the wrong shape: the
                    // channel receiver lives here, so a replacement thread
                    // could not be handed the queue, and every job still in
                    // flight would be lost. Containing the panic per job keeps
                    // the worker and its backlog intact, and costs one capture.
                    //
                    // `AssertUnwindSafe` is sound because nothing here has to
                    // be consistent across jobs: each one is an independent
                    // read-then-write against SQLite with no transaction
                    // spanning them, and the store recovers a poisoned lock
                    // rather than propagating it.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        process(&worker_store, &job.parsed, &job.session_key);
                    }));
                    if outcome.is_err() {
                        tracing::error!(
                            event = "ctx_observe_job_panicked",
                            "CTX-2 capture panicked on one request; worker continues"
                        );
                    }
                }
            })?;

        Ok(Self { tx, store })
    }

    /// The shared sessions store handle (for the CTX-4 injection engine).
    pub fn sessions(&self) -> Arc<SessionsStore> {
        Arc::clone(&self.store)
    }

    /// Hand a request body to the worker. Non-blocking: clones the body once
    /// and enqueues. A send failure means the worker is gone — logged, never
    /// fatal (capture must never break or slow a live request).
    pub fn observe(&self, parsed: &Value, session_key: &str) {
        let job = Job {
            parsed: parsed.clone(),
            session_key: session_key.to_string(),
        };
        if self.tx.send(job).is_err() {
            tracing::warn!(
                event = "ctx_observe_worker_gone",
                "CTX-2 observer worker unavailable; dropping capture"
            );
        }
    }
}

/// Classify + extract + persist for one request. Off the request path; every
/// failure is logged loudly and swallowed so capture never crashes the worker.
fn process(store: &SessionsStore, parsed: &Value, session_key: &str) {
    let conv_id = identity::conversation_key(parsed, session_key);

    let prev = match store.last_prefix(&conv_id) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(event = "ctx_last_prefix_failed", conv = %conv_id, error = %e);
            None
        }
    };

    let class = identity::classify(prev.as_ref(), parsed);
    let from_index = identity::extract_from_index(class, prev.as_ref());
    let events = extract::extract_new_messages(parsed, from_index);

    for ev in &events {
        if let Err(e) = store.insert_event(&to_new_event(&conv_id, ev)) {
            tracing::warn!(event = "ctx_insert_event_failed", conv = %conv_id, error = %e);
        }
    }

    let turn_n = identity::message_count(parsed);
    let hash = identity::prefix_hash(parsed, turn_n);
    if let Err(e) = store.record_prefix(&conv_id, turn_n, &hash) {
        tracing::warn!(event = "ctx_record_prefix_failed", conv = %conv_id, error = %e);
    }

    // CTX-4: index this conversation under its client session key so a later
    // resume/compaction request can link back to it.
    if let Err(e) = store.record_conversation(session_key, &conv_id) {
        tracing::warn!(event = "ctx_record_conversation_failed", conv = %conv_id, error = %e);
    }

    tracing::debug!(
        event = "ctx_observed",
        conv = %conv_id,
        class = ?class,
        turn = turn_n,
        new_events = events.len(),
    );
}

/// Map an extractor output to a sessions-DB row, grouped under the conversation
/// id as `session_id`. `project_dir` stays empty (public bucket) for CTX-2a.
fn to_new_event(conv_id: &str, ev: &ExtractedEvent) -> NewEvent {
    NewEvent::new(
        conv_id.to_string(),
        ev.category.clone(),
        ev.type_.clone(),
        ev.data.clone(),
        ev.priority,
        "ctx-observer",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn observe_persists_events_and_prefix() {
        let dir = TempDir::new().unwrap();
        let store = SessionsStore::open(dir.path().join("s.db")).unwrap();
        let req = json!({
            "system": "sys",
            "messages": [
                {"role":"user","content":"add a feature"},
                {"role":"assistant","content":[
                    {"type":"tool_use","name":"Bash","input":{"command":"git status"}}
                ]}
            ]
        });
        process(&store, &req, "sk");

        let conv = identity::conversation_key(&req, "sk");
        let events = store.get_events(&conv, 10).unwrap();
        // intent (user) + git (bash) = 2 events.
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|e| e.category == "intent"));
        assert!(events.iter().any(|e| e.category == "git"));
        assert!(events.iter().all(|e| e.source_hook == "ctx-observer"));

        // Prefix chain recorded at turn_n == message count.
        let last = store.last_prefix(&conv).unwrap().unwrap();
        assert_eq!(last.turn_n, 2);
    }

    #[test]
    fn continuation_only_extracts_new_messages() {
        let dir = TempDir::new().unwrap();
        let store = SessionsStore::open(dir.path().join("s.db")).unwrap();

        let t1 = json!({
            "system":"sys",
            "messages":[
                {"role":"user","content":"first"},
                {"role":"assistant","content":"ok"}
            ]
        });
        process(&store, &t1, "sk");
        let conv = identity::conversation_key(&t1, "sk");
        assert_eq!(store.get_events(&conv, 10).unwrap().len(), 1); // "first" intent

        // Turn 2 extends the chain; only the new user message is extracted.
        let t2 = json!({
            "system":"sys",
            "messages":[
                {"role":"user","content":"first"},
                {"role":"assistant","content":"ok"},
                {"role":"user","content":"second"}
            ]
        });
        process(&store, &t2, "sk");
        let events = store.get_events(&conv, 10).unwrap();
        // 1 (first) + 1 (second) — "first" is NOT re-extracted.
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].data, "second");
    }
}

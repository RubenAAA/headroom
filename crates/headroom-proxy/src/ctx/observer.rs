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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

use headroom_core::ctx::{NewEvent, SessionsStore};
use serde_json::Value;

use super::extract::{self, ExtractedEvent};
use super::identity;
use super::projects::ProjectStores;

/// A unit of work parked for the background worker.
struct Job {
    parsed: Value,
    session_key: String,
    /// The project this request belongs to — decides which sessions DB the
    /// worker writes to. Resolved on the request path, where the headers and
    /// system prompt are still in scope.
    project_dir: String,
}

/// Handle to the background capture worker. Cheap to clone via `Arc` in
/// `AppState`. Dropping the last handle closes the channel and the worker
/// thread exits.
pub struct CtxObserver {
    tx: Sender<Job>,
    /// The per-project store registry, so the CTX-4 injection engine reads the
    /// events/prefixes this worker writes — from the same file, for the same
    /// project.
    stores: Arc<ProjectStores>,
    /// Captures thrown away because the worker was not there to take them.
    /// Expected to stay at zero — see `observe` for why, and `should_report`
    /// for what happens if it does not.
    dropped: AtomicU64,
}

impl CtxObserver {
    /// Spawn the worker over a per-project store registry.
    ///
    /// Each request's sessions DB is `<store_dir>/sessions/<project-hash>.db`,
    /// opened on first sight of that project. Nothing is opened here: the
    /// project is a property of a request, and at start-up there are none.
    pub fn start(stores: Arc<ProjectStores>) -> std::io::Result<Self> {
        let worker_stores = Arc::clone(&stores);
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
                    let Some(store) = worker_stores.sessions(&job.project_dir) else {
                        // The registry already logged why. One project's DB
                        // failing to open must not stop capture for the rest.
                        continue;
                    };
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        process(&store, &job.parsed, &job.session_key);
                    }));
                    if outcome.is_err() {
                        tracing::error!(
                            event = "ctx_observe_job_panicked",
                            "CTX-2 capture panicked on one request; worker continues"
                        );
                    }
                }
            })?;

        Ok(Self {
            tx,
            stores,
            dropped: AtomicU64::new(0),
        })
    }

    /// The shared per-project store registry (for the CTX-4 injection engine).
    pub fn stores(&self) -> Arc<ProjectStores> {
        Arc::clone(&self.stores)
    }

    /// Hand a request body to the worker. Non-blocking: clones the body once
    /// and enqueues. A send failure means the worker is gone — reported, never
    /// fatal (capture must never break or slow a live request).
    pub fn observe(&self, parsed: &Value, session_key: &str, project_dir: &str) {
        let job = Job {
            parsed: parsed.clone(),
            session_key: session_key.to_string(),
            project_dir: project_dir.to_string(),
        };
        if self.tx.send(job).is_err() {
            // Nothing known can reach this branch: the panic that used to
            // kill the worker is caught per job above, so the loop no longer
            // exits while a sender is alive. It stays because the failure it
            // reports is absolute — the channel is unbounded, so a send can
            // only fail because the receiver is gone, and the receiver lives
            // in the worker thread. A drop is therefore never a busy queue
            // and never a transient: capture is finished until the process
            // restarts, and nothing in the process can bring it back.
            //
            // Counted rather than logged per request because a permanent
            // condition repeated per request buries the one event that
            // explains it — the last worker to die, before the panic was
            // contained, wrote 5,916 identical warnings over 16 hours.
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if should_report(n) {
                tracing::error!(
                    event = "ctx_observe_worker_gone",
                    dropped = n,
                    "CTX-2 observer worker is gone; capture stays off until the proxy restarts"
                );
            }
        }
    }

    /// Captures dropped so far because the worker was gone. Zero unless the
    /// worker has died, which no known path still allows.
    pub fn dropped_captures(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Report the first drop, then only at powers of ten.
///
/// The first line has to be immediate — if a worker ever dies again, an
/// operator needs to see capture stop at the moment it stops. After that the
/// only new information is the order of magnitude, so 5,916 drops cost four
/// lines (1, 10, 100, 1k) and the count carried on each one says how much
/// capture was lost.
fn should_report(n: u64) -> bool {
    let mut threshold = 1u64;
    while threshold < n {
        threshold = match threshold.checked_mul(10) {
            Some(t) => t,
            None => return false,
        };
    }
    threshold == n
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
    use std::sync::atomic::AtomicUsize;
    use tempfile::TempDir;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;

    /// Counts every event emitted on the current thread, so a test can assert
    /// on log volume the same way a log file measures it.
    struct CountEvents(Arc<AtomicUsize>);

    impl<S: tracing::Subscriber> Layer<S> for CountEvents {
        fn on_event(&self, _event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A `CtxObserver` whose worker is already gone: the receiver is dropped,
    /// so every send fails exactly as it does after the worker thread dies.
    fn observer_with_dead_worker(dir: &TempDir) -> CtxObserver {
        let stores = Arc::new(ProjectStores::new(dir.path().to_path_buf()));
        let (tx, rx) = mpsc::channel::<Job>();
        drop(rx);
        CtxObserver {
            tx,
            stores,
            dropped: AtomicU64::new(0),
        }
    }

    #[test]
    fn dead_worker_counts_every_drop_but_reports_a_handful() {
        // What the worst single run in the proxy log actually dropped.
        const DROPS: usize = 5_916;

        let dir = TempDir::new().unwrap();
        let obs = observer_with_dead_worker(&dir);
        let body = json!({"messages":[{"role":"user","content":"hi"}]});

        let seen = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(CountEvents(Arc::clone(&seen)));
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..DROPS {
                obs.observe(&body, "sk", "/home/dev/alpha");
            }
        });

        // Every drop is accounted for even though almost none are logged.
        assert_eq!(obs.dropped_captures(), DROPS as u64);
        // 1, 10, 100, 1_000 — and nothing else.
        assert_eq!(seen.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn report_thresholds_stay_sparse_forever() {
        let reports = (1..=1_000_000u64).filter(|&n| should_report(n)).count();
        assert_eq!(reports, 7); // 1 … 1_000_000

        // A count that overruns the last threshold must not report again, and
        // must not overflow while looking for one.
        assert!(!should_report(u64::MAX));
    }

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

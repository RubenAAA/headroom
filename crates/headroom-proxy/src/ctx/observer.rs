//! Passive session-capture worker (CTX-2a).
//!
//! A pure observer that mirrors `cache_stabilization::capture`'s discipline:
//! the request path only clones the parsed body and hands it to a **detached
//! background worker** over a byte-budgeted channel; identity classification,
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

/// How many bytes of parked request bodies the queue may hold before capture
/// sheds rather than grows. The worker writes SQLite one job at a time while
/// every turn hands it another full copy of the conversation, so a worker that
/// falls behind is the one path here that can hold memory without limit — on
/// 2026-09-10 a proxy reached 26 GB RSS over three hours and the OOM killer
/// took the WSL VM down with it. A healthy queue sits near empty; anything
/// approaching this is a backlog worth losing.
const MAX_QUEUED_BYTES: u64 = 128 * 1024 * 1024;

/// A unit of work parked for the background worker.
struct Job {
    parsed: Value,
    /// Rough heap cost of `parsed`, charged against `MAX_QUEUED_BYTES` while
    /// this job waits and refunded when the worker takes it.
    bytes: u64,
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
    /// Bytes of parked bodies the worker has not taken yet.
    queued_bytes: Arc<AtomicU64>,
    /// Captures shed because the queue was already at its budget.
    shed: AtomicU64,
    /// The budget itself, so a test can set one it can exhaust.
    budget: u64,
}

impl CtxObserver {
    /// Spawn the worker over a per-project store registry.
    ///
    /// Each request's sessions DB is `<store_dir>/sessions/<project-hash>.db`,
    /// opened on first sight of that project. Nothing is opened here: the
    /// project is a property of a request, and at start-up there are none.
    pub fn start(stores: Arc<ProjectStores>) -> std::io::Result<Self> {
        let worker_stores = Arc::clone(&stores);
        let queued_bytes = Arc::new(AtomicU64::new(0));
        let queued_bg = Arc::clone(&queued_bytes);
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("ctx-observer".to_string())
            .spawn(move || {
                // Blocks until the channel closes (all senders dropped).
                for job in rx {
                    // Refund the budget on receipt rather than after the work:
                    // the job is off the queue, and what it still holds is one
                    // body, not a backlog.
                    queued_bg.fetch_sub(job.bytes, Ordering::Relaxed);
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
            queued_bytes,
            shed: AtomicU64::new(0),
            budget: MAX_QUEUED_BYTES,
        })
    }

    /// The shared per-project store registry (for the CTX-4 injection engine).
    pub fn stores(&self) -> Arc<ProjectStores> {
        Arc::clone(&self.stores)
    }

    /// Hand a request body to the worker. Non-blocking: clones the body once
    /// and enqueues. A send failure means the worker is gone — reported, never
    /// fatal (capture must never break or slow a live request).
    ///
    /// Sheds the capture instead of enqueueing it when the queue already holds
    /// `MAX_QUEUED_BYTES`. Dropping the newest is what an `mpsc` sender can do,
    /// and it is the right one to drop anyway: the backlog ahead of it is
    /// older context, and the next turn resends everything this one carried.
    pub fn observe(&self, parsed: &Value, session_key: &str, project_dir: &str) {
        let bytes = approx_size(parsed) as u64;
        // Charge first, refund if that broke the budget. Reading the budget
        // and then adding to it lets every thread in a burst pass a check none
        // of them would pass together, which is the shape of the bug this
        // guards against in the first place.
        if self.queued_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes > self.budget {
            self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
            let n = self.shed.fetch_add(1, Ordering::Relaxed) + 1;
            if should_report(n) {
                tracing::warn!(
                    event = "ctx_observe_queue_full",
                    shed = n,
                    queued_bytes = self.queued_bytes.load(Ordering::Relaxed),
                    max_queued_bytes = self.budget,
                    "CTX-2 capture queue at its byte budget; shedding this capture"
                );
            }
            return;
        }
        let job = Job {
            parsed: parsed.clone(),
            bytes,
            session_key: session_key.to_string(),
            project_dir: project_dir.to_string(),
        };
        if self.tx.send(job).is_err() {
            self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
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

    /// Captures shed because the queue was at its byte budget. Zero unless the
    /// worker fell behind live traffic.
    pub fn shed_captures(&self) -> u64 {
        self.shed.load(Ordering::Relaxed)
    }

    /// Bytes of parked bodies waiting on the worker.
    pub fn queued_bytes(&self) -> u64 {
        self.queued_bytes.load(Ordering::Relaxed)
    }
}

/// Rough heap cost of a parsed body, for the queue budget.
///
/// Walks the tree rather than re-serialising it: the clone this charges for
/// costs more than the walk, and `to_string` on a megabyte of JSON per request
/// would be a real tax on the request path. Counts the string bytes plus a
/// flat per-node overhead, so the number tracks the clone within a small
/// factor — which is all a budget needs.
fn approx_size(v: &Value) -> usize {
    const NODE: usize = 16;
    // Iterative: this runs on the request thread, where a body nested deeply
    // enough to exhaust the stack would take request handling down with it
    // rather than one worker.
    let mut total = 0usize;
    let mut stack = vec![v];
    while let Some(node) = stack.pop() {
        total += NODE;
        match node {
            Value::String(s) => total += s.len(),
            Value::Array(items) => stack.extend(items.iter()),
            Value::Object(map) => {
                for (k, val) in map {
                    total += NODE + k.len();
                    stack.push(val);
                }
            }
            _ => {}
        }
    }
    total
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
        match store.insert_event(&to_new_event(&conv_id, ev)) {
            Ok(ins) if ins.duplicate => {
                crate::observability::ctx_metrics::observe_event_deduped();
                tracing::debug!(
                    event = "ctx_event_deduped",
                    conv = %conv_id,
                    kind = %ev.type_,
                    existing_id = ins.id,
                    "event already recorded for this conversation; not re-inserted"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(event = "ctx_insert_event_failed", conv = %conv_id, error = %e);
            }
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
            queued_bytes: Arc::new(AtomicU64::new(0)),
            shed: AtomicU64::new(0),
            budget: MAX_QUEUED_BYTES,
        }
    }

    /// A live observer whose queue budget is one byte — so every capture
    /// exceeds it, whatever the worker is doing. Nothing else can make the
    /// shed path deterministic: a real backlog depends on losing a race with
    /// a worker that drains in microseconds.
    fn observer_with_no_room(dir: &TempDir) -> CtxObserver {
        let stores = Arc::new(ProjectStores::new(dir.path().to_path_buf()));
        let mut obs = CtxObserver::start(stores).unwrap();
        obs.budget = 1;
        obs
    }

    #[test]
    fn sheds_the_capture_when_the_queue_is_at_its_budget() {
        let dir = TempDir::new().unwrap();
        let obs = observer_with_no_room(&dir);
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});

        obs.observe(&body, "sk", "/home/dev/alpha");

        assert_eq!(obs.shed_captures(), 1, "the capture should be shed");
        assert_eq!(
            obs.queued_bytes(),
            0,
            "a shed capture must not charge the budget it refused"
        );
        assert_eq!(
            obs.dropped_captures(),
            0,
            "shedding is a full queue, not a dead worker"
        );
    }

    #[test]
    fn approx_size_tracks_the_bytes_it_charges_for() {
        let body = json!({"content": "x".repeat(1000)});
        let n = approx_size(&body);
        assert!(
            (1000..2000).contains(&n),
            "a 1000-byte string should cost about 1000 bytes, got {n}"
        );
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

    /// The real client stamps a cache breakpoint on the last message and moves
    /// it forward every turn. That alone used to make every turn look like a
    /// branch, so the extractor re-read the whole conversation and the store
    /// grew one copy of every event per turn. Ten turns of a growing
    /// conversation must leave ten rows, not fifty-five.
    #[test]
    fn a_moving_cache_breakpoint_does_not_re_extract_the_conversation() {
        let dir = TempDir::new().unwrap();
        let store = SessionsStore::open(dir.path().join("s.db")).unwrap();

        let mut messages: Vec<Value> = Vec::new();
        let mut conv = String::new();
        for turn in 0..10 {
            // Last turn's breakpoint moves off as the new tail arrives.
            for m in messages.iter_mut() {
                if let Some(blocks) = m["content"].as_array_mut() {
                    for b in blocks {
                        b.as_object_mut().unwrap().remove("cache_control");
                    }
                }
            }
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("step {turn}"),
                    "cache_control": {"type": "ephemeral"}
                }]
            }));
            let req = json!({"system": "sys", "messages": messages.clone()});
            if turn == 0 {
                conv = identity::conversation_key(&req, "sk");
            }
            process(&store, &req, "sk");
            messages.push(json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}));
        }

        let events = store.get_events(&conv, 100).unwrap();
        assert_eq!(
            events.len(),
            10,
            "one intent per user message, not one per message per turn"
        );
        let texts: Vec<&str> = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(texts.first(), Some(&"step 0"));
        assert_eq!(texts.last(), Some(&"step 9"));
    }

    /// The dedup backstop, seen from the capture path. A branch — the prefix
    /// really did change under us — sends the extractor back to message 0 by
    /// design, and everything it re-reads has already been recorded. The store
    /// refuses those rows and the counter says how many.
    #[test]
    fn a_branch_re_reads_history_and_the_repeats_are_deduped() {
        let dir = TempDir::new().unwrap();
        let store = SessionsStore::open(dir.path().join("s.db")).unwrap();

        let t1 = json!({
            "system":"sys",
            "messages":[
                {"role":"user","content":"do the thing"},
                {"role":"assistant","content":"ok"},
                {"role":"user","content":"and the other thing"}
            ]
        });
        process(&store, &t1, "sk");
        let conv = identity::conversation_key(&t1, "sk");
        assert_eq!(store.get_events(&conv, 10).unwrap().len(), 2);

        // The assistant turn is rewritten in place: a real edit inside the
        // prefix, which is a branch and not a moved breakpoint.
        let t2 = json!({
            "system":"sys",
            "messages":[
                {"role":"user","content":"do the thing"},
                {"role":"assistant","content":"actually, no"},
                {"role":"user","content":"and the other thing"}
            ]
        });

        let before = crate::observability::ctx_metrics::events_deduped_get(
            crate::observability::prometheus::registry(),
        );
        assert_eq!(
            identity::classify(store.last_prefix(&conv).unwrap().as_ref(), &t2),
            identity::Classification::Branch
        );
        process(&store, &t2, "sk");
        let after = crate::observability::ctx_metrics::events_deduped_get(
            crate::observability::prometheus::registry(),
        );

        assert_eq!(
            store.get_events(&conv, 10).unwrap().len(),
            2,
            "the two user intents are still one row each"
        );
        assert_eq!(after, before + 2, "both refused inserts are counted");
    }
}

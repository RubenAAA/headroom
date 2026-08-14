//! CTX-4 — recall / resume injection engine.
//!
//! On the **first sight** of a conversation, prepends a text block into the
//! first user message: a resume snapshot (when the request looks like a
//! compaction/resume of a prior conversation under the same client) or a fresh
//! recall block (top-k BM25 hits from the CTX-1 content store + a static
//! directive). The decision is **persisted once** keyed by `conv_id` and
//! replayed byte-for-byte on every subsequent turn — invariant I4
//! (`docs/ctx-mode-in-headroom-plan.md`).
//!
//! # Cache safety (I1/I4)
//!
//! - The injected block contains nothing volatile (no timestamps — see
//!   `headroom_core::ctx::build_resume_snapshot`), so replaying it verbatim
//!   keeps the cached prefix byte-stable.
//! - It is decided exactly once (`SessionsStore::put_injection` never
//!   overwrites); later turns read the stored bytes and re-inject at the same
//!   position (prepended into the first user message; `system`/`tools` are
//!   never touched).
//! - **Row-miss fail-safe:** if we have seen a conversation before (its
//!   prefix-chain exists) but its injection row is gone (purge/crash), we
//!   inject **nothing** and log loudly. Worst case is a single re-cache, never
//!   an oscillation (absence is byte-stable going forward).
//!
//! # Hot path
//!
//! A write-through in-memory LRU (`conv_id → decision`) fronts the sqlite
//! reads, so steady-state turns never touch the DB. Only first sight (or the
//! first turn after a process restart) does a sqlite read; first sight also
//! does one small synchronous build + `put_injection` write. Build duration is
//! measured and logged.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::Instant;

use headroom_core::ctx::{
    build_recall, build_resume_snapshot, SearchOpts, SessionsStore, INJECT_SENTINEL,
};
use lru::LruCache;
use serde_json::{json, Value};

use super::identity;
use super::projects::ProjectStores;

/// Max prior conversations to consult when resume-linking.
const RESUME_LINK_LOOKBACK: usize = 1;
/// Max stored events pulled to render a resume snapshot.
const MAX_SNAPSHOT_EVENTS: usize = 200;
/// Max content-store hits in a fresh recall block.
const RECALL_LIMIT: usize = 5;

/// A per-conversation injection decision. `Some` = inject these exact bytes;
/// `None` = inject nothing (fresh with a deliberate no-op, or the row-miss
/// fail-safe). Cached so the hot path is a single map lookup.
type Decision = Option<String>;

/// Recall / resume injection engine. Reads the sessions DB the capture
/// observer wrote and runs BM25 recall over the content index — both resolved
/// per request, for the requesting project.
///
/// Scoping matters more here than anywhere else in the ctx layer. Recall is a
/// relevance query, not a keyed lookup: run against a store holding several
/// projects it will happily rank another project's tool output as the best
/// answer and inject it (CTX-2b, observed live).
pub struct InjectEngine {
    stores: std::sync::Arc<ProjectStores>,
    cache: Mutex<LruCache<String, Decision>>,
}

impl InjectEngine {
    /// Build the engine over the shared per-project store registry.
    pub fn new(stores: std::sync::Arc<ProjectStores>) -> Self {
        let cap = NonZeroUsize::new(4096).expect("nonzero");
        Self {
            stores,
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Decide + apply injection for one request. Mutates `parsed` in place;
    /// returns `true` if a block was injected. Never blocks beyond a single
    /// small sqlite read on cold conversations. Every error is logged and
    /// swallowed (injection failures must never break a request).
    /// `budget` is the request's shared injection ceiling. Recall *reserves*
    /// against it rather than being clipped by it: the decision is persisted
    /// once and replayed byte-for-byte into the cached prefix (I4), so cutting
    /// it on a later turn would rewrite bytes the provider had already cached.
    /// Charging it here is what makes the later stages see a smaller ceiling.
    pub fn maybe_inject(
        &self,
        parsed: &mut Value,
        session_key: &str,
        project_dir: &str,
        budget: &crate::injection_budget::InjectionBudget,
    ) -> bool {
        self.maybe_inject_for_request(parsed, session_key, project_dir, budget, "")
    }

    /// Request-correlated production entry point. The decision and output are
    /// identical to [`Self::maybe_inject`]; only failure observability gains the
    /// request id needed to join it to the forwarded turn.
    pub fn maybe_inject_for_request(
        &self,
        parsed: &mut Value,
        session_key: &str,
        project_dir: &str,
        budget: &crate::injection_budget::InjectionBudget,
        request_id: &str,
    ) -> bool {
        let conv_id = identity::conversation_key(parsed, session_key);
        // The cache fronts a per-project store, so it has to be keyed the same
        // way — otherwise the first project to decide a conv_id answers for
        // every other project that derives the same one.
        let cache_key = format!("{project_dir}\u{0}{conv_id}");

        // Hot path: cached decision.
        if let Some(decision) = self.cache_get(&cache_key) {
            let applied = self.apply_within_budget(parsed, &decision, budget);
            if applied {
                crate::observability::ctx_metrics::observe_recall_injection();
            }
            return applied;
        }

        let decision = self.decide(&conv_id, parsed, session_key, project_dir, request_id);
        self.cache_put(&cache_key, decision.clone());
        let applied = self.apply_within_budget(parsed, &decision, budget);
        if applied {
            crate::observability::ctx_metrics::observe_recall_injection();
        }
        applied
    }

    /// Charge the decision against the budget, then apply it verbatim.
    ///
    /// The text handed back by `take` is discarded on purpose — recall is
    /// non-clippable, so it always comes back whole, and `apply` has to write
    /// the exact bytes that were persisted for this conversation. Taking the
    /// returned copy would work today and break silently the moment the stage
    /// became clippable.
    fn apply_within_budget(
        &self,
        parsed: &mut Value,
        decision: &Decision,
        budget: &crate::injection_budget::InjectionBudget,
    ) -> bool {
        let Some(text) = decision else {
            return false;
        };
        if budget
            .take(
                crate::injection_budget::InjectionStage::Recall,
                text.clone(),
            )
            .is_none()
        {
            return false;
        }
        apply(parsed, decision)
    }

    fn cache_get(&self, conv_id: &str) -> Option<Decision> {
        self.cache
            .lock()
            .expect("inject cache poisoned")
            .get(conv_id)
            .cloned()
    }

    fn cache_put(&self, conv_id: &str, decision: Decision) {
        self.cache
            .lock()
            .expect("inject cache poisoned")
            .put(conv_id.to_string(), decision);
    }

    /// Resolve the injection decision for a conversation, reading (and on first
    /// sight, writing) the sessions DB.
    fn decide(
        &self,
        conv_id: &str,
        parsed: &Value,
        session_key: &str,
        project_dir: &str,
        request_id: &str,
    ) -> Decision {
        // No sessions DB for this project means no history to replay and
        // nowhere to persist a new decision. Injecting nothing is byte-stable,
        // which is the direction the fail-safe below already chooses.
        let sessions = self.stores.sessions(project_dir)?;

        // Already decided → replay verbatim (I4).
        match sessions.get_injection(conv_id) {
            Ok(Some(bytes)) => return Some(bytes),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    event = "ctx_inject_get_failed",
                    request_id = %request_id,
                    conv = %conv_id,
                    error = %e
                );
                return None;
            }
        }

        // No injection row. Distinguish a genuine row-miss from this request
        // racing its own capture: the observer writes the prefix chain from a
        // detached worker (`ctx::observer::process`) that runs concurrently
        // with us, keyed on `turn_n = message_count(parsed)` — the same turn we
        // are deciding for. So "a prefix row exists" alone does not mean we saw
        // this conversation on an earlier turn; on first sight the worker may
        // simply have won the race and written the current turn already.
        //
        // Only a row strictly BEHIND the current turn proves earlier history,
        // and that is the case the fail-safe exists for. Treating the race as a
        // row-miss suppressed injection for genuinely-new conversations.
        let current_turn = identity::message_count(parsed);
        let prior_turn = match sessions.last_prefix(conv_id) {
            Ok(Some(p)) if p.turn_n < current_turn => Some(p.turn_n),
            _ => None,
        };
        if let Some(turn_n) = prior_turn {
            tracing::warn!(
                event = "ctx_inject_row_miss",
                request_id = %request_id,
                conv = %conv_id,
                last_turn = turn_n,
                current_turn,
                "injection row missing for a known conversation; injecting nothing (fail-safe)"
            );
            return None;
        }

        // Genuine first sight → build synchronously, persist once, inject.
        let start = Instant::now();
        let built = self.build(
            &sessions,
            conv_id,
            parsed,
            session_key,
            project_dir,
            request_id,
        );
        if let Err(e) = sessions.put_injection(conv_id, &built) {
            tracing::warn!(
                event = "ctx_inject_persist_failed",
                request_id = %request_id,
                conv = %conv_id,
                error = %e
            );
        }
        tracing::debug!(
            event = "ctx_inject_built",
            request_id = %request_id,
            conv = %conv_id,
            bytes = built.len(),
            build_us = start.elapsed().as_micros() as u64,
            "built + persisted first-sight injection"
        );
        Some(built)
    }

    /// Build the injection text: resume snapshot when the request carries a
    /// compaction/resume marker and links to a prior conversation; otherwise a
    /// fresh recall block.
    fn build(
        &self,
        sessions: &SessionsStore,
        conv_id: &str,
        parsed: &Value,
        session_key: &str,
        project_dir: &str,
        request_id: &str,
    ) -> String {
        let first_text = identity::first_user_message_text(parsed).unwrap_or_default();

        if identity::has_compaction_marker(&first_text) {
            if let Some(snapshot) = self.try_resume_snapshot(sessions, conv_id, session_key) {
                return snapshot;
            }
            // Resume marker but nothing linkable → fall through to fresh recall.
        }

        let queries = derive_queries(&first_text);
        let opts = SearchOpts {
            limit: RECALL_LIMIT,
            ..Default::default()
        };
        // Scoped by which file we open: `SearchOpts` cannot filter by project.
        let hits = match self.stores.content(project_dir) {
            Some(content) => match content.search(&queries, &opts) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(
                        event = "ctx_inject_search_failed",
                        request_id = %request_id,
                        error = %e
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        build_recall(&hits, &queries)
    }

    /// Try to render a resume snapshot from the most recent prior conversation
    /// under this session key. `None` if there is no linkable prior with events.
    fn try_resume_snapshot(
        &self,
        sessions: &SessionsStore,
        conv_id: &str,
        session_key: &str,
    ) -> Option<String> {
        let recent = sessions
            .recent_conversations(session_key, conv_id, RESUME_LINK_LOOKBACK)
            .ok()?;
        let prior = recent.first()?;
        let events = sessions.get_events(prior, MAX_SNAPSHOT_EVENTS).ok()?;
        if events.is_empty() {
            return None;
        }
        Some(build_resume_snapshot(&events, 1))
    }
}

/// Derive BM25 recall queries from the first user message text. Minimal: a
/// single collapsed, capped query (deterministic — pure function of the text).
fn derive_queries(first_text: &str) -> Vec<String> {
    let q: String = first_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect();
    if q.is_empty() {
        Vec::new()
    } else {
        vec![q]
    }
}

/// Prepend the injected text as a text block into the first user message.
/// Idempotent: a message already carrying our sentinel is left untouched.
/// Returns whether a block was inserted.
fn apply(parsed: &mut Value, decision: &Decision) -> bool {
    let Some(text) = decision else {
        return false;
    };
    let Some(messages) = parsed.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let Some(msg) = messages
        .iter_mut()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return false;
    };

    if message_has_sentinel(msg) {
        return false;
    }

    let injected = json!({ "type": "text", "text": text });
    match msg.get_mut("content") {
        Some(Value::String(s)) => {
            let original = std::mem::take(s);
            msg["content"] =
                Value::Array(vec![injected, json!({ "type": "text", "text": original })]);
        }
        Some(Value::Array(blocks)) => {
            blocks.insert(0, injected);
        }
        _ => return false,
    }
    true
}

/// Whether a message's content already contains our injection sentinel.
fn message_has_sentinel(msg: &Value) -> bool {
    match msg.get("content") {
        Some(Value::String(s)) => s.contains(INJECT_SENTINEL),
        Some(Value::Array(blocks)) => blocks.iter().any(|b| {
            b.get("text")
                .and_then(Value::as_str)
                .map(|t| t.contains(INJECT_SENTINEL))
                .unwrap_or(false)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// A budget large enough never to bind, so these tests keep exercising
    /// injection behaviour rather than the ceiling. Budget clipping has its
    /// own tests in `injection_budget`.
    fn big_budget() -> crate::injection_budget::InjectionBudget {
        crate::injection_budget::InjectionBudget::new(usize::MAX)
    }

    /// The project these tests run as. Any non-empty path works; what matters
    /// is that the same one is used for reads and writes.
    const PROJECT: &str = "/home/dev/alpha";

    fn engine(dir: &TempDir) -> InjectEngine {
        InjectEngine::new(Arc::new(ProjectStores::new(dir.path().to_path_buf())))
    }

    fn sessions_of(eng: &InjectEngine) -> Arc<SessionsStore> {
        eng.stores.sessions(PROJECT).unwrap()
    }

    fn fresh_req(first: &str) -> Value {
        json!({
            "system": "sys",
            "messages": [{"role":"user","content": first}]
        })
    }

    #[derive(Clone, Default)]
    struct EventCapture(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            use tracing::field::{Field, Visit};
            struct Visitor(String);
            impl Visit for Visitor {
                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!("{}={value:?} ", field.name()));
                }
                fn record_str(&mut self, field: &Field, value: &str) {
                    self.0.push_str(&format!("{}={value} ", field.name()));
                }
                fn record_i64(&mut self, field: &Field, value: i64) {
                    self.0.push_str(&format!("{}={value} ", field.name()));
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    #[test]
    fn injects_once_and_replays_verbatim() {
        let dir = TempDir::new().unwrap();
        let eng = engine(&dir);
        let mut r1 = fresh_req("build a parser");
        assert!(eng.maybe_inject(&mut r1, "sk", PROJECT, &big_budget()));
        // The first user message is now an array whose first block is ours.
        let block0 = &r1["messages"][0]["content"][0];
        assert_eq!(block0["type"], "text");
        assert!(block0["text"]
            .as_str()
            .unwrap()
            .starts_with(INJECT_SENTINEL));

        // A second, later turn of the SAME conversation replays identical bytes.
        let injected_text = block0["text"].as_str().unwrap().to_string();
        let mut r2 = json!({
            "system":"sys",
            "messages":[
                {"role":"user","content":"build a parser"},
                {"role":"assistant","content":"ok"},
                {"role":"user","content":"continue"}
            ]
        });
        assert!(eng.maybe_inject(&mut r2, "sk", PROJECT, &big_budget()));
        assert_eq!(r2["messages"][0]["content"][0]["text"], injected_text);
    }

    #[test]
    fn double_injection_is_guarded() {
        let dir = TempDir::new().unwrap();
        let eng = engine(&dir);
        let mut r = fresh_req("do a thing");
        assert!(eng.maybe_inject(&mut r, "sk", PROJECT, &big_budget()));
        // Applying again to the already-injected body is a no-op.
        assert!(!eng.maybe_inject(&mut r, "sk", PROJECT, &big_budget()));
        // Exactly one sentinel present.
        let blocks = r["messages"][0]["content"].as_array().unwrap();
        let count = blocks
            .iter()
            .filter(|b| {
                b["text"]
                    .as_str()
                    .map(|t| t.contains(INJECT_SENTINEL))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn row_miss_injects_nothing_and_does_not_panic() {
        use tracing_subscriber::layer::SubscriberExt;

        let dir = TempDir::new().unwrap();
        let eng = engine(&dir);
        // A known conversation with NO injection row. The prefix must sit
        // strictly BEHIND this request's turn, which is what proves we saw the
        // conversation on an earlier turn rather than racing our own capture.
        let req = json!({
            "system": "sys",
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"hello"},
                {"role":"user","content":"more"}
            ]
        });
        let conv = identity::conversation_key(&req, "sk");
        sessions_of(&eng).record_prefix(&conv, 1, "h").unwrap();

        let mut r = req.clone();
        let capture = EventCapture::default();
        let lines = capture.0.clone();
        let subscriber = tracing_subscriber::registry().with(capture);
        // Fail-safe: nothing injected, no panic, and the controlled miss is
        // now joinable to its request.
        tracing::subscriber::with_default(subscriber, || {
            assert!(!eng.maybe_inject_for_request(
                &mut r,
                "sk",
                PROJECT,
                &big_budget(),
                "req-row-miss",
            ));
        });
        assert_eq!(r["messages"][0]["content"], json!("hi"));
        let joined = lines.lock().unwrap().join("\n");
        let event = joined
            .lines()
            .find(|line| line.contains("ctx_inject_row_miss"))
            .unwrap_or_else(|| panic!("row-miss event missing from:\n{joined}"));
        assert!(event.contains("request_id=req-row-miss"), "{event}");
    }

    /// The capture observer writes the prefix chain from a detached worker
    /// keyed on the CURRENT turn, so on first sight it can land before inject
    /// reads it. That is a race, not a lost row: treating it as a row-miss
    /// suppressed injection for genuinely-new conversations.
    #[test]
    fn a_prefix_row_at_the_current_turn_is_a_race_not_a_row_miss() {
        let dir = TempDir::new().unwrap();
        let eng = engine(&dir);
        let req = fresh_req("build a parser");
        let conv = identity::conversation_key(&req, "sk");
        // The observer won the race and recorded THIS turn (1 message = turn 1).
        sessions_of(&eng).record_prefix(&conv, 1, "h").unwrap();

        let mut r = req.clone();
        assert!(
            eng.maybe_inject(&mut r, "sk", PROJECT, &big_budget()),
            "first sight must still inject when capture wrote the current turn first"
        );
        assert!(r["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with(INJECT_SENTINEL));
    }

    #[test]
    fn resume_marker_links_to_prior_conversation() {
        let dir = TempDir::new().unwrap();
        let eng = engine(&dir);

        // A prior conversation with events, recorded under session key "sk".
        let prior = "prior_conv_id";
        sessions_of(&eng).record_conversation("sk", prior).unwrap();
        sessions_of(&eng)
            .insert_event(&headroom_core::ctx::NewEvent::new(
                prior,
                "intent",
                "intent",
                "implement the widget",
                2,
                "ctx-observer",
            ))
            .unwrap();

        // A NEW conversation whose first user message is a compaction preamble.
        let mut resume = json!({
            "system":"sys",
            "messages":[{
                "role":"user",
                "content":"This session is being continued from a previous conversation. Summary: ..."
            }]
        });
        assert!(eng.maybe_inject(&mut resume, "sk", PROJECT, &big_budget()));
        let text = resume["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("<session_resume"));
        assert!(text.contains("implement the widget"));
    }
}

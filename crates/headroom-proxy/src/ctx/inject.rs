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
    build_recall, build_resume_snapshot, CtxStore, SearchOpts, SessionsStore, INJECT_SENTINEL,
};
use lru::LruCache;
use serde_json::{json, Value};

use super::identity;

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

/// Recall / resume injection engine. Shares the sessions DB with the capture
/// observer (so it reads the events/prefixes the observer wrote) and holds its
/// own content-store handle for BM25 recall.
pub struct InjectEngine {
    sessions: std::sync::Arc<SessionsStore>,
    content: CtxStore,
    cache: Mutex<LruCache<String, Decision>>,
}

impl InjectEngine {
    /// Build the engine over a shared sessions store and a content-store handle.
    pub fn new(sessions: std::sync::Arc<SessionsStore>, content: CtxStore) -> Self {
        let cap = NonZeroUsize::new(4096).expect("nonzero");
        Self {
            sessions,
            content,
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Decide + apply injection for one request. Mutates `parsed` in place;
    /// returns `true` if a block was injected. Never blocks beyond a single
    /// small sqlite read on cold conversations. Every error is logged and
    /// swallowed (injection failures must never break a request).
    pub fn maybe_inject(&self, parsed: &mut Value, session_key: &str) -> bool {
        self.maybe_inject_for_request(parsed, session_key, "")
    }

    /// Production entry point that adds the request id to every diagnostic,
    /// so an injection failure can be joined to the forwarded request.
    pub fn maybe_inject_for_request(
        &self,
        parsed: &mut Value,
        session_key: &str,
        request_id: &str,
    ) -> bool {
        let conv_id = identity::conversation_key(parsed, session_key);

        // Hot path: cached decision.
        if let Some(decision) = self.cache_get(&conv_id) {
            let applied = apply(parsed, &decision);
            if applied {
                crate::observability::ctx_metrics::observe_recall_injection();
            }
            return applied;
        }

        let decision = self.decide(&conv_id, parsed, session_key, request_id);
        self.cache_put(&conv_id, decision.clone());
        let applied = apply(parsed, &decision);
        if applied {
            crate::observability::ctx_metrics::observe_recall_injection();
        }
        applied
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
        request_id: &str,
    ) -> Decision {
        // Already decided → replay verbatim (I4).
        match self.sessions.get_injection(conv_id) {
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

        // No injection row. Distinguish genuine first sight from a row-miss for
        // a conversation we've recorded before (prefix-chain exists).
        let seen_before = matches!(self.sessions.last_prefix(conv_id), Ok(Some(_)));
        if seen_before {
            tracing::warn!(
                event = "ctx_inject_row_miss",
                request_id = %request_id,
                conv = %conv_id,
                "injection row missing for a known conversation; injecting nothing (fail-safe)"
            );
            return None;
        }

        // Genuine first sight → build synchronously, persist once, inject.
        let start = Instant::now();
        let built = self.build(conv_id, parsed, session_key, request_id);
        if let Err(e) = self.sessions.put_injection(conv_id, &built) {
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
        conv_id: &str,
        parsed: &Value,
        session_key: &str,
        request_id: &str,
    ) -> String {
        let first_text = identity::first_user_message_text(parsed).unwrap_or_default();

        if identity::has_compaction_marker(&first_text) {
            if let Some(snapshot) = self.try_resume_snapshot(conv_id, session_key) {
                return snapshot;
            }
            // Resume marker but nothing linkable → fall through to fresh recall.
        }

        let queries = derive_queries(&first_text);
        let opts = SearchOpts {
            limit: RECALL_LIMIT,
            ..Default::default()
        };
        let hits = match self.content.search(&queries, &opts) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    event = "ctx_inject_search_failed",
                    request_id = %request_id,
                    error = %e
                );
                Vec::new()
            }
        };
        build_recall(&hits, &queries)
    }

    /// Try to render a resume snapshot from the most recent prior conversation
    /// under this session key. `None` if there is no linkable prior with events.
    fn try_resume_snapshot(&self, conv_id: &str, session_key: &str) -> Option<String> {
        let recent = self
            .sessions
            .recent_conversations(session_key, conv_id, RESUME_LINK_LOOKBACK)
            .ok()?;
        let prior = recent.first()?;
        let events = self.sessions.get_events(prior, MAX_SNAPSHOT_EVENTS).ok()?;
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
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    fn engine(dir: &TempDir) -> InjectEngine {
        let sessions = Arc::new(SessionsStore::open(dir.path().join("s.db")).unwrap());
        let content_path = dir.path().join("content.db");
        let content = CtxStore::open(&content_path).unwrap();
        InjectEngine::new(sessions, content)
    }

    fn fresh_req(first: &str) -> Value {
        json!({
            "system": "sys",
            "messages": [{"role":"user","content": first}]
        })
    }

    #[test]
    fn injects_once_and_replays_verbatim() {
        let dir = TempDir::new().unwrap();
        let eng = engine(&dir);
        let mut r1 = fresh_req("build a parser");
        assert!(eng.maybe_inject(&mut r1, "sk"));
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
        assert!(eng.maybe_inject(&mut r2, "sk"));
        assert_eq!(r2["messages"][0]["content"][0]["text"], injected_text);
    }

    #[test]
    fn double_injection_is_guarded() {
        let dir = TempDir::new().unwrap();
        let eng = engine(&dir);
        let mut r = fresh_req("do a thing");
        assert!(eng.maybe_inject(&mut r, "sk"));
        // Applying again to the already-injected body is a no-op.
        assert!(!eng.maybe_inject(&mut r, "sk"));
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
        // Simulate a known conversation (prefix recorded) with NO injection row.
        let req = fresh_req("hi");
        let conv = identity::conversation_key(&req, "sk");
        eng.sessions.record_prefix(&conv, 1, "h").unwrap();

        let mut r = req.clone();
        let capture = EventCapture::default();
        let events = capture.0.clone();
        let subscriber = tracing_subscriber::registry().with(capture);
        tracing::subscriber::with_default(subscriber, || {
            assert!(!eng.maybe_inject_for_request(&mut r, "sk", "req-row-miss"));
        });

        // Fail-safe: nothing injected, no panic, and the miss is joinable.
        assert_eq!(r["messages"][0]["content"], json!("hi"));
        let joined = events.lock().unwrap().join("\n");
        assert!(joined.contains("event=ctx_inject_row_miss"), "{joined}");
        assert!(joined.contains("request_id=req-row-miss"), "{joined}");
    }

    #[test]
    fn resume_marker_links_to_prior_conversation() {
        let dir = TempDir::new().unwrap();
        let eng = engine(&dir);

        // A prior conversation with events, recorded under session key "sk".
        let prior = "prior_conv_id";
        eng.sessions.record_conversation("sk", prior).unwrap();
        eng.sessions
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
        assert!(eng.maybe_inject(&mut resume, "sk"));
        let text = resume["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("<session_resume"));
        assert!(text.contains("implement the widget"));
    }
}

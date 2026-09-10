//! Novel-vs-repeat attribution for per-request compression savings
//! (upstream `427fa76f`).
//!
//! Providers disagree about what a request's `tokens_saved` means, and the
//! disagreement only shows up once something sums it.
//!
//! Anthropic's cached prefix is frozen — the router leaves it alone so the
//! provider's prefix cache keeps hitting — so a turn's `tokens_saved` covers
//! only content that newly entered the conversation. Summing across turns
//! counts each removed token once.
//!
//! OpenAI's `/v1/responses` carries the whole transcript in every request and
//! the router recompresses all of it, so a turn's `tokens_saved` is the
//! running total of everything removed from that conversation so far. Summing
//! across turns counts the same removed token once per remaining turn.
//!
//! This module converts the cumulative series into the incremental one. Per
//! request descriptions keep the wire truth — what left this process —
//! because those tokens really were removed from this request's payload. Only
//! the running totals switch to the novel figure, so a removed token is
//! counted once per conversation instead of once per turn.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Conversations tracked before the oldest is forgotten. A forgotten
/// conversation that is still live restarts from zero and re-counts its
/// transcript once, so the cap trades a bounded, one-off overcount for
/// bounded memory. 512 is far above the number of conversations one client
/// keeps warm.
pub const DEFAULT_MAX_CONVERSATIONS: usize = 512;

/// Identity under which a Responses request's `tokens_saved` is a running total.
///
/// Returns `None` unless BOTH premises of the cumulative accounting hold, in
/// which case the funnel keeps ordinary per-request accounting:
///
/// 1. The request names one conversation. Only an explicit id counts: a
///    top-level `conversation_id`/`session_id`/`thread_id`, one of those
///    inside `metadata`/`client_metadata`, or a transport-scoped `session_id`
///    the caller vouches for. Two things are deliberately NOT accepted: the
///    holdout key's fallbacks (instructions prefix, literal `"responses"`)
///    — two independent conversations with the same instructions would share
///    a running total and suppress each other's savings — and
///    `prompt_cache_key`, which OpenAI documents as shared across a user's
///    sessions and forks.
/// 2. The request carries the whole transcript. With `previous_response_id`
///    or a server-side `conversation` the provider holds prior context and
///    the payload is this turn's increment, so `tokens_saved` is already
///    per-request and must not be differenced.
///
/// Chat-completions and Anthropic-messages bodies (no `input`) return `None`:
/// their handlers freeze the cached prefix, so their `tokens_saved` is
/// novel-only already.
pub fn savings_conversation_key(body: &Value, session_id: Option<&str>) -> Option<String> {
    let body = unwrap_response_create_body(body);
    let map = body.as_object()?;
    if !map.contains_key("input") {
        return None;
    }
    if is_truthy(map.get("previous_response_id")) || is_truthy(map.get("conversation")) {
        return None;
    }

    let mut identity = String::new();
    for key in ["conversation_id", "session_id", "thread_id"] {
        let value = explicit_id(map.get(key));
        if !value.is_empty() {
            identity = format!("{key}:{value}");
            break;
        }
    }
    if identity.is_empty() {
        for container_key in ["client_metadata", "metadata"] {
            let Some(container) = map.get(container_key).and_then(Value::as_object) else {
                continue;
            };
            for key in [
                "conversation_id",
                "conversation_key",
                "session_id",
                "thread_id",
                "codex_session_id",
            ] {
                let value = explicit_id(container.get(key));
                if !value.is_empty() {
                    identity = format!("{container_key}.{key}:{value}");
                    break;
                }
            }
            if !identity.is_empty() {
                break;
            }
        }
    }
    if identity.is_empty() {
        if let Some(sid) = session_id {
            if !sid.is_empty() {
                identity = format!("session:{sid}");
            }
        }
    }
    if identity.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update("savings\x00");
    hasher.update(identity.as_bytes());
    Some(hex_encode(hasher.finalize()))
}

/// Unwrap the `response.create` envelope to the inner create body.
fn unwrap_response_create_body(body: &Value) -> &Value {
    if body.get("type").and_then(Value::as_str) == Some("response.create") {
        if let Some(inner) = body.get("response") {
            if inner.is_object() {
                return inner;
            }
        }
    }
    body
}

/// An explicit conversation id: a non-empty string that is not `"auto"`, or a
/// dict carrying one under the known id keys.
fn explicit_id(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) if !s.is_empty() && !s.eq_ignore_ascii_case("auto") => s.clone(),
        Some(Value::Object(map)) => {
            for key in ["id", "conversation_id", "session_id", "thread_id"] {
                if let Some(Value::String(s)) = map.get(key) {
                    if !s.is_empty() {
                        return s.clone();
                    }
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// Python truthiness for the incremental-input signals: absent, null, false,
/// empty string, and empty containers are all "not incremental".
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(m)) => !m.is_empty(),
    }
}

fn hex_encode(digest: sha2::digest::Output<Sha256>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Cumulative-per-turn savings to novel-this-turn savings, bounded.
pub struct ConversationSavings {
    max_conversations: usize,
    seen: Mutex<VecDeque<(String, i64)>>,
}

impl ConversationSavings {
    /// An empty ledger tracking up to `DEFAULT_MAX_CONVERSATIONS`
    /// conversations. Prefer [`conversation_ledger`] on the live path so one
    /// conversation spans many requests.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_CONVERSATIONS)
    }

    /// An empty ledger with an explicit cap (floored at 1).
    pub fn with_capacity(max_conversations: usize) -> Self {
        Self {
            max_conversations: max_conversations.max(1),
            seen: Mutex::new(VecDeque::new()),
        }
    }

    /// Tokens this request removed for the first time in its conversation.
    ///
    /// `cumulative` is the conversation's running removed-token total as of
    /// this request. Returns `None` when the caller cannot supply both a
    /// conversation key and a total, which means "this path does not
    /// distinguish" — the funnel then falls back to `tokens_saved`, which is
    /// already novel-only on providers that freeze their cached prefix.
    ///
    /// A total that went DOWN means the transcript shrank under it: a
    /// compaction, or a client that dropped history. Nothing was removed for
    /// the first time, and the next turn counts from the lower base rather
    /// than waiting for the old high-water mark to be re-reached.
    pub fn novel(&self, conversation_key: Option<&str>, cumulative: Option<i64>) -> Option<i64> {
        let key = conversation_key.filter(|k| !k.is_empty())?;
        let total = cumulative?.max(0);
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = seen
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, total)| *total)
            .unwrap_or(0);
        seen.retain(|(k, _)| k != key);
        seen.push_back((key.to_string(), total));
        while seen.len() > self.max_conversations {
            seen.pop_front();
        }
        Some((total - previous).max(0))
    }

    /// Forget every conversation. Test helper only.
    pub fn clear(&self) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.clear();
        }
    }
}

impl Default for ConversationSavings {
    fn default() -> Self {
        Self::new()
    }
}

static LEDGER: OnceLock<Mutex<ConversationSavings>> = OnceLock::new();

/// Process-wide ledger. One conversation spans many requests.
pub fn conversation_ledger() -> &'static Mutex<ConversationSavings> {
    LEDGER.get_or_init(|| Mutex::new(ConversationSavings::new()))
}

/// Forget every conversation. Test helper only.
pub fn reset_conversation_ledger() {
    if let Some(ledger) = LEDGER.get() {
        if let Ok(guard) = ledger.lock() {
            guard.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn responses_body() -> Value {
        json!({
            "model": "gpt-5.6",
            "input": [{"role": "user", "content": "hi"}],
            "conversation_id": "conv-1",
        })
    }

    #[test]
    fn key_from_top_level_conversation_id() {
        let key = savings_conversation_key(&responses_body(), None).expect("key");
        assert_eq!(key.len(), 64, "sha256 hex: {key}");
        // Stable: same conversation derives the same key.
        assert_eq!(
            key,
            savings_conversation_key(&responses_body(), None).unwrap()
        );
        // Different conversations diverge.
        let other = json!({
            "model": "gpt-5.6",
            "input": [{"role": "user", "content": "hi"}],
            "conversation_id": "conv-2",
        });
        assert_ne!(key, savings_conversation_key(&other, None).unwrap());
    }

    #[test]
    fn key_from_metadata_and_session_fallback() {
        let meta = json!({
            "input": "hi",
            "metadata": {"codex_session_id": "sess-9"},
        });
        assert!(savings_conversation_key(&meta, None).is_some());
        let bare = json!({"input": "hi"});
        assert!(savings_conversation_key(&bare, None).is_none());
        assert!(savings_conversation_key(&bare, Some("s-1")).is_some());
        // Empty session fallback counts as no identity.
        assert!(savings_conversation_key(&bare, Some("")).is_none());
    }

    #[test]
    fn auto_id_alone_yields_none_but_session_fallback_applies() {
        let auto = json!({"input": "hi", "conversation_id": "auto"});
        assert!(savings_conversation_key(&auto, None).is_none());
        assert!(savings_conversation_key(&auto, Some("s-1")).is_some());
    }

    #[test]
    fn non_responses_bodies_yield_none() {
        let chat = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert!(savings_conversation_key(&chat, Some("s-1")).is_none());
        let anthropic = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"session_id": "x"},
        });
        assert!(savings_conversation_key(&anthropic, None).is_none());
    }

    #[test]
    fn incremental_inputs_yield_none() {
        for body in [
            json!({"input": "hi", "conversation_id": "c", "previous_response_id": "r-1"}),
            json!({"input": "hi", "conversation_id": "c", "conversation": "conv-abc"}),
        ] {
            assert!(
                savings_conversation_key(&body, None).is_none(),
                "incremental input must book per request: {body}"
            );
        }
    }

    #[test]
    fn unwraps_response_create_envelope() {
        let body = json!({
            "type": "response.create",
            "response": {"input": "hi", "thread_id": "t-3"},
        });
        assert!(savings_conversation_key(&body, None).is_some());
    }

    #[test]
    fn novel_first_turn_books_in_full_then_deltas() {
        let ledger = ConversationSavings::new();
        assert_eq!(ledger.novel(Some("k"), Some(100)), Some(100));
        assert_eq!(ledger.novel(Some("k"), Some(150)), Some(50));
        assert_eq!(ledger.novel(Some("k"), Some(150)), Some(0));
        // Independent conversations do not share a running total.
        assert_eq!(ledger.novel(Some("other"), Some(40)), Some(40));
    }

    #[test]
    fn novel_shrunk_transcript_rebases_without_backpay() {
        let ledger = ConversationSavings::new();
        assert_eq!(ledger.novel(Some("k"), Some(100)), Some(100));
        // Compaction shrank the transcript: nothing novel, base resets.
        assert_eq!(ledger.novel(Some("k"), Some(30)), Some(0));
        // Next turn counts from the lower base.
        assert_eq!(ledger.novel(Some("k"), Some(45)), Some(15));
    }

    #[test]
    fn novel_without_key_or_total_falls_back() {
        let ledger = ConversationSavings::new();
        assert_eq!(ledger.novel(None, Some(100)), None);
        assert_eq!(ledger.novel(Some("k"), None), None);
        assert_eq!(ledger.novel(Some(""), Some(100)), None);
        // Negative totals clamp to zero, never a negative booking.
        assert_eq!(ledger.novel(Some("k"), Some(-5)), Some(0));
    }

    #[test]
    fn ledger_evicts_oldest_past_capacity() {
        let ledger = ConversationSavings::with_capacity(2);
        assert_eq!(ledger.novel(Some("a"), Some(10)), Some(10));
        assert_eq!(ledger.novel(Some("b"), Some(20)), Some(20));
        assert_eq!(ledger.novel(Some("c"), Some(30)), Some(30));
        // `a` was forgotten: a still-live `a` restarts from zero (bounded,
        // one-off overcount) rather than growing memory without bound.
        assert_eq!(ledger.novel(Some("a"), Some(10)), Some(10));
    }
}

//! Latest Codex quota snapshot, for the statusline.
//!
//! Claude Code renders its own `rate_limits` in the statusline, but that field
//! only populates for Anthropic subscription auth — there is no hook to fill it
//! for a routed model. So the proxy keeps the last snapshot it saw and serves
//! it at `GET /codex-limits`, and the statusline script renders that segment
//! itself when a codex model is active.
//!
//! Two sources feed it, because the backend is undocumented and has moved
//! before: the `x-codex-*` response headers, and any `rate_limits` object that
//! appears in the SSE stream. Whichever arrives is recorded; neither is
//! required. Nothing here is on the byte path — a missing snapshot costs a
//! statusline segment, never a request.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// What the proxy last observed about the account's Codex quota.
#[derive(Debug, Clone, Default)]
pub struct CodexRateLimitSnapshot {
    /// Unix seconds when this was observed, so a stale segment can be aged out.
    pub observed_at: u64,
    /// The client-facing model alias the turn was routed for.
    pub model: String,
    /// Every `x-codex-*` response header, verbatim.
    pub headers: BTreeMap<String, String>,
    /// A `rate_limits` object lifted from the stream, when one appeared.
    pub rate_limits: Option<Value>,
}

impl CodexRateLimitSnapshot {
    pub fn to_json(&self) -> Value {
        json!({
            "observed_at": self.observed_at,
            "age_seconds": now_unix().saturating_sub(self.observed_at),
            "model": self.model,
            "headers": self.headers,
            "rate_limits": self.rate_limits,
        })
    }
}

/// Cloneable handle to the snapshot. Cheap to clone into every handler.
#[derive(Debug, Clone, Default)]
pub struct CodexRateLimitStore(Arc<RwLock<Option<CodexRateLimitSnapshot>>>);

impl CodexRateLimitStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the `x-codex-*` headers from an upstream response.
    ///
    /// Merges rather than replaces: headers and the stream object arrive at
    /// different moments in the same turn, and a later turn that carries only
    /// one of them should not blank the other.
    pub fn record_headers(&self, model: &str, headers: &http::HeaderMap) -> bool {
        let collected: BTreeMap<String, String> = headers
            .iter()
            .filter(|(name, _)| {
                let n = name.as_str();
                n.starts_with("x-codex-") || n.contains("ratelimit") || n.contains("rate-limit")
            })
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_string(), v.to_string()))
            })
            .collect();
        if collected.is_empty() {
            return false;
        }
        if let Ok(mut slot) = self.0.write() {
            let entry = slot.get_or_insert_with(CodexRateLimitSnapshot::default);
            entry.observed_at = now_unix();
            entry.model = model.to_string();
            entry.headers = collected;
            return true;
        }
        false
    }

    /// Record a `rate_limits` object seen in the stream.
    pub fn record_rate_limits(&self, model: &str, rate_limits: Value) {
        if rate_limits.is_null() {
            return;
        }
        if let Ok(mut slot) = self.0.write() {
            let entry = slot.get_or_insert_with(CodexRateLimitSnapshot::default);
            entry.observed_at = now_unix();
            entry.model = model.to_string();
            entry.rate_limits = Some(rate_limits);
        }
    }

    pub fn snapshot(&self) -> Option<CodexRateLimitSnapshot> {
        self.0.read().ok().and_then(|slot| slot.clone())
    }
}

/// Pull a `rate_limits` object out of an arbitrary SSE payload.
///
/// The backend has carried this at more than one depth, so this looks at the
/// top level and one level down rather than hard-coding a path that a change
/// upstream would silently break.
pub fn extract_rate_limits(chunk: &Value) -> Option<Value> {
    if let Some(found) = chunk.get("rate_limits") {
        if !found.is_null() {
            return Some(found.clone());
        }
    }
    for key in ["response", "info", "item"] {
        if let Some(found) = chunk.get(key).and_then(|v| v.get("rate_limits")) {
            if !found.is_null() {
                return Some(found.clone());
            }
        }
    }
    None
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_codex_and_ratelimit_headers_are_kept() {
        let store = CodexRateLimitStore::new();
        let mut headers = http::HeaderMap::new();
        headers.insert("x-codex-active-limit", "codex".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "42".parse().unwrap());
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        store.record_headers("claude-codex-5.6", &headers);

        let snap = store.snapshot().unwrap();
        assert_eq!(snap.model, "claude-codex-5.6");
        assert_eq!(snap.headers.len(), 2);
        assert_eq!(snap.headers["x-codex-active-limit"], "codex");
        assert_eq!(snap.headers["x-ratelimit-remaining"], "42");
        assert!(!snap.headers.contains_key("authorization"));
    }

    #[test]
    fn nothing_is_recorded_when_no_headers_match() {
        let store = CodexRateLimitStore::new();
        let mut headers = http::HeaderMap::new();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        store.record_headers("m", &headers);
        assert!(store.snapshot().is_none());
    }

    #[test]
    fn headers_and_stream_object_merge_within_a_turn() {
        let store = CodexRateLimitStore::new();
        let mut headers = http::HeaderMap::new();
        headers.insert("x-codex-active-limit", "codex".parse().unwrap());
        store.record_headers("m", &headers);
        store.record_rate_limits("m", json!({"primary": {"used_percent": 3.0}}));

        let snap = store.snapshot().unwrap();
        assert_eq!(snap.headers["x-codex-active-limit"], "codex");
        assert_eq!(snap.rate_limits.unwrap()["primary"]["used_percent"], 3.0);
    }

    #[test]
    fn rate_limits_are_found_at_either_depth() {
        let want = json!({"primary": {"used_percent": 7.5}});
        assert_eq!(
            extract_rate_limits(&json!({"rate_limits": want.clone()})),
            Some(want.clone())
        );
        assert_eq!(
            extract_rate_limits(&json!({"response": {"rate_limits": want.clone()}})),
            Some(want.clone())
        );
        assert_eq!(
            extract_rate_limits(&json!({"info": {"rate_limits": want.clone()}})),
            Some(want)
        );
        assert_eq!(extract_rate_limits(&json!({"usage": {}})), None);
        assert_eq!(extract_rate_limits(&json!({"rate_limits": null})), None);
    }
}

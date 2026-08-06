//! Request logger for the Headroom proxy — base64 image redaction + bounded
//! deque of recent request entries.
//!
//! Mirrors Python's `headroom/proxy/request_logger.py` (`RequestLogger` class)
//! which stores `RequestLog` dataclass entries in a `deque(maxlen=10_000)`
//! and optionally writes JSONL.

use serde_json::Value;

/// Base64 redaction threshold (bytes). Strings shorter than this are
/// never redacted even inside image-bearing paths.
pub const IMAGE_BASE64_REDACT_THRESHOLD_BYTES: usize = 1024;

/// Replacement marker format. Operators grep for `<image:base64-redacted` to
/// count redactions; `bytes=` reports the UTF-8 byte length.
pub fn image_base64_replacement(byte_len: usize) -> String {
    format!("<image:base64-redacted bytes={byte_len}>")
}

/// JSON field names that carry image payloads in either the Anthropic or
/// OpenAI shapes. Strings reached via one of these keys (at any depth)
/// are eligible for the redaction heuristic.
pub const IMAGE_BEARING_FIELD_NAMES: &[&str] = &[
    "data",      // Anthropic image-block shape: source.data
    "url",       // OpenAI vision shape: image_url.url
    "image_url", // OpenAI Responses input_image
    "image",     // Some SDKs put the URL under "image" directly
];

/// Explicit data-URL MIME prefix.
const DATA_IMAGE_URL_PREFIX: &str = "data:image/";

/// Return true if `value` is an over-threshold base64 image.
fn is_base64_image_payload(value: &str) -> bool {
    value.len() >= IMAGE_BASE64_REDACT_THRESHOLD_BYTES && value.starts_with(DATA_IMAGE_URL_PREFIX)
}

/// Recursively redact base64-image payloads in a JSON value.
///
/// Returns a new structure with any over-threshold base64 string replaced
/// by the placeholder. Non-string, non-container values pass through unchanged.
///
/// `in_image_path` is true when the caller reached this value via one of
/// the `IMAGE_BEARING_FIELD_NAMES` keys.
pub fn redact_value(value: &Value, in_image_path: bool, counter: &mut u64) -> Value {
    match value {
        Value::String(s) => {
            let should_redact = is_base64_image_payload(s)
                || (in_image_path && s.len() >= IMAGE_BASE64_REDACT_THRESHOLD_BYTES);
            if should_redact {
                *counter += 1;
                let byte_len = s.len(); // UTF-8 byte length
                Value::String(image_base64_replacement(byte_len))
            } else {
                value.clone()
            }
        }
        Value::Object(map) => {
            let mut new_map = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let child_in_image = IMAGE_BEARING_FIELD_NAMES.contains(&k.as_str());
                new_map.insert(k.clone(), redact_value(v, child_in_image, counter));
            }
            Value::Object(new_map)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| redact_value(v, in_image_path, counter))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// Public entry point for base64-image redaction.
///
/// Walks `payload` and replaces any over-threshold base64 string with a
/// size-only placeholder. Idempotent — applying twice yields the same structure.
///
/// Returns `(redacted_payload, redaction_count)`.
pub fn redact_image_base64(payload: &Value) -> (Value, u64) {
    let mut counter = 0;
    let redacted = redact_value(payload, false, &mut counter);
    (redacted, counter)
}

// ── Request logger (bounded deque) ──

/// A single log entry stored by the logger. Mirrors Python's
/// `headroom.proxy.models.RequestLog` dataclass.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RequestLogEntry {
    pub request_id: String,
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub input_tokens_original: i64,
    pub input_tokens_optimized: i64,
    pub output_tokens: i64,
    pub tokens_saved: i64,
    pub savings_percent: f64,
    pub total_latency_ms: f64,
    pub tags: std::collections::HashMap<String, String>,
    pub cache_hit: bool,
    pub transforms_applied: Vec<String>,
    pub turn_id: Option<String>,
    pub error: Option<String>,
}

impl RequestLogEntry {
    /// Build from a [`headroom_core::request_outcome::RequestOutcome`].
    pub fn from_outcome(outcome: &headroom_core::request_outcome::RequestOutcome) -> Self {
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false);
        let savings_pct = outcome.savings_pct();
        Self {
            request_id: outcome.request_id.clone(),
            timestamp,
            provider: outcome.provider.clone(),
            model: outcome.model.clone(),
            input_tokens_original: outcome.original_tokens,
            input_tokens_optimized: outcome.optimized_tokens,
            output_tokens: outcome.output_tokens,
            tokens_saved: outcome.tokens_saved,
            savings_percent: savings_pct,
            total_latency_ms: outcome.total_latency_ms,
            tags: outcome.tags.clone(),
            cache_hit: outcome.cache_hit(),
            transforms_applied: outcome.transforms_applied.clone(),
            turn_id: outcome.turn_id.clone(),
            error: None,
        }
    }
}

/// Thread-safe bounded request logger. Stores the most recent
/// `MAX_ENTRIES` request log entries in a FIFO deque.
pub struct RequestLogger {
    max_entries: usize,
    inner: std::sync::Mutex<std::collections::VecDeque<RequestLogEntry>>,
}

impl RequestLogger {
    /// Default max entries, matching Python's `MAX_LOG_ENTRIES = 10_000`.
    pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

    pub fn new(max_entries: Option<usize>) -> Self {
        Self {
            max_entries: max_entries.unwrap_or(Self::DEFAULT_MAX_ENTRIES),
            inner: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(
                max_entries.unwrap_or(Self::DEFAULT_MAX_ENTRIES).min(1024),
            )),
        }
    }

    /// Append a log entry. When the deque is full the oldest entry is evicted.
    pub fn log(&self, entry: RequestLogEntry) {
        let mut deque = self.inner.lock().expect("request_logger lock poisoned");
        if deque.len() >= self.max_entries {
            deque.pop_front();
        }
        deque.push_back(entry);
    }

    /// Return the most recent `n` entries (without full message bodies,
    /// matching Python's `get_recent`).
    pub fn get_recent(&self, n: usize) -> Vec<RequestLogEntry> {
        let deque = self.inner.lock().expect("request_logger lock poisoned");
        let len = deque.len();
        let skip = len.saturating_sub(n);
        deque.iter().skip(skip).cloned().collect()
    }

    /// Number of entries currently stored.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("request_logger lock poisoned")
            .len()
    }

    /// True when no entries are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for RequestLogger {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn long_base64() -> String {
        "A".repeat(2000)
    }

    fn short_base64() -> String {
        "A".repeat(100)
    }

    // ── is_base64_image_payload ───────────────────────────────────

    #[test]
    fn long_data_url_is_image() {
        let val = format!("data:image/png;base64,{}", long_base64());
        assert!(is_base64_image_payload(&val));
    }

    #[test]
    fn short_data_url_not_image() {
        let val = format!("data:image/png;base64,{}", short_base64());
        assert!(!is_base64_image_payload(&val));
    }

    #[test]
    fn non_data_url_not_image() {
        assert!(!is_base64_image_payload(&long_base64()));
    }

    // ── redact_value ──────────────────────────────────────────────

    #[test]
    fn redacts_data_url_at_any_path() {
        let data_url = format!("data:image/png;base64,{}", long_base64());
        let payload = json!({"random_field": data_url});
        let mut counter = 0;
        let result = redact_value(&payload, false, &mut counter);
        assert_eq!(counter, 1);
        assert!(result["random_field"]
            .as_str()
            .unwrap()
            .starts_with("<image:base64-redacted"));
    }

    #[test]
    fn redacts_in_image_bearing_path() {
        let payload = json!({"source": {"type": "base64", "data": long_base64()}});
        let mut counter = 0;
        let result = redact_value(&payload, false, &mut counter);
        assert_eq!(counter, 1);
        assert!(result["source"]["data"]
            .as_str()
            .unwrap()
            .starts_with("<image:base64-redacted"));
    }

    #[test]
    fn no_redact_short_string_in_image_path() {
        let payload = json!({"data": short_base64()});
        let mut counter = 0;
        let result = redact_value(&payload, false, &mut counter);
        assert_eq!(counter, 0);
        assert_eq!(result["data"].as_str().unwrap(), short_base64());
    }

    #[test]
    fn no_redact_outside_image_path() {
        let payload = json!({"field": long_base64()});
        let mut counter = 0;
        let result = redact_value(&payload, false, &mut counter);
        assert_eq!(counter, 0);
        assert_eq!(result["field"].as_str().unwrap(), long_base64());
    }

    #[test]
    fn redacts_in_array() {
        let data_url = format!("data:image/jpeg;base64,{}", long_base64());
        let payload = json!([data_url, "normal"]);
        let mut counter = 0;
        let result = redact_value(&payload, false, &mut counter);
        assert_eq!(counter, 1);
        assert!(result[0]
            .as_str()
            .unwrap()
            .starts_with("<image:base64-redacted"));
        assert_eq!(result[1].as_str().unwrap(), "normal");
    }

    #[test]
    fn idempotent() {
        let data_url = format!("data:image/png;base64,{}", long_base64());
        let payload = json!({"source": {"data": data_url}});
        let (first, c1) = redact_image_base64(&payload);
        let (second, c2) = redact_image_base64(&first);
        assert_eq!(c1, 1);
        assert_eq!(c2, 0);
        assert_eq!(first, second);
    }

    #[test]
    fn passthrough_non_string_values() {
        let payload = json!({"num": 42, "bool": true, "null": null});
        let (result, count) = redact_image_base64(&payload);
        assert_eq!(count, 0);
        assert_eq!(result, payload);
    }

    #[test]
    fn openai_image_url_shape() {
        let data_url = format!("data:image/png;base64,{}", long_base64());
        let payload = json!({"type": "image_url", "image_url": {"url": data_url}});
        let mut counter = 0;
        let result = redact_value(&payload, false, &mut counter);
        assert_eq!(counter, 1);
        assert!(result["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("<image:base64-redacted"));
    }

    #[test]
    fn replacement_reports_utf8_byte_length() {
        // "data:image/png;base64," is 22 bytes, plus 2000 A's = 2022 total
        let data_url = format!("data:image/png;base64,{}", long_base64());
        let payload = json!({"url": data_url.clone()});
        let (result, _) = redact_image_base64(&payload);
        let replacement = result["url"].as_str().unwrap();
        let expected_bytes = data_url.len();
        assert!(replacement.contains(&format!("bytes={expected_bytes}")));
    }

    // ── RequestLogger log + get_recent ────────────────────────────

    fn make_entry(id: &str) -> RequestLogEntry {
        RequestLogEntry {
            request_id: id.to_string(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            provider: "anthropic".into(),
            model: "claude-3".into(),
            input_tokens_original: 100,
            input_tokens_optimized: 80,
            output_tokens: 50,
            tokens_saved: 20,
            savings_percent: 20.0,
            total_latency_ms: 42.0,
            tags: std::collections::HashMap::new(),
            cache_hit: false,
            transforms_applied: vec![],
            turn_id: None,
            error: None,
        }
    }

    #[test]
    fn log_and_get_recent_basic() {
        let logger = RequestLogger::new(None);
        for i in 0..5 {
            logger.log(make_entry(&format!("req-{i}")));
        }
        let recent = logger.get_recent(3);
        assert_eq!(recent.len(), 3);
        // get_recent returns chronological order (oldest first in the window)
        assert_eq!(recent[0].request_id, "req-2");
        assert_eq!(recent[1].request_id, "req-3");
        assert_eq!(recent[2].request_id, "req-4");
    }

    #[test]
    fn get_recent_bounded() {
        let logger = RequestLogger::new(None);
        for i in 0..200 {
            logger.log(make_entry(&format!("req-{i}")));
        }
        let recent = logger.get_recent(100);
        assert!(recent.len() <= 100);
    }

    #[test]
    fn get_recent_zero_returns_empty() {
        let logger = RequestLogger::new(None);
        logger.log(make_entry("req-0"));
        logger.log(make_entry("req-1"));
        let recent = logger.get_recent(0);
        assert!(recent.is_empty());
    }

    #[test]
    fn get_recent_over_count_returns_all() {
        let logger = RequestLogger::new(None);
        for i in 0..3 {
            logger.log(make_entry(&format!("req-{i}")));
        }
        let recent = logger.get_recent(100);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].request_id, "req-0");
        assert_eq!(recent[2].request_id, "req-2");
    }

    #[test]
    fn log_entry_json_shape() {
        let entry = RequestLogEntry {
            request_id: "abc-123".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            provider: "anthropic".into(),
            model: "claude-3-opus".into(),
            input_tokens_original: 1000,
            input_tokens_optimized: 800,
            output_tokens: 500,
            tokens_saved: 200,
            savings_percent: 20.0,
            total_latency_ms: 42.5,
            tags: std::collections::HashMap::new(),
            cache_hit: true,
            transforms_applied: vec!["diff_compressor".into()],
            turn_id: Some("turn-1".into()),
            error: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["request_id"], "abc-123");
        assert_eq!(json["provider"], "anthropic");
        assert_eq!(json["model"], "claude-3-opus");
        assert_eq!(json["input_tokens_original"], 1000);
        assert_eq!(json["tokens_saved"], 200);
        assert_eq!(json["savings_percent"], 20.0);
        assert_eq!(json["cache_hit"], true);
        assert_eq!(json["transforms_applied"][0], "diff_compressor");
        assert_eq!(json["turn_id"], "turn-1");
    }

    #[test]
    fn len_and_is_empty() {
        let logger = RequestLogger::new(None);
        assert!(logger.is_empty());
        assert_eq!(logger.len(), 0);
        logger.log(make_entry("req-1"));
        assert!(!logger.is_empty());
        assert_eq!(logger.len(), 1);
    }

    // ─── Integration: full pipeline ───────────────────────────────────

    #[test]
    fn integration_log_windowed_retrieval() {
        let logger = RequestLogger::new(Some(10)); // bounded to 10

        // Log 15 entries — only the last 10 should survive
        for i in 0..15 {
            logger.log(make_entry(&format!("req-{i:03}")));
        }

        assert_eq!(logger.len(), 10);
        let recent = logger.get_recent(5);
        assert_eq!(recent.len(), 5);
        // Window should contain entries 10-14
        assert_eq!(recent[0].request_id, "req-010");
        assert_eq!(recent[4].request_id, "req-014");
    }

    #[test]
    fn integration_concurrent_logging() {
        use std::sync::Arc;
        use std::thread;

        let logger = Arc::new(RequestLogger::new(None));
        let mut handles = vec![];

        for t in 0..4 {
            let logger = logger.clone();
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    let mut entry = make_entry(&format!("t{t}-req-{i}"));
                    entry.model = format!("model-{t}");
                    logger.log(entry);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(logger.len(), 400);
        let recent = logger.get_recent(400);
        assert_eq!(recent.len(), 400);
        // All entries should be present across threads
        let models: Vec<&str> = recent.iter().map(|e| e.model.as_str()).collect();
        assert!(models.contains(&"model-0"));
        assert!(models.contains(&"model-1"));
        assert!(models.contains(&"model-2"));
        assert!(models.contains(&"model-3"));
    }

    #[test]
    fn integration_json_roundtrip() {
        let entry = RequestLogEntry {
            request_id: "roundtrip-test".into(),
            timestamp: "2025-06-15T12:00:00Z".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            input_tokens_original: 5000,
            input_tokens_optimized: 4000,
            output_tokens: 2000,
            tokens_saved: 1000,
            savings_percent: 20.0,
            total_latency_ms: 123.45,
            tags: {
                let mut m = std::collections::HashMap::new();
                m.insert("project".into(), "test-project".into());
                m.insert("phase".into(), "C".into());
                m
            },
            cache_hit: true,
            transforms_applied: vec!["diff_compressor".into(), "smart_crusher".into()],
            turn_id: Some("turn-42".into()),
            error: None,
        };

        let json_str = serde_json::to_string(&entry).unwrap();
        let parsed: RequestLogEntry = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.request_id, entry.request_id);
        assert_eq!(parsed.provider, "openai");
        assert_eq!(parsed.model, "gpt-4o");
        assert_eq!(parsed.input_tokens_original, 5000);
        assert_eq!(parsed.tokens_saved, 1000);
        assert_eq!(parsed.savings_percent, 20.0);
        assert_eq!(parsed.cache_hit, true);
        assert_eq!(parsed.transforms_applied.len(), 2);
        assert_eq!(parsed.turn_id.as_deref(), Some("turn-42"));
        assert!(parsed.error.is_none());
        assert_eq!(parsed.tags.get("project").unwrap(), "test-project");
    }

    #[test]
    fn integration_error_entry_roundtrip() {
        let entry = RequestLogEntry {
            request_id: "error-test".into(),
            timestamp: "2025-06-15T12:00:00Z".into(),
            provider: "anthropic".into(),
            model: "claude-3".into(),
            input_tokens_original: 0,
            input_tokens_optimized: 0,
            output_tokens: 0,
            tokens_saved: 0,
            savings_percent: 0.0,
            total_latency_ms: 5000.0,
            tags: std::collections::HashMap::new(),
            cache_hit: false,
            transforms_applied: vec![],
            turn_id: None,
            error: Some("upstream timeout after 5000ms".into()),
        };

        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["error"], "upstream timeout after 5000ms");
        assert!(json["turn_id"].is_null());
    }

    #[test]
    fn from_outcome_converts_request_outcome() {
        use headroom_core::request_outcome::RequestOutcome;

        let outcome = RequestOutcome {
            request_id: "req-from-outcome".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            status_code: 200,
            original_tokens: 1000,
            optimized_tokens: 400,
            output_tokens: 200,
            tokens_saved: 600,
            attempted_input_tokens: 1000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_5m_tokens: 0,
            cache_write_1h_tokens: 0,
            uncached_input_tokens: 1000,
            cache_inferred: false,
            from_response_cache: false,
            total_latency_ms: 150.5,
            overhead_ms: 12.3,
            ttfb_ms: 45.0,
            pipeline_timing: None,
            transforms_applied: vec!["kompress:user:0.4".into()],
            waste_signals: None,
            num_messages: 5,
            turn_id: Some("turn-42".into()),
            request_messages: None,
            compressed_messages: None,
            tags: std::collections::HashMap::from([("env".into(), "test".into())]),
            client: None,
            project: None,
        };

        let entry = RequestLogEntry::from_outcome(&outcome);

        assert_eq!(entry.request_id, "req-from-outcome");
        assert_eq!(entry.provider, "anthropic");
        assert_eq!(entry.model, "claude-sonnet-4-20250514");
        assert_eq!(entry.input_tokens_original, 1000);
        assert_eq!(entry.input_tokens_optimized, 400);
        assert_eq!(entry.output_tokens, 200);
        assert_eq!(entry.tokens_saved, 600);
        assert!((entry.savings_percent - 60.0).abs() < 0.1);
        assert_eq!(entry.total_latency_ms, 150.5);
        assert_eq!(entry.tags.get("env").unwrap(), "test");
        assert!(!entry.cache_hit);
        assert_eq!(entry.transforms_applied, vec!["kompress:user:0.4"]);
        assert_eq!(entry.turn_id.as_deref(), Some("turn-42"));
        assert!(entry.error.is_none());
        assert!(!entry.timestamp.is_empty());
    }

    // ─── Redaction parity with the extracted Python policy ───────────────
    //
    // Upstream moved this logic into `request_log_redaction_policy.py` without
    // changing behaviour. This pins that Rust still agrees with the extracted
    // module, so the reorganization stays a no-op for the Rust port.

    #[test]
    fn redaction_matches_extracted_python_policy() {
        let big = format!("data:image/png;base64,{}", "A".repeat(1200));
        let bare = "B".repeat(1200);
        let payload = serde_json::json!({
            "image_url": {"url": big},
            "note": bare,
            "nested": {"source": {"data": bare}},
        });
        let mut count = 0u64;
        let out = redact_value(&payload, false, &mut count);

        // The data URL redacts, reporting its UTF-8 byte length.
        assert_eq!(
            out["image_url"]["url"],
            serde_json::json!("<image:base64-redacted bytes=1222>")
        );
        // Bare base64 OUTSIDE an image-bearing path is left alone — the M2
        // remediation: the old density heuristic over-fired on encrypted
        // blobs, signed tokens and minified JSON.
        assert_eq!(out["note"], serde_json::json!(bare));
        // ... but the same bytes UNDER `source.data` do redact, because the
        // caller established the image-bearing path.
        assert_eq!(
            out["nested"]["source"]["data"],
            serde_json::json!("<image:base64-redacted bytes=1200>")
        );
        assert_eq!(count, 2, "Python reports redactions = 2");
    }

    #[test]
    fn under_threshold_data_urls_are_not_redacted() {
        let small = "data:image/png;base64,short";
        let payload = serde_json::json!({"image_url": {"url": small}});
        let mut count = 0u64;
        let out = redact_value(&payload, false, &mut count);
        assert_eq!(out["image_url"]["url"], serde_json::json!(small));
        assert_eq!(count, 0);
    }
}

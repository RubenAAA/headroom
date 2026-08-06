//! Shared tool-schema compaction for the proxy handlers.
//!
//! Strips JSON Schema annotation keys (`$schema`, `title`, `examples`, …)
//! and normalises description whitespace to cut the token cost of tool
//! definitions without changing what they accept. Both the OpenAI and the
//! Anthropic handler call the same logic from here.
//!
//! **Layer 2 — description truncation.** Trims tool and parameter
//! `description` strings to a maximum length, keeping the first complete
//! sentence so the model can still pick the right tool. Opt in via
//! `HEADROOM_TOOL_DESC_MAX_CHARS` (default `0` = off).
//!
//! **Layer 3 — semantic parameter removal.** When a parameter name speaks
//! for itself (`query`, `owner`, `repo`), the `description` adds little.
//! Opt in via `HEADROOM_TOOL_DESC_STRIP_SEMANTIC=1` (default off).
//!
//! **Caching.** Results are keyed by the digest of the tools array plus the
//! compaction config. While the tools don't change, the cached result is
//! reused instead of walking 141+ schemas on every request.
//!
//! Ports Python's `headroom/proxy/tool_schema_compaction.py`.

use std::sync::{LazyLock, Mutex};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use regex_lite::Regex;

/// JSON Schema annotation keys — not constraints. Dropping them does not
/// change the set of valid inputs.
pub const TOOL_SCHEMA_DROP_KEYS: [&str; 10] = [
    "$id",
    "$schema",
    "$comment",
    "deprecated",
    "examples",
    "example",
    "markdownDescription",
    "readOnly",
    "title",
    "writeOnly",
];

/// Parameter names that speak for themselves. When Layer 3 is on, their
/// `description` is dropped rather than truncated.
const SEMANTIC_PARAM_NAMES: &[&str] = &[
    "query",
    "search",
    "filter",
    "sort",
    "order",
    "limit",
    "offset",
    "page",
    "per_page",
    "perpage",
    "cursor",
    "after",
    "before",
    "owner",
    "repo",
    "repository",
    "org",
    "organization",
    "user",
    "username",
    "email",
    "name",
    "title",
    "description",
    "id",
    "number",
    "count",
    "url",
    "path",
    "file",
    "filename",
    "branch",
    "tag",
    "sha",
    "commit",
    "ref",
    "key",
    "token",
    "type",
    "format",
    "state",
    "status",
    "action",
    "method",
    "body",
    "content",
    "message",
    "text",
    "comment",
    "note",
    "start",
    "end",
    "from",
    "to",
    "direction",
    "ascending",
    "dry_run",
    "verbose",
    "force",
    "recursive",
    "include",
    "exclude",
    "pattern",
    "regex",
    "since",
    "until",
];

fn is_drop_key(key: &str) -> bool {
    TOOL_SCHEMA_DROP_KEYS.contains(&key)
}

/// Whether a parameter name is self-explanatory (Layer 3).
fn is_semantic_param_name(name: &str) -> bool {
    let normalized = name.to_lowercase().replace('-', "_");
    SEMANTIC_PARAM_NAMES.contains(&normalized.as_str())
}

// ---------------------------------------------------------------------------
// Env-var helpers
// ---------------------------------------------------------------------------

static TOOL_DESC_MAX_CHARS: Mutex<Option<i64>> = Mutex::new(None);
static STRIP_SEMANTIC: Mutex<Option<bool>> = Mutex::new(None);

/// The configured max description length, read once per process.
///
/// `HEADROOM_TOOL_DESC_MAX_CHARS=0` (the default) disables truncation. A
/// value that does not parse as an integer also yields `0`.
pub fn tool_desc_max_chars() -> i64 {
    let mut slot = TOOL_DESC_MAX_CHARS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        let parsed = std::env::var("HEADROOM_TOOL_DESC_MAX_CHARS")
            .ok()
            .map_or(0, |raw| raw.trim().parse::<i64>().unwrap_or(0));
        *slot = Some(parsed);
    }
    slot.unwrap_or(0)
}

/// Whether Layer 3 (semantic param removal) is enabled, read once per process.
pub fn strip_semantic_params() -> bool {
    let mut slot = STRIP_SEMANTIC.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(std::env::var("HEADROOM_TOOL_DESC_STRIP_SEMANTIC").as_deref() == Ok("1"));
    }
    slot.unwrap_or(false)
}

/// Forget the memoised env values so the next read hits the environment again.
///
/// The Python module has no equivalent — its tests poke the module globals
/// directly. Tests here need the same escape hatch, so it lives behind
/// `cfg(test)`.
#[cfg(test)]
pub(crate) fn reset_env_cache() {
    *TOOL_DESC_MAX_CHARS
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    *STRIP_SEMANTIC.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Serializes tests that touch the process-global env memo and cache.
///
/// `reset_env_cache`, `std::env::set_var` and [`invalidate_cache`] are all
/// global, and the test runner is multi-threaded: without this, one test's
/// reset lands between another's set-up and its assertion. Every test that
/// mutates that state must hold this for its whole body. Poison is recovered
/// rather than propagated so one failing test doesn't cascade.
#[cfg(test)]
pub(crate) fn compaction_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Compaction cache
// ---------------------------------------------------------------------------

/// One memoised compaction result: the compacted tools plus the sizes that
/// decide whether applying it is worth it.
#[derive(Clone)]
struct CacheEntry {
    compacted: Value,
    before: usize,
    after: usize,
}

const CACHE_MAX_ENTRIES: usize = 8;

/// Insertion-ordered, so eviction drops the oldest entry — a plain FIFO, as
/// in Python where `_compaction_cache.pop(next(iter(...)))` removes the first
/// inserted key. A get does not refresh recency.
static COMPACTION_CACHE: LazyLock<Mutex<Vec<(String, CacheEntry)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Deterministic cache key from tools content plus config values.
fn cache_key(tools: &Value, config_vals: &[&str]) -> String {
    let mut canonical = String::new();
    write_canonical_json(tools, &mut canonical);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    for val in config_vals {
        hasher.update(val.as_bytes());
    }
    hex::encode(hasher.finalize())[..16].to_string()
}

/// Serialize `value` with object keys sorted, so the digest is stable even
/// though `serde_json` preserves insertion order in this workspace.
fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (idx, key) in keys.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_canonical_json(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

fn cache_get(key: &str) -> Option<CacheEntry> {
    let cache = COMPACTION_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, entry)| entry.clone())
}

fn cache_put(key: String, compacted: Value, before: usize, after: usize) {
    let mut cache = COMPACTION_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = cache.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = CacheEntry {
            compacted,
            before,
            after,
        };
        return;
    }
    if cache.len() >= CACHE_MAX_ENTRIES {
        cache.remove(0);
    }
    cache.push((
        key,
        CacheEntry {
            compacted,
            before,
            after,
        },
    ));
}

/// Clear the compaction cache (e.g. on a config change).
pub fn invalidate_cache() {
    let mut cache = COMPACTION_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.clear();
}

// ---------------------------------------------------------------------------
// Layer 1: annotation-key compaction
// ---------------------------------------------------------------------------

/// Length of the compact JSON serialisation, in characters.
///
/// Python measures `len(json.dumps(...))` on a `str`, which counts code
/// points rather than bytes despite the "bytes" naming, so this counts chars
/// to stay byte-for-byte comparable with the reference.
fn json_byte_len(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|s| s.chars().count())
        .unwrap_or(0)
}

/// Collapse every whitespace run to a single space and trim the ends.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Recursively compact a tool-schema structure.
///
/// Drops annotation keys ([`TOOL_SCHEMA_DROP_KEYS`]) unless they appear as
/// property *names* inside a `properties` object — a field literally called
/// `"title"` must survive. Normalises `description` strings by collapsing
/// whitespace.
pub fn compact_tool_schema_value(value: &Value) -> Value {
    compact_schema_value_inner(value, None)
}

fn compact_schema_value_inner(value: &Value, parent_key: Option<&str>) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| compact_schema_value_inner(item, parent_key))
                .collect(),
        ),
        Value::Object(map) => {
            let mut compacted = Map::new();
            for (key, child) in map {
                // Only drop annotation keys — never property names inside a
                // JSON Schema `properties` object.
                if parent_key != Some("properties") && is_drop_key(key) {
                    continue;
                }
                if key == "description" {
                    if let Some(text) = child.as_str() {
                        compacted.insert(key.clone(), Value::String(collapse_whitespace(text)));
                        continue;
                    }
                }
                compacted.insert(
                    key.clone(),
                    compact_schema_value_inner(child, Some(key.as_str())),
                );
            }
            Value::Object(compacted)
        }
        other => other.clone(),
    }
}

/// Compact the `tools` array in `payload`.
///
/// Returns `(payload, modified, before_chars, after_chars)`. When compaction
/// does not shrink the payload, the original is returned untouched and
/// `modified` is `false`. Results are cached by tools digest, so repeated
/// calls with the same array return the cached version straight away.
pub fn compact_tools(payload: Value) -> (Value, bool, usize, usize) {
    let Some(tools) = payload.get("tools") else {
        return (payload, false, 0, 0);
    };
    let Some(items) = tools.as_array() else {
        return (payload, false, 0, 0);
    };
    if items.is_empty() {
        return (payload, false, 0, 0);
    }

    let key = cache_key(tools, &["L1"]);
    if let Some(entry) = cache_get(&key) {
        if entry.after >= entry.before {
            return (payload, false, entry.before, entry.after);
        }
        return (
            with_tools(payload, entry.compacted),
            true,
            entry.before,
            entry.after,
        );
    }

    let compacted = compact_tool_schema_value(tools);
    let before = json_byte_len(tools);
    let after = json_byte_len(&compacted);
    cache_put(key, compacted.clone(), before, after);

    if after >= before {
        return (payload, false, before, after);
    }
    (with_tools(payload, compacted), true, before, after)
}

/// Replace `payload["tools"]`, leaving every other field in place.
fn with_tools(mut payload: Value, tools: Value) -> Value {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("tools".to_string(), tools);
    }
    payload
}

// ---------------------------------------------------------------------------
// Layer 2: description truncation
// ---------------------------------------------------------------------------

/// Shortest prefix ending in `.`, `!` or `?` followed by whitespace or the
/// end of the string.
fn first_sentence_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)^(.*?[.!?])(?:\s|$)").unwrap())
}

/// The first sentence of `text`, if it has one.
fn first_sentence(text: &str) -> Option<&str> {
    first_sentence_re()
        .captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
}

/// Truncate `desc` to `max_chars`, keeping the first complete sentence.
///
/// - `max_chars` ≤ 0 returns `desc` unchanged (feature off).
/// - Descriptions already within the budget pass through, whitespace-normalised.
/// - When the first sentence fits, keep it, and append the second when the
///   pair stays within 1.5× `max_chars`.
/// - When the first sentence alone overflows, hard-truncate and append `…`.
fn truncate_description(desc: &str, max_chars: i64) -> String {
    if max_chars <= 0 {
        return desc.to_string();
    }

    // Normalise whitespace first, mirroring Layer 1.
    let desc = collapse_whitespace(desc);
    let max = max_chars as usize;
    if desc.chars().count() <= max {
        return desc;
    }

    if let Some(first) = first_sentence(&desc) {
        let first_len = first.chars().count();
        if first_len <= max {
            let rest = desc[first.len()..].trim();
            if !rest.is_empty() {
                if let Some(second) = first_sentence(rest) {
                    let budget = (max_chars as f64 * 1.5) as usize;
                    if first_len + 1 + second.chars().count() <= budget {
                        return format!("{first} {second}");
                    }
                }
            }
            return first.to_string();
        }
    }

    // First sentence too long → hard truncation.
    let head: String = desc.chars().take(max).collect();
    format!("{}…", head.trim_end())
}

/// Recursively truncate `description` fields in a tool-schema structure.
///
/// With `strip_semantic`, a description living at
/// `properties.<name>.description` where `<name>` is self-explanatory is
/// dropped instead of truncated.
fn truncate_descriptions_in_schema(
    value: &Value,
    max_chars: i64,
    strip_semantic: bool,
    parent_key: Option<&str>,
    grandparent_key: Option<&str>,
) -> Value {
    match value {
        // Python drops the parent/grandparent keys when it recurses into a
        // list, so the ancestry restarts at every array boundary. Mirrored
        // here — otherwise a `properties` sitting directly above an array
        // would keep firing Layer 3 one level too deep.
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| {
                    truncate_descriptions_in_schema(item, max_chars, strip_semantic, None, None)
                })
                .collect(),
        ),
        Value::Object(map) => {
            let mut compacted = Map::new();
            for (key, child) in map {
                if key == "description" {
                    if let Some(text) = child.as_str() {
                        // Layer 3: drop the description on a self-explanatory
                        // param — the name alone is enough.
                        if strip_semantic
                            && grandparent_key == Some("properties")
                            && parent_key.is_some_and(is_semantic_param_name)
                        {
                            continue;
                        }
                        compacted.insert(
                            key.clone(),
                            Value::String(truncate_description(text, max_chars)),
                        );
                        continue;
                    }
                }
                compacted.insert(
                    key.clone(),
                    truncate_descriptions_in_schema(
                        child,
                        max_chars,
                        strip_semantic,
                        Some(key.as_str()),
                        parent_key,
                    ),
                );
            }
            Value::Object(compacted)
        }
        other => other.clone(),
    }
}

/// Truncate the tool descriptions in `payload` to `max_chars`.
///
/// Returns `(payload, modified, before_chars, after_chars)`. With
/// `max_chars` of 0, or when compaction does not shrink the payload, the
/// original is returned untouched.
///
/// With `HEADROOM_TOOL_DESC_STRIP_SEMANTIC=1`, descriptions on
/// self-explanatory parameters (`query`, `owner`, …) are removed outright
/// rather than truncated. Results are cached by tools digest plus config.
pub fn compact_tool_descriptions(payload: Value, max_chars: i64) -> (Value, bool, usize, usize) {
    if max_chars <= 0 {
        return (payload, false, 0, 0);
    }

    let Some(tools) = payload.get("tools") else {
        return (payload, false, 0, 0);
    };
    let Some(items) = tools.as_array() else {
        return (payload, false, 0, 0);
    };
    if items.is_empty() {
        return (payload, false, 0, 0);
    }

    let strip_sem = strip_semantic_params();
    let key = cache_key(
        tools,
        &[
            "L2",
            &max_chars.to_string(),
            if strip_sem { "true" } else { "false" },
        ],
    );
    if let Some(entry) = cache_get(&key) {
        if entry.after >= entry.before {
            return (payload, false, entry.before, entry.after);
        }
        return (
            with_tools(payload, entry.compacted),
            true,
            entry.before,
            entry.after,
        );
    }

    let compacted = truncate_descriptions_in_schema(tools, max_chars, strip_sem, None, None);
    let before = json_byte_len(tools);
    let after = json_byte_len(&compacted);
    cache_put(key, compacted.clone(), before, after);

    if after >= before {
        return (payload, false, before, after);
    }
    (with_tools(payload, compacted), true, before, after)
}

#[cfg(test)]
mod tests {
    // Every expected value below was measured by running the Python
    // reference (`headroom/proxy/tool_schema_compaction.py`) on the same
    // input, not derived by hand.
    use super::*;
    use serde_json::json;

    fn compact(value: &Value) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn semantic_names_match_python_set() {
        // `len(_SEMANTIC_PARAM_NAMES)` == 66.
        let mut sorted = SEMANTIC_PARAM_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 66);
    }

    #[test]
    fn semantic_name_normalisation() {
        // `_is_semantic_param_name('Per-Page')` -> True, `'DRY-RUN'` -> True,
        // `'widget'` -> False.
        assert!(is_semantic_param_name("Per-Page"));
        assert!(is_semantic_param_name("DRY-RUN"));
        assert!(!is_semantic_param_name("widget"));
    }

    #[test]
    fn json_byte_len_counts_chars_not_bytes() {
        // `_json_byte_len({'a': 'ü♥'})` -> 10.
        assert_eq!(json_byte_len(&json!({"a": "ü♥"})), 10);
    }

    #[test]
    fn layer1_drops_annotations_and_normalises_descriptions() {
        let _guard = compaction_test_lock();
        invalidate_cache();

        let payload = json!({
            "model": "m",
            "tools": [{
                "name": "search",
                "description": "  Search   the\n\n  web.  ",
                "$schema": "http://x",
                "title": "Search",
                "input_schema": {
                    "type": "object",
                    "title": "Args",
                    "properties": {
                        "query": {
                            "type": "string",
                            "title": "Query",
                            "description": "The\tquery",
                            "examples": ["a"]
                        },
                        "title": {"type": "string", "description": "A title"}
                    },
                    "required": ["query"]
                }
            }]
        });

        let (out, modified, before, after) = compact_tools(payload);
        // `T.compact_tools(payload)` -> (…, True, 320, 222).
        assert!(modified);
        assert_eq!((before, after), (320, 222));
        assert_eq!(
            compact(&out),
            r#"{"model":"m","tools":[{"name":"search","description":"Search the web.","input_schema":{"type":"object","properties":{"query":{"type":"string","description":"The query"},"title":{"type":"string","description":"A title"}},"required":["query"]}}]}"#
        );
    }

    #[test]
    fn layer1_leaves_clean_schemas_alone() {
        let _guard = compaction_test_lock();
        invalidate_cache();

        let payload = json!({
            "tools": [{"name": "x", "description": "Clean desc.", "input_schema": {"type": "object"}}]
        });
        let original = payload.clone();
        let (out, modified, before, after) = compact_tools(payload);
        // `T.compact_tools(p)` -> (p, False, 75, 75) — the very same object.
        assert!(!modified);
        assert_eq!((before, after), (75, 75));
        assert_eq!(out, original);
    }

    #[test]
    fn layer1_ignores_missing_or_empty_tools() {
        let _guard = compaction_test_lock();
        invalidate_cache();

        // All three return `(payload, False, 0, 0)` in Python.
        for payload in [
            json!({"model": "m"}),
            json!({"tools": []}),
            json!({"tools": "x"}),
        ] {
            let expected = payload.clone();
            let (out, modified, before, after) = compact_tools(payload);
            assert!(!modified);
            assert_eq!((before, after), (0, 0));
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn truncate_description_matches_python() {
        // Measured with `_truncate_description(desc, max_chars)`.
        let cases: [(&str, i64, &str); 11] = [
            ("Short one.", 20, "Short one."),
            (
                "First sentence here. Second one too. Third.",
                20,
                "First sentence here.",
            ),
            (
                "First sentence here. Second.",
                20,
                "First sentence here. Second.",
            ),
            (
                "Averyveryverylongsentencewithoutanybreaksatallhere and more words",
                20,
                "Averyveryverylongsen…",
            ),
            (
                "This is a really long first sentence that exceeds the budget by a lot indeed. Next.",
                30,
                "This is a really long first se…",
            ),
            (
                "No terminator at all in this string whatsoever",
                10,
                "No termina…",
            ),
            ("Multi\nline  desc.   Second sentence.", 15, "Multi line desc…"),
            ("Hi! There? Ok.", 5, "Hi!"),
            ("abc.", 0, "abc."),
            (
                "Word Word Word Word Word Word Word Word Word Word ",
                12,
                "Word Word Wo…",
            ),
            (
                "Ends with space then dot .  More text here.",
                15,
                "Ends with space…",
            ),
        ];
        for (desc, max_chars, expected) in cases {
            assert_eq!(
                truncate_description(desc, max_chars),
                expected,
                "desc={desc:?} max={max_chars}"
            );
        }
    }

    #[test]
    fn truncate_description_counts_chars_not_bytes() {
        // `_truncate_description('Ünïcödé désçrïptïön that is quite long
        // indeed here', 12)` -> 'Ünïcödé désç…'.
        assert_eq!(
            truncate_description("Ünïcödé désçrïptïön that is quite long indeed here", 12),
            "Ünïcödé désç…"
        );
    }

    #[test]
    fn layer3_strips_semantic_param_descriptions() {
        let tools = json!([{
            "name": "x",
            "description": "A long tool description sentence one. Two.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query to run against the index."},
                    "widget_id": {"type": "string", "description": "Identifier of the widget to fetch from the store."}
                }
            }
        }]);

        let stripped = truncate_descriptions_in_schema(&tools, 20, true, None, None);
        assert_eq!(
            compact(&stripped),
            r#"[{"name":"x","description":"A long tool descript…","input_schema":{"type":"object","properties":{"query":{"type":"string"},"widget_id":{"type":"string","description":"Identifier of the wi…"}}}}]"#
        );

        let kept = truncate_descriptions_in_schema(&tools, 20, false, None, None);
        assert_eq!(
            compact(&kept),
            r#"[{"name":"x","description":"A long tool descript…","input_schema":{"type":"object","properties":{"query":{"type":"string","description":"The search query to…"},"widget_id":{"type":"string","description":"Identifier of the wi…"}}}}]"#
        );
    }

    #[test]
    fn layer3_ancestry_restarts_at_array_boundaries() {
        // Python still strips here: the reset happens one level above
        // `properties`, so the grandparent is back in place by the time the
        // description is reached.
        let nested = json!([{"anyOf": [{"properties": {"query": {"description": "A really long description of the query param here."}}}]}]);
        assert_eq!(
            compact(&truncate_descriptions_in_schema(
                &nested, 20, true, None, None
            )),
            r#"[{"anyOf":[{"properties":{"query":{}}}]}]"#
        );

        let direct = json!([{"properties": {"query": {"description": "A really long description of the query param here."}}}]);
        assert_eq!(
            compact(&truncate_descriptions_in_schema(
                &direct, 20, true, None, None
            )),
            r#"[{"properties":{"query":{}}}]"#
        );
    }

    #[test]
    fn layer2_truncates_tool_descriptions() {
        let _guard = compaction_test_lock();
        std::env::remove_var("HEADROOM_TOOL_DESC_STRIP_SEMANTIC");
        reset_env_cache();
        invalidate_cache();

        let payload = json!({
            "tools": [{
                "name": "x",
                "description": "A long tool description sentence one. Two.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "The search query to run against the index."},
                        "widget_id": {"type": "string", "description": "Identifier of the widget to fetch from the store."}
                    }
                }
            }],
            "model": "m"
        });

        let (out, modified, before, after) = compact_tool_descriptions(payload, 20);
        // Python with `HEADROOM_TOOL_DESC_STRIP_SEMANTIC=0`: (…, True, 302, 231).
        assert!(modified);
        assert_eq!((before, after), (302, 231));
        assert_eq!(
            compact(&out),
            r#"{"tools":[{"name":"x","description":"A long tool descript…","input_schema":{"type":"object","properties":{"query":{"type":"string","description":"The search query to…"},"widget_id":{"type":"string","description":"Identifier of the wi…"}}}}],"model":"m"}"#
        );

        reset_env_cache();
        invalidate_cache();
    }

    #[test]
    fn layer2_is_a_no_op_without_a_budget() {
        let _guard = compaction_test_lock();
        invalidate_cache();

        let payload = json!({"tools": [{"name": "x", "description": "Clean desc."}]});
        let original = payload.clone();
        // `T.compact_tool_descriptions(p, 0)` -> (p, False, 0, 0).
        let (out, modified, before, after) = compact_tool_descriptions(payload, 0);
        assert!(!modified);
        assert_eq!((before, after), (0, 0));
        assert_eq!(out, original);

        // `T.compact_tool_descriptions(p2, 200)` -> (p2, False, 42, 42).
        let (out, modified, before, after) = compact_tool_descriptions(original.clone(), 200);
        assert!(!modified);
        assert_eq!((before, after), (42, 42));
        assert_eq!(out, original);
    }

    #[test]
    fn repeated_calls_hit_the_cache() {
        let _guard = compaction_test_lock();
        invalidate_cache();

        let payload = json!({
            "tools": [{"name": "s", "description": "Look   things   up.", "title": "S"}]
        });
        let (first, first_mod, first_before, first_after) = compact_tools(payload.clone());
        let (second, second_mod, second_before, second_after) = compact_tools(payload);
        assert!(first_mod && second_mod);
        assert_eq!(first, second);
        assert_eq!((first_before, first_after), (second_before, second_after));
    }

    #[test]
    fn cache_evicts_oldest_beyond_the_cap() {
        let _guard = compaction_test_lock();
        invalidate_cache();

        for idx in 0..(CACHE_MAX_ENTRIES + 3) {
            let payload = json!({
                "tools": [{"name": format!("tool{idx}"), "description": "Some   spaced   text.", "title": "T"}]
            });
            let (_, modified, _, _) = compact_tools(payload);
            assert!(modified);
        }
        let cache = COMPACTION_CACHE.lock().unwrap();
        assert_eq!(cache.len(), CACHE_MAX_ENTRIES);
        drop(cache);
        invalidate_cache();
    }

    #[test]
    fn invalidate_cache_empties_the_store() {
        let _guard = compaction_test_lock();
        let payload = json!({"tools": [{"name": "z", "description": "A  b.", "title": "Z"}]});
        let (_, modified, _, _) = compact_tools(payload);
        assert!(modified);
        invalidate_cache();
        assert!(COMPACTION_CACHE.lock().unwrap().is_empty());
    }

    #[test]
    fn cache_key_is_order_independent_for_object_keys() {
        let a = json!([{"a": 1, "b": 2}]);
        let b = json!([{"b": 2, "a": 1}]);
        assert_eq!(cache_key(&a, &["L1"]), cache_key(&b, &["L1"]));
        assert_ne!(cache_key(&a, &["L1"]), cache_key(&a, &["L2"]));
    }

    #[test]
    fn env_helpers_read_the_environment_once() {
        let _guard = compaction_test_lock();
        std::env::set_var("HEADROOM_TOOL_DESC_MAX_CHARS", "128");
        std::env::set_var("HEADROOM_TOOL_DESC_STRIP_SEMANTIC", "1");
        reset_env_cache();
        assert_eq!(tool_desc_max_chars(), 128);
        assert!(strip_semantic_params());

        // Memoised: later env changes are ignored until the cache resets.
        std::env::set_var("HEADROOM_TOOL_DESC_MAX_CHARS", "7");
        assert_eq!(tool_desc_max_chars(), 128);

        std::env::set_var("HEADROOM_TOOL_DESC_MAX_CHARS", "not-a-number");
        std::env::remove_var("HEADROOM_TOOL_DESC_STRIP_SEMANTIC");
        reset_env_cache();
        // Python's `int(...)` raises ValueError, caught and mapped to 0.
        assert_eq!(tool_desc_max_chars(), 0);
        assert!(!strip_semantic_params());

        std::env::remove_var("HEADROOM_TOOL_DESC_MAX_CHARS");
        reset_env_cache();
        invalidate_cache();
    }
}
